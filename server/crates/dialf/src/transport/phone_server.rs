//! Phone control plane: WebSocket server.
//!
//! A phone connects, sends [`PhoneToServer::Hello`] with the shared key (or the socket is
//! closed), then exchanges JSON frames. We register the device, spawn a writer task fed by
//! the hub's command channel, and run a reader loop that updates state, resolves command
//! acks, and triggers auto-pickup.

use std::net::SocketAddr;

use anyhow::Context;
use futures_util::{SinkExt, StreamExt};
use tokio::net::{TcpListener, TcpStream};
use tokio_tungstenite::tungstenite::Message;

use crate::daemon::{now_ms, DaemonState};
use crate::protocol::{Action, CallState, Direction, PhoneToServer};
use crate::registry::{CallInfo, DeviceInfo, DeviceKind};

/// Bind and serve the phone WebSocket endpoint until cancelled.
pub async fn serve(state: DaemonState) -> anyhow::Result<()> {
    let addr: SocketAddr = state
        .config
        .ws_bind
        .parse()
        .with_context(|| format!("parse ws_bind `{}`", state.config.ws_bind))?;
    let listener = TcpListener::bind(addr)
        .await
        .with_context(|| format!("bind phone WS server on {addr}"))?;
    tracing::info!(%addr, "phone WebSocket server listening");

    loop {
        let (stream, peer) = listener.accept().await?;
        let state = state.clone();
        tokio::spawn(async move {
            if let Err(e) = handle_conn(stream, state).await {
                tracing::warn!(%peer, error = %e, "phone connection ended with error");
            }
        });
    }
}

async fn handle_conn(stream: TcpStream, state: DaemonState) -> anyhow::Result<()> {
    let ws = tokio_tungstenite::accept_async(stream).await?;
    let (mut sink, mut read) = ws.split();

    // First frame must be a valid Hello.
    let first = read.next().await.context("connection closed before hello")??;
    let hello: PhoneToServer = serde_json::from_str(first.to_text()?)?;
    let (device_id, name) = match hello {
        PhoneToServer::Hello {
            device_id,
            name,
            key,
            ..
        } => {
            if key != state.config.shared_key {
                let _ = sink.send(Message::Close(None)).await;
                anyhow::bail!("rejected device `{device_id}`: bad shared key");
            }
            (device_id, name)
        }
        other => anyhow::bail!("expected hello, got {other:?}"),
    };

    // Register: command channel + device record.
    let (tx, mut rx) = tokio::sync::mpsc::channel(64);
    state.hub.register(&device_id, tx);
    state.registry.lock().unwrap().upsert(DeviceInfo {
        id: device_id.clone(),
        name,
        kind: DeviceKind::Phone,
        last_seen_ms: now_ms(),
        current_call: None,
    });
    tracing::info!(%device_id, "phone connected");

    // Writer task: hub command channel -> WS sink.
    let writer = tokio::spawn(async move {
        while let Some(frame) = rx.recv().await {
            match serde_json::to_string(&frame) {
                Ok(txt) => {
                    if sink.send(Message::Text(txt.into())).await.is_err() {
                        break;
                    }
                }
                Err(e) => tracing::warn!(error = %e, "serialize command failed"),
            }
        }
    });

    // Reader loop.
    let result = reader_loop(&mut read, &state, &device_id).await;

    // Cleanup.
    state.hub.unregister(&device_id);
    state.registry.lock().unwrap().remove(&device_id);
    writer.abort();
    tracing::info!(%device_id, "phone disconnected");
    result
}

async fn reader_loop<S>(read: &mut S, state: &DaemonState, device_id: &str) -> anyhow::Result<()>
where
    S: StreamExt<Item = Result<Message, tokio_tungstenite::tungstenite::Error>> + Unpin,
{
    while let Some(msg) = read.next().await {
        let msg = msg?;
        if msg.is_close() {
            break;
        }
        let text = match msg.to_text() {
            Ok(t) if !t.is_empty() => t,
            _ => continue, // ignore ping/pong/binary/empty
        };
        match serde_json::from_str::<PhoneToServer>(text) {
            Ok(m) => handle_phone_msg(state, device_id, m).await,
            Err(e) => tracing::warn!(%device_id, error = %e, "bad phone frame"),
        }
    }
    Ok(())
}

async fn handle_phone_msg(state: &DaemonState, device_id: &str, msg: PhoneToServer) {
    match msg {
        PhoneToServer::Heartbeat { .. } => {
            if let Some(dev) = state.registry.lock().unwrap().get_mut(device_id) {
                dev.last_seen_ms = now_ms();
            }
        }
        PhoneToServer::CallState {
            call_id,
            state: cs,
            number,
            direction,
        } => {
            {
                let mut reg = state.registry.lock().unwrap();
                if let Some(dev) = reg.get_mut(device_id) {
                    dev.last_seen_ms = now_ms();
                    dev.current_call = if cs == CallState::Ended {
                        None
                    } else {
                        Some(CallInfo {
                            call_id: call_id.clone(),
                            number: number.clone(),
                            state: cs,
                            direction,
                        })
                    };
                }
            }
            // Auto-pickup: inbound ringing call whose number is on the list.
            if cs == CallState::Ringing && direction == Direction::In {
                let matches = number
                    .as_ref()
                    .map(|n| state.config.autopickup.iter().any(|a| a == n))
                    .unwrap_or(false);
                if matches {
                    tracing::info!(%device_id, ?number, "auto-pickup");
                    let _ = state
                        .hub
                        .fire(device_id, Action::Pickup { call_id: Some(call_id) })
                        .await;
                }
            }
        }
        PhoneToServer::Sms {
            direction,
            from,
            to,
            body,
            ts,
        } => {
            tracing::info!(%device_id, ?direction, ?from, len = body.len(), "sms");
            state.record_sms(
                device_id,
                crate::registry::SmsRecord {
                    direction,
                    from,
                    to,
                    body,
                    ts,
                },
            );
        }
        PhoneToServer::Calls { entries } => {
            tracing::info!(%device_id, count = entries.len(), "call log");
            state.set_call_log(device_id, entries);
        }
        PhoneToServer::Ack { cmd_id, ok } => state.hub.resolve_ack(device_id, &cmd_id, ok),
        PhoneToServer::Error { cmd_id, msg } => {
            if let Some(id) = cmd_id {
                state.hub.resolve_ack(device_id, &id, false);
            }
            tracing::warn!(%device_id, "phone error: {msg}");
        }
        PhoneToServer::Hello { .. } => {
            tracing::warn!(%device_id, "unexpected second hello");
        }
    }
}
