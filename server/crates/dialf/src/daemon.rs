//! `dialfd` orchestrator: shared state + the control-API dispatcher.
//!
//! Serves the local control socket, the phone WebSocket plane, and the mDNS advertisement.
//! Real phones register dynamically over WebSocket; an in-process loopback test device is
//! registered only when `--with-loopback` is passed.

use std::collections::HashMap;
use std::path::Path;
use std::sync::{Arc, Mutex};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use anyhow::Context;
use serde_json::json;

use crate::audio::engine::AudioEngine;
use crate::audio::record::{RecordOutput, Recorder};
use crate::config::Config;
use crate::hub::Hub;
use crate::jobs::runner::JobIo as _;
use crate::jobs::{runner, schema};
use crate::loopback::{LoopbackJobIo, LOOPBACK_ID};
use crate::phone::PhoneJobIo;
use crate::protocol::{Action, ControlOp, ControlRequest, ControlResponse};
use crate::registry::{
    CallRecord, DeviceInfo, DeviceKind, MmiResult, Registry, SimInfo, SmsRecord, VoicemailResult,
};
use crate::transport::{control_server, discovery, phone_server};

/// Most recent SMS kept per device.
const INBOX_CAP: usize = 200;

/// Shared daemon state. Cheap to clone (everything is `Arc`).
#[derive(Clone)]
pub struct DaemonState {
    pub registry: Arc<Mutex<Registry>>,
    pub engine: Arc<AudioEngine>,
    pub hub: Arc<Hub>,
    pub config: Arc<Config>,
    /// Recently received/sent SMS, per device id.
    pub inbox: Arc<Mutex<HashMap<String, Vec<SmsRecord>>>>,
    /// Most recent call-log snapshot, per device id (replaced on each `call.list`).
    pub call_log: Arc<Mutex<HashMap<String, Vec<CallRecord>>>>,
    /// Most recent SIM list, per device id (replaced on each `sims.list`).
    pub sims: Arc<Mutex<HashMap<String, Vec<SimInfo>>>>,
    /// Most recent raw MMI/USSD reply, per device id.
    pub mmi_results: Arc<Mutex<HashMap<String, MmiResult>>>,
    /// Most recent voicemail enable/disable result, per device id.
    pub voicemail_results: Arc<Mutex<HashMap<String, VoicemailResult>>>,
    /// When true, audio steps are simulated (no sound card / ten-vad needed).
    pub dry_audio: bool,
}

impl DaemonState {
    /// Append an SMS to a device's inbox (capped to the most recent [`INBOX_CAP`]).
    pub fn record_sms(&self, device_id: &str, rec: SmsRecord) {
        let mut inbox = self.inbox.lock().unwrap();
        let v = inbox.entry(device_id.to_string()).or_default();
        v.push(rec);
        let len = v.len();
        if len > INBOX_CAP {
            v.drain(0..len - INBOX_CAP);
        }
    }

    /// Replace a device's cached call log with a fresh snapshot from the phone.
    pub fn set_call_log(&self, device_id: &str, entries: Vec<CallRecord>) {
        self.call_log
            .lock()
            .unwrap()
            .insert(device_id.to_string(), entries);
    }

    /// Replace a device's cached SIM list with a fresh snapshot from the phone.
    pub fn set_sims(&self, device_id: &str, entries: Vec<SimInfo>) {
        self.sims
            .lock()
            .unwrap()
            .insert(device_id.to_string(), entries);
    }

    /// Record the latest raw MMI/USSD reply from a device.
    pub fn set_mmi_result(&self, device_id: &str, result: MmiResult) {
        self.mmi_results
            .lock()
            .unwrap()
            .insert(device_id.to_string(), result);
    }

    /// Record the latest voicemail enable/disable result from a device.
    pub fn set_voicemail_result(&self, device_id: &str, result: VoicemailResult) {
        self.voicemail_results
            .lock()
            .unwrap()
            .insert(device_id.to_string(), result);
    }
}

/// Milliseconds since the Unix epoch.
pub fn now_ms() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_millis() as i64)
        .unwrap_or(0)
}

/// Run the daemon: set up state and serve the control socket + phone WS plane + mDNS.
/// `with_loopback` registers the in-process simulated test phone (off by default).
pub async fn run(config: Config, dry_audio: bool, with_loopback: bool) -> anyhow::Result<()> {
    let engine = Arc::new(AudioEngine::new(config.audio.clone()));
    let registry = Arc::new(Mutex::new(Registry::new()));
    if with_loopback {
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
        hub: Arc::new(Hub::new()),
        config: Arc::new(config),
        inbox: Arc::new(Mutex::new(HashMap::new())),
        call_log: Arc::new(Mutex::new(HashMap::new())),
        sims: Arc::new(Mutex::new(HashMap::new())),
        mmi_results: Arc::new(Mutex::new(HashMap::new())),
        voicemail_results: Arc::new(Mutex::new(HashMap::new())),
        dry_audio,
    };

    // Advertise on the LAN (non-fatal if it fails). Kept alive for the daemon's lifetime.
    let _mdns = match discovery::advertise(&state.config) {
        Ok(d) => Some(d),
        Err(e) => {
            tracing::warn!(error = %e, "mDNS advertisement failed; phones must use a fixed address");
            None
        }
    };

    let ten_vad = ten_vad_sys::version().unwrap_or_else(|| "stub (not linked)".to_string());
    tracing::info!(
        dry_audio,
        ws = %state.config.ws_bind,
        socket = %state.config.control_socket.display(),
        ten_vad,
        with_loopback,
        "dialfd ready (phone WS plane)"
    );

    tokio::try_join!(
        control_server::serve(state.clone()),
        phone_server::serve(state.clone()),
    )?;
    Ok(())
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
        ControlOp::CallDial {
            device,
            number,
            sim_sub_id,
        } => {
            let dev = resolve_device(state, Some(device))?;
            match device_kind(state, &dev)? {
                DeviceKind::Loopback => loop_io(state, dev).dial(&number)?,
                DeviceKind::Phone => {
                    state
                        .hub
                        .command(
                            &dev,
                            Action::Dial {
                                number: number.clone(),
                                sim_sub_id,
                            },
                        )
                        .await?
                }
            }
            Ok(ok_data(&id, json!({ "dialed": number, "sim_sub_id": sim_sub_id })))
        }
        ControlOp::CallPickup { device } => {
            let dev = resolve_device(state, Some(device))?;
            match device_kind(state, &dev)? {
                DeviceKind::Loopback => loop_io(state, dev).pickup()?,
                DeviceKind::Phone => state.hub.command(&dev, Action::Pickup { call_id: None }).await?,
            }
            Ok(ok_msg(&id))
        }
        ControlOp::CallHangup { device } => {
            let dev = resolve_device(state, Some(device))?;
            match device_kind(state, &dev)? {
                DeviceKind::Loopback => loop_io(state, dev).hangup()?,
                DeviceKind::Phone => state.hub.command(&dev, Action::Hangup { call_id: None }).await?,
            }
            Ok(ok_msg(&id))
        }
        ControlOp::CallReject { device, drop } => {
            let dev = resolve_device(state, Some(device))?;
            match device_kind(state, &dev)? {
                // Loopback has no ring to decline; simulate by clearing the call.
                DeviceKind::Loopback => loop_io(state, dev).hangup()?,
                DeviceKind::Phone => {
                    state
                        .hub
                        .command(&dev, Action::Reject { call_id: None, drop })
                        .await?
                }
            }
            Ok(ok_msg(&id))
        }
        ControlOp::SmsSend { device, to, body } => {
            let dev = resolve_device(state, Some(device))?;
            match device_kind(state, &dev)? {
                DeviceKind::Loopback => loop_io(state, dev).send_sms(&to, &body)?,
                DeviceKind::Phone => {
                    state
                        .hub
                        .command(&dev, Action::SendSms { to, body })
                        .await?
                }
            }
            Ok(ok_msg(&id))
        }
        ControlOp::SmsList { device } => {
            let dev = resolve_device(state, Some(device))?;
            // For a real phone, ask it to report its inbox, then give responses a moment
            // to arrive (they come back as `sms` frames the reader loop records).
            if matches!(device_kind(state, &dev)?, DeviceKind::Phone) {
                let _ = state.hub.fire(&dev, Action::ListSms { since: None }).await;
                tokio::time::sleep(Duration::from_millis(800)).await;
            }
            let msgs = state
                .inbox
                .lock()
                .unwrap()
                .get(&dev)
                .cloned()
                .unwrap_or_default();
            Ok(ok_data(&id, json!({ "messages": msgs })))
        }
        ControlOp::CallList { device } => {
            let dev = resolve_device(state, Some(device))?;
            // Ask a real phone to report its call log, then give the reply a moment to
            // arrive (it comes back as a `calls` frame the reader records).
            if matches!(device_kind(state, &dev)?, DeviceKind::Phone) {
                let _ = state.hub.fire(&dev, Action::ListCalls {}).await;
                tokio::time::sleep(Duration::from_millis(800)).await;
            }
            let calls = state
                .call_log
                .lock()
                .unwrap()
                .get(&dev)
                .cloned()
                .unwrap_or_default();
            Ok(ok_data(&id, json!({ "calls": calls })))
        }
        ControlOp::SimsList { device } => {
            let dev = resolve_device(state, Some(device))?;
            // Ask a real phone to report its SIMs, then wait briefly for the `sims` reply.
            if matches!(device_kind(state, &dev)?, DeviceKind::Phone) {
                let _ = state.hub.fire(&dev, Action::ListSims {}).await;
                tokio::time::sleep(Duration::from_millis(800)).await;
            }
            let sims = state
                .sims
                .lock()
                .unwrap()
                .get(&dev)
                .cloned()
                .unwrap_or_default();
            Ok(ok_data(&id, json!({ "sims": sims })))
        }
        ControlOp::Mmi {
            device,
            code,
            sim_sub_id,
        } => {
            let dev = resolve_device(state, Some(device))?;
            if !matches!(device_kind(state, &dev)?, DeviceKind::Phone) {
                anyhow::bail!("MMI is only supported on real phones");
            }
            state.mmi_results.lock().unwrap().remove(&dev);
            let _ = state
                .hub
                .fire(
                    &dev,
                    Action::Mmi {
                        code: code.clone(),
                        sim_sub_id,
                    },
                )
                .await;
            let mut result = None;
            for _ in 0..30 {
                tokio::time::sleep(Duration::from_millis(500)).await;
                if let Some(r) = state.mmi_results.lock().unwrap().get(&dev).cloned() {
                    result = Some(r);
                    break;
                }
            }
            match result {
                Some(r) => Ok(ok_data(&id, json!(r))),
                None => anyhow::bail!("no MMI response from phone (timed out after 15s)"),
            }
        }
        ControlOp::VoicemailSet {
            device,
            enabled,
            number,
            sim_sub_id,
        } => {
            let dev = resolve_device(state, Some(device))?;
            if !matches!(device_kind(state, &dev)?, DeviceKind::Phone) {
                anyhow::bail!("voicemail control is only supported on real phones");
            }
            // Drop any stale reply, ask the device to apply it, then wait for the result
            // (the device may need a network round-trip that takes several seconds).
            state.voicemail_results.lock().unwrap().remove(&dev);
            let _ = state
                .hub
                .fire(
                    &dev,
                    Action::SetVoicemail {
                        enabled,
                        number,
                        sim_sub_id,
                    },
                )
                .await;
            // Disable tries several codes in sequence, so allow time for multiple round-trips.
            let mut result = None;
            for _ in 0..60 {
                tokio::time::sleep(Duration::from_millis(500)).await;
                if let Some(r) = state.voicemail_results.lock().unwrap().get(&dev).cloned() {
                    result = Some(r);
                    break;
                }
            }
            match result {
                Some(r) => Ok(ok_data(&id, json!(r))),
                None => anyhow::bail!("no voicemail response from device (timed out after 30s)"),
            }
        }
        ControlOp::AudioPlay { file, device: _ } => {
            let engine = state.engine.clone();
            let dry = state.dry_audio;
            tokio::task::spawn_blocking(move || {
                if dry {
                    tracing::info!(%file, "audio.play (dry): skipped");
                    Ok(())
                } else {
                    engine.play_file(Path::new(&file), None)
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
                let text =
                    std::fs::read_to_string(&p).with_context(|| format!("read job file {p}"))?;
                schema::parse(&text).with_context(|| format!("parse job file {p}"))?
            } else if let Some(s) = steps {
                s
            } else {
                anyhow::bail!("job.run requires `path` or `steps`");
            };
            let device_id = resolve_device(state, device)?;
            let kind = device_kind(state, &device_id)?;
            let engine = state.engine.clone();
            let dry = state.dry_audio;

            // Record the call when configured (real audio only).
            let recorder = match (dry, state.config.audio.record_dir.clone()) {
                (false, Some(dir)) => Some(Recorder::new(
                    dir,
                    format!("job-{}", now_ms()),
                    state.config.audio.mix_recording,
                )?),
                _ => None,
            };

            type JobResult = anyhow::Result<(Vec<runner::StepOutcome>, Option<RecordOutput>)>;
            let (outcomes, recording) = match kind {
                DeviceKind::Loopback => {
                    let registry = state.registry.clone();
                    tokio::task::spawn_blocking(move || -> JobResult {
                        let mut io = LoopbackJobIo::new(engine, registry, device_id, dry, recorder);
                        let outcomes = runner::run_job(&job, &mut io)?;
                        Ok((outcomes, io.finish()?))
                    })
                    .await??
                }
                DeviceKind::Phone => {
                    let hub = state.hub.clone();
                    let rt = tokio::runtime::Handle::current();
                    tokio::task::spawn_blocking(move || -> JobResult {
                        let mut io = PhoneJobIo::new(hub, engine, rt, device_id, dry, recorder);
                        let outcomes = runner::run_job(&job, &mut io)?;
                        Ok((outcomes, io.finish()?))
                    })
                    .await??
                }
            };
            let recording = recording.map(|r| json!({ "rx": r.rx, "tx": r.tx, "mix": r.mix }));
            Ok(ok_data(&id, json!({ "steps": outcomes, "recording": recording })))
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
        None, // one-off call/SMS ops never record
    )
}

fn device_kind(state: &DaemonState, device_id: &str) -> anyhow::Result<DeviceKind> {
    state
        .registry
        .lock()
        .unwrap()
        .get(device_id)
        .map(|d| d.kind)
        .ok_or_else(|| anyhow::anyhow!("unknown device `{device_id}`"))
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
