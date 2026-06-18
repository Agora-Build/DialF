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
    /// Run the dialfd daemon (control socket + audio engine; loopback device for M1).
    Daemon {
        /// Simulate audio steps (no sound card / ten-vad needed).
        #[arg(long)]
        dry_audio: bool,
    },
    /// List connected phones.
    Devices,
    /// Place a call: dialf call <device> <number>.
    Call { device: String, number: String },
    /// Answer the ringing call: dialf pickup <device>.
    Pickup { device: String },
    /// Hang up the active call: dialf hangup <device>.
    Hangup { device: String },
    /// Send or list texts.
    Sms {
        #[command(subcommand)]
        action: SmsAction,
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
    },
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

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| tracing_subscriber::EnvFilter::new("info")),
        )
        .init();

    let cli = Cli::parse();
    let config_path = cli.config.clone().unwrap_or_else(Config::default_path);
    let config = Config::load(&config_path)?;
    let socket = config.control_socket.clone();

    match cli.command {
        Command::Daemon { dry_audio } => dialf::daemon::run(config, dry_audio).await,

        Command::Devices => {
            let resp = call(&socket, ControlOp::DevicesList).await?;
            print_response(&resp);
            ok_or_err(resp)
        }
        Command::Call { device, number } => {
            let resp = call(&socket, ControlOp::CallDial { device, number }).await?;
            print_response(&resp);
            ok_or_err(resp)
        }
        Command::Pickup { device } => {
            let resp = call(&socket, ControlOp::CallPickup { device }).await?;
            print_response(&resp);
            ok_or_err(resp)
        }
        Command::Hangup { device } => {
            let resp = call(&socket, ControlOp::CallHangup { device }).await?;
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
        Command::MockPhone {
            server,
            key,
            id,
            name,
            ring,
        } => {
            let key = key.unwrap_or_else(|| config.shared_key.clone());
            mock_phone(server, key, id, name, ring).await
        }
    }
}

/// A minimal phone: connect, hello, optionally ring, then ack every command.
async fn mock_phone(
    server: String,
    key: String,
    id: String,
    name: String,
    ring: Option<String>,
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
    if let Some(data) = &resp.data {
        match serde_json::to_string_pretty(data) {
            Ok(s) => println!("{s}"),
            Err(_) => println!("{data:?}"),
        }
    }
    if let Some(err) = &resp.error {
        eprintln!("error: {err}");
    } else if resp.data.is_none() {
        println!("ok");
    }
}

fn ok_or_err(resp: ControlResponse) -> anyhow::Result<()> {
    if resp.ok == Some(false) {
        anyhow::bail!("{}", resp.error.unwrap_or_else(|| "request failed".into()));
    }
    Ok(())
}
