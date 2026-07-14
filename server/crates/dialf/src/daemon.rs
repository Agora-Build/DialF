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
use crate::config::{AudioConfig, Config};
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
    /// Set by `job.cancel` (sent by `dialf run` on Ctrl+C) to stop the currently-running job.
    /// Reset to `false` when a `job.run` starts. One job runs at a time (see `card_busy`), so a
    /// single flag suffices. The runner checks it between steps; `wait_for_speech` checks it in
    /// its read loop.
    pub job_cancel: Arc<AtomicBool>,
    /// Set by `job.cancel { force: true }` (a *second* Ctrl+C on `dialf run`). Escalates the
    /// graceful `job_cancel` — also interrupts the current `play` (kills the playback child) and
    /// `wait`. Reset with `job_cancel` when a `job.run` starts.
    pub job_force: Arc<AtomicBool>,
    /// Set when the driving phone app relaunches (a changed `instance_id`) mid-job — the job can't
    /// continue meaningfully, so it's aborted and cleaned up. Observed by *every* running job
    /// (`dialf run` and auto-answer), unlike `job_cancel`/`job_force` which are `dialf run` only.
    /// Reset when a job starts.
    pub job_abort: Arc<AtomicBool>,
    /// Last `instance_id` seen per device (the app's per-launch nonce). A different value on a new
    /// connection means the app relaunched (vs a plain reconnect). `None`-reporting apps never
    /// trigger an abort.
    pub instances: Arc<Mutex<HashMap<String, String>>>,
    /// device_id → call_id for daemon-driven calls that were active (marker on disk) when THIS
    /// daemon started — i.e. calls that may have outlived a crash of the previous instance. When the
    /// phone re-reports that exact call active with no job running, it's hung up. Entries are cleared
    /// as they're reconciled. Populated from marker files at startup.
    pub pending_orphans: Arc<Mutex<HashMap<String, String>>>,
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
/// Match tokens for spotting stray audio processes squatting on our card — tool-agnostic. Returns
/// `(devices, tools)`: `devices` = the configured device name(s) (also pulled from a `coreaudio`
/// command arg); `tools` = the audio tool binaries from any explicit command overrides (whatever
/// the user set — `sox` / `arecord` / `ffmpeg` / …), empty when the tool is auto-detected.
fn audio_match(cfg: &AudioConfig) -> (Vec<String>, Vec<String>) {
    let mut devices: Vec<String> = Vec::new();
    let mut tools: Vec<String> = Vec::new();
    for d in [&cfg.capture_device, &cfg.playback_device].into_iter().flatten() {
        let d = d.trim();
        if !d.is_empty() {
            devices.push(d.to_string());
        }
    }
    for cmd in [&cfg.capture_cmd, &cfg.playback_cmd].into_iter().flatten() {
        // The device is the arg right after "coreaudio" (macOS sox); ALSA/ffmpeg users set
        // capture_device/playback_device instead.
        if let Some(i) = cmd.iter().position(|a| a == "coreaudio") {
            if let Some(dev) = cmd.get(i + 1).map(|s| s.trim()).filter(|s| !s.is_empty()) {
                devices.push(dev.to_string());
            }
        }
        // The tool is argv[0] — never hardcoded; whatever the user configured (basename only).
        if let Some(base) = cmd
            .first()
            .map(|f| f.rsplit('/').next().unwrap_or(f).trim())
            .filter(|s| !s.is_empty())
        {
            tools.push(base.to_string());
        }
    }
    devices.sort();
    devices.dedup();
    tools.sort();
    tools.dedup();
    (devices, tools)
}

/// Should a process with this command line be reaped? True when it references one of our devices
/// AND — if the tool is pinned in config — its binary basename matches one of those tools. When
/// the tool is auto-detected (`tools` empty) the device match alone is enough. Pure, so the reap
/// decision is testable without spawning processes.
fn should_reap(cmdline: &str, devices: &[String], tools: &[String]) -> bool {
    if cmdline.is_empty() || !devices.iter().any(|d| cmdline.contains(d.as_str())) {
        return false;
    }
    tools.is_empty() || {
        let bin = cmdline.split_whitespace().next().unwrap_or("");
        let base = bin.rsplit('/').next().unwrap_or(bin);
        tools.iter().any(|t| t == base)
    }
}

/// Kill any leftover audio process bound to our card. A clean shutdown reaps the daemon's own
/// capture/playback children via `Drop`, but a `SIGKILL`/crash can't — those get reparented to
/// init and squat on the card, so the next capture yields "produced no audio" until killed (we hit
/// exactly this: audio children from a hard-kill held the card for days). A freshly starting daemon
/// has no legitimate in-flight job, so any process on *our* device is stale. Matched by device name
/// (and, when the command is pinned, the configured tool) — no hardcoded tool. Best-effort,
/// POSIX-only (needs `pgrep`/`ps`/`kill`); does nothing if they're absent.
fn reap_stray_audio(cfg: &AudioConfig) {
    let (devices, tools) = audio_match(cfg);
    if devices.is_empty() {
        return; // nothing to match on -> no-op (never a false kill)
    }
    // Candidate PIDs: processes whose command line references one of our device names.
    let mut pids: Vec<i32> = Vec::new();
    for dev in &devices {
        if let Ok(o) = std::process::Command::new("pgrep").args(["-f", dev]).output() {
            pids.extend(
                String::from_utf8_lossy(&o.stdout)
                    .lines()
                    .filter_map(|l| l.trim().parse::<i32>().ok()),
            );
        }
    }
    pids.sort_unstable();
    pids.dedup();
    for pid in pids {
        let cmdline = match std::process::Command::new("ps")
            .args(["-p", &pid.to_string(), "-o", "command="])
            .output()
        {
            Ok(o) => String::from_utf8_lossy(&o.stdout).trim().to_string(),
            Err(_) => continue,
        };
        if should_reap(&cmdline, &devices, &tools) {
            tracing::warn!(pid, cmd = %cmdline, "reaping stray audio process on our card (orphaned by a prior hard-kill)");
            let _ = std::process::Command::new("kill")
                .args(["-9", &pid.to_string()])
                .status();
        }
    }
}

/// Which terminal multiplexer (if any) the given env indicates. Pure, so it's testable.
fn multiplexer_name(
    tmux_set: bool,
    term_program: &str,
    sty_set: bool,
    zellij_set: bool,
) -> Option<&'static str> {
    if tmux_set || term_program == "tmux" {
        Some("tmux")
    } else if sty_set {
        Some("screen")
    } else if zellij_set {
        Some("zellij")
    } else {
        None
    }
}

/// macOS attributes microphone access (TCC) to the "responsible process". A daemon started from
/// inside a terminal multiplexer inherits the multiplexer as its responsible process — which usually
/// has no mic grant — so the capture stream opens but delivers silence, and `wait_for_speech` times
/// out with no voice. Warn loudly. launchd services and plain Terminal windows are unaffected;
/// `dialf service install --user` is the fix. No-op off macOS.
fn warn_if_under_multiplexer() {
    if !cfg!(target_os = "macos") {
        return;
    }
    let term_program = std::env::var("TERM_PROGRAM").unwrap_or_default();
    if let Some(mux) = multiplexer_name(
        std::env::var_os("TMUX").is_some(),
        &term_program,
        std::env::var_os("STY").is_some(),
        std::env::var_os("ZELLIJ").is_some(),
    ) {
        tracing::warn!(
            "running under {mux}: on macOS the microphone is attributed to the {mux} process, which \
             usually lacks a mic grant, so audio capture may be SILENT (wait_for_speech times out \
             with no voice). Fix: `dialf service install --user` (launchd gives the daemon its own \
             mic access), or run `dialf daemon` from a plain Terminal window instead of {mux}."
        );
    }
}

pub async fn run(config: Config, config_path: PathBuf) -> anyhow::Result<()> {
    warn_if_under_multiplexer();
    // Clean up `sox` orphaned by a previously hard-killed daemon (SIGKILL bypasses our Drop
    // cleanup). Skip if another daemon is already live on the control socket — its audio children
    // are legitimate, and the binds below will fail cleanly anyway.
    if std::os::unix::net::UnixStream::connect(config.control_socket_path()).is_err() {
        reap_stray_audio(&config.audio);
    }

    let engine = Arc::new(AudioEngine::new(config.audio.clone()));
    let registry = Arc::new(Mutex::new(Registry::new()));
    let (events, _) = tokio::sync::broadcast::channel(EVENT_CHANNEL_CAP);
    // Markers left by a prior (crashed) instance name calls that may still be up — reconciled (hung
    // up) when the phone re-reports them.
    let pending_orphans = load_call_markers(&config);
    if !pending_orphans.is_empty() {
        tracing::warn!(
            count = pending_orphans.len(),
            "found call marker(s) from a prior run — will hang up orphaned calls when the phone re-reports them"
        );
    }
    let state = DaemonState {
        registry,
        engine,
        hub: Arc::new(Hub::new()),
        config: Arc::new(config),
        // Canonicalize first so `config_dir` is an absolute real directory: relative paths inside
        // the config (autoanswer job paths, record_dir) then resolve against the config file's own
        // location regardless of the daemon's CWD (the service runs with cwd=/). Falls back to the
        // path as-given if it can't be canonicalized (e.g. a missing default config).
        config_dir: config_path
            .canonicalize()
            .unwrap_or(config_path)
            .parent()
            .map(|p| p.to_path_buf()),
        card_busy: Arc::new(AtomicBool::new(false)),
        serve_busy: Arc::new(AtomicBool::new(false)),
        job_cancel: Arc::new(AtomicBool::new(false)),
        job_force: Arc::new(AtomicBool::new(false)),
        job_abort: Arc::new(AtomicBool::new(false)),
        instances: Arc::new(Mutex::new(HashMap::new())),
        pending_orphans: Arc::new(Mutex::new(pending_orphans)),
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
        socket = %state.config.control_socket_path().display(),
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
            // One-shot `dialf play` isn't job-cancelable; never force-interrupt it.
            tokio::task::spawn_blocking(move || {
                engine.play_file(Path::new(&file), None, &AtomicBool::new(false))
            })
            .await??;
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
            // Audio-only jobs (no call/sms steps) need no phone — they drive the sound card, or a
            // virtual device like BlackHole, only. Require a real device only when the job (or an
            // explicit `--device`) asks for one, so `dialf run <audio-job>` works with no phone
            // connected (e.g. an agent eval over BlackHole virtual audio).
            let device_id = if device.is_some() || job_needs_phone(&job) {
                resolve_device(state, device)?
            } else {
                "local".to_string()
            };
            // One sound card → one call/recording at a time. Held until the job finishes.
            let _card = state
                .acquire_card()
                .context("phone busy: a call or recording is already in progress")?;
            // Fresh run: clear any stale cancel/force/abort from a prior job so this one isn't
            // killed at once.
            state.job_cancel.store(false, Ordering::SeqCst);
            state.job_force.store(false, Ordering::SeqCst);
            state.job_abort.store(false, Ordering::SeqCst);
            // `dialf run` is outbound/one-shot: the job owns call setup (call.dial, etc.).
            let (outcomes, recording) = run_job_on_device(
                state,
                device_id,
                job,
                false,
                state.job_cancel.clone(),
                state.job_force.clone(),
                state.job_abort.clone(),
            )
            .await?;
            let recording = recording.map(|r| json!({ "rx": r.rx, "tx": r.tx, "mix": r.mix }));
            Ok(ok_data(&id, json!({ "steps": outcomes, "recording": recording })))
        }
        ControlOp::JobCancel { force } => {
            // `dialf run` sends this on Ctrl+C; the running job's runner / wait_for_speech observe
            // the flag and stop. A second Ctrl+C sets `force`, which also interrupts a mid-flight
            // `play`/`wait`. Force implies cancel. The recording is finalized either way. Harmless
            // if no job is running.
            if force {
                state.job_force.store(true, Ordering::SeqCst);
            }
            state.job_cancel.store(true, Ordering::SeqCst);
            tracing::info!(target: "job", "job cancel requested (force={force})");
            Ok(ok_data(&id, json!({ "cancelled": true, "force": force })))
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

/// Resolve a possibly-relative path under `base` (e.g. the config dir). Absolute paths, and the
/// `base = None`/empty case, are returned unchanged.
fn resolve_path_under(base: Option<&Path>, path: &Path) -> PathBuf {
    let joined = match base {
        Some(b) if !b.as_os_str().is_empty() && path.is_relative() => b.join(path),
        _ => path.to_path_buf(),
    };
    // Lexically clean the result (drop interior `./` and repeated separators) so a `./foo` input
    // yields `<base>/foo`, not `<base>/./foo`. Not canonicalize — no filesystem / symlink access,
    // and `..` is left intact.
    joined.components().collect()
}

/// String convenience over [`resolve_path_under`] (job paths are carried as `String`s).
fn resolve_under(base: Option<&Path>, path: &str) -> String {
    resolve_path_under(base, Path::new(path))
        .to_string_lossy()
        .into_owned()
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
                *file = resolve_path_under(Some(base), Path::new(&*file))
                    .to_string_lossy()
                    .into_owned();
            }
        }
    }
    Ok(job)
}

// --- crash-recovery call markers -------------------------------------------
// A marker records that a daemon-driven call is active, so if the daemon crashes we can hang the
// orphaned call up on restart. One file per device (content: "device_id\ncall_id"), co-located with
// the control socket. Only calls driven by a job (card held) get a marker, so a user's *manual*
// call never does — and is never touched by the orphan cleanup.

fn call_marker_dir(cfg: &Config) -> PathBuf {
    cfg.control_socket_path()
        .parent()
        .map(Path::to_path_buf)
        .unwrap_or_else(std::env::temp_dir)
        .join("dialf-callmarks")
}

fn call_marker_file(cfg: &Config, device_id: &str) -> PathBuf {
    let safe: String = device_id
        .chars()
        .map(|c| if c.is_ascii_alphanumeric() || c == '-' || c == '_' { c } else { '_' })
        .collect();
    call_marker_dir(cfg).join(format!("call-{safe}"))
}

/// Record `device_id`'s active daemon-driven call. Best-effort (a failure just means no crash
/// recovery for this call).
pub fn write_call_marker(cfg: &Config, device_id: &str, call_id: &str) {
    let dir = call_marker_dir(cfg);
    if std::fs::create_dir_all(&dir).is_ok() {
        let _ = std::fs::write(call_marker_file(cfg, device_id), format!("{device_id}\n{call_id}"));
    }
}

/// Clear a device's call marker (its call ended, or was cleaned up). Best-effort.
pub fn remove_call_marker(cfg: &Config, device_id: &str) {
    let _ = std::fs::remove_file(call_marker_file(cfg, device_id));
}

/// Read markers left by a prior daemon instance: `device_id -> call_id`.
fn load_call_markers(cfg: &Config) -> HashMap<String, String> {
    let mut out = HashMap::new();
    if let Ok(entries) = std::fs::read_dir(call_marker_dir(cfg)) {
        for e in entries.flatten() {
            if let Ok(text) = std::fs::read_to_string(e.path()) {
                let mut lines = text.lines();
                if let (Some(dev), Some(call)) = (lines.next(), lines.next()) {
                    out.insert(dev.to_string(), call.to_string());
                }
            }
        }
    }
    out
}

/// True if the job has any step that talks to the phone (a call or SMS). Audio-only jobs — just
/// `audio.play` / `audio.wait_for_speech` / `wait` / `log` — need no device and can drive the
/// sound card, or a virtual device like BlackHole, with no phone connected.
fn job_needs_phone(job: &[schema::Step]) -> bool {
    job.iter().any(|s| {
        matches!(
            s.kind,
            schema::StepKind::CallDial { .. }
                | schema::StepKind::CallWaitAnswered { .. }
                | schema::StepKind::CallAnswer
                | schema::StepKind::CallHangup
                | schema::StepKind::SmsSend { .. }
        )
    })
}

/// Run a parsed job against a device, recording when configured. The duplex session is started
/// inside the blocking task so the capture thread records rx before the first step runs; the
/// recording is always finalized (even on job error) so mix.wav is written and both legs padded.
pub async fn run_job_on_device(
    state: &DaemonState,
    device_id: String,
    job: Vec<schema::Step>,
    inbound: bool,
    cancel: Arc<AtomicBool>,
    force: Arc<AtomicBool>,
    abort: Arc<AtomicBool>,
) -> anyhow::Result<(Vec<runner::StepOutcome>, Option<RecordOutput>)> {
    let engine = state.engine.clone();
    // A relative `record_dir` resolves against the config file's dir (like autoanswer job paths),
    // so recordings land next to the config regardless of the daemon's CWD; absolute is unchanged.
    let record_dir = state
        .config
        .audio
        .record_dir
        .as_deref()
        .map(|d| resolve_path_under(state.config_dir.as_deref(), d));
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
        let mut io = PhoneJobIo::new(
            hub, engine, rt, registry, device_id, session, inbound, cancel, force, abort,
        );
        let run = runner::run_job(&job, &mut io);
        // Finalize the recording BEFORE propagating a run error, so a cancelled/force-cancelled
        // job still saves its audio files (rx/tx/mix) up to the cancel point.
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

    #[test]
    fn audio_match_devices_and_tool_basename() {
        let mut cfg = AudioConfig::default();
        cfg.capture_device = Some("MiniFuse 2".to_string()); // named-device fields
        cfg.playback_device = Some("MiniFuse 2".to_string());
        // A pinned command with a full tool path + coreaudio device — tool-agnostic: we take the
        // tool from argv[0], never a hardcoded name, and reduce it to its basename.
        cfg.capture_cmd = Some(vec![
            "/opt/homebrew/bin/sox".into(),
            "-t".into(),
            "coreaudio".into(),
            "USB Audio".into(),
            "-t".into(),
            "raw".into(),
        ]);
        // A different tool proves nothing is hardcoded to sox.
        cfg.playback_cmd = Some(vec!["ffmpeg".into(), "-i".into(), "{file}".into()]);

        let (devices, tools) = audio_match(&cfg);
        assert!(devices.contains(&"MiniFuse 2".to_string()));
        assert!(devices.contains(&"USB Audio".to_string())); // pulled from the coreaudio arg
        assert_eq!(devices.iter().filter(|d| *d == "MiniFuse 2").count(), 1); // deduped
        assert_eq!(tools, vec!["ffmpeg".to_string(), "sox".to_string()]); // basenames, sorted

        // Nothing configured -> no devices -> reap is a no-op.
        let (d, t) = audio_match(&AudioConfig::default());
        assert!(d.is_empty() && t.is_empty());
    }

    #[test]
    fn should_reap_matches_device_and_tool() {
        let devices = vec!["MiniFuse 2".to_string()];
        let tools = vec!["sox".to_string()];
        // Our tool on our device -> reap (full path is reduced to its basename).
        assert!(should_reap(
            "/opt/homebrew/bin/sox -t coreaudio MiniFuse 2 -t raw",
            &devices,
            &tools
        ));
        // Our device but a different (pinned) tool -> leave it alone.
        assert!(!should_reap(
            "/usr/bin/ffmpeg -f avfoundation -i MiniFuse 2",
            &devices,
            &tools
        ));
        // A different device -> not ours.
        assert!(!should_reap(
            "/opt/homebrew/bin/sox -t coreaudio Other Card",
            &devices,
            &tools
        ));
        // Auto-detected tool (none pinned) -> device match alone is enough, any tool.
        assert!(should_reap("/usr/bin/arecord -D MiniFuse 2", &devices, &[]));
        // Empty command line -> never.
        assert!(!should_reap("", &devices, &tools));
    }

    #[test]
    fn job_needs_phone_only_for_call_or_sms() {
        // Audio-only job -> runs with no phone (BlackHole / sound-card eval).
        let audio_only =
            schema::parse("- type: audio.play\n  file: p.wav\n- type: audio.wait_for_speech\n")
                .unwrap();
        assert!(!job_needs_phone(&audio_only));

        // Any call/SMS step -> a phone is required.
        let with_call =
            schema::parse("- type: audio.play\n  file: p.wav\n- type: call.hangup\n").unwrap();
        assert!(job_needs_phone(&with_call));
        let with_sms = schema::parse("- type: sms.send\n  to: \"+100\"\n  body: hi\n").unwrap();
        assert!(job_needs_phone(&with_sms));
    }

    #[test]
    fn detects_terminal_multiplexers() {
        assert_eq!(multiplexer_name(true, "", false, false), Some("tmux")); // $TMUX set
        assert_eq!(multiplexer_name(false, "tmux", false, false), Some("tmux")); // TERM_PROGRAM
        assert_eq!(multiplexer_name(false, "", true, false), Some("screen")); // $STY
        assert_eq!(multiplexer_name(false, "", false, true), Some("zellij")); // $ZELLIJ
        // Plain terminals -> no warning.
        assert_eq!(multiplexer_name(false, "Apple_Terminal", false, false), None);
        assert_eq!(multiplexer_name(false, "iTerm.app", false, false), None);
    }

    #[test]
    fn call_markers_round_trip() {
        let dir = std::env::temp_dir().join(format!("dialf-markers-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        let mut cfg = Config::default();
        cfg.control_socket = Some(dir.join("dialfd.sock")); // marker dir = <dir>/dialf-callmarks
        assert!(load_call_markers(&cfg).is_empty());

        write_call_marker(&cfg, "pixel-9-pro", "call-abc");
        write_call_marker(&cfg, "other/../evil", "call-xyz"); // unsafe chars are sanitized in the name
        let loaded = load_call_markers(&cfg);
        assert_eq!(loaded.get("pixel-9-pro"), Some(&"call-abc".to_string()));
        assert_eq!(loaded.get("other/../evil"), Some(&"call-xyz".to_string())); // real id kept in file

        remove_call_marker(&cfg, "pixel-9-pro");
        let loaded = load_call_markers(&cfg);
        assert!(!loaded.contains_key("pixel-9-pro"));
        assert_eq!(loaded.get("other/../evil"), Some(&"call-xyz".to_string()));
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn resolve_paths_relative_under_base_absolute_unchanged() {
        let base = Path::new("/etc/dialf");
        // Relative -> joined under the base (config/job dir).
        assert_eq!(
            resolve_path_under(Some(base), Path::new("jobs/x.yaml")),
            PathBuf::from("/etc/dialf/jobs/x.yaml")
        );
        // A leading `./` is normalized away, not concatenated (no interior `/./`).
        assert_eq!(
            resolve_path_under(Some(base), Path::new("./jobs/x.yaml")),
            PathBuf::from("/etc/dialf/jobs/x.yaml")
        );
        assert_eq!(resolve_under(Some(base), "./jobs/x.yaml"), "/etc/dialf/jobs/x.yaml");
        // Absolute -> used as-is.
        assert_eq!(
            resolve_path_under(Some(base), Path::new("/var/rec")),
            PathBuf::from("/var/rec")
        );
        // Empty base / no base -> unchanged (can't anchor).
        assert_eq!(resolve_path_under(Some(Path::new("")), Path::new("rel")), PathBuf::from("rel"));
        assert_eq!(resolve_path_under(None, Path::new("rel")), PathBuf::from("rel"));
        // String convenience (used for autoanswer job paths) mirrors it.
        assert_eq!(resolve_under(Some(base), "jobs/x.yaml"), "/etc/dialf/jobs/x.yaml");
        assert_eq!(resolve_under(Some(base), "/abs/x.yaml"), "/abs/x.yaml");
    }

    #[test]
    fn load_job_file_resolves_audio_play_against_job_dir() {
        // A relative `audio.play` in a job resolves against the job file's own dir; absolute stays.
        let dir = std::env::temp_dir().join(format!("dialf-jobpath-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let job_path = dir.join("job.yaml");
        // `./prompts/…` must resolve to `<dir>/prompts/…`, not `<dir>/./prompts/…`.
        std::fs::write(
            &job_path,
            "- type: audio.play\n  file: ./prompts/hi.wav\n- type: audio.play\n  file: /abs/there.wav\n",
        )
        .unwrap();
        let steps = load_job_file(&job_path.to_string_lossy()).unwrap();
        let files: Vec<&str> = steps
            .iter()
            .filter_map(|s| match &s.kind {
                schema::StepKind::AudioPlay { file } => Some(file.as_str()),
                _ => None,
            })
            .collect();
        assert_eq!(files[0], dir.join("prompts/hi.wav").to_string_lossy());
        assert!(!files[0].contains("/./"), "interior /./ not normalized: {}", files[0]);
        assert_eq!(files[1], "/abs/there.wav");
        let _ = std::fs::remove_dir_all(&dir);
    }

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
            job_cancel: Arc::new(AtomicBool::new(false)),
            job_force: Arc::new(AtomicBool::new(false)),
            job_abort: Arc::new(AtomicBool::new(false)),
            instances: Arc::new(Mutex::new(HashMap::new())),
            pending_orphans: Arc::new(Mutex::new(HashMap::new())),
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
