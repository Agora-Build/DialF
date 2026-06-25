//! Local control API server: line-delimited JSON over a Unix domain socket.
//!
//! Each request is one [`ControlRequest`] JSON line; the daemon replies with one
//! [`ControlResponse`] line — except `autoanswer.serve`, which is connection-scoped and
//! streams many `done: false` lines until the client disconnects (see [`serve_autoanswer`]).

use anyhow::Context;
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader, Lines};
use tokio::net::unix::OwnedReadHalf;
use tokio::net::{UnixListener, UnixStream};
use tokio::sync::broadcast::error::RecvError;

use crate::daemon::{self, AutoanswerOverride, DaemonState};
use crate::protocol::{ControlOp, ControlRequest, ControlResponse};

/// Bind the control socket and serve connections until cancelled.
pub async fn serve(state: DaemonState) -> anyhow::Result<()> {
    let path = state.config.control_socket.clone();
    if let Some(parent) = path.parent() {
        let _ = std::fs::create_dir_all(parent);
    }
    // Clear any stale socket from a previous run.
    let _ = std::fs::remove_file(&path);
    let listener = UnixListener::bind(&path)
        .with_context(|| format!("bind control socket {}", path.display()))?;
    tracing::info!(socket = %path.display(), "control server listening");

    loop {
        let (stream, _addr) = listener.accept().await?;
        let state = state.clone();
        tokio::spawn(async move {
            if let Err(e) = handle_conn(stream, state).await {
                tracing::warn!(error = %e, "control connection error");
            }
        });
    }
}

async fn handle_conn(stream: UnixStream, state: DaemonState) -> anyhow::Result<()> {
    let (read, mut write) = stream.into_split();
    let mut lines = BufReader::new(read).lines();
    while let Some(line) = lines.next_line().await? {
        if line.trim().is_empty() {
            continue;
        }
        let req = match serde_json::from_str::<ControlRequest>(&line) {
            Ok(req) => req,
            Err(e) => {
                write_line(
                    &mut write,
                    &ControlResponse {
                        id: String::new(),
                        done: true,
                        ok: Some(false),
                        error: Some(format!("bad request: {e}")),
                        data: None,
                    },
                )
                .await?;
                continue;
            }
        };
        // `autoanswer.serve` owns the connection for its lifetime: it registers overrides,
        // streams events, and reverts on disconnect. Everything else is one-shot.
        if matches!(req.op, ControlOp::AutoanswerServe { .. }) {
            if let ControlOp::AutoanswerServe {
                numbers,
                path,
                device,
            } = req.op
            {
                serve_autoanswer(&state, &req.id, numbers, path, device, &mut write, &mut lines)
                    .await?;
            }
            break; // client disconnected; this connection is finished
        }
        let resp = daemon::handle(&state, req).await;
        write_line(&mut write, &resp).await?;
    }
    Ok(())
}

/// Handle a `autoanswer.serve` request: register the override for each number, then stream
/// daemon event lines until the client disconnects, at which point the override is removed
/// (reverting to `config.autoanswer`). Tied to this connection — no state outlives it.
async fn serve_autoanswer(
    state: &DaemonState,
    id: &str,
    numbers: Vec<String>,
    path: String,
    device: Option<String>,
    write: &mut tokio::net::unix::OwnedWriteHalf,
    lines: &mut Lines<BufReader<OwnedReadHalf>>,
) -> anyhow::Result<()> {
    // One phone to drive → only one serve session at a time. Reject a second instance (e.g.
    // a second `dialf run --autoanswer &`) instead of letting them fight over the card.
    let _serve = match state.acquire_serve() {
        Some(guard) => guard,
        None => {
            return write_line(
                write,
                &ControlResponse {
                    id: id.to_string(),
                    done: true,
                    ok: Some(false),
                    error: Some(
                        "a dialf serve session is already running (one phone — only one at a time)"
                            .to_string(),
                    ),
                    data: None,
                },
            )
            .await;
        }
    };

    let token = state.next_serve_token();
    for n in &numbers {
        state.register_override(
            n.clone(),
            AutoanswerOverride {
                token,
                path: path.clone(),
                device: device.clone(),
            },
        );
    }
    let mut rx = state.events.subscribe();

    // Always remove this connection's overrides on the way out, however we exit.
    let outcome = async {
        let confirm = format!("serving inbound: {} (overrides config)", numbers.join(", "));
        write_event(write, id, &confirm).await?;
        loop {
            tokio::select! {
                // The client never sends more lines; a None/Err means it disconnected.
                line = lines.next_line() => match line {
                    Ok(Some(_)) => continue,
                    _ => break,
                },
                ev = rx.recv() => match ev {
                    Ok(text) => write_event(write, id, &text).await?,
                    Err(RecvError::Lagged(_)) => continue,
                    Err(RecvError::Closed) => break,
                },
            }
        }
        anyhow::Ok(())
    }
    .await;

    state.clear_overrides(token);
    outcome
}

/// Write one streamed event line (a non-terminal `done: false` response carrying `{event}`).
async fn write_event(
    write: &mut tokio::net::unix::OwnedWriteHalf,
    id: &str,
    text: &str,
) -> anyhow::Result<()> {
    write_line(
        write,
        &ControlResponse {
            id: id.to_string(),
            done: false,
            ok: Some(true),
            error: None,
            data: Some(serde_json::json!({ "event": text })),
        },
    )
    .await
}

/// Serialize a response and write it as one JSON line.
async fn write_line(
    write: &mut tokio::net::unix::OwnedWriteHalf,
    resp: &ControlResponse,
) -> anyhow::Result<()> {
    let mut buf = serde_json::to_string(resp)?;
    buf.push('\n');
    write.write_all(buf.as_bytes()).await?;
    write.flush().await?;
    Ok(())
}
