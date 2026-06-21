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
    child: Child,
}

impl Drop for Advertiser {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

/// Start advertising `dialfd` via the OS mDNS responder. Keep the returned value alive.
pub fn advertise(config: &Config) -> anyhow::Result<Advertiser> {
    let addr: SocketAddr = config
        .ws_bind
        .parse()
        .with_context(|| format!("parse ws_bind `{}`", config.ws_bind))?;
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
    Ok(Advertiser { child })
}
