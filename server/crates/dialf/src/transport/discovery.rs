//! mDNS service advertisement so phones can auto-discover `dialfd` on the LAN.
//!
//! Advertises `_dialfd._tcp.local.` with the phone WebSocket port. Addresses are
//! auto-detected from the host's interfaces. The returned [`ServiceDaemon`] must be kept
//! alive for the advertisement to persist.

use std::net::SocketAddr;

use anyhow::Context;
use mdns_sd::{ServiceDaemon, ServiceInfo};

use crate::config::{Config, DEFAULT_SERVICE_TYPE};

/// Start advertising `dialfd`. Keep the returned daemon alive.
pub fn advertise(config: &Config) -> anyhow::Result<ServiceDaemon> {
    let addr: SocketAddr = config
        .ws_bind
        .parse()
        .with_context(|| format!("parse ws_bind `{}`", config.ws_bind))?;
    let port = addr.port();

    let mdns = ServiceDaemon::new().context("create mDNS daemon")?;
    let host_name = format!("{}.local.", config.instance_name);
    let port_str = port.to_string();
    let properties = [
        ("version", env!("CARGO_PKG_VERSION")),
        ("ws_port", port_str.as_str()),
    ];

    let service = ServiceInfo::new(
        DEFAULT_SERVICE_TYPE,
        &config.instance_name,
        &host_name,
        "", // addresses filled in by enable_addr_auto
        port,
        &properties[..],
    )
    .context("build mDNS service info")?
    .enable_addr_auto();

    mdns.register(service).context("register mDNS service")?;
    tracing::info!(
        service = DEFAULT_SERVICE_TYPE,
        instance = %config.instance_name,
        port,
        "advertising via mDNS"
    );
    Ok(mdns)
}
