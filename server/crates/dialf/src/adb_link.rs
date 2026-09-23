//! Wireless adb: report each phone's wireless-debugging endpoint and, when enabled, keep it
//! connected in this host's adb server — so adb, scrcpy and `devices share` work with no cable.
//!
//! The phone finds its own port (it browses `_adb-tls-connect._tcp` and keeps the service whose
//! IP is its own) and reports it on its heartbeat; dialfd already knows the IP from the WS peer.
//! Reporting is always on. Connecting is opt-in (`adb_share.autoconnect`), because it can start
//! an adb server under the dialfd service, and on macOS that asks for Local Network permission.
//!
//! adb does not reconnect a wireless device on its own after a drop, so every heartbeat re-checks
//! the host's device list and reconnects when the phone has gone. The phone counts as present
//! under either serial adb may use: `ip:port`, or the mDNS name adb gives a freshly paired phone it
//! connected by itself — connecting both would list it twice. Backoff limits only `adb connect`,
//! so an unpaired or blocked phone isn't retried every 30 s, but a fix is still seen within one
//! heartbeat.

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use serde::Serialize;

use crate::daemon::DaemonState;
use crate::protocol::AdbReport;
use crate::share::adb::list_devices;
use crate::share::Upstream;

const HOST_ADB_SERVER: &str = "127.0.0.1:5037";
const CONNECT_TIMEOUT: Duration = Duration::from_secs(10);
const LIST_TIMEOUT: Duration = Duration::from_secs(2);
const FIRST_RETRY: Duration = Duration::from_secs(30);
const MAX_RETRY: Duration = Duration::from_secs(300);

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum AdbState {
    /// Wireless debugging is off, or its port is not known yet.
    Off,
    /// The endpoint is known; not connected (auto-connect off, or not attempted yet).
    Available,
    Connecting,
    Connected,
    /// This host's adb key has never been paired with the phone.
    NeedsPairing,
    /// macOS Local Network permission is missing for whoever owns the adb server.
    BlockedLocalNetwork,
    AdbNotFound,
    Error,
}

/// What `devices.list` shows under `adb`.
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct AdbStatus {
    #[serde(skip_serializing_if = "Option::is_none")]
    pub endpoint: Option<String>,
    /// The serial adb lists the phone under, when connected — `ip:port`, or the mDNS name adb
    /// uses for a phone it connected by itself. What `adb -s` wants.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub serial: Option<String>,
    pub state: AdbState,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub detail: Option<String>,
}

impl AdbStatus {
    fn new(endpoint: Option<&str>, state: AdbState, detail: Option<String>) -> Self {
        Self {
            endpoint: endpoint.map(str::to_string),
            serial: None,
            state,
            detail,
        }
    }

    fn connected(endpoint: &str, serial: String) -> Self {
        Self {
            serial: Some(serial),
            ..Self::new(Some(endpoint), AdbState::Connected, None)
        }
    }
}

/// Every serial adb may list this phone under: the `ip:port` dialfd connects, and the mDNS form
/// adb uses when it connects a paired phone by itself.
pub fn serials(endpoint: &str, service: Option<&str>) -> Vec<String> {
    let mut v = vec![endpoint.to_string()];
    if let Some(name) = service.filter(|n| !n.is_empty()) {
        v.push(format!("{name}._adb-tls-connect._tcp"));
    }
    v
}

/// `ip:port` as adb names it, bracketing IPv6.
pub fn endpoint(ip: &str, port: u16) -> String {
    if ip.contains(':') {
        format!("[{ip}]:{port}")
    } else {
        format!("{ip}:{port}")
    }
}

const PAIRING_HINT: &str = "this host is not paired with the phone — pair once with `adb pair \
     <ip>:<pairing port>`, using the code from Wireless debugging → Pair device with pairing code";

/// Read `adb connect` output. `macos` decides what `No route to host` means.
pub fn classify_connect(output: &str, macos: bool) -> (AdbState, Option<String>) {
    let out = output.trim();
    let lower = out.to_ascii_lowercase();
    if lower.contains("connected to") {
        // Also covers "already connected to".
        return (AdbState::Connected, None);
    }
    if lower.contains("failed to authenticate") || lower.contains("unauthorized") || bare_failure(&lower) {
        return (AdbState::NeedsPairing, Some(PAIRING_HINT.to_string()));
    }
    if macos && lower.contains("no route to host") {
        // The phone is holding a live WebSocket to us while reporting this port, so the network
        // path exists. On macOS that leaves the Local Network privacy block, which fails exactly
        // like this for a process without the grant.
        return (
            AdbState::BlockedLocalNetwork,
            Some(
                "macOS Local Network permission is missing for the adb server. Allow it in System \
                 Settings → Privacy & Security → Local Network, and start the adb server (or \
                 dialfd) from a plain Terminal window or as a service — under tmux macOS never \
                 grants it"
                    .to_string(),
            ),
        );
    }
    let detail = if out.is_empty() {
        "adb connect printed nothing".to_string()
    } else {
        out.to_string()
    };
    (AdbState::Error, Some(detail))
}

/// `failed to connect to <host>:<port>` with no reason after it. Network failures always carry
/// one (`…: Connection refused`, `…: No route to host`); this bare form is what adb 37 prints when
/// TCP connects but the phone rejects the TLS handshake — i.e. this host is not paired. Observed on
/// a Pixel after "Forget" on the pairing; older adb said "failed to authenticate" instead.
fn bare_failure(lower: &str) -> bool {
    lower
        .strip_prefix("failed to connect to ")
        .is_some_and(|rest| !rest.contains(' ') && !rest.contains('\''))
}

/// Where adb lives, in order: config, PATH, the SDK's default location. The service runs with a
/// fixed PATH that holds none of the SDK directories, so the last step is what usually works.
pub fn resolve_adb(
    configured: Option<&Path>,
    on_path: Option<PathBuf>,
    home: Option<&Path>,
    macos: bool,
    exists: impl Fn(&Path) -> bool,
) -> Option<PathBuf> {
    if let Some(p) = configured {
        // A wrong configured path is reported, not quietly replaced by something else.
        return exists(p).then(|| p.to_path_buf());
    }
    if on_path.is_some() {
        return on_path;
    }
    let sdk = if macos { "Library/Android/sdk" } else { "Android/Sdk" };
    let default = home?.join(sdk).join("platform-tools/adb");
    exists(&default).then_some(default)
}

/// Expand a leading `~/` against `home`. Anything else is returned unchanged, including `~user/`.
pub fn expand_home(path: &Path, home: Option<&Path>) -> PathBuf {
    match (path.strip_prefix("~"), home) {
        (Ok(rest), Some(h)) => h.join(rest),
        _ => path.to_path_buf(),
    }
}

/// Delay before the next connect attempt after a failure: 30 s doubling to 5 min.
#[derive(Debug, Clone)]
pub struct Backoff {
    delay: Duration,
    not_before: Option<Instant>,
}

impl Default for Backoff {
    fn default() -> Self {
        Self {
            delay: FIRST_RETRY,
            not_before: None,
        }
    }
}

impl Backoff {
    pub fn ready(&self, now: Instant) -> bool {
        match self.not_before {
            None => true,
            Some(t) => now >= t,
        }
    }

    pub fn failed(&mut self, now: Instant) {
        self.not_before = Some(now + self.delay);
        self.delay = (self.delay * 2).min(MAX_RETRY);
    }

    pub fn reset(&mut self) {
        *self = Self::default();
    }
}

#[derive(Debug, Default)]
pub struct Link {
    endpoint: Option<String>,
    backoff: Backoff,
    busy: bool,
}

/// Per-device link state, keyed by dialf device id.
pub type Links = Arc<Mutex<HashMap<String, Link>>>;

/// Handle a heartbeat's `adb` report. Never blocks: records, and spawns the check.
pub fn on_report(state: &DaemonState, device_id: &str, report: AdbReport) {
    let Some(ip) = state
        .registry
        .lock()
        .unwrap()
        .get(device_id)
        .and_then(|d| d.addr.clone())
    else {
        return;
    };
    let ep = match (report.wifi_enabled, report.port) {
        (false, _) => {
            return set_status(state, device_id, AdbStatus::new(None, AdbState::Off, None));
        }
        (true, None) => {
            let detail = "wireless debugging is on; the phone has not found its port yet";
            let status = AdbStatus::new(None, AdbState::Off, Some(detail.to_string()));
            return set_status(state, device_id, status);
        }
        (true, Some(port)) => endpoint(&ip, port),
    };

    let (previous, may_connect) = {
        let mut links = state.adb_links.lock().unwrap();
        let link = links.entry(device_id.to_string()).or_default();
        if link.busy {
            return;
        }
        let changed = link.endpoint.as_deref() != Some(ep.as_str());
        // A new port is a fresh start: whatever failed before was about the old one.
        let previous = if changed {
            link.backoff.reset();
            link.endpoint.replace(ep.clone())
        } else {
            None
        };
        link.busy = true;
        // Backoff limits only `adb connect`. The loopback look at adb's device list is cheap and
        // runs on every heartbeat, so a re-pair (or a connection made by hand, or by adb itself)
        // shows up within one heartbeat rather than after a backoff of up to five minutes.
        (previous, changed || link.backoff.ready(Instant::now()))
    };

    let shown = state
        .registry
        .lock()
        .unwrap()
        .get(device_id)
        .and_then(|d| d.adb.as_ref().and_then(|s| s.endpoint.clone()));
    if shown.as_deref() != Some(ep.as_str()) {
        set_status(state, device_id, AdbStatus::new(Some(&ep), AdbState::Available, None));
    }

    let serials = serials(&ep, report.service.as_deref());
    let (state, id) = (state.clone(), device_id.to_string());
    tokio::spawn(async move {
        let outcome = drive(&state, &id, &ep, &serials, previous.as_deref(), may_connect).await;
        if let Some(link) = state.adb_links.lock().unwrap().get_mut(&id) {
            link.busy = false;
            match outcome.as_ref().map(|o| o.state) {
                // Still waiting out a backoff: nothing new learned.
                None => {}
                Some(AdbState::Connected | AdbState::Available) => link.backoff.reset(),
                Some(_) => link.backoff.failed(Instant::now()),
            }
        }
        if let Some(outcome) = outcome {
            set_status(&state, &id, outcome);
        }
    });
}

/// Check, and when enabled and allowed, connect one endpoint. `None`: waiting out a backoff,
/// so the shown state stands.
async fn drive(
    state: &DaemonState,
    id: &str,
    ep: &str,
    serials: &[String],
    previous: Option<&str>,
    may_connect: bool,
) -> Option<AdbStatus> {
    // Loopback to the host adb server is not subject to Local Network privacy, so this works
    // even when `adb connect` would not. Any serial counts: connecting `ip:port` as well would
    // list the phone twice, and a bare `adb shell` then fails with "more than one device".
    match host_state(serials).await {
        Some((serial, st)) if st == "device" => return Some(AdbStatus::connected(ep, serial)),
        Some((_, st)) if st == "unauthorized" => {
            let hint = Some(PAIRING_HINT.to_string());
            return Some(AdbStatus::new(Some(ep), AdbState::NeedsPairing, hint));
        }
        _ => {}
    }
    if !may_connect {
        return None;
    }
    Some(connect(state, id, ep, previous).await)
}

async fn connect(state: &DaemonState, id: &str, ep: &str, previous: Option<&str>) -> AdbStatus {

    let cfg = &state.config.adb_share;
    if !cfg.autoconnect {
        let detail = "auto-connect is off (adb_share.autoconnect in config.yaml)";
        return AdbStatus::new(Some(ep), AdbState::Available, Some(detail.to_string()));
    }

    let home = std::env::var_os("HOME").map(PathBuf::from);
    // Like every path in config.yaml: absolute, or relative to the config file — plus `~/`,
    // which is how an SDK path is naturally written.
    let configured = cfg.adb.as_deref().map(|p| {
        crate::daemon::resolve_path_under(
            state.config_dir.as_deref(),
            &expand_home(p, home.as_deref()),
        )
    });
    let Some(adb) = resolve_adb(
        configured.as_deref(),
        which::which("adb").ok(),
        home.as_deref(),
        cfg!(target_os = "macos"),
        |p| p.is_file(),
    ) else {
        let detail = "adb not found on the service PATH or in the default SDK location — set \
                      adb_share.adb in config.yaml";
        return AdbStatus::new(Some(ep), AdbState::AdbNotFound, Some(detail.to_string()));
    };

    if let Some(old) = previous {
        let _ = run_adb(&adb, &["disconnect", old]).await;
    }

    set_status(state, id, AdbStatus::new(Some(ep), AdbState::Connecting, None));
    let output = match run_adb(&adb, &["connect", ep]).await {
        Ok(out) => out,
        Err(e) => return AdbStatus::new(Some(ep), AdbState::Error, Some(format!("{e:#}"))),
    };
    let (s, detail) = classify_connect(&output, cfg!(target_os = "macos"));
    tracing::info!(endpoint = %ep, state = ?s, "adb autoconnect");
    if s != AdbState::Connected {
        return AdbStatus::new(Some(ep), s, detail);
    }
    match host_state(&[ep.to_string()]).await {
        Some((serial, st)) if st == "device" => AdbStatus::connected(ep, serial),
        _ => {
            let detail = format!(
                "adb reported `{}` but the phone is not in its device list",
                output.trim()
            );
            AdbStatus::new(Some(ep), AdbState::Error, Some(detail))
        }
    }
}

/// The first of `serials` the host adb server lists, with its state; `device` preferred.
async fn host_state(serials: &[String]) -> Option<(String, String)> {
    let upstream = Upstream::Tcp(HOST_ADB_SERVER.to_string());
    let devices = tokio::time::timeout(LIST_TIMEOUT, list_devices(&upstream))
        .await
        .ok()?
        .ok()?;
    let listed: Vec<_> = devices.into_iter().filter(|d| serials.contains(&d.serial)).collect();
    listed
        .iter()
        .find(|d| d.state == "device")
        .or_else(|| listed.first())
        .map(|d| (d.serial.clone(), d.state.clone()))
}

async fn run_adb(adb: &Path, args: &[&str]) -> anyhow::Result<String> {
    let child = tokio::process::Command::new(adb)
        .args(args)
        .kill_on_drop(true)
        .output();
    let out = tokio::time::timeout(CONNECT_TIMEOUT, child)
        .await
        .map_err(|_| {
            anyhow::anyhow!(
                "`adb {}` timed out after {}s",
                args.join(" "),
                CONNECT_TIMEOUT.as_secs()
            )
        })??;
    // adb prints the interesting part to stdout or stderr depending on version.
    Ok(format!(
        "{}{}",
        String::from_utf8_lossy(&out.stdout),
        String::from_utf8_lossy(&out.stderr)
    ))
}

fn set_status(state: &DaemonState, device_id: &str, status: AdbStatus) {
    if let Some(dev) = state.registry.lock().unwrap().get_mut(device_id) {
        dev.adb = Some(status);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn connected_output_in_both_forms() {
        let (s, _) = classify_connect("connected to 192.168.1.5:41195\n", true);
        assert_eq!(s, AdbState::Connected);
        let (s, _) = classify_connect("already connected to 192.168.1.5:41195", false);
        assert_eq!(s, AdbState::Connected);
    }

    #[test]
    fn an_unpaired_host_is_told_how_to_pair() {
        // adb 37's exact output after the phone forgot this host (Pixel, Android 17): no reason.
        let (s, d) = classify_connect("failed to connect to 192.168.100.179:42317\n", true);
        assert_eq!(s, AdbState::NeedsPairing);
        assert!(d.unwrap().contains("adb pair"));
        // Older adb wording.
        let (s, _) = classify_connect("failed to authenticate to 192.168.1.5:41195", true);
        assert_eq!(s, AdbState::NeedsPairing);
    }

    /// A reason after the address means the network, not pairing — must not become needs_pairing.
    #[test]
    fn a_failure_with_a_reason_is_not_a_pairing_problem() {
        let refused = "failed to connect to '192.168.1.5:41195': Connection refused";
        assert_eq!(classify_connect(refused, true).0, AdbState::Error);
        let unquoted = "failed to connect to 192.168.1.5:41195: Operation timed out";
        assert_eq!(classify_connect(unquoted, false).0, AdbState::Error);
    }

    /// On macOS this is the Local Network privacy block, not the network — the case that cost
    /// the debugging session this feature came out of.
    #[test]
    fn no_route_means_local_network_only_on_macos() {
        let out = "failed to connect to '192.168.1.5:41195': No route to host";
        let (s, d) = classify_connect(out, true);
        assert_eq!(s, AdbState::BlockedLocalNetwork);
        assert!(d.unwrap().contains("Local Network"));
        // Linux has no such gate, so it is reported as the plain error it is.
        let (s, d) = classify_connect(out, false);
        assert_eq!(s, AdbState::Error);
        assert!(d.unwrap().contains("No route to host"));
    }

    #[test]
    fn unknown_output_is_passed_through() {
        let (s, d) = classify_connect("failed to connect: Connection refused", true);
        assert_eq!(s, AdbState::Error);
        assert_eq!(d.as_deref(), Some("failed to connect: Connection refused"));
        let (_, d) = classify_connect("", true);
        assert_eq!(d.as_deref(), Some("adb connect printed nothing"));
    }

    #[test]
    fn both_serials_adb_may_use() {
        assert_eq!(serials("10.0.0.5:41195", None), vec!["10.0.0.5:41195"]);
        assert_eq!(serials("10.0.0.5:41195", Some("")), vec!["10.0.0.5:41195"]);
        // The form adb 37 listed after pairing a Pixel, verbatim.
        assert_eq!(
            serials("10.0.0.5:41195", Some("adb-48071FDAP0045Q-YX799R")),
            vec!["10.0.0.5:41195", "adb-48071FDAP0045Q-YX799R._adb-tls-connect._tcp"]
        );
    }

    #[test]
    fn endpoints_bracket_ipv6() {
        assert_eq!(endpoint("192.168.1.5", 41195), "192.168.1.5:41195");
        assert_eq!(endpoint("fe80::1", 41195), "[fe80::1]:41195");
    }

    #[test]
    fn adb_lookup_order() {
        let home = Path::new("/home/u");
        let cfg = Path::new("/opt/adb");
        let none = |_: &Path| false;
        let all = |_: &Path| true;
        let on_path = || Some(PathBuf::from("/usr/bin/adb"));

        assert_eq!(resolve_adb(Some(cfg), on_path(), Some(home), true, all), Some(cfg.into()));
        assert_eq!(resolve_adb(Some(cfg), on_path(), Some(home), true, none), None);
        assert_eq!(resolve_adb(None, on_path(), Some(home), true, none), on_path());
        assert_eq!(
            resolve_adb(None, None, Some(home), true, all),
            Some("/home/u/Library/Android/sdk/platform-tools/adb".into())
        );
        assert_eq!(
            resolve_adb(None, None, Some(home), false, all),
            Some("/home/u/Android/Sdk/platform-tools/adb".into())
        );
        assert_eq!(resolve_adb(None, None, Some(home), true, none), None);
        assert_eq!(resolve_adb(None, None, None, true, all), None);
    }

    #[test]
    fn a_configured_path_may_start_with_home() {
        let home = Some(Path::new("/Users/u"));
        assert_eq!(
            expand_home(Path::new("~/Library/Android/sdk/platform-tools/adb"), home),
            PathBuf::from("/Users/u/Library/Android/sdk/platform-tools/adb")
        );
        assert_eq!(expand_home(Path::new("/opt/adb"), home), PathBuf::from("/opt/adb"));
        assert_eq!(expand_home(Path::new("tools/adb"), home), PathBuf::from("tools/adb"));
        // `~other/` is another user's home; not ours to guess.
        assert_eq!(expand_home(Path::new("~other/adb"), home), PathBuf::from("~other/adb"));
        assert_eq!(expand_home(Path::new("~/adb"), None), PathBuf::from("~/adb"));
    }

    #[test]
    fn backoff_doubles_caps_and_resets() {
        let t0 = Instant::now();
        let mut b = Backoff::default();
        assert!(b.ready(t0));

        b.failed(t0);
        assert!(!b.ready(t0 + Duration::from_secs(29)));
        assert!(b.ready(t0 + Duration::from_secs(30)));

        b.failed(t0);
        assert!(!b.ready(t0 + Duration::from_secs(59)));
        assert!(b.ready(t0 + Duration::from_secs(60)));

        for _ in 0..10 {
            b.failed(t0);
        }
        assert!(!b.ready(t0 + MAX_RETRY - Duration::from_secs(1)));
        assert!(b.ready(t0 + MAX_RETRY), "capped at five minutes");

        b.reset();
        assert!(b.ready(t0));
    }

    #[test]
    fn status_omits_what_is_unknown() {
        let v = serde_json::to_value(AdbStatus::new(None, AdbState::Off, None)).unwrap();
        assert_eq!(v, serde_json::json!({ "state": "off" }));
        let status = AdbStatus::new(Some("1.2.3.4:5"), AdbState::NeedsPairing, Some("x".into()));
        let v = serde_json::to_value(status).unwrap();
        assert_eq!(v["state"], "needs_pairing");
        assert_eq!(v["endpoint"], "1.2.3.4:5");
    }
}
