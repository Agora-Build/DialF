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
use std::time::Duration;

use anyhow::{bail, Context};

/// What kind of device endpoint is being shared.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Profile {
    /// The Android debug bridge server (`adb`).
    Adb,
}

impl Profile {
    /// The token used on the wire, and in `dialf devices share --adb`.
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

    /// Default port the share listens on.
    pub fn default_port(self) -> u16 {
        match self {
            Profile::Adb => 5939,
        }
    }

    /// Default port the client shim exposes locally. Distinct from [`Self::default_port`] so
    /// running both ends on one machine doesn't collide.
    pub fn default_client_port(self) -> u16 {
        match self {
            // One above adb's own 5037, so a shim and a local adb server coexist.
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

/// Non-loopback IPv4 addresses of this host, for telling the user where to connect.
///
/// A share bound to `0.0.0.0` is reachable at any of these; printing them beats printing
/// `0.0.0.0`, which nobody can type into `adb -H`.
pub fn local_addresses() -> Vec<std::net::Ipv4Addr> {
    let mut out = Vec::new();
    unsafe {
        let mut head: *mut libc::ifaddrs = std::ptr::null_mut();
        if libc::getifaddrs(&mut head) != 0 {
            return out;
        }
        let mut cur = head;
        while !cur.is_null() {
            let ifa = &*cur;
            cur = ifa.ifa_next;
            if ifa.ifa_addr.is_null() || (ifa.ifa_flags & libc::IFF_UP as u32) == 0 {
                continue;
            }
            if (*ifa.ifa_addr).sa_family as i32 != libc::AF_INET {
                continue;
            }
            let sin = &*(ifa.ifa_addr as *const libc::sockaddr_in);
            let ip = std::net::Ipv4Addr::from(u32::from_be(sin.sin_addr.s_addr));
            if !ip.is_loopback() && !ip.is_link_local() && !out.contains(&ip) {
                out.push(ip);
            }
        }
        libc::freeifaddrs(head);
    }
    // Overlay addresses (Netbird/Tailscale use 100.64/10) first: on a mesh they are the ones
    // that reach the other machine, and they are encrypted where the LAN is not.
    out.sort_by_key(|ip| if ip.octets()[0] == 100 { 0 } else { 1 });
    out
}

/// A resolved, validated share configuration — the shape the listener actually runs from.
///
/// Built by [`ShareConfig::resolve`], which is where a bad address stops the share from ever
/// binding and where a requested token is minted.
#[derive(Debug, Clone)]
pub struct ResolvedShare {
    pub profile: Profile,
    pub bind: std::net::SocketAddr,
    pub upstream: Upstream,
    /// Which devices this share exposes. Enforced per connection by [`adb::gate`], not merely
    /// advertised — see that module for why a byte splice can't do it.
    pub targets: Targets,
    /// The secret this share hands out, when one was asked for.
    ///
    /// Generated per share and held only here — never written to config, never logged. It is
    /// printed once when the share starts, and a share restarted later has a different one.
    /// `None` means the share is open: anyone who can reach the port can drive the phone.
    pub token: Option<String>,
    /// How long the share runs before stopping itself. `None` never expires.
    pub expires_after: Option<Duration>,
}

impl ResolvedShare {
    /// True when the listener is reachable from off-box. Worth saying out loud in logs: it is
    /// the difference between "a token guards my loopback" and "a token guards my phone".
    pub fn is_public(&self) -> bool {
        !self.bind.ip().is_loopback()
    }

    /// Whether connections must pass the handshake — i.e. whether a token was issued.
    pub fn require_auth(&self) -> bool {
        self.token.is_some()
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
    /// `host:port` to listen on. Defaults to `0.0.0.0:5939`, i.e. reachable from the network —
    /// pair it with `--token`, or with a trusted network, or bind loopback and tunnel in.
    pub bind: Option<String>,
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
    /// Seconds before the share stops itself. Zero or negative never expires.
    ///
    /// A share is a door held open; the default closes it after an hour so a forgotten one
    /// does not stay open overnight.
    #[serde(default = "default_expire_after")]
    pub expire_after: i64,
}

/// One hour: long enough for a work session, short enough that forgetting is survivable.
pub const DEFAULT_EXPIRE_AFTER: i64 = 3600;

/// Beyond this, an expiry is long enough to be worth a second look.
pub const LONG_EXPIRY_WARN: i64 = 24 * 3600;

fn default_expire_after() -> i64 {
    DEFAULT_EXPIRE_AFTER
}

impl Default for ShareConfig {
    fn default() -> Self {
        Self {
            enabled: false,
            bind: None,
            upstream: None,
            targets: Vec::new(),
            all: false,
            expire_after: DEFAULT_EXPIRE_AFTER,
        }
    }
}

impl ShareConfig {
    /// Validate and fill in defaults, or explain why this share must not listen.
    ///
    /// `with_token` issues a fresh secret for this share; it lives only in the returned value,
    /// so it cannot leak through the config file and every restart mints a new one.
    pub fn resolve(&self, profile: Profile, with_token: bool) -> anyhow::Result<ResolvedShare> {
        let bind_str = self
            .bind
            .clone()
            .unwrap_or_else(|| format!("0.0.0.0:{}", profile.default_port()));
        let bind: std::net::SocketAddr = bind_str
            .parse()
            .with_context(|| format!("parse share bind `{bind_str}` (want host:port)"))?;

        let token = with_token.then(handshake::new_token);
        let expires_after = (self.expire_after > 0)
            .then(|| Duration::from_secs(self.expire_after as u64));

        let upstream = match &self.upstream {
            Some(s) => s.parse()?,
            None => profile.default_upstream(),
        };

        let targets = self.resolve_targets()?;

        Ok(ResolvedShare {
            profile,
            bind,
            upstream,
            targets,
            token,
            expires_after,
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
        // The share port and the shim's local port must differ, or running both ends on one
        // machine collides.
        assert_ne!(Profile::Adb.default_port(), Profile::Adb.default_client_port());
    }

    #[test]
    fn defaults_bind_to_the_network_and_expire_in_an_hour() {
        let cfg = ShareConfig { all: true, ..Default::default() };
        let r = cfg.resolve(Profile::Adb, false).unwrap();
        assert_eq!(r.bind.to_string(), "0.0.0.0:5939");
        assert!(r.is_public(), "the default bind is reachable from the network");
        assert_eq!(r.upstream, Upstream::Tcp("127.0.0.1:5037".to_string()));
        assert_eq!(r.expires_after, Some(Duration::from_secs(3600)));
    }

    #[test]
    fn a_token_is_issued_only_when_asked_for() {
        // No token means an open share — allowed, and the reason the CLI warns loudly.
        let cfg = ShareConfig { all: true, ..Default::default() };
        let open = cfg.resolve(Profile::Adb, false).unwrap();
        assert!(open.token.is_none());
        assert!(!open.require_auth());

        let guarded = cfg.resolve(Profile::Adb, true).unwrap();
        assert!(guarded.require_auth());
        let token = guarded.token.unwrap();
        assert!(token.starts_with(handshake::TOKEN_PREFIX), "got: {token}");
        assert_eq!(token.len(), 16);
    }

    #[test]
    fn each_resolve_mints_a_fresh_token() {
        // Tokens live only in the resolved share, so a second share must not reuse the first
        // one — there is no stored value for it to match against anyway.
        let cfg = ShareConfig { all: true, ..Default::default() };
        let a = cfg.resolve(Profile::Adb, true).unwrap().token.unwrap();
        let b = cfg.resolve(Profile::Adb, true).unwrap().token.unwrap();
        assert_ne!(a, b);
    }

    #[test]
    fn a_non_positive_expiry_means_never() {
        for secs in [0, -1, -3600] {
            let cfg = ShareConfig { all: true, expire_after: secs, ..Default::default() };
            assert_eq!(cfg.resolve(Profile::Adb, false).unwrap().expires_after, None);
        }
        let cfg = ShareConfig { all: true, expire_after: 90, ..Default::default() };
        assert_eq!(
            cfg.resolve(Profile::Adb, false).unwrap().expires_after,
            Some(Duration::from_secs(90))
        );
    }

    #[test]
    fn a_loopback_bind_is_not_public() {
        let cfg = ShareConfig {
            bind: Some("127.0.0.1:5939".to_string()),
            all: true,
            ..Default::default()
        };
        assert!(!cfg.resolve(Profile::Adb, false).unwrap().is_public());
    }

    #[test]
    fn resolve_rejects_a_bad_bind() {
        let cfg = ShareConfig {
            bind: Some("not-an-address".to_string()),
            all: true,
            ..Default::default()
        };
        assert!(cfg.resolve(Profile::Adb, false).is_err());
    }

    #[test]
    fn sharing_nothing_is_an_error_not_everything() {
        // Silence must never be read as "share the whole host".
        let cfg = ShareConfig::default();
        assert!(cfg.resolve(Profile::Adb, false).is_err());
        let both = ShareConfig {
            all: true,
            targets: vec!["A1".to_string()],
            ..Default::default()
        };
        assert!(both.resolve(Profile::Adb, false).is_err(), "all + targets is ambiguous");
    }

    #[test]
    fn config_parses_from_yaml_with_defaults() {
        let cfg: ShareConfig = serde_yaml::from_str("all: true\n").unwrap();
        assert!(!cfg.enabled); // never on by default
        assert!(cfg.bind.is_none());
        assert_eq!(cfg.expire_after, DEFAULT_EXPIRE_AFTER);

        let full: ShareConfig = serde_yaml::from_str(
            "enabled: true\nbind: 0.0.0.0:5939\nupstream: tcp:127.0.0.1:5037\nall: true\nexpire_after: 120\n",
        )
        .unwrap();
        assert!(full.enabled);
        let r = full.resolve(Profile::Adb, false).unwrap();
        assert_eq!(r.bind.to_string(), "0.0.0.0:5939");
        assert_eq!(r.expires_after, Some(Duration::from_secs(120)));
    }

    #[test]
    fn a_token_never_round_trips_through_config() {
        // Serialising a config must not be able to carry a share secret to disk.
        let yaml = serde_yaml::to_string(&ShareConfig::default()).unwrap();
        assert!(!yaml.contains("token"), "config grew a token field: {yaml}");
    }

    #[test]
    fn local_addresses_are_usable_targets() {
        // Whatever this host has, none of it should be something a peer cannot dial.
        for ip in local_addresses() {
            assert!(!ip.is_loopback(), "{ip} is loopback");
            assert!(!ip.is_link_local(), "{ip} is link-local (needs a zone id)");
        }
    }
}
