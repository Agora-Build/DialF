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
