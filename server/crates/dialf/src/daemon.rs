//! `dialfd` orchestrator: shared state + the control-API dispatcher.
//!
//! For M1 it registers the in-process [`crate::loopback`] phone and serves the local
//! control socket. M2 adds the phone WebSocket server and mDNS advertiser.

use std::path::Path;
use std::sync::{Arc, Mutex};
use std::time::{SystemTime, UNIX_EPOCH};

use anyhow::Context;
use serde_json::json;

use crate::audio::engine::AudioEngine;
use crate::config::Config;
use crate::jobs::{runner, schema};
use crate::loopback::{LoopbackJobIo, LOOPBACK_ID};
use crate::protocol::{ControlOp, ControlRequest, ControlResponse};
use crate::registry::{DeviceInfo, DeviceKind, Registry};
use crate::transport::control_server;

/// Shared daemon state. Cheap to clone (everything is `Arc`).
#[derive(Clone)]
pub struct DaemonState {
    pub registry: Arc<Mutex<Registry>>,
    pub engine: Arc<AudioEngine>,
    pub config: Arc<Config>,
    /// When true, audio steps are simulated (no sound card / ten-vad needed).
    pub dry_audio: bool,
}

/// Milliseconds since the Unix epoch.
pub fn now_ms() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_millis() as i64)
        .unwrap_or(0)
}

/// Run the daemon: set up state, register the loopback device, serve the control socket.
pub async fn run(config: Config, dry_audio: bool) -> anyhow::Result<()> {
    let engine = Arc::new(AudioEngine::new(config.audio.clone()));
    let registry = Arc::new(Mutex::new(Registry::new()));
    {
        let mut reg = registry.lock().expect("registry lock");
        reg.upsert(DeviceInfo {
            id: LOOPBACK_ID.to_string(),
            name: "loopback".to_string(),
            kind: DeviceKind::Loopback,
            last_seen_ms: now_ms(),
            current_call: None,
        });
    }
    let state = DaemonState {
        registry,
        engine,
        config: Arc::new(config),
        dry_audio,
    };
    tracing::info!(dry_audio, "dialfd ready (M1: loopback device + control socket)");
    control_server::serve(state).await
}

/// Dispatch a control request, never failing the connection — errors become error
/// responses.
pub async fn handle(state: &DaemonState, req: ControlRequest) -> ControlResponse {
    let id = req.id.clone();
    match try_handle(state, req).await {
        Ok(resp) => resp,
        Err(e) => ControlResponse {
            id,
            done: true,
            ok: Some(false),
            error: Some(e.to_string()),
            data: None,
        },
    }
}

async fn try_handle(state: &DaemonState, req: ControlRequest) -> anyhow::Result<ControlResponse> {
    let id = req.id.clone();
    match req.op {
        ControlOp::DevicesList => {
            let list = state.registry.lock().unwrap().list();
            Ok(ok_data(&id, json!(list)))
        }
        ControlOp::CallDial { device, number } => {
            let dev = resolve_device(state, Some(device))?;
            loop_io(state, dev).dial(&number)?;
            Ok(ok_data(&id, json!({ "dialed": number })))
        }
        ControlOp::CallPickup { device } => {
            let dev = resolve_device(state, Some(device))?;
            loop_io(state, dev).pickup()?;
            Ok(ok_msg(&id))
        }
        ControlOp::CallHangup { device } => {
            let dev = resolve_device(state, Some(device))?;
            loop_io(state, dev).hangup()?;
            Ok(ok_msg(&id))
        }
        ControlOp::SmsSend { device, to, body } => {
            let dev = resolve_device(state, Some(device))?;
            loop_io(state, dev).send_sms(&to, &body)?;
            Ok(ok_msg(&id))
        }
        ControlOp::SmsList { device } => {
            let _dev = resolve_device(state, Some(device))?;
            // Loopback keeps no inbox; M2 returns the real list.
            Ok(ok_data(&id, json!({ "messages": [] })))
        }
        ControlOp::AudioPlay { file, device: _ } => {
            let engine = state.engine.clone();
            let dry = state.dry_audio;
            tokio::task::spawn_blocking(move || {
                if dry {
                    tracing::info!(%file, "audio.play (dry): skipped");
                    Ok(())
                } else {
                    engine.play_file(Path::new(&file))
                }
            })
            .await??;
            Ok(ok_msg(&id))
        }
        ControlOp::JobRun {
            path,
            steps,
            device,
        } => {
            let job = if let Some(p) = path {
                let text = std::fs::read_to_string(&p)
                    .with_context(|| format!("read job file {p}"))?;
                schema::parse(&text).with_context(|| format!("parse job file {p}"))?
            } else if let Some(s) = steps {
                s
            } else {
                anyhow::bail!("job.run requires `path` or `steps`");
            };
            let device_id = resolve_device(state, device)?;
            let engine = state.engine.clone();
            let registry = state.registry.clone();
            let dry = state.dry_audio;
            let outcomes = tokio::task::spawn_blocking(move || {
                let mut io = LoopbackJobIo::new(engine, registry, device_id, dry);
                runner::run_job(&job, &mut io)
            })
            .await??;
            Ok(ok_data(&id, json!({ "steps": outcomes })))
        }
        ControlOp::JobStatus { job_id } => {
            anyhow::bail!("job.status not tracked yet (job_id={job_id})")
        }
    }
}

fn loop_io(state: &DaemonState, device_id: String) -> LoopbackJobIo {
    LoopbackJobIo::new(
        state.engine.clone(),
        state.registry.clone(),
        device_id,
        state.dry_audio,
    )
}

fn resolve_device(state: &DaemonState, device: Option<String>) -> anyhow::Result<String> {
    let reg = state.registry.lock().unwrap();
    match device {
        Some(d) => {
            if reg.get(&d).is_some() {
                Ok(d)
            } else {
                anyhow::bail!("unknown device `{d}`")
            }
        }
        None => reg
            .sole_device_id()
            .context("no device specified and not exactly one is connected"),
    }
}

fn ok_msg(id: &str) -> ControlResponse {
    ControlResponse {
        id: id.to_string(),
        done: true,
        ok: Some(true),
        error: None,
        data: None,
    }
}

fn ok_data(id: &str, data: serde_json::Value) -> ControlResponse {
    ControlResponse {
        id: id.to_string(),
        done: true,
        ok: Some(true),
        error: None,
        data: Some(data),
    }
}

// Bring JobIo methods into scope for the direct call/sms ops above.
use crate::jobs::runner::JobIo as _;
