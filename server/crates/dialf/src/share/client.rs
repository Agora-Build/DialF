//! Remote side: present a shared endpoint back as a plain local port.
//!
//! `dialf devices connect <host>` listens on `127.0.0.1:5038` and, for each local connection,
//! opens one to the host, performs the handshake, and splices. Stock tooling then works
//! unchanged — `adb -H 127.0.0.1 -P 5038 shell`, or `ADB_SERVER_SOCKET=tcp:127.0.0.1:5038`.
//!
//! The handshake runs per connection because adb opens a fresh one per command. That costs a
//! round trip per invocation and keeps the shim stateless.
//!
//! This exists only because the adb client has no way to authenticate, so *something* must
//! perform the handshake for it. An open (loopback) share needs no handshake and therefore no
//! shim — point adb at it directly, through an SSH tunnel if it is on another host. Connecting
//! the shim to an open share is refused with that advice rather than silently half-working.

use std::net::SocketAddr;

use anyhow::{bail, Context};
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::net::{TcpListener, TcpStream};

use super::handshake::{self, MAGIC, MAX_LINE};
use super::Profile;

/// Split a `host`, `host:port`, or bracketed-IPv6 target into an address for `profile`,
/// supplying the default port when none is given.
///
/// IPv6 literals must be bracketed to be unambiguous — `::1:5038` cannot be split by "last
/// colon" without guessing. An unbracketed literal is rejected with an explanation rather
/// than silently mis-parsed (the phone app's crash loop came from exactly this, see
/// `DaemonCandidates`).
pub fn target_addr(target: &str, profile: Profile) -> anyhow::Result<String> {
    let t = target.trim();
    if t.is_empty() {
        bail!("empty share target");
    }
    if let Some(rest) = t.strip_prefix('[') {
        let (host, tail) = rest
            .split_once(']')
            .context("unterminated `[` in IPv6 target (want `[::1]:5038`)")?;
        return Ok(match tail.strip_prefix(':') {
            Some(port) if !port.is_empty() => format!("[{host}]:{port}"),
            _ => format!("[{host}]:{}", profile.default_port()),
        });
    }
    match t.rsplit_once(':') {
        // More than one colon and no brackets: a bare IPv6 literal.
        Some(_) if t.matches(':').count() > 1 => {
            bail!("bracket IPv6 targets, e.g. `[{t}]:{}`", profile.default_port())
        }
        Some((host, port)) if !host.is_empty() && !port.is_empty() => Ok(format!("{host}:{port}")),
        _ => Ok(format!("{t}:{}", profile.default_port())),
    }
}

/// Open one authenticated connection to a shared endpoint.
pub async fn connect(addr: &str, profile: Profile, token: &str) -> anyhow::Result<TcpStream> {
    let mut s = TcpStream::connect(addr)
        .await
        .with_context(|| format!("connect share at {addr}"))?;

    s.write_all(format!("{MAGIC} {profile}\n").as_bytes()).await?;
    s.flush().await?;

    let challenge = read_line(&mut s).await?;
    let nonce = match challenge.strip_prefix("NONCE ") {
        Some(n) if !n.is_empty() => n.to_string(),
        _ => bail!(describe_refusal(&challenge)),
    };

    let auth = handshake::expected_auth(token, &nonce);
    s.write_all(format!("AUTH {auth}\n").as_bytes()).await?;
    s.flush().await?;

    let verdict = read_line(&mut s).await?;
    if verdict != "OK" {
        bail!(describe_refusal(&verdict));
    }
    Ok(s)
}

/// Turn a server `ERR …` line into something a person can act on.
fn describe_refusal(line: &str) -> String {
    match line.strip_prefix("ERR ") {
        Some("auth failed") => {
            "share rejected the token — check `adb_share.token` on the host matches the one \
             passed here (--token / DIALF_SHARE_TOKEN)"
                .to_string()
        }
        Some("bad magic") => {
            "the endpoint is not a dialf share (bad magic) — check the host and port".to_string()
        }
        Some(other) => format!("share refused the connection: {other}"),
        None if line.is_empty() => "share closed the connection during the handshake".to_string(),
        None => format!("unexpected reply from share: {line}"),
    }
}

async fn read_line(stream: &mut TcpStream) -> anyhow::Result<String> {
    let mut reader = BufReader::new(stream);
    let mut buf = Vec::new();
    let n = tokio::io::AsyncReadExt::take(&mut reader, MAX_LINE as u64)
        .read_until(b'\n', &mut buf)
        .await?;
    if n == 0 {
        bail!("share closed the connection during the handshake");
    }
    Ok(String::from_utf8_lossy(&buf).trim().to_string())
}

/// Run the shim until cancelled: accept locally, authenticate upstream, splice.
///
/// `bind` is loopback by design — this port inherits the upstream's full authority, so
/// re-exporting it to a network would hand that authority on with no gate at all.
pub async fn run(
    bind: SocketAddr,
    target: String,
    profile: Profile,
    token: String,
) -> anyhow::Result<()> {
    let listener = TcpListener::bind(bind)
        .await
        .with_context(|| format!("bind local {profile} port {bind}"))?;
    run_on(listener, target, profile, token).await
}

/// [`run`] on an already-bound listener, so a caller that needs the assigned port (binding on
/// port 0) can read it before the loop starts.
pub async fn run_on(
    listener: TcpListener,
    target: String,
    profile: Profile,
    token: String,
) -> anyhow::Result<()> {
    let bind = listener.local_addr()?;

    // Fail fast on a bad token or unreachable host, rather than at the first adb command.
    connect(&target, profile, &token)
        .await
        .with_context(|| format!("initial handshake with {target}"))?;

    println!("{profile} share ready: {target} -> {bind}");
    println!("  adb -H {} -P {}  (or ADB_SERVER_SOCKET=tcp:{bind})", bind.ip(), bind.port());
    println!("  Ctrl+C to stop");

    loop {
        let (mut local, _) = listener.accept().await?;
        let target = target.clone();
        let token = token.clone();
        tokio::spawn(async move {
            match connect(&target, profile, &token).await {
                Ok(mut up) => {
                    let _ = tokio::io::copy_bidirectional(&mut local, &mut up).await;
                }
                Err(e) => {
                    eprintln!("share connection failed: {e:#}");
                    let _ = local.shutdown().await;
                }
            }
        });
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn target_gets_the_default_port_when_bare() {
        assert_eq!(target_addr("lab-mac", Profile::Adb).unwrap(), "lab-mac:5038");
        assert_eq!(
            target_addr("192.168.1.50", Profile::Adb).unwrap(),
            "192.168.1.50:5038"
        );
    }

    #[test]
    fn an_explicit_port_wins() {
        assert_eq!(target_addr("lab-mac:9000", Profile::Adb).unwrap(), "lab-mac:9000");
    }

    #[test]
    fn ipv6_must_be_bracketed() {
        assert_eq!(target_addr("[::1]:9000", Profile::Adb).unwrap(), "[::1]:9000");
        assert_eq!(target_addr("[::1]", Profile::Adb).unwrap(), "[::1]:5038");
        // Bare IPv6 is ambiguous; guessing here is what crashed the phone app on an
        // unbracketed URL, so it's an error with the fix in the message.
        let err = target_addr("fe80::1:2:3", Profile::Adb).unwrap_err();
        assert!(err.to_string().contains("bracket"), "got: {err}");
        assert!(target_addr("[::1", Profile::Adb).is_err());
    }

    #[test]
    fn empty_targets_are_rejected() {
        assert!(target_addr("", Profile::Adb).is_err());
        assert!(target_addr("   ", Profile::Adb).is_err());
    }

    #[test]
    fn refusals_explain_themselves() {
        // The common failure is a token mismatch; the message must name where to look.
        let msg = describe_refusal("ERR auth failed");
        assert!(msg.contains("token"), "got: {msg}");
        assert!(describe_refusal("ERR bad magic").contains("not a dialf share"));
        assert!(describe_refusal("").contains("closed"));
    }
}
