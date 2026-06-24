//! [`JobIo`] for a real connected phone.
//!
//! Audio steps use the [`AudioEngine`] (same as loopback); call/SMS steps become hub
//! commands. The job runner is synchronous and runs on a blocking task, so async hub
//! calls are bridged via a captured [`tokio::runtime::Handle`].

use std::path::Path;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use tokio::runtime::Handle;

use crate::audio::engine::AudioEngine;
use crate::audio::record::{DuplexSession, RecordOutput};
use crate::audio::vad::{EndReason, TurnConfig};
use crate::hub::Hub;
use crate::jobs::runner::JobIo;
use crate::protocol::{Action, CallState};
use crate::registry::Registry;

/// Drives a connected phone for a job.
pub struct PhoneJobIo {
    hub: Arc<Hub>,
    engine: Arc<AudioEngine>,
    rt: Handle,
    registry: Arc<Mutex<Registry>>,
    device_id: String,
    dry_audio: bool,
    session: Option<DuplexSession>,
}

impl PhoneJobIo {
    /// Build a phone JobIo. `rt` is the tokio handle used to drive async hub calls from
    /// the blocking job thread.
    #[allow(clippy::too_many_arguments)]
    pub fn new(
        hub: Arc<Hub>,
        engine: Arc<AudioEngine>,
        rt: Handle,
        registry: Arc<Mutex<Registry>>,
        device_id: impl Into<String>,
        dry_audio: bool,
        session: Option<DuplexSession>,
    ) -> Self {
        Self {
            hub,
            engine,
            rt,
            registry,
            device_id: device_id.into(),
            dry_audio,
            session,
        }
    }

    /// Finalize any recording, returning the written file paths.
    pub fn finish(self) -> anyhow::Result<Option<RecordOutput>> {
        match self.session {
            Some(s) => Ok(Some(s.finish()?)),
            None => Ok(None),
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
        self.engine.play_file(Path::new(file), self.session.as_mut())
    }

    fn wait_for_speech(&mut self, turn: TurnConfig) -> anyhow::Result<EndReason> {
        if self.dry_audio {
            tracing::info!("audio.wait_for_speech (dry): returning Silence immediately");
            return Ok(EndReason::Silence);
        }
        self.engine.wait_for_speech(turn, self.session.as_mut())
    }

    fn dial(&mut self, number: &str) -> anyhow::Result<()> {
        self.cmd(Action::Dial {
            number: number.to_string(),
            sim_sub_id: None, // job-driven dials use the default SIM
        })
    }

    fn wait_for_answer(&mut self, timeout_ms: u64) -> anyhow::Result<()> {
        // Poll the registry (updated by the phone's call_state frames) until the call is
        // active. dialing/ringing -> keep waiting; ended after it appeared -> the callee
        // never answered; timeout -> give up.
        let deadline = Instant::now() + Duration::from_millis(timeout_ms);
        let mut seen = false;
        loop {
            let state = self
                .registry
                .lock()
                .unwrap()
                .get(&self.device_id)
                .and_then(|d| d.current_call.as_ref().map(|c| c.state));
            match state {
                Some(CallState::Active) => return Ok(()),
                Some(_) => seen = true, // dialing / ringing
                None if seen => anyhow::bail!("call ended before it was answered"),
                None => {} // not placed yet — keep waiting
            }
            if Instant::now() >= deadline {
                anyhow::bail!("call not answered within {timeout_ms}ms");
            }
            std::thread::sleep(Duration::from_millis(150));
        }
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
