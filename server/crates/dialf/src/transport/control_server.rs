//! Local control API server: line-delimited JSON over a Unix domain socket.
//!
//! Each request is one [`ControlRequest`] JSON line; the daemon replies with one (M1) or
//! more (future: streamed) [`ControlResponse`] JSON lines.

use anyhow::Context;
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::net::{UnixListener, UnixStream};

use crate::daemon::{self, DaemonState};
use crate::protocol::{ControlRequest, ControlResponse};

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
        let resp = match serde_json::from_str::<ControlRequest>(&line) {
            Ok(req) => daemon::handle(&state, req).await,
            Err(e) => ControlResponse {
                id: String::new(),
                done: true,
                ok: Some(false),
                error: Some(format!("bad request: {e}")),
                data: None,
            },
        };
        let mut buf = serde_json::to_string(&resp)?;
        buf.push('\n');
        write.write_all(buf.as_bytes()).await?;
        write.flush().await?;
    }
    Ok(())
}
