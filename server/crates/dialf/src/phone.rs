//! [`JobIo`] for a real connected phone.
//!
//! Audio steps use the [`AudioEngine`]; call/SMS steps become hub commands. The job runner
//! is synchronous and runs on a blocking task, so async hub calls are bridged via a captured
//! [`tokio::runtime::Handle`].

use std::path::Path;
use std::sync::atomic::AtomicBool;
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
    session: Option<DuplexSession>,
    /// True once this job placed/answered a call, so `call_ended` knows to watch the call.
    in_call: bool,
    /// True once we've observed the call go active, so a later `None` means it really ended
    /// (not just "not connected yet").
    saw_active: bool,
    /// Auto-answer (inbound) run: the daemon already answered, so call-setup steps are skipped.
    inbound: bool,
    /// Set by `job.cancel` (Ctrl+C on `dialf run`). The runner checks `cancelled()` between steps
    /// and `wait_for_speech` checks it in its read loop, so the job stops promptly.
    cancel: Arc<AtomicBool>,
}

impl PhoneJobIo {
    /// Build a phone JobIo. `rt` is the tokio handle used to drive async hub calls from
    /// the blocking job thread.
    pub fn new(
        hub: Arc<Hub>,
        engine: Arc<AudioEngine>,
        rt: Handle,
        registry: Arc<Mutex<Registry>>,
        device_id: impl Into<String>,
        session: Option<DuplexSession>,
        inbound: bool,
        cancel: Arc<AtomicBool>,
    ) -> Self {
        Self {
            hub,
            engine,
            rt,
            registry,
            device_id: device_id.into(),
            session,
            // Inbound (auto-answered): the call already exists, so watch it for end immediately.
            in_call: inbound,
            saw_active: false,
            inbound,
            cancel,
        }
    }

    /// The device's current call state, if any (read from the registry the reader loop updates).
    fn call_state(&self) -> Option<CallState> {
        self.registry
            .lock()
            .unwrap()
            .get(&self.device_id)
            .and_then(|d| d.current_call.as_ref().map(|c| c.state))
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
        self.engine.play_file(Path::new(file), self.session.as_mut())
    }

    fn wait_for_speech(&mut self, turn: TurnConfig) -> anyhow::Result<EndReason> {
        self.engine
            .wait_for_speech(turn, self.session.as_mut(), &self.cancel)
    }

    fn dial(&mut self, number: &str) -> anyhow::Result<()> {
        self.in_call = true;
        self.saw_active = false; // fresh call
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

    fn answer(&mut self) -> anyhow::Result<()> {
        self.in_call = true;
        self.saw_active = false; // fresh call
        self.cmd(Action::Answer { call_id: None })
    }

    fn hangup(&mut self) -> anyhow::Result<()> {
        // If we saw the call go active and it's now gone, the far end already hung up — there's
        // nothing to hang up, so succeed instead of erroring on the phone's "no call to hang up".
        // (Only skipped when the call is demonstrably gone; an active call is still hung up.)
        let already_ended = self.saw_active && self.call_state().is_none();
        // We're ending the call ourselves, so stop watching for "ended" — otherwise steps after
        // call.hangup (a final log, a follow-up SMS) would be skipped.
        self.in_call = false;
        self.saw_active = false;
        if already_ended {
            return Ok(());
        }
        self.cmd(Action::Hangup { call_id: None })
    }

    fn call_ended(&mut self) -> bool {
        let state = self.call_state();
        call_ended_decision(self.in_call, &mut self.saw_active, state)
    }

    fn inbound_mode(&self) -> bool {
        self.inbound
    }

    fn cancelled(&self) -> bool {
        self.cancel.load(std::sync::atomic::Ordering::Relaxed)
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

/// Decide whether the call has ended, given whether this job is in a call, whether we've ever
/// seen it active (updated in place), and the current call state. Pure so it can be tested
/// without a live phone: only a call that was once active and is now gone counts as ended —
/// "not connected yet" and "no call at all" (record-only) do not.
fn call_ended_decision(in_call: bool, saw_active: &mut bool, state: Option<CallState>) -> bool {
    if !in_call {
        return false;
    }
    match state {
        Some(CallState::Active) => {
            *saw_active = true;
            false
        }
        None if *saw_active => true,
        _ => false,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn call_ended_only_after_active_then_gone() {
        // Not in a call (record-only) -> never "ended", even with no call state.
        let mut seen = false;
        assert!(!call_ended_decision(false, &mut seen, None));

        // In a call but not yet active (dialing/ringing) -> not ended.
        let mut seen = false;
        assert!(!call_ended_decision(true, &mut seen, Some(CallState::Ringing)));
        assert!(!call_ended_decision(true, &mut seen, None)); // not connected yet, not ended

        // Goes active, then disappears -> ended.
        assert!(!call_ended_decision(true, &mut seen, Some(CallState::Active)));
        assert!(seen);
        assert!(call_ended_decision(true, &mut seen, None));
    }
}
