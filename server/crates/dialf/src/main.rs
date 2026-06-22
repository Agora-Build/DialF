//! `dialf` — CLI entrypoint.
//!
//! `dialf daemon` runs `dialfd`; the other subcommands are thin clients that send a
//! [`ControlRequest`] over the local Unix control socket and print the response.

use std::path::{Path, PathBuf};

use anyhow::Context;
use clap::{Parser, Subcommand};
use futures_util::{SinkExt, StreamExt};
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::net::UnixStream;
use tokio_tungstenite::tungstenite::Message;

use dialf::config::Config;
use dialf::protocol::{
    CallState, ControlOp, ControlRequest, ControlResponse, Direction, PhoneToServer, ServerToPhone,
};

#[derive(Parser)]
#[command(name = "dialf", version, about = "Drive a phone's calls via dialfd")]
struct Cli {
    /// Path to the config file (defaults to ~/.config/dialf/config.yaml).
    #[arg(long, global = true)]
    config: Option<PathBuf>,

    #[command(subcommand)]
    command: Command,
}

#[derive(Subcommand)]
enum Command {
    /// Run the dialfd daemon (control socket + audio engine + phone WS plane).
    Daemon {
        /// Simulate audio steps (no sound card / ten-vad needed).
        #[arg(long)]
        dry_audio: bool,
        /// Also register the in-process simulated phone (off by default; real phones only).
        #[arg(long)]
        with_loopback: bool,
    },
    /// List connected phones.
    Devices,
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
    /// Run a YAML job against a device.
    Run {
        /// Path to the YAML job file.
        path: PathBuf,
        /// Target device id (defaults to the only connected device).
        #[arg(long)]
        device: Option<String>,
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
    /// (testing) Run a mock phone that connects to dialfd and acks commands.
    #[command(hide = true)]
    MockPhone {
        /// dialfd phone WS address (host:port).
        #[arg(long, default_value = "127.0.0.1:8765")]
        server: String,
        /// Shared key (defaults to the config's shared_key).
        #[arg(long)]
        key: Option<String>,
        /// Device id to register as.
        #[arg(long, default_value = "mock-phone")]
        id: String,
        /// Friendly device name.
        #[arg(long, default_value = "Mock Phone")]
        name: String,
        /// Emit an inbound ringing call from this number on connect (tests auto-pickup).
        #[arg(long)]
        ring: Option<String>,
        /// Emit an inbound SMS on connect, formatted "from:body" (tests receive).
        #[arg(long)]
        incoming_sms: Option<String>,
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
    /// List recent texts: dialf sms list <device>.
    List { device: String },
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
    /// Answer the ringing call: dialf call pickup <device>.
    Pickup { device: String },
    /// Hang up the active call: dialf call hangup <device>.
    Hangup { device: String },
    /// Decline the ringing call: dialf call reject <device>.
    Reject { device: String },
    /// List the recent call log: dialf call list <device>.
    List { device: String },
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

    let cli = Cli::parse();
    let config_path = cli.config.clone().unwrap_or_else(Config::default_path);
    let config = Config::load(&config_path)?;
    let socket = config.control_socket.clone();

    match cli.command {
        Command::Daemon {
            dry_audio,
            with_loopback,
        } => dialf::daemon::run(config, dry_audio, with_loopback).await,

        Command::Devices => {
            let resp = call(&socket, ControlOp::DevicesList).await?;
            print_response(&resp);
            ok_or_err(resp)
        }
        Command::Call { action } => {
            let op = match action {
                CallAction::Dial { device, number, sim } => ControlOp::CallDial {
                    device,
                    number,
                    sim_sub_id: sim,
                },
                CallAction::Pickup { device } => ControlOp::CallPickup { device },
                CallAction::Hangup { device } => ControlOp::CallHangup { device },
                CallAction::Reject { device } => ControlOp::CallReject { device },
                CallAction::List { device } => ControlOp::CallList { device },
            };
            let resp = call(&socket, op).await?;
            print_response(&resp);
            ok_or_err(resp)
        }
        Command::Sms { action } => {
            let op = match action {
                SmsAction::Send { device, to, body } => ControlOp::SmsSend { device, to, body },
                SmsAction::List { device } => ControlOp::SmsList { device },
            };
            let resp = call(&socket, op).await?;
            print_response(&resp);
            ok_or_err(resp)
        }
        Command::Sims { device } => {
            let resp = call(&socket, ControlOp::SimsList { device }).await?;
            print_response(&resp);
            ok_or_err(resp)
        }
        Command::Voicemail { action } => {
            // GSM supplementary-service codes for all conditional call-forwarding:
            //   #004#  deactivate (caller no longer forwarded to voicemail)
            //   *004#  reactivate
            let (device, sim, code) = match action {
                VoicemailAction::Off { device, sim } => (device, sim, "#004#".to_string()),
                VoicemailAction::On {
                    device,
                    number: Some(n),
                    sim,
                } => (device, sim, format!("**004*{n}#")),
                VoicemailAction::On {
                    device,
                    number: None,
                    sim,
                } => (device, sim, "*004#".to_string()),
            };
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
        Command::Run { path, device } => {
            let op = ControlOp::JobRun {
                path: Some(path.to_string_lossy().to_string()),
                steps: None,
                device,
            };
            let resp = call(&socket, op).await?;
            print_response(&resp);
            ok_or_err(resp)
        }
        Command::Play { file } => {
            let op = ControlOp::AudioPlay {
                file: file.to_string_lossy().to_string(),
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
        Command::MockPhone {
            server,
            key,
            id,
            name,
            ring,
            incoming_sms,
        } => {
            let key = key.unwrap_or_else(|| config.shared_key.clone());
            mock_phone(server, key, id, name, ring, incoming_sms).await
        }
    }
}

/// A minimal phone: connect, hello, optionally ring / deliver an SMS, then ack commands.
async fn mock_phone(
    server: String,
    key: String,
    id: String,
    name: String,
    ring: Option<String>,
    incoming_sms: Option<String>,
) -> anyhow::Result<()> {
    let url = format!("ws://{server}");
    let (ws, _) = tokio_tungstenite::connect_async(&url)
        .await
        .with_context(|| format!("connect {url}"))?;
    let (mut sink, mut stream) = ws.split();

    let hello = PhoneToServer::Hello {
        device_id: id.clone(),
        name,
        key,
        caps: vec!["call".into(), "sms".into()],
        app_version: Some("mock".into()),
    };
    sink.send(Message::Text(serde_json::to_string(&hello)?.into()))
        .await?;
    println!("[mock {id}] connected to {server}");

    if let Some(number) = ring {
        let cs = PhoneToServer::CallState {
            call_id: "ring-1".into(),
            state: CallState::Ringing,
            number: Some(number.clone()),
            direction: Direction::In,
        };
        sink.send(Message::Text(serde_json::to_string(&cs)?.into()))
            .await?;
        println!("[mock {id}] ringing from {number}");
    }

    if let Some(s) = incoming_sms {
        let (from, body) = s.split_once(':').unwrap_or(("unknown", s.as_str()));
        let sms = PhoneToServer::Sms {
            direction: Direction::In,
            from: Some(from.to_string()),
            to: None,
            body: body.to_string(),
            ts: 0,
        };
        sink.send(Message::Text(serde_json::to_string(&sms)?.into()))
            .await?;
        println!("[mock {id}] incoming sms from {from}");
    }

    while let Some(msg) = stream.next().await {
        let msg = msg?;
        if msg.is_close() {
            break;
        }
        let Ok(text) = msg.to_text() else { continue };
        if text.is_empty() {
            continue;
        }
        if let Ok(ServerToPhone::Cmd { cmd_id, action }) = serde_json::from_str(text) {
            println!("[mock {id}] cmd: {action:?}");
            let ack = PhoneToServer::Ack { cmd_id, ok: true };
            sink.send(Message::Text(serde_json::to_string(&ack)?.into()))
                .await?;
        }
    }
    println!("[mock {id}] disconnected");
    Ok(())
}

/// For prebuilt distributions: if `TEN_VAD_MODEL` is unset and a `ten-vad.onnx` sits next
/// to the executable (bundled in the release tarball), point ten-vad at it. Source builds
/// leave it unset and use the path baked in at compile time.
fn ensure_model_env() {
    if std::env::var_os("TEN_VAD_MODEL").is_some() {
        return;
    }
    if let Ok(exe) = std::env::current_exe() {
        if let Some(dir) = exe.parent() {
            let model = dir.join("ten-vad.onnx");
            if model.exists() {
                std::env::set_var("TEN_VAD_MODEL", model);
            }
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

fn ok_or_err(resp: ControlResponse) -> anyhow::Result<()> {
    if resp.ok == Some(false) {
        anyhow::bail!("{}", resp.error.unwrap_or_else(|| "request failed".into()));
    }
    Ok(())
}
