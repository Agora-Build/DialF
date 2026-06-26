//! Phone control plane: WebSocket server.
//!
//! A phone connects, sends [`PhoneToServer::Hello`] with the shared key (or the socket is
//! closed), then exchanges JSON frames. We register the device, spawn a writer task fed by
//! the hub's command channel, and run a reader loop that updates state, resolves command
//! acks, and triggers auto-answer.

use std::net::SocketAddr;
use std::time::Duration;

use anyhow::Context;
use futures_util::{SinkExt, StreamExt};
use tokio::net::{TcpListener, TcpStream};
use tokio_tungstenite::tungstenite::Message;

use crate::daemon::{self, now_ms, DaemonState, InboundHandler};
use crate::protocol::{Action, CallState, Direction, PhoneToServer, ServerToPhone};
use crate::registry::{CallInfo, DeviceInfo};

/// Phones heartbeat every 30s; treat one as gone after it misses ~3 (no frame for this long).
/// A half-open TCP socket doesn't error on its own, so the reader loop can block forever — this
/// is how a silently-dropped phone leaves `dialf devices` and stops timing out commands.
const STALE_AFTER_MS: i64 = 90_000;
/// How often the reaper scans for stale phones.
const REAP_INTERVAL: Duration = Duration::from_secs(15);

/// Periodically drop phones whose connection has gone silent (mirrors the clean-disconnect
/// cleanup: unregister from the hub + remove from the registry). Runs for the daemon's life.
pub async fn reap_stale(state: DaemonState) -> anyhow::Result<()> {
    let mut tick = tokio::time::interval(REAP_INTERVAL);
    loop {
        tick.tick().await;
        let cutoff = now_ms() - STALE_AFTER_MS;
        let reaped = state.registry.lock().unwrap().reap_older_than(cutoff);
        for id in reaped {
            tracing::warn!(device = %id, "reaping stale phone: no heartbeat in ~{}s", STALE_AFTER_MS / 1000);
            state.hub.drop_device(&id);
            state.emit(format!("device {id} dropped (stale connection — no heartbeat)"));
        }
    }
}

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

    // Register: command channel + device record. `gen` identifies *this* socket so a later
    // reconnect supersedes us cleanly (and our own teardown can't clobber a newer connection).
    // `cancel` lets a superseding reconnect tell this reader to close the socket.
    let (tx, mut rx) = tokio::sync::mpsc::channel(64);
    let cancel = std::sync::Arc::new(tokio::sync::Notify::new());
    let gen = state.hub.register(&device_id, tx, cancel.clone());
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

    // Reader loop — but stop early if a newer connection supersedes us, so this socket closes
    // instead of lingering half-open (which is how connections used to pile up).
    let result = tokio::select! {
        r = reader_loop(&mut read, &state, &device_id) => r,
        _ = cancel.notified() => {
            tracing::info!(%device_id, gen, "connection superseded by a newer one; closing socket");
            Ok(())
        }
    };

    // Cleanup — but only drop the device if we're still the current connection. If a newer
    // reconnect has superseded us, `unregister` is a no-op and we must leave its record intact
    // (this is the bug that made commands time out after a silent reconnect).
    if state.hub.unregister(&device_id, gen) {
        state.registry.lock().unwrap().remove(&device_id);
        tracing::info!(%device_id, "phone disconnected");
    } else {
        tracing::info!(%device_id, gen, "old socket closed; a newer connection is active");
    }
    writer.abort();
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
            // Reply so the app can confirm the daemon is alive and reconnect if it goes silent.
            state.hub.send_frame(device_id, ServerToPhone::HeartbeatAck);
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
        PhoneToServer::Ack { cmd_id, ok } => {
            let result = if ok {
                Ok(())
            } else {
                Err("phone reported failure".to_string())
            };
            state.hub.resolve_ack(device_id, &cmd_id, result);
        }
        PhoneToServer::Error { cmd_id, msg } => {
            // Propagate the phone's own reason to the waiting command (not a generic failure).
            if let Some(id) = cmd_id {
                state.hub.resolve_ack(device_id, &id, Err(msg.clone()));
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
        // Answer the inbound call ourselves — this is the "auto" in auto-answer. The job's own
        // call.answer / call.dial / call.wait_answered then become no-ops (inbound mode).
        if let Err(e) = state
            .hub
            .command(&device_id, Action::Answer { call_id: Some(call_id) })
            .await
        {
            tracing::warn!(error = %format!("{e:#}"), "auto-answer: failed to answer; not running job");
            state.emit(format!("{n} → answer failed: {e:#}"));
            return;
        }
        let job = match daemon::load_job_file(&path) {
            Ok(j) => j,
            Err(e) => {
                // Call is answered; we just have no script to run.
                tracing::error!(error = %format!("{e:#}"), job = %path, "auto-answer job load failed (call answered, no script)");
                state.emit(format!("{n} → job load failed: {e:#} (call answered, no script)"));
                return;
            }
        };
        match daemon::run_job_on_device(&state, device_id.clone(), job, true).await {
            Ok((outcomes, _)) => {
                match outcomes
                    .iter()
                    .position(|o| o.summary == crate::jobs::runner::CALL_ENDED_SUMMARY)
                {
                    Some(pos) => {
                        state.emit(format!("{n} → caller hung up"));
                        for o in outcomes.iter().skip(pos + 1) {
                            state.emit(format!("  {}", o.summary)); // "audio.play skipped", …
                        }
                        state.emit("waiting for the next call".to_string());
                    }
                    None => state.emit(format!("{n} → done")),
                }
            }
            Err(e) => {
                tracing::error!(error = %format!("{e:#}"), "auto-answer job failed");
                state.emit(format!("{n} → job error: {e:#}"));
            }
        }
    });
}
