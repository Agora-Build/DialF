//! Local control API server: line-delimited JSON over a Unix domain socket.
//!
//! Each request is one [`ControlRequest`] JSON line; the daemon replies with one
//! [`ControlResponse`] line — except `autoanswer.serve`, which is connection-scoped and
//! streams many `done: false` lines until the client disconnects (see [`serve_autoanswer`]).

use std::os::unix::ffi::OsStrExt;
use std::os::unix::fs::PermissionsExt;
use std::path::Path;

use anyhow::Context;
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader, Lines};
use tokio::net::unix::OwnedReadHalf;
use tokio::net::{UnixListener, UnixStream};
use tokio::sync::broadcast::error::RecvError;

use crate::daemon::{self, AutoanswerOverride, DaemonState};
use crate::protocol::{ControlOp, ControlRequest, ControlResponse};

/// Bind the control socket and serve connections until cancelled.
pub async fn serve(state: DaemonState) -> anyhow::Result<()> {
    let path = state.config.control_socket_path();
    if let Some(parent) = path.parent() {
        let _ = std::fs::create_dir_all(parent);
    }
    // Clear any stale socket from a previous run.
    let _ = std::fs::remove_file(&path);
    let listener = UnixListener::bind(&path)
        .with_context(|| format!("bind control socket {}", path.display()))?;
    apply_socket_perms(
        &path,
        state.config.control_socket_group.as_deref(),
        state.config.control_socket_mode.as_deref(),
    )?;
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

/// Apply the configured group + mode to the control socket, so a shared (system) daemon lets its
/// group's members connect. No-op when neither is set (per-user scope keeps the default 0700 dir).
fn apply_socket_perms(path: &Path, group: Option<&str>, mode: Option<&str>) -> anyhow::Result<()> {
    if let Some(group) = group {
        let gid = gid_for_group(group).with_context(|| format!("control_socket_group `{group}`"))?;
        let c_path = std::ffi::CString::new(path.as_os_str().as_bytes())?;
        // owner = (uid_t)-1 (u32::MAX) keeps the current owner; only the group changes.
        let rc = unsafe { libc::chown(c_path.as_ptr(), u32::MAX, gid) };
        if rc != 0 {
            return Err(std::io::Error::last_os_error()).with_context(|| {
                format!("chgrp control socket {} to {group}", path.display())
            });
        }
    }
    if let Some(mode) = mode {
        let bits = parse_octal_mode(mode)
            .with_context(|| format!("control_socket_mode `{mode}` (want octal, e.g. 0660)"))?;
        std::fs::set_permissions(path, std::fs::Permissions::from_mode(bits))
            .with_context(|| format!("chmod control socket {} to {mode}", path.display()))?;
    }
    Ok(())
}

/// Resolve a group name to its gid via `getgrnam`; errors if the group doesn't exist.
fn gid_for_group(name: &str) -> anyhow::Result<u32> {
    let c_name = std::ffi::CString::new(name)?;
    let grp = unsafe { libc::getgrnam(c_name.as_ptr()) };
    if grp.is_null() {
        anyhow::bail!("group `{name}` not found (create it first, e.g. `groupadd {name}`)");
    }
    Ok(unsafe { (*grp).gr_gid })
}

/// Parse an octal permission string like "0660" or "660" into mode bits.
fn parse_octal_mode(s: &str) -> anyhow::Result<u32> {
    let t = s.trim().trim_start_matches("0o");
    Ok(u32::from_str_radix(t, 8)?)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_octal_mode_variants() {
        assert_eq!(parse_octal_mode("0660").unwrap(), 0o660);
        assert_eq!(parse_octal_mode("660").unwrap(), 0o660);
        assert_eq!(parse_octal_mode("0o640").unwrap(), 0o640);
        assert_eq!(parse_octal_mode(" 0600 ").unwrap(), 0o600);
        assert!(parse_octal_mode("nope").is_err());
        assert!(parse_octal_mode("0899").is_err()); // 8 and 9 aren't octal digits
    }
}
