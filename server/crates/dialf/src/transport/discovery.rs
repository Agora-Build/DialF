//! mDNS service advertisement so phones can auto-discover `dialfd` on the LAN.
//!
//! Advertises `_dialfd._tcp` with the phone WebSocket port via the **OS-native** mDNS
//! responder — `dns-sd` (Bonjour) on macOS, `avahi-publish` on Linux. We shell out rather
//! than use an in-process mDNS crate because the native responders handle multicast
//! interface/routing correctly (a userspace crate failed to emit multicast on macOS).
//!
//! Keep the returned [`Advert`] alive; dropping it unregisters.

use std::io::{BufRead, BufReader};
use std::net::SocketAddr;
use std::process::{Child, Command, Stdio};
use std::sync::{mpsc, Arc, Mutex};
use std::time::Duration;

use serde_json::json;

use crate::config::{Config, DEFAULT_SERVICE_TYPE};
use crate::reachability::{self, Finding};

/// How long to wait for `avahi-publish` to confirm. It fails within milliseconds when
/// avahi-daemon is down or refuses us, and confirms as fast when all is well.
const CONFIRM_WAIT: Duration = Duration::from_secs(3);

/// The advertisement's state for the daemon's lifetime.
pub enum Advert {
    Active(Advertiser),
    /// Loopback bind: deliberately not advertised.
    Loopback,
    Failed(Finding),
}

/// Holds the native mDNS registration process; unregisters on drop.
pub struct Advertiser {
    child: Child,
    via: &'static str,
    /// What the responder last said on stderr — its reason, if it later exits.
    stderr_tail: Arc<Mutex<String>>,
}

impl Drop for Advertiser {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

impl Advert {
    /// Current state for `server.info`. An advertiser that has since exited (avahi-daemon
    /// restarted or stopped) is reported as failed with its reason — it does not come back
    /// until dialfd restarts.
    pub fn report(&mut self) -> serde_json::Value {
        if let Advert::Active(a) = self {
            if let Ok(Some(status)) = a.child.try_wait() {
                let said = a.stderr_tail.lock().unwrap().clone();
                let f = if a.via == "avahi-publish" {
                    reachability::avahi_failure(&said, reachability::distro())
                } else {
                    Finding {
                        problem: format!("{} exited ({status}): {}", a.via, said.trim()),
                        fix: "restart dialfd".into(),
                    }
                };
                *self = Advert::Failed(f);
            }
        }
        match self {
            Advert::Active(a) => json!({ "state": "advertising", "via": a.via }),
            Advert::Loopback => json!({ "state": "off", "reason": "ws_bind is loopback" }),
            Advert::Failed(f) => json!({ "state": "failed", "problem": f.problem, "fix": f.fix }),
        }
    }
}

/// Start advertising `dialfd` via the OS mDNS responder, and log the outcome — on failure,
/// the cause and the fix for this OS.
pub fn advertise(config: &Config) -> Advert {
    let advert = start(config);
    match &advert {
        Advert::Active(a) => tracing::info!(via = a.via, "advertising via mDNS"),
        Advert::Loopback => {
            tracing::info!(ws_bind = %config.ws_bind, "loopback bind — not advertising via mDNS")
        }
        Advert::Failed(f) => tracing::warn!(
            problem = %f.problem,
            fix = %f.fix,
            "mDNS advertisement failed — phones will not discover this daemon (they can still \
             connect to a typed-in address)"
        ),
    }
    advert
}

fn start(config: &Config) -> Advert {
    let addr: SocketAddr = match config.ws_bind.parse() {
        Ok(a) => a,
        Err(e) => {
            return Advert::Failed(Finding {
                problem: format!("ws_bind `{}` is not an address: {e}", config.ws_bind),
                fix: "set ws_bind to e.g. 0.0.0.0:8765 in the config".into(),
            })
        }
    };
    // Clear orphaned advertisers from dead daemons before registering our own — see
    // reap_stale_advertisers. Do this even when we won't advertise ourselves.
    reap_stale_advertisers();

    // A loopback bind is unreachable from the LAN — advertising it would only hand phones
    // an unconnectable decoy (and a scratch/test daemon on 127.0.0.1 must never pollute
    // the network's discovery).
    if addr.ip().is_loopback() {
        return Advert::Loopback;
    }
    let port = addr.port().to_string();
    let instance = &config.instance_name;
    // The CLIs take the bare service type (no instance, no trailing .local).
    let service_type = DEFAULT_SERVICE_TYPE
        .trim_end_matches('.')
        .trim_end_matches(".local");
    let ver = format!("ver={}", env!("CARGO_PKG_VERSION"));

    if cfg!(target_os = "macos") {
        // dns-sd -R <name> <type> <domain> <port> [k=v ...]
        return match Command::new("dns-sd")
            .args(["-R", instance, service_type, "local.", &port, &ver])
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .spawn()
        {
            Ok(child) => Advert::Active(Advertiser {
                child,
                via: "dns-sd",
                stderr_tail: Default::default(),
            }),
            Err(e) => Advert::Failed(Finding {
                problem: format!("could not run `dns-sd` (Bonjour): {e}"),
                fix: "dns-sd ships with macOS at /usr/bin/dns-sd — check that /usr/bin is on \
                      the daemon's PATH"
                    .into(),
            }),
        };
    }

    // avahi-publish -s <name> <type> <port> [k=v ...]
    let spawned = Command::new("avahi-publish")
        .args(["-s", instance, service_type, &port, &ver])
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn();
    let mut child = match spawned {
        Ok(c) => c,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
            return Advert::Failed(Finding {
                problem: "`avahi-publish` is not installed (or not on the daemon's PATH), so \
                          dialfd cannot advertise itself and phones cannot discover it"
                    .into(),
                fix: reachability::avahi_install_fix(reachability::distro()),
            })
        }
        Err(e) => {
            return Advert::Failed(Finding {
                problem: format!("could not run `avahi-publish`: {e}"),
                fix: reachability::avahi_install_fix(reachability::distro()),
            })
        }
    };

    // stdout says "Established under name …" once registered; EOF means it exited. It is
    // drained for the child's lifetime so the pipe never fills.
    let (tx, rx) = mpsc::channel::<String>();
    let stdout = child.stdout.take().expect("piped");
    std::thread::spawn(move || {
        for line in BufReader::new(stdout).lines().map_while(Result::ok) {
            let _ = tx.send(line);
        }
    });
    let stderr_tail: Arc<Mutex<String>> = Default::default();
    let stderr = child.stderr.take().expect("piped");
    let tail = stderr_tail.clone();
    let relay = std::thread::spawn(move || {
        for line in BufReader::new(stderr).lines().map_while(Result::ok) {
            tracing::warn!(line = %line, "avahi-publish");
            *tail.lock().unwrap() = line;
        }
    });

    let deadline = std::time::Instant::now() + CONFIRM_WAIT;
    loop {
        match rx.recv_timeout(deadline.saturating_duration_since(std::time::Instant::now())) {
            Ok(line) if line.contains("Established") => break,
            Ok(_) => continue,
            Err(mpsc::RecvTimeoutError::Timeout) => {
                tracing::info!("avahi-publish has not confirmed yet — assuming it will");
                break;
            }
            Err(mpsc::RecvTimeoutError::Disconnected) => {
                let _ = child.wait();
                let _ = relay.join();
                let said = stderr_tail.lock().unwrap().clone();
                return Advert::Failed(reachability::avahi_failure(&said, reachability::distro()));
            }
        }
    }
    Advert::Active(Advertiser { child, via: "avahi-publish", stderr_tail })
}

/// Best-effort: kill ORPHANED `_dialfd._tcp` advertisers — whatever their instance name.
/// A live daemon's advertiser is its child process; when a daemon dies without `Drop`
/// (SIGKILL, `launchctl bootout`, a killed scratch run) the advertiser is reparented to
/// pid 1 and keeps advertising a dead endpoint forever, luring phones away from live
/// daemons. Orphaned (ppid 1) + our service type is a precise signature: a healthy
/// daemon's advertiser is never touched, regardless of scope or instance name.
fn reap_stale_advertisers() {
    let needle = DEFAULT_SERVICE_TYPE.trim_end_matches('.').trim_end_matches(".local");
    let Ok(out) = Command::new("ps").args(["-axo", "pid=,ppid=,args="]).output() else {
        return;
    };
    for line in String::from_utf8_lossy(&out.stdout).lines() {
        let mut it = line.split_whitespace();
        let (Some(pid), Some(ppid)) = (it.next(), it.next()) else { continue };
        let cmd = it.collect::<Vec<_>>().join(" ");
        if ppid != "1"
            || !cmd.contains(needle)
            || !(cmd.starts_with("dns-sd") || cmd.starts_with("avahi-publish"))
        {
            continue;
        }
        if let Ok(pid) = pid.parse::<i32>() {
            tracing::info!(pid, cmd = %cmd, "reaping orphaned mDNS advertiser (its daemon is gone)");
            unsafe { libc::kill(pid, libc::SIGTERM) };
        }
    }
}
