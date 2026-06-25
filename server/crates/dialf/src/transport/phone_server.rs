//! Phone control plane: WebSocket server.
//!
//! A phone connects, sends [`PhoneToServer::Hello`] with the shared key (or the socket is
//! closed), then exchanges JSON frames. We register the device, spawn a writer task fed by
//! the hub's command channel, and run a reader loop that updates state, resolves command
//! acks, and triggers auto-answer.

use std::net::SocketAddr;

use anyhow::Context;
use futures_util::{SinkExt, StreamExt};
use tokio::net::{TcpListener, TcpStream};
use tokio_tungstenite::tungstenite::Message;

use crate::daemon::{self, now_ms, DaemonState, InboundHandler};
use crate::protocol::{Action, CallState, Direction, PhoneToServer};
use crate::registry::{CallInfo, DeviceInfo};

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
            // Auto-answer: inbound ringing call whose number is configured (or overridden).
            if cs == CallState::Ringing && direction == Direction::In {
                if let Some(handler) = number.as_deref().and_then(|n| state.resolve_inbound(n)) {
                    trigger_autoanswer(state, device_id, call_id, number, handler).await;
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
        PhoneToServer::Sims { entries } => {
            tracing::info!(%device_id, count = entries.len(), "sims");
            state.set_sims(device_id, entries);
        }
        PhoneToServer::MmiResult {
            code,
            success,
            response,
        } => {
            tracing::info!(%device_id, %code, success, "mmi result");
            state.set_mmi_result(
                device_id,
                crate::registry::MmiResult {
                    code,
                    success,
                    response,
                },
            );
        }
        PhoneToServer::VoicemailResult {
            enabled,
            success,
            response,
        } => {
            tracing::info!(%device_id, enabled, success, "voicemail result");
            state.set_voicemail_result(
                device_id,
                crate::registry::VoicemailResult {
                    enabled,
                    success,
                    response,
                },
            );
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

/// Act on a matched inbound ringing call: answer only, or answer + run a job.
async fn trigger_autoanswer(
    state: &DaemonState,
    device_id: &str,
    call_id: String,
    number: Option<String>,
    handler: InboundHandler,
) {
    let (path, want_device) = match handler {
        InboundHandler::AnswerOnly => {
            tracing::info!(%device_id, ?number, "auto-answer");
            let _ = state
                .hub
                .fire(device_id, Action::Answer { call_id: Some(call_id) })
                .await;
            return;
        }
        InboundHandler::Job { path, device } => (path, device),
    };

    // An override may pin to one phone; ignore calls landing on a different device.
    if let Some(want) = &want_device {
        if want != device_id {
            return;
        }
    }
    // One sound card → one call at a time. Skip + log if the card is already in use. This
    // also dedupes repeated `ringing` frames for the same call.
    let Some(card) = state.acquire_card() else {
        let n = number.as_deref().unwrap_or("?");
        tracing::warn!(%device_id, number = %n, "auto-answer skipped: phone busy");
        state.emit(format!("skipped {n} (phone busy)"));
        return;
    };

    let n = number.unwrap_or_default();
    tracing::info!(%device_id, number = %n, job = %path, "auto-answer job");
    state.emit(format!("answered {n} → running {path}"));

    // Detached: the reader loop MUST keep flowing so the job's wait_for_answer /
    // wait_for_speech observe later call_state frames. Never await the job inline here.
    let state = state.clone();
    let device_id = device_id.to_string();
    tokio::spawn(async move {
        // Hold the card lock for the whole job; released when this task ends (any path).
        let _card = card;
        let job = match daemon::load_job_file(&path) {
            Ok(j) => j,
            Err(e) => {
                // Don't leave the call ringing — fall back to a plain answer.
                tracing::error!(error = %format!("{e:#}"), job = %path, "auto-answer job load failed; answering only");
                state.emit(format!("job load failed ({path}): {e:#}; answered only"));
                let _ = state
                    .hub
                    .fire(&device_id, Action::Answer { call_id: Some(call_id) })
                    .await;
                return;
            }
        };
        match daemon::run_job_on_device(&state, device_id.clone(), job).await {
            Ok(_) => state.emit(format!("{n} → done")),
            Err(e) => {
                tracing::error!(error = %format!("{e:#}"), "auto-answer job failed");
                state.emit(format!("{n} → job error: {e:#}"));
            }
        }
    });
}
