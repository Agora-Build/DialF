//! mDNS service advertisement so phones can auto-discover `dialfd` on the LAN.
//!
//! Advertises `_dialfd._tcp` with the phone WebSocket port via the **OS-native** mDNS
//! responder — `dns-sd` (Bonjour) on macOS, `avahi-publish` on Linux. We shell out rather
//! than use an in-process mDNS crate because the native responders handle multicast
//! interface/routing correctly (a userspace crate failed to emit multicast on macOS).
//!
//! The returned [`Advertiser`] keeps the registration child alive; drop it to unregister.

use std::net::SocketAddr;
use std::process::{Child, Command, Stdio};

use anyhow::Context;

use crate::config::{Config, DEFAULT_SERVICE_TYPE};

/// Holds the native mDNS registration process; unregisters on drop.
pub struct Advertiser {
    child: Option<Child>,
}

impl Drop for Advertiser {
    fn drop(&mut self) {
        if let Some(child) = &mut self.child {
            let _ = child.kill();
            let _ = child.wait();
        }
    }
}

/// Start advertising `dialfd` via the OS mDNS responder. Keep the returned value alive.
pub fn advertise(config: &Config) -> anyhow::Result<Advertiser> {
    let addr: SocketAddr = config
        .ws_bind
        .parse()
        .with_context(|| format!("parse ws_bind `{}`", config.ws_bind))?;
    // Clear orphaned advertisers from dead daemons before registering our own — see
    // reap_stale_advertisers. Do this even when we won't advertise ourselves.
    reap_stale_advertisers();

    // A loopback bind is unreachable from the LAN — advertising it would only hand phones
    // an unconnectable decoy (and a scratch/test daemon on 127.0.0.1 must never pollute
    // the network's discovery).
    if addr.ip().is_loopback() {
        tracing::info!(ws_bind = %config.ws_bind, "loopback bind — not advertising via mDNS");
        return Ok(Advertiser { child: None });
    }
    let port = addr.port().to_string();
    let instance = &config.instance_name;
    // The CLIs take the bare service type (no instance, no trailing .local).
    let service_type = DEFAULT_SERVICE_TYPE
        .trim_end_matches('.')
        .trim_end_matches(".local");
    let ver = format!("ver={}", env!("CARGO_PKG_VERSION"));

    let (tool, child) = if cfg!(target_os = "macos") {
        // dns-sd -R <name> <type> <domain> <port> [k=v ...]
        let child = Command::new("dns-sd")
            .args(["-R", instance, service_type, "local.", &port, &ver])
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .spawn()
            .context("spawn `dns-sd` (Bonjour) — is it on PATH?")?;
        ("dns-sd", child)
    } else {
        // avahi-publish -s <name> <type> <port> [k=v ...]
        let child = Command::new("avahi-publish")
            .args(["-s", instance, service_type, &port, &ver])
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .spawn()
            .context("spawn `avahi-publish` — install avahi-utils?")?;
        ("avahi-publish", child)
    };

    tracing::info!(
        service = service_type,
        instance = %instance,
        port = %port,
        via = tool,
        "advertising via mDNS"
    );
    Ok(Advertiser { child: Some(child) })
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
