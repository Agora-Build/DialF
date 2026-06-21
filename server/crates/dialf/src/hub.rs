//! Connection hub for real phones (M2).
//!
//! Tracks each connected phone's outbound command channel and pending-ack waiters, and
//! turns an [`Action`] into a `cmd` frame whose [`PhoneToServer::Ack`] is matched back by
//! [`Hub::resolve_ack`]. The loopback phone (M1) does not go through the hub.

use std::collections::HashMap;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use tokio::sync::{mpsc, oneshot};

use crate::protocol::{Action, CmdId, ServerToPhone};

type AckTx = oneshot::Sender<Result<(), String>>;

/// One connected phone's send side + pending acks.
struct WsConn {
    tx: mpsc::Sender<ServerToPhone>,
    acks: Mutex<HashMap<CmdId, AckTx>>,
}

/// Manages all connected phone WebSocket connections.
pub struct Hub {
    conns: Mutex<HashMap<String, Arc<WsConn>>>,
    seq: AtomicU64,
    cmd_timeout: Duration,
}

impl Hub {
    /// New, empty hub with a 10s command timeout.
    pub fn new() -> Self {
        Self {
            conns: Mutex::new(HashMap::new()),
            seq: AtomicU64::new(1),
            cmd_timeout: Duration::from_secs(10),
        }
    }

    /// Register a connection's command channel; replaces any existing entry.
    pub fn register(&self, device_id: &str, tx: mpsc::Sender<ServerToPhone>) {
        let conn = Arc::new(WsConn {
            tx,
            acks: Mutex::new(HashMap::new()),
        });
        self.conns.lock().unwrap().insert(device_id.to_string(), conn);
    }

    /// Remove a connection and fail any pending acks.
    pub fn unregister(&self, device_id: &str) {
        if let Some(conn) = self.conns.lock().unwrap().remove(device_id) {
            let mut acks = conn.acks.lock().unwrap();
            for (_, tx) in acks.drain() {
                let _ = tx.send(Err("device disconnected".to_string()));
            }
        }
    }

    /// Resolve a pending command ack (called from the phone read loop).
    pub fn resolve_ack(&self, device_id: &str, cmd_id: &str, ok: bool) {
        let conn = self.conns.lock().unwrap().get(device_id).cloned();
        if let Some(conn) = conn {
            if let Some(tx) = conn.acks.lock().unwrap().remove(cmd_id) {
                let _ = tx.send(if ok {
                    Ok(())
                } else {
                    Err("phone reported failure".to_string())
                });
            }
        }
    }

    fn conn(&self, device_id: &str) -> anyhow::Result<Arc<WsConn>> {
        self.conns
            .lock()
            .unwrap()
            .get(device_id)
            .cloned()
            .ok_or_else(|| anyhow::anyhow!("device `{device_id}` not connected"))
    }

    fn next_cmd_id(&self) -> String {
        format!("c-{}", self.seq.fetch_add(1, Ordering::Relaxed))
    }

    /// Send a command and await the phone's ack (with timeout).
    pub async fn command(&self, device_id: &str, action: Action) -> anyhow::Result<()> {
        let conn = self.conn(device_id)?;
        let cmd_id = self.next_cmd_id();
        let (ack_tx, ack_rx) = oneshot::channel();
        conn.acks.lock().unwrap().insert(cmd_id.clone(), ack_tx);

        let frame = ServerToPhone::Cmd {
            cmd_id: cmd_id.clone(),
            action,
        };
        if conn.tx.send(frame).await.is_err() {
            conn.acks.lock().unwrap().remove(&cmd_id);
            anyhow::bail!("device `{device_id}` send channel closed");
        }

        match tokio::time::timeout(self.cmd_timeout, ack_rx).await {
            Ok(Ok(Ok(()))) => Ok(()),
            Ok(Ok(Err(e))) => anyhow::bail!("phone error: {e}"),
            Ok(Err(_)) => anyhow::bail!("ack channel dropped"),
            Err(_) => {
                conn.acks.lock().unwrap().remove(&cmd_id);
                anyhow::bail!("command to `{device_id}` timed out")
            }
        }
    }

    /// Send a command without awaiting the ack (e.g. auto-pickup from the read loop).
    pub async fn fire(&self, device_id: &str, action: Action) -> anyhow::Result<()> {
        let conn = self.conn(device_id)?;
        let frame = ServerToPhone::Cmd {
            cmd_id: self.next_cmd_id(),
            action,
        };
        conn.tx
            .send(frame)
            .await
            .map_err(|_| anyhow::anyhow!("device `{device_id}` send channel closed"))
    }
}

impl Default for Hub {
    fn default() -> Self {
        Self::new()
    }
}
