//! `dialf` — CLI entrypoint.
//!
//! `dialf daemon` runs `dialfd`; the other subcommands are thin clients that send a
//! [`ControlRequest`] over the local Unix control socket and print the response.

use std::path::{Path, PathBuf};

use anyhow::Context;
use clap::{Parser, Subcommand};
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::net::UnixStream;

use dialf::config::Config;
use dialf::protocol::{ControlOp, ControlRequest, ControlResponse};

#[derive(Parser)]
#[command(name = "dialf", version, about = "Drive a phone's calls via dialfd")]
struct Cli {
    #[command(subcommand)]
    command: Command,
}

#[derive(Subcommand)]
enum Command {
    /// Run the dialfd daemon (control socket + audio engine + phone WS plane).
    Daemon {
        /// Path to the config file (defaults to ~/.config/dialf/config.yaml).
        #[arg(long)]
        config: Option<PathBuf>,
    },
    /// List connected phones: dialf devices [--human].
    Devices {
        /// Pretty, human-readable output (one line per phone).
        #[arg(long)]
        human: bool,
    },
    /// Place/answer/hang up calls and read the call log.
    Call {
        #[command(subcommand)]
        action: CallAction,
    },
    /// Send or list texts.
    Sms {
        #[command(subcommand)]
        action: SmsAction,
    },
    /// List the device's active SIMs (slot, number, carrier).
    Sims { device: String },
    /// Turn carrier voicemail (conditional call-forwarding) on/off via MMI codes.
    Voicemail {
        #[command(subcommand)]
        action: VoicemailAction,
    },
    /// (advanced) Send a raw MMI/USSD code to a device: dialf mmi <device> <code> [--sim N].
    Mmi {
        device: String,
        code: String,
        /// SIM subscription id (from `dialf sims`); omit for the default SIM.
        #[arg(long)]
        sim: Option<i32>,
    },
    /// Run a YAML job against a device — or, with `--autoanswer`, serve it for inbound calls.
    Run {
        /// Path to the YAML job file.
        path: PathBuf,
        /// Target device id (defaults to the only connected device).
        #[arg(long)]
        device: Option<String>,
        /// Numbers to auto-answer with this job (comma-separated or repeated). When set,
        /// `dialf run` serves in the foreground: it answers matching inbound calls with this
        /// job (overriding config.yaml) and reverts on exit. Otherwise the job runs once.
        #[arg(long, value_delimiter = ',')]
        autoanswer: Vec<String>,
    },
    /// Play an audio file out the sound card.
    Play { file: PathBuf },
    /// Install/manage dialfd as an OS background service (launchd/systemd).
    Service {
        #[command(subcommand)]
        action: ServiceAction,
        /// Install for the current user (login) instead of system-wide (boot).
        #[arg(long, global = true)]
        user: bool,
    },
}

#[derive(Subcommand)]
enum ServiceAction {
    /// Write + load the service unit (auto-starts; system scope needs sudo).
    Install {
        /// Config file path baked into the service command.
        #[arg(long)]
        config: Option<PathBuf>,
    },
    /// Stop + remove the service unit.
    Uninstall,
    /// Start the installed service.
    Start,
    /// Stop the running service.
    Stop,
    /// Show service status.
    Status,
}

#[derive(Subcommand)]
enum SmsAction {
    /// Send a text: dialf sms send <device> <to> <body>.
    Send {
        device: String,
        to: String,
        body: String,
    },
    /// List recent texts: dialf sms list <device> [--human].
    List {
        device: String,
        /// Pretty, human-readable output (formatted times and numbers).
        #[arg(long)]
        human: bool,
    },
}

#[derive(Subcommand)]
enum VoicemailAction {
    /// Disable voicemail forwarding (dials #004#): dialf voicemail off <device> [--sim N].
    Off {
        device: String,
        /// SIM subscription id (from `dialf sims`); omit for the default SIM.
        #[arg(long)]
        sim: Option<i32>,
    },
    /// Re-enable voicemail forwarding: dialf voicemail on <device> [--number <vm#>] [--sim N].
    /// Without --number, dials *004# (re-activate); some carriers (e.g. T-Mobile) require
    /// --number to re-register the forwarding target as **004*<number>#.
    On {
        device: String,
        /// Voicemail number to forward to (registers via **004*<number>#).
        #[arg(long)]
        number: Option<String>,
        /// SIM subscription id (from `dialf sims`); omit for the default SIM.
        #[arg(long)]
        sim: Option<i32>,
    },
}

#[derive(Subcommand)]
enum CallAction {
    /// Place a call: dialf call dial <device> <number> [--sim <sub_id>].
    Dial {
        device: String,
        number: String,
        /// SIM subscription id to call on (from `dialf sims`); omit for the default SIM.
        #[arg(long)]
        sim: Option<i32>,
    },
    /// Answer the ringing call: dialf call answer <device>.
    Answer { device: String },
    /// Hang up the active call: dialf call hangup <device>.
    Hangup { device: String },
    /// Decline the ringing call: dialf call reject <device> [--drop].
    Reject {
        device: String,
        /// Answer then instantly hang up so the caller can't leave voicemail.
        #[arg(long)]
        drop: bool,
    },
    /// List the recent call log: dialf call list <device> [--human].
    List {
        device: String,
        /// Pretty, human-readable output (formatted times, numbers, durations).
        #[arg(long)]
        human: bool,
    },
}

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| tracing_subscriber::EnvFilter::new("info")),
        )
        .init();

    ensure_model_env();

    // `dialf --version` reports both the CLI and the running daemon (dialfd) version, so a
    // CLI/daemon mismatch after an upgrade is obvious. Handle it before clap so we can query
    // the control socket (clap's built-in version flag is build-time only).
    let argv: Vec<String> = std::env::args().skip(1).collect();
    let wants_version = argv.iter().any(|a| a == "-V" || a == "--version");
    let wants_help = argv.iter().any(|a| a == "-h" || a == "--help");
    if wants_version && !wants_help {
        return print_versions().await;
    }

    let cli = Cli::parse();

    // Client commands connect to the daemon's control socket. Config is a *daemon* concern —
    // only `daemon` and `service install` take `--config` — so clients resolve the socket from the
    // default config, falling back to the built-in default if there's no config file yet.
    let socket = Config::load(&Config::default_path())
        .map(|c| c.control_socket)
        .unwrap_or_else(|_| Config::default().control_socket);

    match cli.command {
        Command::Daemon { config } => {
            // An explicit `--config` that doesn't exist is a user error — fail loudly rather than
            // silently starting with built-in defaults. The implicit default path may legitimately
            // be absent (fresh install) and falls back to defaults.
            let explicit = config.is_some();
            let config_path = config.unwrap_or_else(Config::default_path);
            if explicit && !config_path.exists() {
                anyhow::bail!("config not found: {}", config_path.display());
            }
            let cfg = Config::load(&config_path)?;
            dialf::daemon::run(cfg, config_path).await
        }

        Command::Devices { human } => {
            let resp = call(&socket, ControlOp::DevicesList).await?;
            if human && resp.ok != Some(false) {
                match resp.data.as_ref().and_then(|v| v.as_array()) {
                    Some(rows) if !rows.is_empty() => rows.iter().for_each(human_device),
                    _ => println!("(no phones connected)"),
                }
            } else {
                print_response(&resp);
            }
            ok_or_err(resp)
        }
        Command::Call { action } => {
            let mut human = false;
            let op = match action {
                CallAction::Dial { device, number, sim } => ControlOp::CallDial {
                    device,
                    number,
                    sim_sub_id: sim,
                },
                CallAction::Answer { device } => ControlOp::CallAnswer { device },
                CallAction::Hangup { device } => ControlOp::CallHangup { device },
                CallAction::Reject { device, drop } => ControlOp::CallReject { device, drop },
                CallAction::List { device, human: h } => {
                    human = h;
                    ControlOp::CallList { device }
                }
            };
            let resp = call(&socket, op).await?;
            print_list(&resp, human, "calls", human_calls);
            ok_or_err(resp)
        }
        Command::Sms { action } => {
            let mut human = false;
            let op = match action {
                SmsAction::Send { device, to, body } => ControlOp::SmsSend { device, to, body },
                SmsAction::List { device, human: h } => {
                    human = h;
                    ControlOp::SmsList { device }
                }
            };
            let resp = call(&socket, op).await?;
            print_list(&resp, human, "messages", human_sms);
            ok_or_err(resp)
        }
        Command::Sims { device } => {
            let resp = call(&socket, ControlOp::SimsList { device }).await?;
            print_response(&resp);
            ok_or_err(resp)
        }
        Command::Voicemail { action } => {
            // Express intent only; the device maps it to its platform mechanism.
            let (device, enabled, number, sim) = match action {
                VoicemailAction::Off { device, sim } => (device, false, None, sim),
                VoicemailAction::On {
                    device,
                    number,
                    sim,
                } => (device, true, number, sim),
            };
            if !enabled {
                eprintln!(
                    "note: voicemail off may not work with AT&T or T-Mobile (network-level \
                     voicemail) — you may need to call your carrier's customer service. \
                     To keep callers out of voicemail regardless, use: dialf call reject --drop"
                );
            }
            let resp = call(
                &socket,
                ControlOp::VoicemailSet {
                    device,
                    enabled,
                    number,
                    sim_sub_id: sim,
                },
            )
            .await?;
            print_response(&resp);
            ok_or_err(resp)
        }
        Command::Mmi { device, code, sim } => {
            let resp = call(
                &socket,
                ControlOp::Mmi {
                    device,
                    code,
                    sim_sub_id: sim,
                },
            )
            .await?;
            print_response(&resp);
            ok_or_err(resp)
        }
        Command::Run {
            path,
            device,
            autoanswer,
        } => {
            // dialfd reads the job file from *its* working directory (often a service with
            // cwd=/), so resolve the path against the CLI's cwd before sending it.
            let abs = std::fs::canonicalize(&path)
                .with_context(|| format!("job file not found: {}", path.display()))?
                .to_string_lossy()
                .to_string();
            if autoanswer.is_empty() {
                let op = ControlOp::JobRun {
                    path: Some(abs),
                    steps: None,
                    device,
                };
                let resp = call_run(&socket, op).await?;
                print_response(&resp);
                if job_ended_on_hangup(&resp) {
                    println!(
                        "\ncaller hung up — ran once, exiting. Use `--autoanswer <numbers>` to keep answering calls."
                    );
                }
                ok_or_err(resp)
            } else {
                serve_autoanswer(&socket, autoanswer, abs, device).await
            }
        }
        Command::Play { file } => {
            // Resolve against the CLI's cwd — dialfd opens the file in its own dir.
            let abs = std::fs::canonicalize(&file)
                .with_context(|| format!("audio file not found: {}", file.display()))?;
            let op = ControlOp::AudioPlay {
                file: abs.to_string_lossy().to_string(),
                device: None,
            };
            let resp = call(&socket, op).await?;
            print_response(&resp);
            ok_or_err(resp)
        }
        Command::Service { action, user } => {
            let scope = if user {
                dialf::service::Scope::User
            } else {
                dialf::service::Scope::System
            };
            let (act, config) = match action {
                ServiceAction::Install { config } => (dialf::service::Action::Install, config),
                ServiceAction::Uninstall => (dialf::service::Action::Uninstall, None),
                ServiceAction::Start => (dialf::service::Action::Start, None),
                ServiceAction::Stop => (dialf::service::Action::Stop, None),
                ServiceAction::Status => (dialf::service::Action::Status, None),
            };
            dialf::service::run(act, scope, config)
        }
    }
}

/// For prebuilt distributions: if `TEN_VAD_MODEL` is unset and a `ten-vad.onnx` sits next
/// to the executable (bundled in the release tarball), point ten-vad at it. Source builds
/// leave it unset and use the path baked in at compile time.
fn ensure_model_env() {
    if std::env::var_os("TEN_VAD_MODEL").is_some() {
        return;
    }
    if let Ok(exe) = std::env::current_exe() {
        // Resolve symlinks: the service runs the daemon via a stable symlink
        // (~/.local/bin/dialfd), but the bundled model lives next to the *real* binary.
        let exe = std::fs::canonicalize(&exe).unwrap_or(exe);
        if let Some(dir) = exe.parent() {
            let model = dir.join("ten-vad.onnx");
            if model.exists() {
                std::env::set_var("TEN_VAD_MODEL", model);
            }
        }
    }
}

/// Print the CLI version, and the running daemon's version if it's reachable.
/// Parse a `MAJOR.MINOR.PATCH` version (ignoring any pre-release/build suffix) for comparison.
fn parse_ver(s: &str) -> Option<(u64, u64, u64)> {
    let core = s.split(['-', '+']).next().unwrap_or(s);
    let mut it = core.split('.');
    let major = it.next()?.parse().ok()?;
    let minor = it.next().map_or(Ok(0), str::parse).ok()?;
    let patch = it.next().map_or(Ok(0), str::parse).ok()?;
    Some((major, minor, patch))
}

/// How the running daemon's version relates to this CLI's.
#[derive(Debug, PartialEq, Eq)]
enum VerRel {
    Older,
    Newer,
    Same,
    /// Either side didn't parse as a version — say nothing about which is ahead.
    Unknown,
}

fn ver_rel(daemon: &str, cli: &str) -> VerRel {
    match (parse_ver(daemon), parse_ver(cli)) {
        (Some(d), Some(c)) if d < c => VerRel::Older,
        (Some(d), Some(c)) if d > c => VerRel::Newer,
        (Some(_), Some(_)) => VerRel::Same,
        _ => VerRel::Unknown,
    }
}

async fn print_versions() -> anyhow::Result<()> {
    let cli = env!("CARGO_PKG_VERSION");
    println!("dialf  (CLI):    {cli}");
    // Resolve the control socket from the default config (config is a daemon concern; clients
    // don't take `--config`).
    let socket = Config::load(&Config::default_path())
        .map(|c| c.control_socket)
        .unwrap_or_else(|_| Config::default().control_socket);
    // Cap the query so `--version` can never hang on an unresponsive daemon.
    let q = tokio::time::timeout(
        std::time::Duration::from_millis(1500),
        call(&socket, ControlOp::ServerInfo),
    )
    .await;
    match q {
        Ok(Ok(resp)) => match resp
            .data
            .as_ref()
            .and_then(|d| d.get("version"))
            .and_then(|v| v.as_str())
        {
            Some(v) => match ver_rel(v, cli) {
                VerRel::Older => {
                    println!("dialfd (daemon): {v}  (older than the CLI {cli})");
                    println!(
                        "  ↳ update the service: `dialf service install` (add `--user` if you installed it per-user)"
                    );
                }
                VerRel::Newer => {
                    println!("dialfd (daemon): {v}  (newer than the CLI {cli})");
                    println!("  ↳ update the CLI: `npm i -g @agora-build/dialf`");
                }
                VerRel::Same => println!("dialfd (daemon): {v}  (up to date)"),
                VerRel::Unknown => println!("dialfd (daemon): {v}"),
            },
            // Reachable but doesn't report a version => a daemon older than `server.info`.
            None => println!(
                "dialfd (daemon): running, older than the CLI — re-run `dialf service install` (add `--user` if you installed it per-user) to update"
            ),
        },
        Ok(Err(_)) => println!("dialfd (daemon): not running"),
        Err(_) => println!("dialfd (daemon): not responding"),
    }
    Ok(())
}

/// Foreground serve: register an auto-answer override (this job for these numbers), stream the
/// daemon's event lines, and revert on exit. Ctrl-C (or the daemon closing) ends the session —
/// the held connection drops, so the daemon removes the override. A call already in progress
/// finishes its script on the daemon (the job runs there, not in this CLI).
async fn serve_autoanswer(
    socket: &Path,
    numbers: Vec<String>,
    path: String,
    device: Option<String>,
) -> anyhow::Result<()> {
    let mut stream = UnixStream::connect(socket).await.with_context(|| {
        format!(
            "connect control socket {} — is dialfd running? (dialf daemon)",
            socket.display()
        )
    })?;
    let req = ControlRequest {
        id: "1".to_string(),
        op: ControlOp::AutoanswerServe {
            numbers,
            path,
            device,
        },
    };
    let mut line = serde_json::to_string(&req)?;
    line.push('\n');
    stream.write_all(line.as_bytes()).await?;
    stream.flush().await?;

    let (read, _write) = stream.into_split();
    let mut lines = BufReader::new(read).lines();
    println!("(Ctrl-C to stop; an in-progress call finishes on the daemon)");
    loop {
        tokio::select! {
            _ = tokio::signal::ctrl_c() => {
                println!("\n--autoanswer stopped serving — reverted to config.yaml");
                return Ok(());
            }
            next = lines.next_line() => match next? {
                Some(l) => {
                    let resp: ControlResponse = match serde_json::from_str(&l) {
                        Ok(r) => r,
                        Err(_) => continue,
                    };
                    if resp.ok == Some(false) {
                        anyhow::bail!(resp.error.unwrap_or_else(|| "serve rejected".to_string()));
                    }
                    if let Some(ev) = resp
                        .data
                        .as_ref()
                        .and_then(|d| d.get("event"))
                        .and_then(|v| v.as_str())
                    {
                        println!("{ev}");
                    }
                }
                None => {
                    println!("daemon closed the connection");
                    return Ok(());
                }
            },
        }
    }
}

/// Send one control request and read one response line.
async fn call(socket: &Path, op: ControlOp) -> anyhow::Result<ControlResponse> {
    let mut stream = UnixStream::connect(socket).await.with_context(|| {
        format!(
            "connect control socket {} — is dialfd running? (dialf daemon)",
            socket.display()
        )
    })?;
    let req = ControlRequest {
        id: "1".to_string(),
        op,
    };
    let mut line = serde_json::to_string(&req)?;
    line.push('\n');
    stream.write_all(line.as_bytes()).await?;
    stream.flush().await?;

    let (read, _write) = stream.into_split();
    let mut lines = BufReader::new(read).lines();
    let resp_line = lines
        .next_line()
        .await?
        .context("no response from dialfd")?;
    let resp: ControlResponse = serde_json::from_str(&resp_line)?;
    Ok(resp)
}

/// Send a `job.run` and await the result, but on Ctrl+C send `job.cancel` so the daemon actually
/// stops the job — otherwise the CLI just detaches while the daemon runs the job to completion
/// (holding the card, waiting out `wait_for_speech` timeouts).
///
/// Two-level: the first Ctrl+C is a *graceful* cancel (stop at the next step boundary, interrupt a
/// long `wait_for_speech`, but let a `play`/`wait` finish). The second Ctrl+C is a *force* cancel
/// (`force: true`) — the daemon also cuts the current `play`/`wait` short. Either way the daemon
/// finalizes the recording, so the audio files are saved. We keep awaiting the daemon's response
/// so the run's outcome (and recording paths) still print; a third Ctrl+C hard-exits the client.
async fn call_run(socket: &Path, op: ControlOp) -> anyhow::Result<ControlResponse> {
    let fut = call(socket, op);
    tokio::pin!(fut);
    let mut ctrlc_count = 0u8;
    loop {
        tokio::select! {
            resp = &mut fut => return resp,
            _ = tokio::signal::ctrl_c() => {
                ctrlc_count += 1;
                match ctrlc_count {
                    1 => {
                        eprintln!("\ncancelling job… (Ctrl+C again to force-stop the current step)");
                        let _ = call(socket, ControlOp::JobCancel { force: false }).await;
                    }
                    2 => {
                        eprintln!("force-stopping the current step… (audio files are still saved)");
                        let _ = call(socket, ControlOp::JobCancel { force: true }).await;
                    }
                    _ => {
                        eprintln!("quit.");
                        std::process::exit(130);
                    }
                }
            }
        }
    }
}

/// Print a list response: human-formatted when `human`, else the raw JSON. `key` is the
/// array field in the response data; `fmt` renders one row.
fn print_list(resp: &ControlResponse, human: bool, key: &str, fmt: fn(&serde_json::Value)) {
    if human && resp.ok != Some(false) {
        match resp.data.as_ref().and_then(|d| d.get(key)).and_then(|v| v.as_array()) {
            Some(rows) if !rows.is_empty() => rows.iter().for_each(fmt),
            _ => println!("(none)"),
        }
    } else {
        print_response(resp);
    }
}

fn human_calls(c: &serde_json::Value) {
    let ts = c.get("ts").and_then(|v| v.as_i64()).unwrap_or(0);
    let kind = c.get("kind").and_then(|v| v.as_str()).unwrap_or("?");
    let num = c.get("number").and_then(|v| v.as_str()).unwrap_or("(unknown)");
    let dur = c.get("duration").and_then(|v| v.as_i64()).unwrap_or(0);
    println!(
        "{}  {:<9} {:<17} {}",
        fmt_ts(ts),
        kind,
        fmt_number(num),
        fmt_duration(dur)
    );
}

fn human_device(d: &serde_json::Value) {
    let id = d.get("id").and_then(|v| v.as_str()).unwrap_or("?");
    let name = d.get("name").and_then(|v| v.as_str()).unwrap_or("");
    let addr = d.get("addr").and_then(|v| v.as_str()).unwrap_or("-");
    let seen = d.get("last_seen_ms").and_then(|v| v.as_i64()).unwrap_or(0);
    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|x| x.as_millis() as i64)
        .unwrap_or(seen);
    let ago = ((now - seen).max(0)) / 1000;
    let call = match d.get("current_call") {
        Some(c) if !c.is_null() => {
            let num = c.get("number").and_then(|v| v.as_str()).unwrap_or("(unknown)");
            let st = c.get("state").and_then(|v| v.as_str()).unwrap_or("?");
            format!("in call {} ({st})", fmt_number(num))
        }
        _ => "idle".to_string(),
    };
    println!("{id:<20} {name:<16} {addr:<16} seen {ago}s ago   {call}");
}

fn human_sms(m: &serde_json::Value) {
    let ts = m.get("ts").and_then(|v| v.as_i64()).unwrap_or(0);
    let dir = m.get("direction").and_then(|v| v.as_str()).unwrap_or("in");
    let (arrow, who) = if dir == "out" {
        ("->", m.get("to").and_then(|v| v.as_str()))
    } else {
        ("<-", m.get("from").and_then(|v| v.as_str()))
    };
    let body = m.get("body").and_then(|v| v.as_str()).unwrap_or("");
    println!(
        "{}  {} {:<17} {}",
        fmt_ts(ts),
        arrow,
        fmt_number(who.unwrap_or("(unknown)")),
        body.replace('\n', " ")
    );
}

/// Epoch milliseconds -> local "YYYY-MM-DD HH:MM:SS".
fn fmt_ts(ms: i64) -> String {
    use chrono::{Local, TimeZone};
    match Local.timestamp_millis_opt(ms).single() {
        Some(dt) => dt.format("%Y-%m-%d %H:%M:%S").to_string(),
        None => ms.to_string(),
    }
}

/// Seconds -> largest sensible unit: <1m seconds, <1h minutes, <1d hours, else days.
fn fmt_duration(secs: i64) -> String {
    if secs < 60 {
        format!("{secs}s")
    } else if secs < 3600 {
        format!("{}m", secs / 60)
    } else if secs < 86400 {
        format!("{}h", secs / 3600)
    } else {
        format!("{}d", secs / 86400)
    }
}

/// Format a phone number with high confidence using libphonenumber metadata: valid numbers
/// are rendered in international format (country code + grouped national number); numbers
/// that don't parse to a valid number (short codes, alphanumeric senders) pass through.
/// A bare number with no country code is interpreted as US (NANP).
fn fmt_number(s: &str) -> String {
    use phonenumber::{country, Mode};
    match phonenumber::parse(Some(country::US), s) {
        Ok(n) if phonenumber::is_valid(&n) => n.format().mode(Mode::International).to_string(),
        _ => s.to_string(),
    }
}

fn print_response(resp: &ControlResponse) {
    // Errors are surfaced once, by `ok_or_err` returning `Err` (the runtime prints them).
    if resp.ok == Some(false) {
        return;
    }
    if let Some(data) = &resp.data {
        match serde_json::to_string_pretty(data) {
            Ok(s) => println!("{s}"),
            Err(_) => println!("{data:?}"),
        }
    } else {
        println!("ok");
    }
}

/// True if a `job.run` response stopped early because the far end hung up (the runner records a
/// marker outcome). Used to print a one-shot "exiting" hint.
fn job_ended_on_hangup(resp: &ControlResponse) -> bool {
    resp.data
        .as_ref()
        .and_then(|d| d.get("steps"))
        .and_then(|s| s.as_array())
        .is_some_and(|steps| {
            steps.iter().any(|o| {
                o.get("summary").and_then(|v| v.as_str())
                    == Some(dialf::jobs::runner::CALL_ENDED_SUMMARY)
            })
        })
}

fn ok_or_err(resp: ControlResponse) -> anyhow::Result<()> {
    if resp.ok == Some(false) {
        anyhow::bail!("{}", resp.error.unwrap_or_else(|| "request failed".into()));
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::{fmt_duration, fmt_number, ver_rel, VerRel};

    #[test]
    fn version_relation() {
        // The `dialf --version` upgrade prompt hinges on these comparisons.
        assert_eq!(ver_rel("0.1.19", "0.1.20"), VerRel::Older); // daemon behind → reinstall service
        assert_eq!(ver_rel("0.1.20", "0.1.20"), VerRel::Same);
        assert_eq!(ver_rel("0.2.0", "0.1.20"), VerRel::Newer); // daemon ahead → upgrade CLI
        assert_eq!(ver_rel("1.0.0", "0.9.9"), VerRel::Newer);
        // Pre-release/build suffixes are ignored; the core compares.
        assert_eq!(ver_rel("0.1.20-dev", "0.1.20"), VerRel::Same);
        // Unparseable either side → Unknown (no claim about which is ahead).
        assert_eq!(ver_rel("unknown", "0.1.20"), VerRel::Unknown);
    }

    #[test]
    fn duration_units() {
        assert_eq!(fmt_duration(7), "7s");
        assert_eq!(fmt_duration(59), "59s");
        assert_eq!(fmt_duration(60), "1m");
        assert_eq!(fmt_duration(3599), "59m");
        assert_eq!(fmt_duration(3600), "1h");
        assert_eq!(fmt_duration(86399), "23h");
        assert_eq!(fmt_duration(86400), "1d");
        assert_eq!(fmt_duration(200000), "2d");
    }

    #[test]
    fn number_format() {
        // Fake but library-valid numbers -> international format (high-confidence).
        assert!(fmt_number("+12015550123").starts_with("+1 ")); // US
        assert!(fmt_number("+8613912345678").starts_with("+86 ")); // CN
        assert!(fmt_number("+525512345678").starts_with("+52 ")); // MX
        // A bare US number gets the +1 country code added.
        assert!(fmt_number("2015550123").starts_with("+1 "));
        // Short codes / non-numbers aren't valid phone numbers -> passed through unchanged.
        assert_eq!(fmt_number("123"), "123");
        assert_eq!(fmt_number("3002"), "3002");
    }
}
