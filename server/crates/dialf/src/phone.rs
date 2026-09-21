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
    /// Set by `job.cancel { force: true }` (a *second* Ctrl+C). Unlike `cancel`, this also
    /// interrupts a mid-flight `play` (kills the playback child) and `wait` (sleep) so the current
    /// step doesn't run to completion. The capture is never killed — `finish()` still saves the
    /// recording.
    force: Arc<AtomicBool>,
    /// Set when the driving phone app relaunches mid-job (a changed `instance_id`). Stops the job
    /// like a cancel — shared across all job types, so an app crash+restart aborts auto-answer jobs
    /// too, not just `dialf run`.
    abort: Arc<AtomicBool>,
    /// Job start, used as the timestamp origin when nothing is being recorded.
    started: Instant,
    /// Call disposition, accumulated as the job runs so the result can carry it without a
    /// follow-up `call.list` — which would race the next call.
    call: CallTrack,
}

/// What the job observed about the call it placed or answered.
#[derive(Debug, Default)]
struct CallTrack {
    /// When the call was placed or answered — the origin for `answer_latency_ms`.
    began: Option<Instant>,
    /// When it went active.
    answered: Option<Instant>,
    /// When it ended, however it ended.
    ended: Option<Instant>,
    /// The far-end number, as dialled or as reported for an inbound call.
    remote_number: Option<String>,
    /// SIM subscription id, when the phone reported one.
    sim: Option<String>,
    /// Why the call finished. `None` until something decides.
    end_reason: Option<CallEnd>,
}

/// How a call finished, for the `job.run` result.
#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize)]
#[serde(rename_all = "snake_case")]
pub enum CallEnd {
    /// The job ran its course and the call was still up (or we hung up).
    Completed,
    /// The far end hung up mid-job.
    FarEndHangup,
    /// Never answered within `call.wait_answered`'s timeout.
    NoAnswer,
}

/// Call disposition reported alongside the step outcomes.
#[derive(Debug, Clone, serde::Serialize)]
pub struct CallSummary {
    /// Dial → answered. `None` for an inbound call, which was already connected.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub answer_latency_ms: Option<u64>,
    /// Answered → ended (or → now, if still up when the job finished).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub duration_ms: Option<u64>,
    pub end_reason: CallEnd,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub remote_number: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub sim: Option<String>,
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
        force: Arc<AtomicBool>,
        abort: Arc<AtomicBool>,
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
            force,
            abort,
            started: Instant::now(),
            call: CallTrack {
                // An auto-answered call is already connected when the job starts, so its
                // clock begins here and there is no answer latency to report.
                began: inbound.then(Instant::now),
                answered: inbound.then(Instant::now),
                ..CallTrack::default()
            },
        }
    }

    /// The call disposition observed so far, or `None` if this job never had a call.
    pub fn call_summary(&self) -> Option<CallSummary> {
        let began = self.call.began?;
        let answered = self.call.answered;
        let end = self.call.ended.unwrap_or_else(Instant::now);
        Some(CallSummary {
            // Inbound calls were already up; reporting a latency there would be inventing one.
            answer_latency_ms: (!self.inbound)
                .then(|| answered.map(|a| a.duration_since(began).as_millis() as u64))
                .flatten(),
            duration_ms: answered.map(|a| end.duration_since(a).as_millis() as u64),
            end_reason: self.call.end_reason.unwrap_or(CallEnd::Completed),
            remote_number: self.call.remote_number.clone(),
            sim: self.call.sim.clone(),
        })
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
        // `force` (2nd Ctrl+C) kills the playback mid-file; a graceful cancel lets it finish.
        self.engine
            .play_file(Path::new(file), self.session.as_mut(), &self.force)
    }

    fn wait_for_speech(&mut self, turn: TurnConfig) -> anyhow::Result<EndReason> {
        self.engine
            .wait_for_speech(turn, self.session.as_mut(), &self.cancel)
    }

    fn wait_for_speech_start(
        &mut self,
        timeout_ms: u64,
        wait_after_start_ms: u64,
        onset_duration_ms: u64,
    ) -> anyhow::Result<bool> {
        self.engine.wait_for_speech_start(
            timeout_ms,
            wait_after_start_ms,
            onset_duration_ms,
            self.session.as_mut(),
            &self.cancel,
        )
    }

    /// Milliseconds since recording start, read off the recording's own frame clock so a
    /// timestamp cannot drift from the audio it describes. Without a session there is no
    /// recording to be relative to, so fall back to wall time from the job's start.
    fn now_ms(&mut self) -> u64 {
        match &self.session {
            Some(s) => {
                let rate = s.sample_rate().max(1) as u64;
                s.rx_len() * 1000 / rate
            }
            None => self.started.elapsed().as_millis() as u64,
        }
    }

    fn dial(&mut self, number: &str) -> anyhow::Result<()> {
        self.in_call = true;
        self.saw_active = false; // fresh call
        self.call.began = Some(Instant::now());
        self.call.answered = None;
        self.call.ended = None;
        self.call.end_reason = None;
        self.call.remote_number = Some(number.to_string());
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
            // Cancelled (any Ctrl+C level / app relaunch) while the far end is still ringing:
            // nothing here is worth letting finish — abandon the call. Hang up explicitly,
            // because the runner skips the job's own call.hangup once a job is cancelled.
            if JobIo::cancelled(self) {
                let _ = self.hangup();
                anyhow::bail!("cancelled while waiting for the call to be answered");
            }
            let state = self
                .registry
                .lock()
                .unwrap()
                .get(&self.device_id)
                .and_then(|d| d.current_call.as_ref().map(|c| c.state));
            match state {
                Some(CallState::Active) => {
                    // First sighting only: a later re-entry must not restate the answer time
                    // and shorten the reported duration.
                    self.call.answered.get_or_insert_with(Instant::now);
                    return Ok(());
                }
                Some(_) => seen = true, // dialing / ringing
                None if seen => {
                    self.call.ended = Some(Instant::now());
                    self.call.end_reason = Some(CallEnd::NoAnswer);
                    anyhow::bail!("call ended before it was answered");
                }
                None => {} // not placed yet — keep waiting
            }
            if Instant::now() >= deadline {
                self.call.ended = Some(Instant::now());
                self.call.end_reason = Some(CallEnd::NoAnswer);
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

    /// Our own hangup closes the call cleanly — distinct from the far end dropping it, which
    /// `call_ended` records as `far_end_hangup`.
    fn hangup(&mut self) -> anyhow::Result<()> {
        if self.call.ended.is_none() {
            self.call.ended = Some(Instant::now());
            self.call.end_reason = Some(CallEnd::Completed);
        }
        // We're ending the call ourselves, so stop watching for "ended" — otherwise steps after
        // call.hangup (a final log, a follow-up SMS) would be skipped.
        self.in_call = false;
        self.saw_active = false;
        // Always send the hang-up. We used to skip it when our view of the call state was empty
        // (`saw_active && call_state().is_none()`), assuming the far end had already hung up — but a
        // control-link reconnect (Doze) can blank the registry's current call while the phone call
        // is still up, and skipping there once left a live call running. The phone tracks the call
        // itself, so the command still lands. If the call is genuinely gone the phone replies
        // "no call to hang up" — that's the desired end state, so treat it as success.
        match self.cmd(Action::Hangup { call_id: None }) {
            Err(e) if e.to_string().contains("no call to hang up") => Ok(()),
            other => other,
        }
    }

    fn call_ended(&mut self) -> bool {
        let state = self.call_state();
        // Note the moment the call first goes active even when the job never waited for an
        // answer (an inbound run, or a script that dials and goes straight to playing).
        if state == Some(CallState::Active) {
            self.call.answered.get_or_insert_with(Instant::now);
        }
        let ended = call_ended_decision(self.in_call, &mut self.saw_active, state);
        if ended && self.call.ended.is_none() {
            self.call.ended = Some(Instant::now());
            self.call.end_reason = Some(CallEnd::FarEndHangup);
        }
        ended
    }

    fn inbound_mode(&self) -> bool {
        self.inbound
    }

    fn cancelled(&self) -> bool {
        use std::sync::atomic::Ordering::Relaxed;
        self.cancel.load(Relaxed) || self.force.load(Relaxed) || self.abort.load(Relaxed)
    }

    fn send_sms(&mut self, to: &str, body: &str) -> anyhow::Result<()> {
        self.cmd(Action::SendSms {
            to: to.to_string(),
            body: body.to_string(),
        })
    }

    fn sleep(&mut self, ms: u64) -> anyhow::Result<()> {
        // Poll so a force cancel (2nd Ctrl+C) can cut a long `wait` short; a graceful cancel lets
        // the wait run out (it's checked between steps, not here).
        let deadline = Instant::now() + Duration::from_millis(ms);
        while Instant::now() < deadline {
            if self.force.load(std::sync::atomic::Ordering::Relaxed) {
                break;
            }
            let remaining = deadline.saturating_duration_since(Instant::now());
            std::thread::sleep(remaining.min(Duration::from_millis(50)));
        }
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

    #[test]
    fn wait_for_answer_interrupted_by_cancel() {
        // A cancel (any Ctrl+C level) must break the ringing-wait immediately — not sit out
        // the full timeout_ms — and report it as a cancellation.
        let rt = tokio::runtime::Runtime::new().unwrap();
        let cancel = Arc::new(AtomicBool::new(true)); // cancelled before the wait starts
        let mut io = PhoneJobIo::new(
            Arc::new(Hub::new()),
            Arc::new(AudioEngine::new(crate::config::AudioConfig::default())),
            rt.handle().clone(),
            Arc::new(Mutex::new(Registry::new())),
            "no-such-phone",
            None,
            false,
            cancel,
            Arc::new(AtomicBool::new(false)),
            Arc::new(AtomicBool::new(false)),
        );
        let start = Instant::now();
        let err = io.wait_for_answer(30_000).unwrap_err().to_string();
        assert!(err.contains("cancelled"), "{err}");
        assert!(
            start.elapsed() < Duration::from_secs(2),
            "should not wait out the timeout, took {:?}",
            start.elapsed()
        );
    }
}
