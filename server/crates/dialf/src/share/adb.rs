//! ADB protocol gate: enforce *which* devices a share exposes.
//!
//! A blind byte splice would hand a remote peer every device attached to this host, plus the
//! ability to kill the local adb server. The adb wire protocol makes each connection state
//! its intent up front — one length-prefixed request — so reading just that first request is
//! enough to decide whether to let the connection through, and cheap enough not to matter.
//!
//! Wire format: `<4 hex digits: payload length><payload>`, answered with `OKAY` or
//! `FAIL<4 hex><reason>`. After a `transport` request succeeds the connection becomes an
//! opaque device stream, which is why everything past that point is spliced untouched.
//!
//! Three things this stops that a raw proxy cannot:
//! - `host:kill` — a remote `adb kill-server`, or merely a peer on a different platform-tools
//!   version (the adb client kills a mismatched server automatically), would otherwise take
//!   down the host's adb server.
//! - `host:devices` leaking the full inventory of attached devices.
//! - `transport-any` quietly landing on a device that was never shared, which would make
//!   `--target` advisory rather than enforced.

use anyhow::{bail, Context};
use tokio::io::{AsyncRead, AsyncReadExt, AsyncWrite, AsyncWriteExt};

use super::Upstream;

/// Which devices a share exposes.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Targets {
    /// Every device attached to the host, now and later.
    All,
    /// Only these serials (adb's device id — `4B2B1C`, or `192.168.1.5:5555` for adb-over-WiFi).
    Only(Vec<String>),
}

impl Targets {
    pub fn allows(&self, serial: &str) -> bool {
        match self {
            Targets::All => true,
            Targets::Only(list) => list.iter().any(|s| s == serial),
        }
    }

    /// The single device to resolve an unqualified request to, when that is unambiguous.
    fn only_one(&self) -> Option<&str> {
        match self {
            Targets::Only(list) if list.len() == 1 => Some(&list[0]),
            _ => None,
        }
    }

    pub fn describe(&self) -> String {
        match self {
            Targets::All => "all attached devices".to_string(),
            Targets::Only(list) => list.join(", "),
        }
    }
}

/// What to do with a connection, decided from its first request.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Decision {
    /// Send the request as-is, then splice the rest of the connection untouched.
    Forward,
    /// Send this rewritten request instead, then splice.
    Rewrite(String),
    /// Forward, then filter the device list coming back. `streaming` keeps filtering, for
    /// `track-devices` (what `adb wait-for-device` uses).
    FilterList { streaming: bool },
    /// Never reaches the upstream; the peer gets this as a `FAIL`.
    Reject(String),
}

/// Decide a connection's fate from its first request.
///
/// Pure, so the whole policy is testable without a socket — which matters, because every
/// mistake here is a device exposed that shouldn't be.
pub fn decide(request: &str, targets: &Targets) -> Decision {
    let req = request.trim_end_matches('\0');

    // Destructive or host-mutating commands are never proxied, whatever is shared.
    if req == "host:kill" {
        return Decision::Reject(
            "host:kill is blocked by dialf device sharing (it would stop the host's adb server)"
                .to_string(),
        );
    }
    if req.starts_with("host:connect:") || req.starts_with("host:disconnect") {
        return Decision::Reject(
            "attaching/detaching devices on the host is blocked by dialf device sharing"
                .to_string(),
        );
    }

    // Device inventory: answer only with what is shared.
    match req {
        "host:devices" | "host:devices-l" => return Decision::FilterList { streaming: false },
        "host:track-devices" | "host:track-devices-l" => {
            return Decision::FilterList { streaming: true }
        }
        _ => {}
    }

    // Explicitly-addressed device: allow only if shared.
    if let Some(serial) = req
        .strip_prefix("host:transport:")
        .or_else(|| req.strip_prefix("host:tport:serial:"))
    {
        return if targets.allows(serial) {
            Decision::Forward
        } else {
            Decision::Reject(not_shared(serial, targets))
        };
    }

    // `host-serial:<serial>:<command>` — the serial can itself contain a colon
    // (`192.168.1.5:5555`), so match against known serials rather than splitting on ':'.
    if let Some(rest) = req.strip_prefix("host-serial:") {
        return match matching_serial(rest, targets) {
            Some(_) => Decision::Forward,
            None => Decision::Reject(format!(
                "`{rest}` is not a shared device (sharing: {})",
                targets.describe()
            )),
        };
    }

    // Unqualified device selection. Resolving it to the one shared device is what makes
    // `adb -H … shell` work without `-s`; with several shared it would be a coin flip over
    // which device you get, so make the caller say.
    if req == "host:transport-any"
        || req == "host:transport-usb"
        || req == "host:transport-local"
        || req == "host:tport:any"
        || req == "host:tport:usb"
        || req == "host:tport:local"
    {
        return match targets {
            Targets::All => Decision::Forward,
            t => match t.only_one() {
                Some(serial) if req.starts_with("host:tport:") => {
                    Decision::Rewrite(format!("host:tport:serial:{serial}"))
                }
                Some(serial) => Decision::Rewrite(format!("host:transport:{serial}")),
                None => Decision::Reject(format!(
                    "several devices are shared ({}) — pick one with `adb -s <serial>`",
                    t.describe()
                )),
            },
        };
    }

    // Everything else (host:version, host:features, …) is metadata about the server itself.
    Decision::Forward
}

fn not_shared(serial: &str, targets: &Targets) -> String {
    format!(
        "device `{serial}` is not shared by this host (sharing: {})",
        targets.describe()
    )
}

/// The shared serial that `rest` (`<serial>:<command>`) addresses, if any.
fn matching_serial<'a>(rest: &str, targets: &'a Targets) -> Option<&'a str> {
    match targets {
        // With everything shared, the serial is whatever precedes the last colon-separated
        // command; we don't need to know it.
        Targets::All => Some(""),
        Targets::Only(list) => list
            .iter()
            .find(|s| rest.strip_prefix(s.as_str()).is_some_and(|r| r.starts_with(':')))
            .map(|s| s.as_str()),
    }
}

/// Keep only shared devices in a `host:devices` payload.
///
/// Lines are `<serial>\t<state>` (or, for `-l`, `<serial> <state> key:value …`). An empty
/// payload is the legitimate "nothing attached" answer and stays empty.
pub fn filter_device_list(payload: &str, targets: &Targets) -> String {
    if matches!(targets, Targets::All) {
        return payload.to_string();
    }
    let mut out = String::new();
    for line in payload.lines() {
        let serial = line.split_whitespace().next().unwrap_or("");
        if !serial.is_empty() && targets.allows(serial) {
            out.push_str(line);
            out.push('\n');
        }
    }
    out
}

/// One device as the adb server reports it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Device {
    pub serial: String,
    /// `device`, `offline`, `unauthorized`, …
    pub state: String,
    /// `model:Pixel_9_Pro` style extras from `devices-l`, when present.
    pub model: Option<String>,
}

impl Device {
    /// Whether this device is actually usable (vs offline/unauthorized).
    pub fn is_ready(&self) -> bool {
        self.state == "device"
    }
}

/// Parse a `host:devices-l` payload.
pub fn parse_device_list(payload: &str) -> Vec<Device> {
    payload
        .lines()
        .filter(|l| !l.trim().is_empty())
        .filter_map(|line| {
            let mut parts = line.split_whitespace();
            let serial = parts.next()?.to_string();
            let state = parts.next().unwrap_or("unknown").to_string();
            let model = parts
                .find_map(|p| p.strip_prefix("model:"))
                .map(|m| m.replace('_', " "));
            Some(Device {
                serial,
                state,
                model,
            })
        })
        .collect()
}

// ---- wire helpers -------------------------------------------------------------------

/// Read one `<4 hex len><payload>` message.
pub async fn read_message<R: AsyncRead + Unpin>(r: &mut R) -> anyhow::Result<String> {
    let mut len_buf = [0u8; 4];
    r.read_exact(&mut len_buf).await?;
    let len = usize::from_str_radix(std::str::from_utf8(&len_buf)?, 16)
        .with_context(|| format!("bad adb length prefix {:?}", String::from_utf8_lossy(&len_buf)))?;
    // adb's own cap; anything larger is a malformed or hostile peer.
    if len > 64 * 1024 {
        bail!("adb message too long ({len} bytes)");
    }
    let mut buf = vec![0u8; len];
    r.read_exact(&mut buf).await?;
    Ok(String::from_utf8_lossy(&buf).to_string())
}

/// Write one `<4 hex len><payload>` message.
pub async fn write_message<W: AsyncWrite + Unpin>(w: &mut W, payload: &str) -> anyhow::Result<()> {
    w.write_all(format!("{:04x}", payload.len()).as_bytes()).await?;
    w.write_all(payload.as_bytes()).await?;
    w.flush().await?;
    Ok(())
}

/// Tell the peer why its request was refused, in the form adb expects so the error surfaces
/// as a message rather than a dropped connection.
pub async fn write_fail<W: AsyncWrite + Unpin>(w: &mut W, reason: &str) -> anyhow::Result<()> {
    w.write_all(b"FAIL").await?;
    write_message(w, reason).await
}

/// Apply the sharing policy to one connection, then get out of the way.
///
/// Reads only the first request; once a connection is cleared it is spliced byte for byte, so
/// `shell`, `install` and `push`/`pull` behave exactly as they do locally.
pub async fn gate<D, U>(down: &mut D, up: &mut U, targets: &Targets) -> anyhow::Result<()>
where
    D: AsyncRead + AsyncWrite + Unpin,
    U: AsyncRead + AsyncWrite + Unpin,
{
    let request = read_message(down).await?;
    let decision = decide(&request, targets);
    tracing::debug!(%request, ?decision, "adb share request");

    let forwarded = match &decision {
        Decision::Reject(reason) => {
            tracing::warn!(%request, %reason, "adb share request refused");
            write_fail(down, reason).await?;
            return Ok(());
        }
        Decision::Rewrite(replacement) => replacement.clone(),
        _ => request.clone(),
    };
    write_message(up, &forwarded).await?;

    if let Decision::FilterList { streaming } = decision {
        return relay_filtered_list(down, up, targets, streaming).await;
    }

    tokio::io::copy_bidirectional(down, up).await?;
    Ok(())
}

/// Relay a device-list response, keeping only shared devices.
async fn relay_filtered_list<D, U>(
    down: &mut D,
    up: &mut U,
    targets: &Targets,
    streaming: bool,
) -> anyhow::Result<()>
where
    D: AsyncRead + AsyncWrite + Unpin,
    U: AsyncRead + AsyncWrite + Unpin,
{
    let mut status = [0u8; 4];
    up.read_exact(&mut status).await?;
    down.write_all(&status).await?;
    down.flush().await?;
    if &status != b"OKAY" {
        // Pass the server's own failure through verbatim rather than inventing one.
        tokio::io::copy_bidirectional(down, up).await?;
        return Ok(());
    }

    loop {
        // A closed upstream ends the stream normally — `track-devices` has no terminator.
        let Ok(payload) = read_message(up).await else {
            return Ok(());
        };
        write_message(down, &filter_device_list(&payload, targets)).await?;
        if !streaming {
            return Ok(());
        }
    }
}

/// Ask the local adb server what is attached.
pub async fn list_devices(upstream: &Upstream) -> anyhow::Result<Vec<Device>> {
    let Upstream::Tcp(addr) = upstream else {
        bail!("listing devices needs a tcp upstream, got {upstream}");
    };
    let mut s = tokio::net::TcpStream::connect(addr)
        .await
        .with_context(|| format!("connect adb server at {addr} — is it running?"))?;
    write_message(&mut s, "host:devices-l").await?;
    let mut status = [0u8; 4];
    s.read_exact(&mut status).await?;
    if &status != b"OKAY" {
        let reason = read_message(&mut s).await.unwrap_or_default();
        bail!("adb server refused host:devices-l: {reason}");
    }
    Ok(parse_device_list(&read_message(&mut s).await?))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn only(serials: &[&str]) -> Targets {
        Targets::Only(serials.iter().map(|s| s.to_string()).collect())
    }

    #[test]
    fn kill_is_always_blocked() {
        // Not just a policy choice: a version-mismatched adb client sends this on its own.
        for t in [Targets::All, only(&["A"])] {
            assert!(matches!(decide("host:kill", &t), Decision::Reject(_)));
        }
    }

    #[test]
    fn host_mutating_requests_are_blocked() {
        assert!(matches!(
            decide("host:connect:192.168.1.9:5555", &Targets::All),
            Decision::Reject(_)
        ));
        assert!(matches!(
            decide("host:disconnect:192.168.1.9:5555", &Targets::All),
            Decision::Reject(_)
        ));
    }

    #[test]
    fn a_shared_device_can_be_addressed_directly() {
        let t = only(&["4B2B1C"]);
        assert_eq!(decide("host:transport:4B2B1C", &t), Decision::Forward);
        assert_eq!(decide("host:tport:serial:4B2B1C", &t), Decision::Forward);
    }

    #[test]
    fn an_unshared_device_is_refused_by_name() {
        let t = only(&["4B2B1C"]);
        let Decision::Reject(msg) = decide("host:transport:9F1E2D", &t) else {
            panic!("expected a rejection");
        };
        // The message must name both the device asked for and what is on offer.
        assert!(msg.contains("9F1E2D"), "got: {msg}");
        assert!(msg.contains("4B2B1C"), "got: {msg}");
    }

    #[test]
    fn transport_any_resolves_to_the_single_shared_device() {
        // This is what makes `adb -H … shell` work without -s.
        let t = only(&["4B2B1C"]);
        assert_eq!(
            decide("host:transport-any", &t),
            Decision::Rewrite("host:transport:4B2B1C".to_string())
        );
        assert_eq!(
            decide("host:tport:any", &t),
            Decision::Rewrite("host:tport:serial:4B2B1C".to_string())
        );
        // transport-usb/local could otherwise select an unshared device.
        assert_eq!(
            decide("host:transport-usb", &t),
            Decision::Rewrite("host:transport:4B2B1C".to_string())
        );
    }

    #[test]
    fn transport_any_is_refused_when_several_are_shared() {
        // Silently picking one would make --target a coin flip.
        let t = only(&["A1", "B2"]);
        let Decision::Reject(msg) = decide("host:transport-any", &t) else {
            panic!("expected a rejection");
        };
        assert!(msg.contains("-s"), "message should say how to disambiguate: {msg}");
    }

    #[test]
    fn transport_any_passes_through_when_everything_is_shared() {
        assert_eq!(decide("host:transport-any", &Targets::All), Decision::Forward);
    }

    #[test]
    fn host_serial_handles_serials_containing_colons() {
        // adb-over-WiFi serials are `ip:port`, so splitting on the first colon would break
        // exactly the device this project drives.
        let t = only(&["192.168.100.179:33415"]);
        assert_eq!(
            decide("host-serial:192.168.100.179:33415:get-state", &t),
            Decision::Forward
        );
        assert!(matches!(
            decide("host-serial:10.0.0.5:5555:get-state", &t),
            Decision::Reject(_)
        ));
    }

    #[test]
    fn a_serial_that_is_a_prefix_of_another_is_not_confused() {
        // `4B2B1C` must not authorize `4B2B1CDEAD`.
        let t = only(&["4B2B1C"]);
        assert!(matches!(
            decide("host:transport:4B2B1CDEAD", &t),
            Decision::Reject(_)
        ));
        assert!(matches!(
            decide("host-serial:4B2B1CDEAD:get-state", &t),
            Decision::Reject(_)
        ));
    }

    #[test]
    fn device_listings_are_filtered_to_shared_devices() {
        let payload = "4B2B1C\tdevice\n9F1E2D\tdevice\n7C3A55\toffline\n";
        let filtered = filter_device_list(payload, &only(&["4B2B1C", "7C3A55"]));
        assert!(filtered.contains("4B2B1C"));
        assert!(filtered.contains("7C3A55"));
        assert!(!filtered.contains("9F1E2D"), "leaked an unshared device");
        // Sharing everything is a pass-through, byte for byte.
        assert_eq!(filter_device_list(payload, &Targets::All), payload);
    }

    #[test]
    fn filtering_handles_the_long_format_and_empty_lists() {
        let long = "4B2B1C  device product:caiman model:Pixel_9_Pro device:caiman\n\
                    9F1E2D  device product:bluejay model:Pixel_6a device:bluejay\n";
        let filtered = filter_device_list(long, &only(&["9F1E2D"]));
        assert!(filtered.contains("Pixel_6a"));
        assert!(!filtered.contains("Pixel_9_Pro"));
        // "nothing attached" must stay "nothing attached", not become malformed.
        assert_eq!(filter_device_list("", &only(&["A"])), "");
    }

    #[test]
    fn device_list_parses_serial_state_and_model() {
        let devices = parse_device_list(
            "4B2B1C  device product:caiman model:Pixel_9_Pro device:caiman\n\
             9F1E2D  offline\n",
        );
        assert_eq!(devices.len(), 2);
        assert_eq!(devices[0].serial, "4B2B1C");
        assert_eq!(devices[0].model.as_deref(), Some("Pixel 9 Pro"));
        assert!(devices[0].is_ready());
        assert_eq!(devices[1].state, "offline");
        assert!(!devices[1].is_ready(), "offline devices are not usable");
        assert!(devices[1].model.is_none());
        assert!(parse_device_list("").is_empty());
    }

    #[test]
    fn track_devices_is_filtered_as_a_stream() {
        assert_eq!(
            decide("host:track-devices", &only(&["A"])),
            Decision::FilterList { streaming: true }
        );
        assert_eq!(
            decide("host:devices-l", &only(&["A"])),
            Decision::FilterList { streaming: false }
        );
    }

    #[test]
    fn server_metadata_requests_pass_through() {
        assert_eq!(decide("host:version", &only(&["A"])), Decision::Forward);
        assert_eq!(decide("host:features", &only(&["A"])), Decision::Forward);
    }

    #[tokio::test]
    async fn messages_round_trip_on_the_wire() {
        let mut buf: Vec<u8> = Vec::new();
        write_message(&mut buf, "host:devices").await.unwrap();
        assert_eq!(&buf[..4], b"000c");
        let mut cursor = std::io::Cursor::new(buf);
        assert_eq!(read_message(&mut cursor).await.unwrap(), "host:devices");
    }

    #[tokio::test]
    async fn an_absurd_length_prefix_is_refused() {
        // ffff is within adb's own cap; a bogus prefix must not make us allocate wildly.
        let mut cursor = std::io::Cursor::new(b"zzzz".to_vec());
        assert!(read_message(&mut cursor).await.is_err());
    }
}
