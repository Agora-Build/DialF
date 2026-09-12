//! End-to-end device sharing: the real client shim against the real host listener.
//!
//! The unit tests in `share::server` and `share::adb` cover auth and policy in isolation.
//! These drive both halves together — shim, token gate, adb gate, splice — so a change
//! applied to only one side fails here rather than on someone's LAN.
//!
//! The last two tests run a real `adb` through the share and skip when adb or a device is
//! absent, in the style of the ten-vad skip in `audio_pipeline.rs`.

use std::net::SocketAddr;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;
use std::time::Duration;

use dialf::share::adb;
use dialf::share::server::ShareHandle;
use dialf::share::{client, server, Profile, ShareConfig};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::{TcpListener, TcpStream};

const TOKEN: &str = "an-end-to-end-share-token";
const DEV_A: &str = "AAAA1111";
const DEV_B: &str = "BBBB2222";

/// Stands in for the adb server: answers `host:devices*` with a canned list, and treats
/// anything else as a transport request — OKAY, then echo, so tests can check the splice.
struct FakeAdb {
    addr: SocketAddr,
    hits: Arc<AtomicU64>,
    /// Requests the upstream actually received — what a blocked request must never appear in.
    seen: Arc<tokio::sync::Mutex<Vec<String>>>,
}

async fn fake_adb(devices: &'static str) -> FakeAdb {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    let hits = Arc::new(AtomicU64::new(0));
    let seen = Arc::new(tokio::sync::Mutex::new(Vec::new()));
    let (h, sn) = (hits.clone(), seen.clone());
    tokio::spawn(async move {
        loop {
            let Ok((mut s, _)) = listener.accept().await else {
                return;
            };
            h.fetch_add(1, Ordering::SeqCst);
            let sn = sn.clone();
            tokio::spawn(async move {
                let Ok(req) = adb::read_message(&mut s).await else {
                    return;
                };
                sn.lock().await.push(req.clone());
                if req.starts_with("host:devices") || req.starts_with("host:track-devices") {
                    let _ = s.write_all(b"OKAY").await;
                    let _ = adb::write_message(&mut s, devices).await;
                    return;
                }
                let _ = s.write_all(b"OKAY").await;
                let mut buf = vec![0u8; 64 * 1024];
                loop {
                    match s.read(&mut buf).await {
                        Ok(0) | Err(_) => return,
                        Ok(n) => {
                            if s.write_all(&buf[..n]).await.is_err() {
                                return;
                            }
                        }
                    }
                }
            });
        }
    });
    FakeAdb { addr, hits, seen }
}

async fn start_share_with(upstream: &str, targets: Vec<String>, all: bool) -> ShareHandle {
    let cfg = ShareConfig {
        enabled: false,
        bind: None,
        token: TOKEN.to_string(),
        upstream: Some(format!("tcp:{upstream}")),
        targets,
        all,
        // Loopback binds are open by default; these exercise the authenticated path.
        require_token: Some(true),
    };
    // Port 0: the listener reports the port it was assigned, so parallel tests never race
    // over a "free" port that something else grabbed in between.
    let mut resolved = cfg.resolve(Profile::Adb).unwrap();
    resolved.bind = "127.0.0.1:0".parse().unwrap();
    server::start(resolved).await.unwrap()
}

async fn start_share(upstream: &str) -> ShareHandle {
    start_share_with(upstream, Vec::new(), true).await
}

/// Start the real shim and return the local address to point clients at.
async fn start_shim(target: SocketAddr, token: &str) -> (SocketAddr, tokio::task::JoinHandle<()>) {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let bind = listener.local_addr().unwrap();
    let token = token.to_string();
    let task = tokio::spawn(async move {
        let _ = client::run_on(listener, target.to_string(), Profile::Adb, token).await;
    });
    (bind, task)
}

/// Open a connection through the shim and get past the adb gate with `request`.
async fn open_via_shim(shim: SocketAddr, request: &str) -> TcpStream {
    let mut c = TcpStream::connect(shim).await.unwrap();
    adb::write_message(&mut c, request).await.unwrap();
    let mut status = [0u8; 4];
    c.read_exact(&mut status).await.unwrap();
    assert_eq!(&status, b"OKAY", "gate refused `{request}`");
    c
}

#[tokio::test]
async fn shim_and_listener_move_bytes_end_to_end() {
    let up = fake_adb("").await;
    let share = start_share(&up.addr.to_string()).await;
    let (shim, task) = start_shim(share.config.bind, TOKEN).await;

    let mut c = open_via_shim(shim, "host:transport-any").await;
    c.write_all(b"round-trip through shim and share").await.unwrap();
    let mut buf = [0u8; 64];
    let n = c.read(&mut buf).await.unwrap();
    assert_eq!(&buf[..n], b"round-trip through shim and share");

    task.abort();
    share.stop().await;
}

#[tokio::test]
async fn a_payload_larger_than_one_buffer_survives_intact() {
    // `adb push` moves megabytes through this path; a splice that only ever sees small
    // writes would pass the other tests and still corrupt a real file transfer.
    let up = fake_adb("").await;
    let share = start_share(&up.addr.to_string()).await;
    let (shim, task) = start_shim(share.config.bind, TOKEN).await;

    let payload: Vec<u8> = (0..1_000_000u32).map(|i| (i % 251) as u8).collect();
    let mut c = open_via_shim(shim, "host:transport-any").await;
    let (mut r, mut w) = c.split();

    let sent = payload.clone();
    let writer = async move {
        w.write_all(&sent).await.unwrap();
        w.shutdown().await.unwrap();
    };
    let mut got = Vec::with_capacity(payload.len());
    let reader = async {
        r.read_to_end(&mut got).await.unwrap();
    };
    tokio::join!(writer, reader);

    assert_eq!(got.len(), payload.len(), "byte count changed in transit");
    assert_eq!(got, payload, "payload corrupted in transit");

    task.abort();
    share.stop().await;
}

#[tokio::test]
async fn many_concurrent_connections_are_served() {
    // adb opens a fresh connection per command, each re-running both gates, so concurrency
    // here is the normal case rather than a stress test.
    let up = fake_adb("").await;
    let share = start_share(&up.addr.to_string()).await;
    let (shim, task) = start_shim(share.config.bind, TOKEN).await;

    let mut joins = Vec::new();
    for i in 0..20u32 {
        joins.push(tokio::spawn(async move {
            let msg = format!("connection-{i}");
            let mut c = open_via_shim(shim, "host:transport-any").await;
            c.write_all(msg.as_bytes()).await.unwrap();
            let mut buf = vec![0u8; msg.len()];
            c.read_exact(&mut buf).await.unwrap();
            assert_eq!(buf, msg.as_bytes(), "responses crossed between connections");
        }));
    }
    for j in joins {
        j.await.unwrap();
    }
    assert!(share.served_connections() >= 20);

    task.abort();
    share.stop().await;
}

#[tokio::test]
async fn the_shim_reports_a_token_mismatch_clearly() {
    // The most likely real failure. The message has to name the token, or the user is left
    // staring at a closed socket.
    let up = fake_adb("").await;
    let share = start_share(&up.addr.to_string()).await;

    let err = client::connect(&share.config.bind.to_string(), Profile::Adb, "not-the-right-token")
        .await
        .unwrap_err();
    let msg = format!("{err:#}");
    assert!(msg.contains("token"), "unhelpful message: {msg}");
    assert_eq!(up.hits.load(Ordering::SeqCst), 0, "upstream must stay untouched");

    share.stop().await;
}

#[tokio::test]
async fn an_unshared_device_cannot_be_reached_through_the_share() {
    // The point of --target: holding the token is not enough to reach a device that was
    // never shared, and the attempt must not even open an upstream connection.
    let up = fake_adb("").await;
    let share = start_share_with(&up.addr.to_string(), vec![DEV_A.to_string()], false).await;
    let (shim, task) = start_shim(share.config.bind, TOKEN).await;

    let mut c = TcpStream::connect(shim).await.unwrap();
    adb::write_message(&mut c, &format!("host:transport:{DEV_B}")).await.unwrap();
    let mut status = [0u8; 4];
    c.read_exact(&mut status).await.unwrap();
    assert_eq!(&status, b"FAIL", "an unshared device must be refused");
    let reason = adb::read_message(&mut c).await.unwrap();
    assert!(reason.contains(DEV_B), "reason should name the device: {reason}");

    assert!(
        !up.seen.lock().await.iter().any(|r| r.contains(DEV_B)),
        "the refused request must never reach the adb server"
    );

    // The shared device still works on the same share.
    let _ok = open_via_shim(shim, &format!("host:transport:{DEV_A}")).await;

    task.abort();
    share.stop().await;
}

#[tokio::test]
async fn the_device_list_shows_only_shared_devices() {
    // Otherwise a remote sees an inventory of everything plugged into the host.
    let up = fake_adb("AAAA1111\tdevice\nBBBB2222\tdevice\n").await;
    let share = start_share_with(&up.addr.to_string(), vec![DEV_A.to_string()], false).await;
    let (shim, task) = start_shim(share.config.bind, TOKEN).await;

    let mut c = TcpStream::connect(shim).await.unwrap();
    adb::write_message(&mut c, "host:devices").await.unwrap();
    let mut status = [0u8; 4];
    c.read_exact(&mut status).await.unwrap();
    assert_eq!(&status, b"OKAY");
    let list = adb::read_message(&mut c).await.unwrap();
    assert!(list.contains(DEV_A), "shared device missing from the list: {list:?}");
    assert!(!list.contains(DEV_B), "unshared device leaked in the list: {list:?}");

    task.abort();
    share.stop().await;
}

#[tokio::test]
async fn killing_the_hosts_adb_server_is_blocked() {
    // A remote `adb kill-server` — or just a peer on a different platform-tools version,
    // since the adb client kills a mismatched server by itself — would otherwise stop adb
    // on the host.
    let up = fake_adb("").await;
    let share = start_share(&up.addr.to_string()).await;
    let (shim, task) = start_shim(share.config.bind, TOKEN).await;

    let mut c = TcpStream::connect(shim).await.unwrap();
    adb::write_message(&mut c, "host:kill").await.unwrap();
    let mut status = [0u8; 4];
    c.read_exact(&mut status).await.unwrap();
    assert_eq!(&status, b"FAIL", "host:kill must be refused");

    assert!(
        !up.seen.lock().await.iter().any(|r| r == "host:kill"),
        "host:kill must never reach the adb server"
    );

    task.abort();
    share.stop().await;
}

#[tokio::test]
async fn a_dead_upstream_fails_fast_instead_of_hanging() {
    // Share is up, adb server is not — the common "forgot to start adb" case. The client
    // must get a closed connection promptly rather than block.
    // Bind then drop, so the address is one nothing is listening on.
    let dead = {
        let l = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let a = l.local_addr().unwrap();
        drop(l);
        a
    };
    let share = start_share(&dead.to_string()).await;

    let mut c = client::connect(&share.config.bind.to_string(), Profile::Adb, TOKEN)
        .await
        .expect("handshake still succeeds — the share itself is healthy");
    adb::write_message(&mut c, "host:transport-any").await.unwrap();
    let mut buf = [0u8; 16];
    let n = tokio::time::timeout(Duration::from_secs(10), c.read(&mut buf))
        .await
        .expect("must not hang when the upstream is down")
        .unwrap_or(0);
    assert_eq!(n, 0, "expected a close, not data");

    share.stop().await;
}

#[tokio::test]
async fn a_restarted_share_is_reachable_again() {
    // stop/start is the advertised way to close an exposed port temporarily; the second
    // start must not be poisoned by the first (a leaked listener would fail to bind).
    let up = fake_adb("").await;

    // A fixed port below the ephemeral range (49152+ on macOS): every other test binds port
    // 0, so nothing here can be handed this one, and "was the port released?" stays a real
    // question rather than a race with a sibling test.
    let cfg = ShareConfig {
        enabled: false,
        bind: Some("127.0.0.1:45871".to_string()),
        token: TOKEN.to_string(),
        upstream: Some(format!("tcp:{}", up.addr)),
        targets: Vec::new(),
        all: true,
        require_token: Some(true),
    };
    let share = server::start(cfg.resolve(Profile::Adb).unwrap()).await.unwrap();
    let addr = share.config.bind;
    share.stop().await;

    // The port comes back — but allow a moment for it. Measured on macOS, re-binding right
    // after `stop()` returns fails with EADDRINUSE about half the time and succeeds ~28ms
    // later, so the last of the teardown happens below us in the kernel. Asserting an
    // *immediate* re-bind would be testing socket teardown latency, not this code; what
    // matters is that the port is genuinely released rather than leaked, and that a bounded
    // wait is enough. (`stopping_closes_the_port` covers "a stopped share stops serving".)
    let deadline = std::time::Instant::now() + Duration::from_secs(5);
    loop {
        match TcpListener::bind(addr).await {
            Ok(_) => break,
            Err(e) if std::time::Instant::now() < deadline => {
                tokio::time::sleep(Duration::from_millis(25)).await;
                let _ = e;
            }
            Err(e) => panic!("stop leaked the listening port: {e}"),
        }
    }

    // And a fresh share serves normally afterwards.
    let again = start_share(&up.addr.to_string()).await;
    let mut c = client::connect(&again.config.bind.to_string(), Profile::Adb, TOKEN)
        .await
        .unwrap();
    adb::write_message(&mut c, "host:transport-any").await.unwrap();
    let mut status = [0u8; 4];
    c.read_exact(&mut status).await.unwrap();
    assert_eq!(&status, b"OKAY");
    c.write_all(b"still here").await.unwrap();
    let mut buf = [0u8; 32];
    let n = c.read(&mut buf).await.unwrap();
    assert_eq!(&buf[..n], b"still here");

    again.stop().await;
}

/// Serial of an attached, ready device, or `None` to skip the real-adb tests.
async fn attached_serial() -> Option<(std::path::PathBuf, String)> {
    let adb_bin = which::which("adb").ok()?;
    let out = tokio::process::Command::new(&adb_bin)
        .arg("devices")
        .output()
        .await
        .ok()?;
    let serial = String::from_utf8_lossy(&out.stdout)
        .lines()
        .skip(1)
        .find_map(|l| {
            let mut p = l.split_whitespace();
            let s = p.next()?;
            (p.next()? == "device").then(|| s.to_string())
        })?;
    Some((adb_bin, serial))
}

/// The real thing: a real `adb` client, through the shim, through the share, to the real adb
/// server, scoped to one real serial.
///
/// Multi-threaded on purpose: `#[tokio::test]` is single-threaded by default, and waiting on
/// a child process there starves the shim task — the test deadlocks instead of failing.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn real_adb_works_through_the_share() {
    let Some((adb_bin, serial)) = attached_serial().await else {
        eprintln!("no adb device attached; skipping real-adb share test");
        return;
    };

    let share = start_share_with("127.0.0.1:5037", vec![serial.clone()], false).await;
    let (shim, task) = start_shim(share.config.bind, TOKEN).await;
    let port = shim.port().to_string();

    let out = tokio::process::Command::new(&adb_bin)
        .args(["-H", "127.0.0.1", "-P", &port, "devices"])
        .output()
        .await
        .expect("run adb through the share");
    let stdout = String::from_utf8_lossy(&out.stdout);
    assert!(
        stdout.contains(&serial),
        "adb did not see the shared device through the share:\n{stdout}{}",
        String::from_utf8_lossy(&out.stderr)
    );

    // A shell command exercises a long-lived connection, not just the short `devices` one,
    // and proves transport-any resolved to the one shared device without an explicit -s.
    let shell = tokio::process::Command::new(&adb_bin)
        .args(["-H", "127.0.0.1", "-P", &port, "shell", "echo", "through-the-share"])
        .output()
        .await
        .expect("run adb shell through the share");
    assert_eq!(
        String::from_utf8_lossy(&shell.stdout).trim(),
        "through-the-share",
        "stderr: {}",
        String::from_utf8_lossy(&shell.stderr)
    );

    task.abort();
    share.stop().await;
}

/// A share that targets a serial which is not the attached one must expose nothing, even
/// though the adb server behind it has a perfectly good device.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn real_adb_cannot_see_an_unshared_device() {
    let Some((adb_bin, serial)) = attached_serial().await else {
        eprintln!("no adb device attached; skipping real-adb scoping test");
        return;
    };

    let share =
        start_share_with("127.0.0.1:5037", vec!["NOTATTACHED0000".to_string()], false).await;
    let (shim, task) = start_shim(share.config.bind, TOKEN).await;
    let port = shim.port().to_string();

    let out = tokio::process::Command::new(&adb_bin)
        .args(["-H", "127.0.0.1", "-P", &port, "devices"])
        .output()
        .await
        .expect("run adb through the share");
    let stdout = String::from_utf8_lossy(&out.stdout);
    assert!(
        !stdout.contains(&serial),
        "the real device leaked through a share that does not target it:\n{stdout}"
    );

    task.abort();
    share.stop().await;
}

// ---- open (loopback) shares -----------------------------------------------------------
//
// A loopback share needs no token, because the token would guard a door already ajar: whoever
// can reach this port can reach `127.0.0.1:5037` directly. That is what lets stock adb — which
// cannot perform a handshake — connect through an SSH tunnel with no shim.

/// An open share: loopback bind, no token, no handshake.
async fn start_open_share(upstream: &str) -> ShareHandle {
    let cfg = ShareConfig {
        enabled: false,
        bind: Some("127.0.0.1:0".to_string()),
        token: String::new(),
        upstream: Some(format!("tcp:{upstream}")),
        targets: Vec::new(),
        all: true,
        require_token: None, // the default rule: loopback => open
    };
    let resolved = cfg.resolve(Profile::Adb).unwrap();
    assert!(!resolved.require_auth, "a loopback share should default to open");
    server::start(resolved).await.unwrap()
}

#[tokio::test]
async fn an_open_share_takes_plain_adb_with_no_handshake() {
    let up = fake_adb("").await;
    let share = start_open_share(&up.addr.to_string()).await;

    // Straight to the share — no shim, no token, exactly what `adb -H` does.
    let mut c = TcpStream::connect(share.config.bind).await.unwrap();
    adb::write_message(&mut c, "host:transport-any").await.unwrap();
    let mut status = [0u8; 4];
    c.read_exact(&mut status).await.unwrap();
    assert_eq!(&status, b"OKAY");
    c.write_all(b"no handshake needed").await.unwrap();
    let mut buf = [0u8; 32];
    let n = c.read(&mut buf).await.unwrap();
    assert_eq!(&buf[..n], b"no handshake needed");

    share.stop().await;
}

#[tokio::test]
async fn an_open_share_still_enforces_device_scope_and_blocks_kill() {
    // Open means "no token", not "no policy" — the adb gate is independent of the token gate.
    let up = fake_adb("AAAA1111\tdevice\nBBBB2222\tdevice\n").await;
    let cfg = ShareConfig {
        enabled: false,
        bind: Some("127.0.0.1:0".to_string()),
        token: String::new(),
        upstream: Some(format!("tcp:{}", up.addr)),
        targets: vec![DEV_A.to_string()],
        all: false,
        require_token: None,
    };
    let share = server::start(cfg.resolve(Profile::Adb).unwrap()).await.unwrap();

    let mut c = TcpStream::connect(share.config.bind).await.unwrap();
    adb::write_message(&mut c, &format!("host:transport:{DEV_B}")).await.unwrap();
    let mut status = [0u8; 4];
    c.read_exact(&mut status).await.unwrap();
    assert_eq!(&status, b"FAIL", "scope must hold on an open share too");

    let mut k = TcpStream::connect(share.config.bind).await.unwrap();
    adb::write_message(&mut k, "host:kill").await.unwrap();
    k.read_exact(&mut status).await.unwrap();
    assert_eq!(&status, b"FAIL", "host:kill must stay blocked on an open share");

    share.stop().await;
}

#[tokio::test]
async fn a_shim_pointed_at_an_open_share_is_told_what_to_do() {
    // Otherwise the shim's preamble is parsed as an adb request and the user gets a bare
    // disconnect with nothing to act on.
    let up = fake_adb("").await;
    let share = start_open_share(&up.addr.to_string()).await;

    let err = client::connect(&share.config.bind.to_string(), Profile::Adb, "any-token-at-all")
        .await
        .unwrap_err();
    let msg = format!("{err:#}");
    assert!(
        msg.contains("needs no token") && msg.contains("adb -H"),
        "message should say to use adb directly: {msg}"
    );

    share.stop().await;
}

/// Real `adb` straight at an open loopback share — the no-shim workflow, end to end.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn real_adb_connects_to_an_open_share_without_a_shim() {
    let Some((adb_bin, serial)) = attached_serial().await else {
        eprintln!("no adb device attached; skipping open-share real-adb test");
        return;
    };
    let share = start_open_share("127.0.0.1:5037").await;
    let port = share.config.bind.port().to_string();

    let out = tokio::process::Command::new(&adb_bin)
        .args(["-H", "127.0.0.1", "-P", &port, "devices"])
        .output()
        .await
        .expect("run adb against the open share");
    let stdout = String::from_utf8_lossy(&out.stdout);
    assert!(
        stdout.contains(&serial),
        "stock adb could not use the open share:\n{stdout}{}",
        String::from_utf8_lossy(&out.stderr)
    );

    share.stop().await;
}
