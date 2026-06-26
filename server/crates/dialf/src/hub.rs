//! Connection hub for real phones.
//!
//! Tracks each connected phone's outbound command channel and pending-ack waiters, and
//! turns an [`Action`] into a `cmd` frame whose [`PhoneToServer::Ack`] is matched back by
//! [`Hub::resolve_ack`].

use std::collections::HashMap;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use tokio::sync::{mpsc, oneshot, Notify};

use crate::protocol::{Action, CmdId, ServerToPhone};

type AckTx = oneshot::Sender<Result<(), String>>;

/// One connected phone's send side + pending acks.
struct WsConn {
    /// Generation: a unique id for this socket, so a later reconnection can supersede this one
    /// and a dying older socket can't clobber the current registration (see `unregister`).
    gen: u64,
    tx: mpsc::Sender<ServerToPhone>,
    acks: Mutex<HashMap<CmdId, AckTx>>,
    /// Fired when a newer connection supersedes this one, so its reader loop closes the socket
    /// instead of lingering (a half-open reader holds the TCP open forever otherwise).
    cancel: Arc<Notify>,
}

/// Manages all connected phone WebSocket connections.
pub struct Hub {
    conns: Mutex<HashMap<String, Arc<WsConn>>>,
    seq: AtomicU64,
    conn_gen: AtomicU64,
    cmd_timeout: Duration,
}

impl Hub {
    /// New, empty hub with a 10s command timeout.
    pub fn new() -> Self {
        Self {
            conns: Mutex::new(HashMap::new()),
            seq: AtomicU64::new(1),
            conn_gen: AtomicU64::new(1),
            cmd_timeout: Duration::from_secs(10),
        }
    }

    /// Register a connection's command channel; supersedes any existing entry. Returns this
    /// connection's generation, which the caller passes back to [`Hub::unregister`] on
    /// disconnect so only the *current* socket is ever torn down.
    pub fn register(
        &self,
        device_id: &str,
        tx: mpsc::Sender<ServerToPhone>,
        cancel: Arc<Notify>,
    ) -> u64 {
        let gen = self.conn_gen.fetch_add(1, Ordering::Relaxed);
        let conn = Arc::new(WsConn {
            gen,
            tx,
            acks: Mutex::new(HashMap::new()),
            cancel,
        });
        let prev = self.conns.lock().unwrap().insert(device_id.to_string(), conn);
        if let Some(prev) = prev {
            // The phone opened a new socket without closing the old one. Commands now target the
            // new socket; fail any acks still pending on the old one so they don't hang, and
            // signal its reader to close the socket so connections can't pile up.
            tracing::warn!(
                %device_id, old_gen = prev.gen, new_gen = gen,
                "phone reconnected without closing its previous socket — superseding it"
            );
            for (_, tx) in prev.acks.lock().unwrap().drain() {
                let _ = tx.send(Err("connection superseded".to_string()));
            }
            prev.cancel.notify_one();
        }
        gen
    }

    /// Remove a connection (identified by its `gen`) and fail its pending acks. A no-op if a
    /// newer connection has already taken over — returns whether this call actually removed it,
    /// so the caller knows whether to also drop the device from the registry.
    pub fn unregister(&self, device_id: &str, gen: u64) -> bool {
        let mut conns = self.conns.lock().unwrap();
        match conns.get(device_id) {
            Some(conn) if conn.gen == gen => {
                let conn = conns.remove(device_id).unwrap();
                for (_, tx) in conn.acks.lock().unwrap().drain() {
                    let _ = tx.send(Err("device disconnected".to_string()));
                }
                true
            }
            _ => false, // a newer connection owns this device now; leave it alone
        }
    }

    /// Unconditionally drop a device's connection (used by the stale-connection reaper, which
    /// has already confirmed the device is gone). Fails any pending acks and closes the socket
    /// (via the connection's cancel) so a reaped-but-still-open connection can't linger and
    /// block the phone from reconnecting.
    pub fn drop_device(&self, device_id: &str) {
        if let Some(conn) = self.conns.lock().unwrap().remove(device_id) {
            for (_, tx) in conn.acks.lock().unwrap().drain() {
                let _ = tx.send(Err("device disconnected".to_string()));
            }
            conn.cancel.notify_one();
        }
    }

    /// Send a frame to a device's current connection, best-effort and non-blocking (used for
    /// heartbeat acks). Dropped if the device is gone or its send queue is full.
    pub fn send_frame(&self, device_id: &str, frame: ServerToPhone) {
        if let Some(conn) = self.conns.lock().unwrap().get(device_id).cloned() {
            let _ = conn.tx.try_send(frame);
        }
    }

    /// Resolve a pending command ack (called from the phone read loop). `result` carries the
    /// phone's outcome — `Err(msg)` propagates the phone's own reason (e.g. "no call to answer")
    /// back to the caller rather than a generic failure.
    pub fn resolve_ack(&self, device_id: &str, cmd_id: &str, result: Result<(), String>) {
        let conn = self.conns.lock().unwrap().get(device_id).cloned();
        let waiter = conn.and_then(|c| c.acks.lock().unwrap().remove(cmd_id));
        match waiter {
            Some(tx) => {
                let _ = tx.send(result);
            }
            // The ack arrived but no command is waiting on the current connection — typically
            // the command was sent on a now-superseded socket, or it already timed out.
            None => tracing::warn!(%device_id, %cmd_id, "ack with no matching pending command"),
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
        let kind = action.kind();
        let (ack_tx, ack_rx) = oneshot::channel();
        conn.acks.lock().unwrap().insert(cmd_id.clone(), ack_tx);

        let frame = ServerToPhone::Cmd {
            cmd_id: cmd_id.clone(),
            action,
        };
        if conn.tx.send(frame).await.is_err() {
            conn.acks.lock().unwrap().remove(&cmd_id);
            anyhow::bail!("`{kind}` to `{device_id}`: send channel closed (phone disconnected)");
        }

        match tokio::time::timeout(self.cmd_timeout, ack_rx).await {
            Ok(Ok(Ok(()))) => Ok(()),
            Ok(Ok(Err(e))) => anyhow::bail!("`{kind}` to `{device_id}`: phone reported: {e}"),
            Ok(Err(_)) => anyhow::bail!("`{kind}` to `{device_id}`: ack channel dropped"),
            Err(_) => {
                conn.acks.lock().unwrap().remove(&cmd_id);
                anyhow::bail!(
                    "`{kind}` to `{device_id}` timed out (no ack within {}s — phone may be asleep or its connection stale)",
                    self.cmd_timeout.as_secs()
                )
            }
        }
    }

    /// Send a command without awaiting the ack (e.g. auto-answer from the read loop).
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn newer_connection_supersedes_and_old_teardown_is_noop() {
        let hub = Hub::new();
        let (tx1, _rx1) = mpsc::channel(4);
        let (tx2, _rx2) = mpsc::channel(4);

        let gen1 = hub.register("dev", tx1, Arc::new(Notify::new()));
        let gen2 = hub.register("dev", tx2, Arc::new(Notify::new())); // reconnected; supersedes gen1
        assert_ne!(gen1, gen2);

        // The OLD socket tearing down must NOT remove the current (newer) connection —
        // this is the bug that made commands time out after a silent reconnect.
        assert!(!hub.unregister("dev", gen1));
        assert!(hub.conn("dev").is_ok(), "newer connection stays registered");

        // The current connection's own teardown does remove it.
        assert!(hub.unregister("dev", gen2));
        assert!(hub.conn("dev").is_err(), "device gone after current connection closes");
    }
}
