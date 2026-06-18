//! [`JobIo`] for a real connected phone (M2).
//!
//! Audio steps use the [`AudioEngine`] (same as loopback); call/SMS steps become hub
//! commands. The job runner is synchronous and runs on a blocking task, so async hub
//! calls are bridged via a captured [`tokio::runtime::Handle`].

use std::path::Path;
use std::sync::Arc;
use std::time::Duration;

use tokio::runtime::Handle;

use crate::audio::engine::AudioEngine;
use crate::audio::vad::{EndReason, TurnConfig};
use crate::hub::Hub;
use crate::jobs::runner::JobIo;
use crate::protocol::Action;

/// Drives a connected phone for a job.
pub struct PhoneJobIo {
    hub: Arc<Hub>,
    engine: Arc<AudioEngine>,
    rt: Handle,
    device_id: String,
    dry_audio: bool,
}

impl PhoneJobIo {
    /// Build a phone JobIo. `rt` is the tokio handle used to drive async hub calls from
    /// the blocking job thread.
    pub fn new(
        hub: Arc<Hub>,
        engine: Arc<AudioEngine>,
        rt: Handle,
        device_id: impl Into<String>,
        dry_audio: bool,
    ) -> Self {
        Self {
            hub,
            engine,
            rt,
            device_id: device_id.into(),
            dry_audio,
        }
    }

    fn cmd(&self, action: Action) -> anyhow::Result<()> {
        let hub = self.hub.clone();
        let device = self.device_id.clone();
        self.rt.block_on(hub.command(&device, action))
    }
}

impl JobIo for PhoneJobIo {
    fn play(&mut self, file: &str) -> anyhow::Result<()> {
        if self.dry_audio {
            tracing::info!(file, "audio.play (dry): skipped");
            return Ok(());
        }
        self.engine.play_file(Path::new(file))
    }

    fn wait_for_speech(&mut self, turn: TurnConfig) -> anyhow::Result<EndReason> {
        if self.dry_audio {
            tracing::info!("audio.wait_for_speech (dry): returning Silence immediately");
            return Ok(EndReason::Silence);
        }
        self.engine.wait_for_speech(turn)
    }

    fn dial(&mut self, number: &str) -> anyhow::Result<()> {
        self.cmd(Action::Dial {
            number: number.to_string(),
        })
    }

    fn pickup(&mut self) -> anyhow::Result<()> {
        self.cmd(Action::Pickup { call_id: None })
    }

    fn hangup(&mut self) -> anyhow::Result<()> {
        self.cmd(Action::Hangup { call_id: None })
    }

    fn send_sms(&mut self, to: &str, body: &str) -> anyhow::Result<()> {
        self.cmd(Action::SendSms {
            to: to.to_string(),
            body: body.to_string(),
        })
    }

    fn sleep(&mut self, ms: u64) -> anyhow::Result<()> {
        std::thread::sleep(Duration::from_millis(ms));
        Ok(())
    }

    fn log(&mut self, message: &str) {
        tracing::info!(target: "job", "{message}");
    }
}
