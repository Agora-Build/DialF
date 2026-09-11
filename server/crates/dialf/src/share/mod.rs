//! Sharing a locally-attached device with another machine.
//!
//! The daemon holds the device — `adb` owns the USB (or adb-over-WiFi) connection through its
//! server on `127.0.0.1:5037`. This module puts an authenticated TCP listener in front of
//! that socket so a remote machine can reach it, and a client-side shim that presents the
//! remote endpoint back as a plain local port for stock tooling:
//!
//! ```text
//! remote:  adb -H 127.0.0.1 -P 5038  ->  dialf devices connect <host>   (client.rs)
//!                                             |  authenticated TCP
//!   host:  adb server 127.0.0.1:5037  <-  dialfd share listener         (server.rs)
//!                                             |  per-connection policy  (adb.rs)
//! ```
//!
//! Two gates, in order. [`handshake`] proves the peer holds the shared token before anything
//! is forwarded; [`adb`] then reads the connection's first request to enforce *which* devices
//! it may reach. Past that the sockets are spliced byte for byte, which is why `shell`,
//! `install` and `push`/`pull` behave exactly as they do locally (adb's file transfer is
//! in-band on the same connection).
//!
//! Only the `adb` profile ships today. The transport, auth and lifecycle are generic, so the
//! iOS analogue — usbmuxd on a Unix socket, redirected with `USBMUXD_SOCKET_ADDRESS` — reuses
//! all of it; only per-device filtering is protocol-specific and would need its own gate.

pub mod adb;
pub mod client;
pub mod handshake;
pub mod server;

pub use adb::Targets;

use std::fmt;
use std::path::PathBuf;

use anyhow::{bail, Context};

/// What kind of device endpoint is being shared.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Profile {
    /// The Android debug bridge server (`adb`).
    Adb,
}

impl Profile {
    /// The token used on the wire, and in `dialf adb …`.
    pub fn as_str(self) -> &'static str {
        match self {
            Profile::Adb => "adb",
        }
    }

    /// Where this profile's endpoint lives when config doesn't override it.
    pub fn default_upstream(self) -> Upstream {
        match self {
            Profile::Adb => Upstream::Tcp("127.0.0.1:5037".to_string()),
        }
    }

    /// Default port for the listener that exposes it, and for the client shim.
    pub fn default_port(self) -> u16 {
        match self {
            // One above adb's own 5037, so both can run on one host.
            Profile::Adb => 5038,
        }
    }
}

impl fmt::Display for Profile {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

impl std::str::FromStr for Profile {
    type Err = anyhow::Error;

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        match s.trim().to_ascii_lowercase().as_str() {
            "adb" | "android" => Ok(Profile::Adb),
            other => bail!("unknown share profile `{other}` (known: adb)"),
        }
    }
}

/// The local endpoint a share proxies to.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Upstream {
    /// `tcp:host:port` — the adb server.
    Tcp(String),
    /// `unix:/path` — reserved for usbmuxd; parsed now so config written today stays valid.
    Unix(PathBuf),
}

impl fmt::Display for Upstream {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Upstream::Tcp(addr) => write!(f, "tcp:{addr}"),
            Upstream::Unix(path) => write!(f, "unix:{}", path.display()),
        }
    }
}

impl std::str::FromStr for Upstream {
    type Err = anyhow::Error;

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        let s = s.trim();
        if let Some(rest) = s.strip_prefix("tcp:") {
            if rest.is_empty() {
                bail!("upstream `{s}` has no address after `tcp:`");
            }
            // Catch a missing port here rather than at connect time, when the error would
            // surface as a confusing DNS failure.
            if !rest.contains(':') {
                bail!("upstream `{s}` needs a port, e.g. tcp:127.0.0.1:5037");
            }
            return Ok(Upstream::Tcp(rest.to_string()));
        }
        if let Some(rest) = s.strip_prefix("unix:") {
            if rest.is_empty() {
                bail!("upstream `{s}` has no path after `unix:`");
            }
            return Ok(Upstream::Unix(PathBuf::from(rest)));
        }
        bail!("upstream `{s}` must start with `tcp:` or `unix:`")
    }
}

/// A resolved, validated share configuration — the shape the listener actually runs from.
///
/// Built by [`ShareConfig::resolve`], which is where a bad token or address stops the share
/// from ever binding.
#[derive(Debug, Clone)]
pub struct ResolvedShare {
    pub profile: Profile,
    pub bind: std::net::SocketAddr,
    pub token: String,
    pub upstream: Upstream,
    /// Which devices this share exposes. Enforced per connection by [`adb::gate`], not merely
    /// advertised — see that module for why a byte splice can't do it.
    pub targets: Targets,
}

impl ResolvedShare {
    /// True when the listener is reachable from off-box. Worth saying out loud in logs: it is
    /// the difference between "a token guards my loopback" and "a token guards my phone".
    pub fn is_public(&self) -> bool {
        !self.bind.ip().is_loopback()
    }
}

/// Sharing settings as they appear in `config.yaml`.
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
#[serde(default)]
pub struct ShareConfig {
    /// Start this share when the daemon starts. Runtime `share.start` works either way — this
    /// only decides whether it comes back on its own, so leaving it false keeps an exposed
    /// port from outliving the session that wanted it.
    pub enabled: bool,
    /// `host:port` to listen on. Defaults to loopback: opting into a LAN bind should be a
    /// deliberate edit, not something inherited from a default.
    pub bind: Option<String>,
    /// Shared secret a client must prove it holds. No default — see [`handshake::check_token`].
    pub token: String,
    /// Override the endpoint being shared (`tcp:host:port` / `unix:/path`).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub upstream: Option<String>,
    /// Device serials to expose. Empty means none — sharing must name what it exposes, or
    /// set `all: true`. A host that grows a second phone should not silently start sharing it.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub targets: Vec<String>,
    /// Expose every attached device, including ones plugged in later.
    #[serde(default)]
    pub all: bool,
}

impl Default for ShareConfig {
    fn default() -> Self {
        Self {
            enabled: false,
            bind: None,
            token: String::new(),
            upstream: None,
            targets: Vec::new(),
            all: false,
        }
    }
}

impl ShareConfig {
    /// Validate and fill in defaults, or explain why this share must not listen.
    pub fn resolve(&self, profile: Profile) -> anyhow::Result<ResolvedShare> {
        handshake::check_token(&self.token)?;

        let bind_str = self
            .bind
            .clone()
            .unwrap_or_else(|| format!("127.0.0.1:{}", profile.default_port()));
        let bind: std::net::SocketAddr = bind_str
            .parse()
            .with_context(|| format!("parse share bind `{bind_str}` (want host:port)"))?;

        let upstream = match &self.upstream {
            Some(s) => s.parse()?,
            None => profile.default_upstream(),
        };

        let targets = self.resolve_targets()?;

        Ok(ResolvedShare {
            profile,
            bind,
            token: self.token.trim().to_string(),
            upstream,
            targets,
        })
    }

    /// Which devices to expose, refusing the ambiguous "none named, none allowed" case.
    ///
    /// Silence means *nothing*, not *everything*: a share that exposed the whole host because
    /// a field was left blank is the failure mode worth designing against.
    pub fn resolve_targets(&self) -> anyhow::Result<Targets> {
        let named: Vec<String> = self
            .targets
            .iter()
            .map(|s| s.trim().to_string())
            .filter(|s| !s.is_empty())
            .collect();
        match (self.all, named.is_empty()) {
            (true, true) => Ok(Targets::All),
            (true, false) => bail!("set either `all: true` or `targets:`, not both"),
            (false, false) => Ok(Targets::Only(named)),
            (false, true) => bail!(
                "no devices named — list serials under `targets:` (or set `all: true`); \
                 `dialf devices share --list` shows what is attached"
            ),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn upstream_parses_both_forms() {
        assert_eq!(
            "tcp:127.0.0.1:5037".parse::<Upstream>().unwrap(),
            Upstream::Tcp("127.0.0.1:5037".to_string())
        );
        assert_eq!(
            "unix:/var/run/usbmuxd".parse::<Upstream>().unwrap(),
            Upstream::Unix(PathBuf::from("/var/run/usbmuxd"))
        );
        // Round-trips through Display, so a status line can be pasted back into config.
        let u: Upstream = "tcp:10.0.0.2:5037".parse().unwrap();
        assert_eq!(u.to_string().parse::<Upstream>().unwrap(), u);
    }

    #[test]
    fn upstream_rejects_junk() {
        assert!("127.0.0.1:5037".parse::<Upstream>().is_err()); // no scheme
        assert!("tcp:".parse::<Upstream>().is_err());
        assert!("unix:".parse::<Upstream>().is_err());
        assert!("http://host".parse::<Upstream>().is_err());
        // A missing port would otherwise fail much later, as a DNS error.
        assert!("tcp:127.0.0.1".parse::<Upstream>().is_err());
    }

    #[test]
    fn profile_parses_and_round_trips() {
        assert_eq!("adb".parse::<Profile>().unwrap(), Profile::Adb);
        assert_eq!("ADB".parse::<Profile>().unwrap(), Profile::Adb);
        assert!("ios".parse::<Profile>().is_err()); // not shipped yet; must not silently pass
        assert_eq!(Profile::Adb.to_string(), "adb");
    }

    #[test]
    fn resolve_defaults_to_loopback_and_the_adb_server() {
        let cfg = ShareConfig {
            token: "a-perfectly-fine-token".to_string(),
            all: true,
            ..Default::default()
        };
        let r = cfg.resolve(Profile::Adb).unwrap();
        assert_eq!(r.bind.to_string(), "127.0.0.1:5038");
        assert_eq!(r.upstream, Upstream::Tcp("127.0.0.1:5037".to_string()));
        assert!(!r.is_public()); // the default must never be off-box
    }

    #[test]
    fn resolve_refuses_a_weak_token() {
        // The gate is the whole feature; a bad token must stop the bind, not warn about it.
        let weak = ShareConfig::default();
        assert!(weak.resolve(Profile::Adb).is_err());
        let placeholder = ShareConfig {
            token: "change-me".to_string(),
            ..Default::default()
        };
        assert!(placeholder.resolve(Profile::Adb).is_err());
    }

    #[test]
    fn resolve_reports_a_public_bind() {
        let cfg = ShareConfig {
            token: "a-perfectly-fine-token".to_string(),
            bind: Some("0.0.0.0:5038".to_string()),
            all: true,
            ..Default::default()
        };
        assert!(cfg.resolve(Profile::Adb).unwrap().is_public());
    }

    #[test]
    fn resolve_rejects_a_bad_bind() {
        let cfg = ShareConfig {
            token: "a-perfectly-fine-token".to_string(),
            bind: Some("not-an-address".to_string()),
            all: true,
            ..Default::default()
        };
        assert!(cfg.resolve(Profile::Adb).is_err());
    }

    #[test]
    fn config_parses_from_yaml_with_defaults() {
        let cfg: ShareConfig = serde_yaml::from_str("token: a-perfectly-fine-token\n").unwrap();
        assert!(!cfg.enabled); // never on by default
        assert!(cfg.bind.is_none());
        assert!(cfg.upstream.is_none());

        let full: ShareConfig = serde_yaml::from_str(
            "enabled: true\nbind: 0.0.0.0:5038\ntoken: a-perfectly-fine-token\nupstream: tcp:127.0.0.1:5037\nall: true\n",
        )
        .unwrap();
        assert!(full.enabled);
        assert_eq!(full.resolve(Profile::Adb).unwrap().bind.to_string(), "0.0.0.0:5038");
    }
}
