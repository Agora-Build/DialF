//! `dialfd` orchestrator: shared state + the control-API dispatcher.
//!
//! Serves the local control socket, the phone WebSocket plane, and the mDNS advertisement.
//! Phones register dynamically over WebSocket as they connect.

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::atomic::AtomicBool;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use anyhow::Context;
use serde_json::json;

use crate::audio::engine::AudioEngine;
use crate::audio::record::RecordOutput;
use crate::config::Config;
use crate::hub::Hub;
use crate::jobs::{runner, schema};
use crate::phone::PhoneJobIo;
use crate::protocol::{Action, ControlOp, ControlRequest, ControlResponse};
use crate::registry::{CallRecord, MmiResult, Registry, SimInfo, SmsRecord, VoicemailResult};
use crate::transport::{control_server, discovery, phone_server};

/// Most recent SMS kept per device.
const INBOX_CAP: usize = 200;
/// Capacity of the daemon→serve-client event broadcast (lines are dropped if a slow client
/// lags past this; foreground-serve output is best-effort, not a guaranteed log).
const EVENT_CHANNEL_CAP: usize = 256;

/// A live, in-memory auto-answer override registered by a `dialf run --autoanswer` client.
/// Takes precedence over `config.autoanswer`; removed when that client disconnects.
#[derive(Clone)]
pub struct AutoanswerOverride {
    /// Registration token — identifies the owning serve connection so it removes only its own.
    pub token: u64,
    /// Absolute path to the job file to run on a matching inbound call.
    pub path: String,
    /// Restrict to this device id; `None` answers on whichever phone receives the call.
    pub device: Option<String>,
}

/// Shared daemon state. Cheap to clone (everything is `Arc`).
#[derive(Clone)]
pub struct DaemonState {
    pub registry: Arc<Mutex<Registry>>,
    pub engine: Arc<AudioEngine>,
    pub hub: Arc<Hub>,
    pub config: Arc<Config>,
    /// Directory of the loaded config file; relative `autoanswer` job paths resolve here.
    pub config_dir: Option<PathBuf>,
    /// Held while a job is actively using the sound card. One card → one call/recording at a
    /// time, so anything that plays/records acquires this first (explicit `job.run` errors on
    /// contention; auto-answer skips + logs).
    pub card_busy: Arc<AtomicBool>,
    /// Held for the lifetime of a `dialf run --autoanswer` serve session. Only one serve may
    /// run at a time (one phone to drive), so a second registration is rejected.
    pub serve_busy: Arc<AtomicBool>,
    /// Live auto-answer overrides (number → handler), keyed by phone number.
    pub overrides: Arc<Mutex<HashMap<String, AutoanswerOverride>>>,
    /// Monotonic source of override registration tokens.
    pub serve_token: Arc<AtomicU64>,
    /// Broadcast of human-readable event lines to foreground-serve clients.
    pub events: tokio::sync::broadcast::Sender<String>,
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
}

/// RAII lock on the sound card (one call/recording at a time). Releases on drop.
pub struct CardGuard {
    flag: Arc<AtomicBool>,
}
impl Drop for CardGuard {
    fn drop(&mut self) {
        self.flag.store(false, Ordering::Release);
    }
}

/// RAII lock on the single serve slot (one `dialf run --autoanswer` at a time). Releases on drop.
pub struct ServeGuard {
    flag: Arc<AtomicBool>,
}
impl Drop for ServeGuard {
    fn drop(&mut self) {
        self.flag.store(false, Ordering::Release);
    }
}

/// How `dialfd` should handle an inbound call from a matched number.
#[derive(Debug, PartialEq, Eq)]
pub enum InboundHandler {
    /// Just answer the call (config value was null / no override job).
    AnswerOnly,
    /// Answer and run this job file (absolute path), optionally pinned to a device.
    Job {
        path: String,
        device: Option<String>,
    },
}

impl DaemonState {
    /// Emit a human-readable event line. It goes to the daemon log (so config-based autoanswer,
    /// which has no attached client, is still observable) and, timestamped, to any
    /// foreground-serve clients (`dialf run --autoanswer`). Best-effort on both.
    pub fn emit(&self, line: impl Into<String>) {
        let line = line.into();
        tracing::info!(target: "event", "{line}");
        let ts = chrono::Local::now().format("%H:%M:%S");
        let _ = self.events.send(format!("{ts}  {line}"));
    }

    /// Allocate a fresh override-registration token.
    pub fn next_serve_token(&self) -> u64 {
        self.serve_token.fetch_add(1, Ordering::Relaxed)
    }

    /// Register one number → job override under `token`. Overwrites any prior entry for the
    /// number (last writer wins); the owner removes only entries still bearing its token.
    pub fn register_override(&self, number: String, ov: AutoanswerOverride) {
        self.overrides.lock().unwrap().insert(number, ov);
    }

    /// Remove every override registered under `token` (called when a serve client disconnects).
    pub fn clear_overrides(&self, token: u64) {
        self.overrides.lock().unwrap().retain(|_, ov| ov.token != token);
    }

    /// Resolve how an inbound call from `number` should be handled: a live override wins over
    /// `config.autoanswer`; `None` means the number is not configured (let it ring).
    pub fn resolve_inbound(&self, number: &str) -> Option<InboundHandler> {
        if let Some(ov) = self.overrides.lock().unwrap().get(number) {
            return Some(InboundHandler::Job {
                path: ov.path.clone(),
                device: ov.device.clone(),
            });
        }
        match self.config.autoanswer.get(number) {
            None => None,
            Some(None) => Some(InboundHandler::AnswerOnly),
            Some(Some(path)) => Some(InboundHandler::Job {
                path: resolve_under(self.config_dir.as_deref(), path),
                device: None,
            }),
        }
    }

    /// Acquire the sound-card lock for a call/recording. Returns a guard that releases on drop,
    /// or `None` if a job is already using the card (caller decides: error vs skip + log).
    pub fn acquire_card(&self) -> Option<CardGuard> {
        self.card_busy
            .compare_exchange(false, true, Ordering::AcqRel, Ordering::Acquire)
            .ok()
            .map(|_| CardGuard {
                flag: self.card_busy.clone(),
            })
    }

    /// Acquire the single serve slot for a `dialf run --autoanswer` session. Returns a guard
    /// that releases on drop, or `None` if a serve session is already registered.
    pub fn acquire_serve(&self) -> Option<ServeGuard> {
        self.serve_busy
            .compare_exchange(false, true, Ordering::AcqRel, Ordering::Acquire)
            .ok()
            .map(|_| ServeGuard {
                flag: self.serve_busy.clone(),
            })
    }

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
pub async fn run(config: Config, config_path: PathBuf) -> anyhow::Result<()> {
    let engine = Arc::new(AudioEngine::new(config.audio.clone()));
    let registry = Arc::new(Mutex::new(Registry::new()));
    let (events, _) = tokio::sync::broadcast::channel(EVENT_CHANNEL_CAP);
    let state = DaemonState {
        registry,
        engine,
        hub: Arc::new(Hub::new()),
        config: Arc::new(config),
        config_dir: config_path.parent().map(|p| p.to_path_buf()),
        card_busy: Arc::new(AtomicBool::new(false)),
        serve_busy: Arc::new(AtomicBool::new(false)),
        overrides: Arc::new(Mutex::new(HashMap::new())),
        serve_token: Arc::new(AtomicU64::new(1)),
        events,
        inbox: Arc::new(Mutex::new(HashMap::new())),
        call_log: Arc::new(Mutex::new(HashMap::new())),
        sims: Arc::new(Mutex::new(HashMap::new())),
        mmi_results: Arc::new(Mutex::new(HashMap::new())),
        voicemail_results: Arc::new(Mutex::new(HashMap::new())),
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
        ws = %state.config.ws_bind,
        socket = %state.config.control_socket.display(),
        ten_vad,
        "dialfd ready (phone WS plane)"
    );

    tokio::try_join!(
        control_server::serve(state.clone()),
        phone_server::serve(state.clone()),
        phone_server::reap_stale(state.clone()),
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
            // Full cause chain ("{:#}"), so wrapped errors surface the underlying reason —
            // e.g. a YAML parse error includes "… at line N column M".
            error: Some(format!("{e:#}")),
            data: None,
        },
    }
}

async fn try_handle(state: &DaemonState, req: ControlRequest) -> anyhow::Result<ControlResponse> {
    let id = req.id.clone();
    match req.op {
        ControlOp::ServerInfo => Ok(ok_data(
            &id,
            json!({
                "version": env!("CARGO_PKG_VERSION"),
                "ten_vad": ten_vad_sys::version().unwrap_or_else(|| "stub".to_string()),
            }),
        )),
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
            state
                .hub
                .command(
                    &dev,
                    Action::Dial {
                        number: number.clone(),
                        sim_sub_id,
                    },
                )
                .await?;
            Ok(ok_data(&id, json!({ "dialed": number, "sim_sub_id": sim_sub_id })))
        }
        ControlOp::CallAnswer { device } => {
            let dev = resolve_device(state, Some(device))?;
            state.hub.command(&dev, Action::Answer { call_id: None }).await?;
            Ok(ok_msg(&id))
        }
        ControlOp::CallHangup { device } => {
            let dev = resolve_device(state, Some(device))?;
            state.hub.command(&dev, Action::Hangup { call_id: None }).await?;
            Ok(ok_msg(&id))
        }
        ControlOp::CallReject { device, drop } => {
            let dev = resolve_device(state, Some(device))?;
            state
                .hub
                .command(&dev, Action::Reject { call_id: None, drop })
                .await?;
            Ok(ok_msg(&id))
        }
        ControlOp::SmsSend { device, to, body } => {
            let dev = resolve_device(state, Some(device))?;
            state.hub.command(&dev, Action::SendSms { to, body }).await?;
            Ok(ok_msg(&id))
        }
        ControlOp::SmsList { device } => {
            let dev = resolve_device(state, Some(device))?;
            // Ask the phone to report its inbox, then give responses a moment to arrive
            // (they come back as `sms` frames the reader loop records).
            let _ = state.hub.fire(&dev, Action::ListSms { since: None }).await;
            tokio::time::sleep(Duration::from_millis(800)).await;
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
            // Ask the phone to report its call log, then give the reply a moment to arrive
            // (it comes back as a `calls` frame the reader records).
            let _ = state.hub.fire(&dev, Action::ListCalls {}).await;
            tokio::time::sleep(Duration::from_millis(800)).await;
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
            // Ask the phone to report its SIMs, then wait briefly for the `sims` reply.
            let _ = state.hub.fire(&dev, Action::ListSims {}).await;
            tokio::time::sleep(Duration::from_millis(800)).await;
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
            tokio::task::spawn_blocking(move || engine.play_file(Path::new(&file), None)).await??;
            Ok(ok_msg(&id))
        }
        ControlOp::JobRun {
            path,
            steps,
            device,
        } => {
            let job = match (path, steps) {
                (Some(p), _) => load_job_file(&p)?,
                (None, Some(s)) => s,
                (None, None) => anyhow::bail!("job.run requires `path` or `steps`"),
            };
            let device_id = resolve_device(state, device)?;
            // One sound card → one call/recording at a time. Held until the job finishes.
            let _card = state
                .acquire_card()
                .context("phone busy: a call or recording is already in progress")?;
            // `dialf run` is outbound/one-shot: the job owns call setup (call.dial, etc.).
            let (outcomes, recording) = run_job_on_device(state, device_id, job, false).await?;
            let recording = recording.map(|r| json!({ "rx": r.rx, "tx": r.tx, "mix": r.mix }));
            Ok(ok_data(&id, json!({ "steps": outcomes, "recording": recording })))
        }
        ControlOp::AutoanswerServe { .. } => {
            // Streamed + connection-scoped: handled directly in control_server, never here.
            anyhow::bail!("autoanswer.serve must be handled on its own connection")
        }
        ControlOp::JobStatus { job_id } => {
            anyhow::bail!("job.status not tracked yet (job_id={job_id})")
        }
    }
}

/// Resolve a possibly-relative path under `base` (e.g. the config dir). Absolute paths, and
/// the `base = None`/empty case, are returned unchanged.
fn resolve_under(base: Option<&Path>, path: &str) -> String {
    match base {
        Some(b) if !b.as_os_str().is_empty() && Path::new(path).is_relative() => {
            b.join(path).to_string_lossy().into_owned()
        }
        _ => path.to_string(),
    }
}

/// Read + parse a job file, resolving relative `audio.play` paths against the job file's own
/// directory so the job is portable regardless of the daemon's working directory (it usually
/// runs as a service with cwd=/).
pub fn load_job_file(path: &str) -> anyhow::Result<Vec<schema::Step>> {
    let text = std::fs::read_to_string(path).with_context(|| format!("read job file {path}"))?;
    let mut job = schema::parse(&text).with_context(|| format!("parse job file {path}"))?;
    if let Some(base) = Path::new(path).parent().filter(|b| !b.as_os_str().is_empty()) {
        for step in &mut job {
            if let schema::StepKind::AudioPlay { file } = &mut step.kind {
                if Path::new(file).is_relative() {
                    *file = base.join(&*file).to_string_lossy().into_owned();
                }
            }
        }
    }
    Ok(job)
}

/// Run a parsed job against a device, recording when configured. The duplex session is started
/// inside the blocking task so the capture thread records rx before the first step runs; the
/// recording is always finalized (even on job error) so mix.wav is written and both legs padded.
pub async fn run_job_on_device(
    state: &DaemonState,
    device_id: String,
    job: Vec<schema::Step>,
    inbound: bool,
) -> anyhow::Result<(Vec<runner::StepOutcome>, Option<RecordOutput>)> {
    let engine = state.engine.clone();
    let record_dir = state.config.audio.record_dir.clone();
    let mix_recording = state.config.audio.mix_recording;
    let mix_tx_left = state.config.audio.mix_channels.tx_left();
    let hub = state.hub.clone();
    let registry = state.registry.clone();
    let rt = tokio::runtime::Handle::current();

    type JobResult = anyhow::Result<(Vec<runner::StepOutcome>, Option<RecordOutput>)>;
    tokio::task::spawn_blocking(move || -> JobResult {
        let session = match record_dir {
            Some(dir) => {
                Some(engine.start_duplex(
                    dir,
                    format!("dialf-job-{}", now_ms()),
                    mix_recording,
                    mix_tx_left,
                )?)
            }
            None => None,
        };
        let mut io = PhoneJobIo::new(hub, engine, rt, registry, device_id, session, inbound);
        let run = runner::run_job(&job, &mut io);
        let recording = io.finish()?;
        Ok((run?, recording))
    })
    .await?
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

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::BTreeMap;

    fn test_state(autoanswer: BTreeMap<String, Option<String>>) -> DaemonState {
        let mut config = Config::default();
        config.autoanswer = autoanswer;
        let (events, _) = tokio::sync::broadcast::channel(16);
        DaemonState {
            registry: Arc::new(Mutex::new(Registry::new())),
            engine: Arc::new(AudioEngine::new(config.audio.clone())),
            hub: Arc::new(Hub::new()),
            config: Arc::new(config),
            config_dir: Some(PathBuf::from("/etc/dialf")),
            card_busy: Arc::new(AtomicBool::new(false)),
            serve_busy: Arc::new(AtomicBool::new(false)),
            overrides: Arc::new(Mutex::new(HashMap::new())),
            serve_token: Arc::new(AtomicU64::new(1)),
            events,
            inbox: Arc::new(Mutex::new(HashMap::new())),
            call_log: Arc::new(Mutex::new(HashMap::new())),
            sims: Arc::new(Mutex::new(HashMap::new())),
            mmi_results: Arc::new(Mutex::new(HashMap::new())),
            voicemail_results: Arc::new(Mutex::new(HashMap::new())),
        }
    }

    #[test]
    fn resolve_under_joins_relative_keeps_absolute() {
        let base = Path::new("/etc/dialf");
        // Relative paths resolve under the base (the config dir).
        assert_eq!(resolve_under(Some(base), "jobs/x.yaml"), "/etc/dialf/jobs/x.yaml");
        // Absolute paths pass through unchanged.
        assert_eq!(resolve_under(Some(base), "/abs/x.yaml"), "/abs/x.yaml");
        // No base → unchanged.
        assert_eq!(resolve_under(None, "jobs/x.yaml"), "jobs/x.yaml");
    }

    #[test]
    fn resolve_inbound_reads_config_jobs_and_answer_only() {
        let mut cfg = BTreeMap::new();
        cfg.insert("+1111".to_string(), Some("jobs/a.yaml".to_string()));
        cfg.insert("+2222".to_string(), None);
        let state = test_state(cfg);

        // Some(path) → Job, resolved under the config dir; no device pin from config.
        assert_eq!(
            state.resolve_inbound("+1111"),
            Some(InboundHandler::Job {
                path: "/etc/dialf/jobs/a.yaml".to_string(),
                device: None,
            })
        );
        // None → answer only.
        assert_eq!(state.resolve_inbound("+2222"), Some(InboundHandler::AnswerOnly));
        // Unknown number → not handled.
        assert_eq!(state.resolve_inbound("+9999"), None);
    }

    #[test]
    fn override_takes_precedence_and_reverts_on_clear() {
        let mut cfg = BTreeMap::new();
        cfg.insert("+1111".to_string(), Some("jobs/a.yaml".to_string()));
        let state = test_state(cfg);

        state.register_override(
            "+1111".to_string(),
            AutoanswerOverride {
                token: 7,
                path: "/tmp/override.yaml".to_string(),
                device: Some("phone1".to_string()),
            },
        );
        // Override wins over config, keeping its own absolute path + device pin.
        assert_eq!(
            state.resolve_inbound("+1111"),
            Some(InboundHandler::Job {
                path: "/tmp/override.yaml".to_string(),
                device: Some("phone1".to_string()),
            })
        );
        // Removing the token's overrides reverts to the config entry.
        state.clear_overrides(7);
        assert_eq!(
            state.resolve_inbound("+1111"),
            Some(InboundHandler::Job {
                path: "/etc/dialf/jobs/a.yaml".to_string(),
                device: None,
            })
        );
    }

    #[test]
    fn card_lock_is_exclusive_and_releases_on_drop() {
        let state = test_state(BTreeMap::new());
        let guard = state.acquire_card().expect("first acquire succeeds");
        assert!(state.acquire_card().is_none(), "second acquire blocked while held");
        drop(guard);
        assert!(state.acquire_card().is_some(), "reacquire after release");
    }

    #[test]
    fn serve_lock_allows_only_one_session() {
        let state = test_state(BTreeMap::new());
        let guard = state.acquire_serve().expect("first serve registers");
        assert!(state.acquire_serve().is_none(), "second serve rejected");
        drop(guard);
        assert!(state.acquire_serve().is_some(), "reacquire after release");
    }
}
