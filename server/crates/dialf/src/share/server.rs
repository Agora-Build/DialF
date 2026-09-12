//! Host side: the listener that fronts the local device endpoint.
//!
//! A share either issues a token or it doesn't. With one, every connection must pass
//! [`handshake`] and a rejected peer never opens an upstream socket at all — that ordering is
//! the security property this module provides, pinned down by `rejects_a_wrong_token`.
//! Without one the share is open, and stock `adb -H … -P …` connects with no client shim,
//! since the adb client cannot perform a handshake.
//!
//! Either way the share **expires**: the accept loop stops itself at a deadline, so a door
//! left open at the end of a session closes on its own.

use std::collections::HashSet;
use std::io;
use std::net::{IpAddr, SocketAddr};
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use anyhow::Context;
use tokio::io::{AsyncBufReadExt, AsyncReadExt, AsyncWriteExt, BufReader};
use tokio::net::{TcpListener, TcpStream, UnixStream};
use tokio::sync::Notify;

use super::handshake::{self, MAGIC, MAX_LINE};
use super::{ResolvedShare, Upstream};

/// How long a peer gets to finish the handshake. Short, so a scanner holding sockets open
/// can't accumulate them.
const HANDSHAKE_TIMEOUT: Duration = Duration::from_secs(5);

/// How long [`ShareHandle::stop`] waits for a clean exit before aborting the task.
const SHUTDOWN_GRACE: Duration = Duration::from_secs(2);

/// A running share: the task serving it, plus the switch that stops it.
pub struct ShareHandle {
    pub config: ResolvedShare,
    pub active: Arc<AtomicU64>,
    pub served: Arc<AtomicU64>,
    /// Wall-clock moment the share stops itself, for reporting. `None` never expires.
    pub expires_at: Option<std::time::SystemTime>,
    /// Cleared when the accept loop exits — by `stop`, or by the share expiring on its own.
    live: Arc<AtomicBool>,
    /// Forward-port listeners, stopped with the share.
    forwards: Vec<tokio::task::JoinHandle<()>>,
    cancel: Arc<Notify>,
    task: tokio::task::JoinHandle<()>,
}

impl ShareHandle {
    /// Whether the share is still listening. False once it expired or was stopped.
    pub fn is_live(&self) -> bool {
        self.live.load(Ordering::Relaxed)
    }

    /// Seconds left before the share expires, if it expires at all.
    pub fn seconds_remaining(&self) -> Option<u64> {
        let at = self.expires_at?;
        Some(
            at.duration_since(std::time::SystemTime::now())
                .map(|d| d.as_secs())
                .unwrap_or(0),
        )
    }

    /// Connections currently spliced through.
    pub fn active_connections(&self) -> u64 {
        self.active.load(Ordering::Relaxed)
    }

    /// Connections authenticated since this share started.
    pub fn served_connections(&self) -> u64 {
        self.served.load(Ordering::Relaxed)
    }

    /// Stop listening and drop in-flight connections.
    ///
    /// Returns only once the accept loop has exited and released the port, so an immediate
    /// restart on the same address works. Aborting the task instead would resolve before the
    /// listener was dropped, and `share stop && share start` would hit `AddrInUse`.
    pub async fn stop(self) {
        // `notify_waiters` wakes a loop currently parked on `notified()`; `notify_one` leaves
        // a permit for one that is mid-iteration, so the signal can't be missed either way.
        self.cancel.notify_waiters();
        self.cancel.notify_one();
        for f in &self.forwards {
            f.abort();
        }
        let abort = self.task.abort_handle();
        if tokio::time::timeout(SHUTDOWN_GRACE, self.task).await.is_err() {
            tracing::warn!("share did not stop within {SHUTDOWN_GRACE:?}; aborting it");
            abort.abort();
        }
    }
}

/// Bind the share and serve it in the background.
///
/// Binding happens here, before returning, so `share.start` can report "address in use" to the
/// caller instead of failing invisibly in a spawned task.
pub async fn start(mut config: ResolvedShare) -> anyhow::Result<ShareHandle> {
    let listener = TcpListener::bind(config.bind)
        .await
        .with_context(|| format!("bind {} share on {}", config.profile, config.bind))?;
    // Report where we actually landed, so port 0 shows the assigned port rather than ":0".
    config.bind = listener.local_addr().unwrap_or(config.bind);

    if config.is_public() && !config.require_auth() {
        tracing::warn!(
            bind = %config.bind,
            "device share is open on the network — anyone who can reach this port can drive \
             the phone; use a token or a trusted network"
        );
    } else if config.is_public() {
        tracing::warn!(
            bind = %config.bind,
            "device share is reachable off-box — authenticated but NOT encrypted; use a VPN \
             or SSH tunnel outside a trusted LAN"
        );
    }
    tracing::info!(
        bind = %config.bind,
        upstream = %config.upstream,
        profile = %config.profile,
        auth = config.require_auth(),
        expires_in = ?config.expires_after,
        "device share listening"
    );

    let cancel = Arc::new(Notify::new());
    let active = Arc::new(AtomicU64::new(0));
    let served = Arc::new(AtomicU64::new(0));
    let live = Arc::new(AtomicBool::new(true));
    let expires_at = config.expires_after.map(|d| std::time::SystemTime::now() + d);
    let authed: Authed = Arc::new(Mutex::new(HashSet::new()));

    // Forward ports bind before we report success, so "address in use" surfaces to the caller
    // rather than disappearing into a spawned task.
    let mut forwards = Vec::new();
    for port in config.forward_ports.clone() {
        let addr = SocketAddr::new(config.bind.ip(), port);
        let fwd = TcpListener::bind(addr)
            .await
            .with_context(|| format!("bind forward port {addr}"))?;
        tracing::info!(%addr, "forwarding to 127.0.0.1:{port}");
        forwards.push(tokio::spawn(forward_loop(
            fwd,
            port,
            config.require_auth(),
            authed.clone(),
            cancel.clone(),
        )));
    }

    let task = tokio::spawn(accept_loop(
        listener,
        config.clone(),
        cancel.clone(),
        active.clone(),
        served.clone(),
        live.clone(),
        authed,
    ));

    Ok(ShareHandle {
        config,
        active,
        served,
        expires_at,
        live,
        forwards,
        cancel,
        task,
    })
}

/// Peers that have completed the token handshake on this share.
type Authed = Arc<Mutex<HashSet<IpAddr>>>;

/// Proxy one extra port straight through to `127.0.0.1:<port>` on this host.
///
/// This carries a tool's tunnel (scrcpy video, a Flutter VM service), not adb protocol, and
/// the tool opens it with a plain socket — it cannot perform our handshake. So on a
/// token-protected share the gate is the peer's address: only somebody who already
/// authenticated on the adb port gets through here. That is weaker than the handshake
/// (addresses can be spoofed, and a NAT makes several machines look like one), but it keeps a
/// forward port from being a hole straight past the token.
async fn forward_loop(
    listener: TcpListener,
    port: u16,
    require_auth: bool,
    authed: Authed,
    cancel: Arc<Notify>,
) {
    loop {
        let accepted = tokio::select! {
            _ = cancel.notified() => return,
            r = listener.accept() => r,
        };
        let Ok((mut down, peer)) = accepted else {
            continue;
        };
        if require_auth && !authed.lock().unwrap().contains(&peer.ip()) {
            tracing::warn!(%peer, port, "forward port refused: that address has not authenticated");
            continue;
        }
        tokio::spawn(async move {
            match TcpStream::connect((std::net::Ipv4Addr::LOCALHOST, port)).await {
                Ok(mut up) => {
                    let _ = tokio::io::copy_bidirectional(&mut down, &mut up).await;
                }
                Err(e) => tracing::debug!(
                    port,
                    error = %e,
                    "forward port has nothing behind it yet (is the tunnel set up?)"
                ),
            }
        });
    }
}

async fn accept_loop(
    listener: TcpListener,
    config: ResolvedShare,
    cancel: Arc<Notify>,
    active: Arc<AtomicU64>,
    served: Arc<AtomicU64>,
    live: Arc<AtomicBool>,
    authed: Authed,
) {
    // A share is a door held open. Unless told otherwise it closes itself, so one forgotten
    // at the end of a session doesn't stay open overnight.
    let deadline = config
        .expires_after
        .map(|d| tokio::time::Instant::now() + d);

    loop {
        let accepted = tokio::select! {
            _ = cancel.notified() => break,
            _ = async {
                match deadline {
                    Some(at) => tokio::time::sleep_until(at).await,
                    // No expiry: never completes, so the select waits on the other branches.
                    None => std::future::pending().await,
                }
            } => {
                // Expiry closes the door *and* clears the room: a connection opened before
                // the deadline would otherwise keep the phone reachable indefinitely, which
                // is exactly what the expiry is meant to prevent.
                tracing::info!(bind = %config.bind, "device share expired");
                cancel.notify_waiters();
                break;
            }
            r = listener.accept() => r,
        };
        let (stream, peer) = match accepted {
            Ok(v) => v,
            Err(e) => {
                tracing::warn!(error = %e, "share accept failed");
                continue;
            }
        };

        let config = config.clone();
        let active = active.clone();
        let served = served.clone();
        let cancel = cancel.clone();
        let authed = authed.clone();
        tokio::spawn(async move {
            match serve_conn(stream, peer, &config, &active, &served, cancel, authed).await {
                Ok(()) => {}
                Err(e) => tracing::debug!(%peer, error = %e, "share connection ended"),
            }
        });
    }
    live.store(false, Ordering::Relaxed);
}

async fn serve_conn(
    stream: TcpStream,
    peer: SocketAddr,
    config: &ResolvedShare,
    active: &Arc<AtomicU64>,
    served: &Arc<AtomicU64>,
    cancel: Arc<Notify>,
    authed: Authed,
) -> anyhow::Result<()> {
    let mut stream = if config.require_auth() {
        match tokio::time::timeout(HANDSHAKE_TIMEOUT, authenticate(stream, config)).await {
            Ok(Ok(s)) => s,
            Ok(Err(e)) => {
                // Rejections are logged at warn with the peer, so a brute-force attempt is
                // visible in the daemon log rather than silent.
                tracing::warn!(%peer, reason = %e, "share connection rejected");
                return Ok(());
            }
            Err(_) => {
                tracing::warn!(%peer, "share handshake timed out");
                return Ok(());
            }
        }
    } else {
        // Open (loopback) share: the peer should be plain adb. A shim that tries to handshake
        // would otherwise have its preamble parsed as an adb request and get a bare
        // disconnect, so tell it what to do instead.
        match tokio::time::timeout(HANDSHAKE_TIMEOUT, decline_handshake(stream)).await {
            Ok(Ok(s)) => s,
            Ok(Err(e)) => {
                tracing::debug!(%peer, reason = %e, "share connection ended before use");
                return Ok(());
            }
            Err(_) => {
                tracing::warn!(%peer, "share peer sent nothing");
                return Ok(());
            }
        }
    };

    // Remember the address so this peer's tunnel traffic can reach a forward port.
    authed.lock().unwrap().insert(peer.ip());
    served.fetch_add(1, Ordering::Relaxed);
    active.fetch_add(1, Ordering::Relaxed);
    tracing::info!(%peer, upstream = %config.upstream, "share connection authenticated");

    let result = splice(&mut stream, config, cancel).await;
    active.fetch_sub(1, Ordering::Relaxed);
    result
}

/// On an open share, turn away a peer that opened with a handshake.
///
/// `DIAL` is not valid hex, so it can never be an adb length prefix — peeking four bytes
/// distinguishes a shim from an adb client without consuming anything.
async fn decline_handshake(stream: TcpStream) -> anyhow::Result<TcpStream> {
    let mut head = [0u8; 4];
    let n = stream.peek(&mut head).await?;
    if n == 4 && &head == MAGIC.as_bytes()[..4].as_ref() {
        let mut stream = stream;
        stream
            .write_all(b"ERR this share needs no token; point adb straight at it \
                        (adb -H <host> -P <port>)\n")
            .await?;
        anyhow::bail!("shim connected to an open share");
    }
    Ok(stream)
}

/// Run the challenge-response. Returns the stream only when the peer proved it holds the
/// token; every failure path returns an error and the caller closes without touching upstream.
async fn authenticate(stream: TcpStream, config: &ResolvedShare) -> anyhow::Result<TcpStream> {
    let token = config.token.as_deref().unwrap_or_default();
    let (read, mut write) = stream.into_split();
    let mut lines = BufReader::new(read);

    let hello = read_line(&mut lines).await?;
    let mut parts = hello.split_whitespace();
    let magic = parts.next().unwrap_or_default();
    if magic != MAGIC {
        let _ = write.write_all(b"ERR bad magic\n").await;
        anyhow::bail!("bad magic `{magic}`");
    }
    let profile = parts.next().unwrap_or_default();
    if profile != config.profile.as_str() {
        let _ = write
            .write_all(format!("ERR profile mismatch (serving {})\n", config.profile).as_bytes())
            .await;
        anyhow::bail!("profile `{profile}` (serving {})", config.profile);
    }

    let nonce = handshake::new_nonce();
    write.write_all(format!("NONCE {nonce}\n").as_bytes()).await?;
    write.flush().await?;

    let answer = read_line(&mut lines).await?;
    let auth = answer.strip_prefix("AUTH ").unwrap_or_default();
    if !handshake::verify(token, &nonce, auth) {
        let _ = write.write_all(b"ERR auth failed\n").await;
        anyhow::bail!("auth failed");
    }

    write.write_all(b"OK\n").await?;
    write.flush().await?;

    // Rejoin the halves. `BufReader` may hold bytes the peer pipelined behind the handshake,
    // but the adb client sends nothing until it sees OK, so an empty buffer is expected; a
    // non-empty one means a peer we don't understand.
    if !lines.buffer().is_empty() {
        anyhow::bail!("peer sent data before the handshake completed");
    }
    let read = lines.into_inner();
    Ok(read.reunite(write).expect("halves are from the same stream"))
}

/// Read one `\n`-terminated line, refusing anything longer than [`MAX_LINE`] so a peer can't
/// make us buffer without bound.
async fn read_line<R: tokio::io::AsyncRead + Unpin>(
    reader: &mut BufReader<R>,
) -> anyhow::Result<String> {
    let mut buf = Vec::new();
    let n = (&mut *reader)
        .take(MAX_LINE as u64)
        .read_until(b'\n', &mut buf)
        .await?;
    if n == 0 {
        anyhow::bail!("peer closed during handshake");
    }
    if !buf.ends_with(b"\n") {
        anyhow::bail!("handshake line exceeded {MAX_LINE} bytes");
    }
    Ok(String::from_utf8_lossy(&buf).trim().to_string())
}

/// Connect the upstream endpoint and relay, applying the profile's device policy.
async fn splice(
    downstream: &mut TcpStream,
    config: &ResolvedShare,
    cancel: Arc<Notify>,
) -> anyhow::Result<()> {
    match &config.upstream {
        Upstream::Tcp(addr) => {
            let mut up = TcpStream::connect(addr)
                .await
                .with_context(|| format!("connect upstream {addr}"))?;
            pump(downstream, &mut up, config, cancel).await
        }
        Upstream::Unix(path) => {
            let mut up = UnixStream::connect(path)
                .await
                .with_context(|| format!("connect upstream {}", path.display()))?;
            pump(downstream, &mut up, config, cancel).await
        }
    }
}

/// Relay one connection, unblocking if the share is stopped underneath it.
///
/// adb connections go through [`crate::share::adb::gate`], which enforces the target list and
/// blocks host-destructive requests before forwarding anything.
async fn pump<U>(
    down: &mut TcpStream,
    up: &mut U,
    config: &ResolvedShare,
    cancel: Arc<Notify>,
) -> anyhow::Result<()>
where
    U: tokio::io::AsyncRead + tokio::io::AsyncWrite + Unpin,
{
    let relay = async {
        match config.profile {
            crate::share::Profile::Adb => crate::share::adb::gate(down, up, &config.targets).await,
        }
    };
    tokio::select! {
        _ = cancel.notified() => Ok(()),
        r = relay => match r {
            Ok(()) => Ok(()),
            // A peer hanging up mid-transfer is normal (Ctrl+C'd adb), not an error worth
            // surfacing.
            Err(e) => match e.downcast_ref::<io::Error>() {
                Some(io) if is_disconnect(io) => Ok(()),
                _ => Err(e),
            },
        },
    }
}

fn is_disconnect(e: &io::Error) -> bool {
    matches!(
        e.kind(),
        io::ErrorKind::BrokenPipe
            | io::ErrorKind::ConnectionReset
            | io::ErrorKind::ConnectionAborted
            | io::ErrorKind::UnexpectedEof
    )
}

/// Whether the upstream endpoint currently accepts connections — reported by `share status`,
/// since a share that is listening but has no adb server behind it looks fine until used.
pub async fn upstream_reachable(upstream: &Upstream) -> bool {
    match upstream {
        Upstream::Tcp(addr) => tokio::time::timeout(Duration::from_secs(2), TcpStream::connect(addr))
            .await
            .map(|r| r.is_ok())
            .unwrap_or(false),
        Upstream::Unix(path) => {
            tokio::time::timeout(Duration::from_secs(2), UnixStream::connect(path))
                .await
                .map(|r| r.is_ok())
                .unwrap_or(false)
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::share::{Profile, ShareConfig};
    use tokio::io::AsyncReadExt;


    /// Stands in for the adb server: consumes the one framed request the gate forwards, then
    /// echoes. `hits` counts connections, which is how we prove a rejected peer never reached
    /// the upstream at all.
    struct FakeUpstream {
        addr: SocketAddr,
        hits: Arc<AtomicU64>,
    }

    async fn fake_upstream() -> FakeUpstream {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let hits = Arc::new(AtomicU64::new(0));
        let h = hits.clone();
        tokio::spawn(async move {
            loop {
                let Ok((mut s, _)) = listener.accept().await else {
                    return;
                };
                h.fetch_add(1, Ordering::SeqCst);
                tokio::spawn(async move {
                    if crate::share::adb::read_message(&mut s).await.is_err() {
                        return;
                    }
                    let mut buf = [0u8; 1024];
                    while let Ok(n) = s.read(&mut buf).await {
                        if n == 0 || s.write_all(&buf[..n]).await.is_err() {
                            return;
                        }
                    }
                });
            }
        });
        FakeUpstream { addr, hits }
    }

    fn token_of(share: &ShareHandle) -> String {
        share.config.token.clone().expect("share should have issued a token")
    }

    async fn start_share(upstream: &FakeUpstream) -> ShareHandle {
        let cfg = ShareConfig {
            enabled: false,
            bind: Some("127.0.0.1:0".to_string()),
            upstream: Some(format!("tcp:{}", upstream.addr)),
            targets: Vec::new(),
            all: true,
            expire_after: 3600,
            forward_ports: Vec::new(),
        };
        // Bind on port 0, then recover the real port for clients to dial.
        let mut resolved = cfg.resolve(Profile::Adb, true).unwrap();
        let probe = TcpListener::bind("127.0.0.1:0").await.unwrap();
        resolved.bind = probe.local_addr().unwrap();
        drop(probe);
        start(resolved).await.unwrap()
    }

    /// Drive the client half of the handshake by hand, so the test doesn't depend on
    /// `client.rs` being correct.
    async fn handshake_as_client(addr: SocketAddr, token: &str) -> anyhow::Result<TcpStream> {
        let mut s = TcpStream::connect(addr).await?;
        s.write_all(format!("{MAGIC} adb\n").as_bytes()).await?;
        let mut reader = BufReader::new(&mut s);
        let challenge = read_line(&mut reader).await?;
        let nonce = challenge
            .strip_prefix("NONCE ")
            .context("expected a NONCE line")?
            .to_string();
        let auth = handshake::expected_auth(token, &nonce);
        s.write_all(format!("AUTH {auth}\n").as_bytes()).await?;
        let mut reader = BufReader::new(&mut s);
        let verdict = read_line(&mut reader).await?;
        if verdict != "OK" {
            anyhow::bail!("{verdict}");
        }
        Ok(s)
    }

    #[tokio::test]
    async fn authenticated_traffic_reaches_the_upstream_both_ways() {
        let up = fake_upstream().await;
        let share = start_share(&up).await;

        let mut s = handshake_as_client(share.config.bind, &token_of(&share)).await.unwrap();
        // Past the token gate the connection still has to pass the adb gate, so send a real
        // request before the raw payload.
        crate::share::adb::write_message(&mut s, "host:transport-any").await.unwrap();
        s.write_all(b"hello-through-the-share").await.unwrap();
        let mut buf = [0u8; 32];
        let n = s.read(&mut buf).await.unwrap();
        assert_eq!(&buf[..n], b"hello-through-the-share");
        assert_eq!(up.hits.load(Ordering::SeqCst), 1);
        assert_eq!(share.served_connections(), 1);

        share.stop().await;
    }

    #[tokio::test]
    async fn rejects_a_wrong_token_without_touching_the_upstream() {
        // The security-critical case: a failed handshake must not open an upstream socket.
        let up = fake_upstream().await;
        let share = start_share(&up).await;

        let err = handshake_as_client(share.config.bind, "the-wrong-token-here")
            .await
            .unwrap_err();
        assert!(err.to_string().contains("auth failed"), "got: {err}");
        assert_eq!(up.hits.load(Ordering::SeqCst), 0, "upstream must not be reached");
        assert_eq!(share.served_connections(), 0);

        share.stop().await;
    }

    #[tokio::test]
    async fn rejects_a_bad_magic_and_a_wrong_profile() {
        let up = fake_upstream().await;
        let share = start_share(&up).await;

        for hello in ["GARBAGE/9 adb\n", "DIALF-SHARE/1 ios\n"] {
            let mut s = TcpStream::connect(share.config.bind).await.unwrap();
            s.write_all(hello.as_bytes()).await.unwrap();
            let mut reader = BufReader::new(&mut s);
            let reply = read_line(&mut reader).await.unwrap();
            assert!(reply.starts_with("ERR"), "expected ERR, got {reply}");
        }
        assert_eq!(up.hits.load(Ordering::SeqCst), 0);

        share.stop().await;
    }

    #[tokio::test]
    async fn an_oversized_handshake_line_is_refused() {
        let up = fake_upstream().await;
        let share = start_share(&up).await;

        let mut s = TcpStream::connect(share.config.bind).await.unwrap();
        // No newline within MAX_LINE: the server must give up rather than buffer on.
        s.write_all(&vec![b'A'; MAX_LINE * 4]).await.unwrap();
        let mut buf = [0u8; 64];
        let n = s.read(&mut buf).await.unwrap_or(0);
        assert!(n == 0 || buf.starts_with(b"ERR"), "expected refusal");
        assert_eq!(up.hits.load(Ordering::SeqCst), 0);

        share.stop().await;
    }

    #[tokio::test]
    async fn a_silent_peer_is_dropped_rather_than_held() {
        // Connect and say nothing. HANDSHAKE_TIMEOUT is 5s, so waiting it out keeps this test
        // honest about the timeout actually firing.
        let up = fake_upstream().await;
        let share = start_share(&up).await;

        let mut s = TcpStream::connect(share.config.bind).await.unwrap();
        let mut buf = [0u8; 16];
        let n = tokio::time::timeout(HANDSHAKE_TIMEOUT * 2, s.read(&mut buf))
            .await
            .expect("server should close the connection, not hold it")
            .unwrap_or(0);
        assert_eq!(n, 0, "expected a close, got data");
        assert_eq!(up.hits.load(Ordering::SeqCst), 0);

        share.stop().await;
    }

    #[tokio::test]
    async fn stopping_closes_the_port() {
        let up = fake_upstream().await;
        let share = start_share(&up).await;
        let addr = share.config.bind;
        let tok = token_of(&share);
        share.stop().await;

        // Give the listener a moment to actually release the port.
        tokio::time::sleep(Duration::from_millis(100)).await;
        assert!(
            handshake_as_client(addr, &tok).await.is_err(),
            "share should refuse connections once stopped"
        );
    }

    #[tokio::test]
    async fn a_share_only_has_a_token_when_one_was_asked_for() {
        // Every issued token is machine-generated, so the only way to be tokenless now is to
        // not ask for one — which is an open share, not an error.
        let cfg = ShareConfig { all: true, ..Default::default() };
        assert!(cfg.resolve(Profile::Adb, false).unwrap().token.is_none());
        assert!(cfg.resolve(Profile::Adb, true).unwrap().token.is_some());
    }

    #[tokio::test]
    async fn upstream_reachability_is_reported() {
        let up = fake_upstream().await;
        assert!(upstream_reachable(&Upstream::Tcp(up.addr.to_string())).await);
        // Port 1 on loopback: nothing listens there.
        assert!(!upstream_reachable(&Upstream::Tcp("127.0.0.1:1".to_string())).await);
    }
}
