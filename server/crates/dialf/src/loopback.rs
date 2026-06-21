//! In-process loopback phone for hardware-free M1 runs.
//!
//! Implements [`JobIo`] by driving the [`AudioEngine`] for audio steps and updating the
//! [`Registry`]'s call state for call/SMS steps. With `dry_audio = true`, audio steps are
//! logged and skipped — useful before a sound card / the ten-vad native lib are present.
//! M2 replaces this with a real phone over WebSocket.

use std::path::Path;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use crate::audio::engine::AudioEngine;
use crate::audio::record::{RecordOutput, Recorder};
use crate::audio::vad::{EndReason, TurnConfig};
use crate::jobs::runner::JobIo;
use crate::protocol::{CallState, Direction};
use crate::registry::{CallInfo, Registry};

static CALL_SEQ: AtomicU64 = AtomicU64::new(1);

/// Default id of the loopback device.
pub const LOOPBACK_ID: &str = "loopback";

/// A [`JobIo`] backed by the loopback phone + the real audio engine.
pub struct LoopbackJobIo {
    engine: Arc<AudioEngine>,
    registry: Arc<Mutex<Registry>>,
    device_id: String,
    /// When true, audio steps are logged and skipped instead of touching the card.
    dry_audio: bool,
    /// When set, audio is recorded (rx/tx legs + optional mix).
    recorder: Option<Recorder>,
}

impl LoopbackJobIo {
    /// Build a loopback JobIo for `device_id`.
    pub fn new(
        engine: Arc<AudioEngine>,
        registry: Arc<Mutex<Registry>>,
        device_id: impl Into<String>,
        dry_audio: bool,
        recorder: Option<Recorder>,
    ) -> Self {
        Self {
            engine,
            registry,
            device_id: device_id.into(),
            dry_audio,
            recorder,
        }
    }

    /// Finalize any recording, returning the written file paths.
    pub fn finish(self) -> anyhow::Result<Option<RecordOutput>> {
        match self.recorder {
            Some(r) => Ok(Some(r.finish()?)),
            None => Ok(None),
        }
    }

    fn set_call(&self, call: Option<CallInfo>) {
        if let Ok(mut reg) = self.registry.lock() {
            if let Some(dev) = reg.get_mut(&self.device_id) {
                dev.current_call = call;
            }
        }
    }

    fn current_call_id(&self) -> Option<String> {
        self.registry
            .lock()
            .ok()
            .and_then(|reg| reg.get(&self.device_id).and_then(|d| d.current_call.as_ref().map(|c| c.call_id.clone())))
    }
}

fn next_call_id() -> String {
    format!("lc-{}", CALL_SEQ.fetch_add(1, Ordering::Relaxed))
}

impl JobIo for LoopbackJobIo {
    fn play(&mut self, file: &str) -> anyhow::Result<()> {
        if self.dry_audio {
            tracing::info!(file, "audio.play (dry): skipped");
            return Ok(());
        }
        self.engine.play_file(Path::new(file), self.recorder.as_mut())
    }

    fn wait_for_speech(&mut self, turn: TurnConfig) -> anyhow::Result<EndReason> {
        if self.dry_audio {
            tracing::info!(
                silence_ms = turn.silence_duration_ms,
                timeout_ms = turn.end_timeout_ms,
                "audio.wait_for_speech (dry): returning Silence immediately"
            );
            return Ok(EndReason::Silence);
        }
        self.engine.wait_for_speech(turn, self.recorder.as_mut())
    }

    fn dial(&mut self, number: &str) -> anyhow::Result<()> {
        let call = CallInfo {
            call_id: next_call_id(),
            number: Some(number.to_string()),
            state: CallState::Active,
            direction: Direction::Out,
        };
        tracing::info!(device = %self.device_id, %number, call_id = %call.call_id, "loopback: dial");
        self.set_call(Some(call));
        Ok(())
    }

    fn pickup(&mut self) -> anyhow::Result<()> {
        // If a call is already tracked, mark active; else synthesize an inbound one.
        let call = match self.current_call_id() {
            Some(call_id) => CallInfo {
                call_id,
                number: None,
                state: CallState::Active,
                direction: Direction::In,
            },
            None => CallInfo {
                call_id: next_call_id(),
                number: None,
                state: CallState::Active,
                direction: Direction::In,
            },
        };
        tracing::info!(device = %self.device_id, call_id = %call.call_id, "loopback: pickup");
        self.set_call(Some(call));
        Ok(())
    }

    fn hangup(&mut self) -> anyhow::Result<()> {
        tracing::info!(device = %self.device_id, "loopback: hangup");
        self.set_call(None);
        Ok(())
    }

    fn send_sms(&mut self, to: &str, body: &str) -> anyhow::Result<()> {
        tracing::info!(device = %self.device_id, %to, len = body.len(), "loopback: send_sms");
        Ok(())
    }

    fn sleep(&mut self, ms: u64) -> anyhow::Result<()> {
        std::thread::sleep(Duration::from_millis(ms));
        Ok(())
    }

    fn log(&mut self, message: &str) {
        tracing::info!(target: "job", "{message}");
    }
}
