//! Job runner: executes a list of [`Step`]s against a [`JobIo`] implementation.
//!
//! The runner is pure control flow — all side effects go through [`JobIo`]. The real
//! implementation ties the audio engine to a connected phone; the [`tests`] mock here
//! exercises the runner without hardware.

use serde::Serialize;

use crate::audio::vad::{EndReason, TurnConfig};
use crate::jobs::schema::{Step, StepKind};

/// Side-effecting operations a job step can request.
pub trait JobIo {
    /// Play an audio file out the sound card.
    fn play(&mut self, file: &str) -> anyhow::Result<()>;
    /// Capture until the speaker's turn ends; returns why it ended.
    fn wait_for_speech(&mut self, turn: TurnConfig) -> anyhow::Result<EndReason>;
    /// Block until the far end *starts* speaking, then `wait_after_start_ms` longer, and
    /// return while it is still talking. `Ok(false)` means the timeout elapsed with no
    /// speech — a missing sample, not an error.
    fn wait_for_speech_start(
        &mut self,
        timeout_ms: u64,
        wait_after_start_ms: u64,
        onset_duration_ms: u64,
    ) -> anyhow::Result<bool>;
    /// Place an outbound call.
    fn dial(&mut self, number: &str) -> anyhow::Result<()>;
    /// Block until the current call is answered (active), or `timeout_ms` elapses.
    fn wait_for_answer(&mut self, timeout_ms: u64) -> anyhow::Result<()>;
    /// Answer the ringing call.
    fn answer(&mut self) -> anyhow::Result<()>;
    /// Hang up the active call.
    fn hangup(&mut self) -> anyhow::Result<()>;
    /// Send a text message.
    fn send_sms(&mut self, to: &str, body: &str) -> anyhow::Result<()>;
    /// Sleep for `ms` milliseconds.
    fn sleep(&mut self, ms: u64) -> anyhow::Result<()>;
    /// Emit a log line.
    fn log(&mut self, message: &str);

    /// Whether the call this job was running has ended (e.g. the far end hung up). The runner
    /// checks this between steps and stops early so the job doesn't keep playing prompts into a
    /// dead call, hold the sound card, or fail on a `hangup` that has nothing to hang up.
    /// Default `false` for IO backends without a phone (e.g. record-only / tests).
    fn call_ended(&mut self) -> bool {
        false
    }

    /// Whether this run is for an auto-answered inbound call (the daemon already answered it).
    /// In that mode the runner skips the call-setup steps (call.dial / call.wait_answered /
    /// call.answer) — the call already exists — and warns. Default `false` (outbound / one-shot).
    fn inbound_mode(&self) -> bool {
        false
    }

    /// Whether the job has been cancelled (e.g. Ctrl+C on `dialf run`, which sends `job.cancel`).
    /// The runner checks this between steps and stops early. Default `false`.
    fn cancelled(&self) -> bool {
        false
    }

    /// Milliseconds since **recording start**, for stamping step outcomes.
    ///
    /// The real backend reads the recording's own frame clock, so a timestamp and the audio
    /// it describes cannot drift apart. The default is wall-clock from the first call, which
    /// is all a record-less run (or a test) can offer.
    fn now_ms(&mut self) -> u64 {
        0
    }
}

/// Why a step stopped. Libretto's outcome vocabulary — an offline analyser branches on this
/// rather than parsing `summary`, which is prose and may change.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum StepEnd {
    /// Ran to its natural end.
    Completed,
    /// Hit its own timeout. Not necessarily a failure — see `audio.wait_for_speech_start`.
    Timeout,
    /// Never ran: an earlier step ended the job, or auto-answer mode made it meaningless.
    Skipped,
    /// The job was cancelled (Ctrl+C on `dialf run` → `job.cancel`).
    Cancelled,
    /// The far end hung up.
    CallEnded,
}

/// Outcome of a single executed step (for status reporting / streaming).
///
/// `t_start_ms`/`t_end_ms` are relative to **recording start**, so an outcome can be laid
/// directly over the rx/tx legs — which is what makes it possible to say which span of far-end
/// speech answered which prompt.
#[derive(Debug, Clone, Serialize)]
pub struct StepOutcome {
    pub index: usize,
    /// Echo of the step's `id`, when the caller set one.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub id: Option<String>,
    /// The step's wire name (`audio.play`, …).
    #[serde(rename = "type")]
    pub step_type: &'static str,
    pub description: Option<String>,
    pub t_start_ms: u64,
    pub t_end_ms: u64,
    pub end_reason: StepEnd,
    pub summary: String,
}

impl StepOutcome {
    /// An outcome for a step that never ran.
    fn skipped(index: usize, step: &Step, at_ms: u64, end_reason: StepEnd, summary: String) -> Self {
        Self {
            index,
            id: step.id.clone(),
            step_type: step.kind.name(),
            description: step.description.clone(),
            // Zero-width at the moment the job stopped: the step occupied no time.
            t_start_ms: at_ms,
            t_end_ms: at_ms,
            end_reason,
            summary,
        }
    }
}

/// Summary recorded as the final outcome when a job stops because the far end hung up.
/// Callers (e.g. the auto-answer serve stream) can match on it to report "caller hung up".
pub const CALL_ENDED_SUMMARY: &str = "caller hung up — remaining steps skipped";

/// Summary recorded when a job is cancelled (Ctrl+C on `dialf run` → `job.cancel`).
pub const CANCELLED_SUMMARY: &str = "cancelled — remaining steps skipped";

/// Run all `steps`, stopping at the first error. Returns per-step outcomes.
pub fn run_job(steps: &[Step], io: &mut dyn JobIo) -> anyhow::Result<Vec<StepOutcome>> {
    let mut outcomes = Vec::with_capacity(steps.len());
    for (index, step) in steps.iter().enumerate() {
        let t_start_ms = io.now_ms();
        // In auto-answer mode the daemon already set up (answered) the call, so call-setup steps
        // are meaningless — skip them with a warning rather than, e.g., placing a second call.
        if io.inbound_mode()
            && matches!(
                step.kind,
                StepKind::CallDial { .. } | StepKind::CallWaitAnswered { .. } | StepKind::CallAnswer
            )
        {
            tracing::warn!(target: "job", "{} skipped — auto-answer mode (the call is already inbound)", step.kind.name());
            outcomes.push(StepOutcome::skipped(
                index,
                step,
                t_start_ms,
                StepEnd::Skipped,
                format!("{} skipped (auto-answer mode)", step.kind.name()),
            ));
            continue;
        }
        if let Some(d) = &step.description {
            io.log(d);
        }
        let (summary, end_reason) = run_step(&step.kind, io)?;
        outcomes.push(StepOutcome {
            index,
            id: step.id.clone(),
            step_type: step.kind.name(),
            description: step.description.clone(),
            t_start_ms,
            t_end_ms: io.now_ms(),
            end_reason,
            summary,
        });
        // Cancelled (Ctrl+C on `dialf run`) — stop now; don't play more prompts or hold the card.
        if io.cancelled() {
            tracing::info!(target: "job", "job cancelled — stopping after step {index}");
            let at = io.now_ms();
            outcomes.push(StepOutcome {
                index: index + 1,
                id: None,
                step_type: "control.log",
                description: Some("cancelled".to_string()),
                t_start_ms: at,
                t_end_ms: at,
                end_reason: StepEnd::Cancelled,
                summary: CANCELLED_SUMMARY.to_string(),
            });
            push_skipped(&mut outcomes, steps, index, at);
            break;
        }
        // The far end hung up — stop here rather than run the remaining steps (more prompts, a
        // doomed hangup) against a call that no longer exists. Record it as a visible outcome so
        // it shows up in `dialf run` output / the serve stream, not just the daemon log.
        if io.call_ended() {
            tracing::info!(target: "job", "call ended (far end hung up) — stopping after step {index}");
            let at = io.now_ms();
            outcomes.push(StepOutcome {
                index: index + 1,
                id: None,
                step_type: "control.log",
                description: Some("call ended".to_string()),
                t_start_ms: at,
                t_end_ms: at,
                end_reason: StepEnd::CallEnded,
                summary: CALL_ENDED_SUMMARY.to_string(),
            });
            // Record each remaining step as skipped, so it's clear what didn't run.
            push_skipped(&mut outcomes, steps, index, at);
            break;
        }
    }
    Ok(outcomes)
}

/// Report every step after `index` as not-run, so the caller can see exactly what was lost.
fn push_skipped(
    outcomes: &mut Vec<StepOutcome>,
    steps: &[Step],
    index: usize,
    at_ms: u64,
) {
    for (j, skipped) in steps.iter().enumerate().skip(index + 1) {
        outcomes.push(StepOutcome::skipped(
            j,
            skipped,
            at_ms,
            StepEnd::Skipped,
            format!("{} skipped", skipped.kind.name()),
        ));
    }
}

fn run_step(kind: &StepKind, io: &mut dyn JobIo) -> anyhow::Result<(String, StepEnd)> {
    let out = match kind {
        StepKind::AudioPlay { file } => {
            io.play(file)?;
            (format!("played {file}"), StepEnd::Completed)
        }
        StepKind::AudioWaitForSpeech {
            end_timeout_ms,
            silence_duration_ms,
            onset_duration_ms,
        } => {
            let turn = TurnConfig {
                silence_duration_ms: *silence_duration_ms,
                end_timeout_ms: *end_timeout_ms,
                onset_duration_ms: *onset_duration_ms,
                ..TurnConfig::default()
            };
            let reason = io.wait_for_speech(turn)?;
            // A turn that timed out still produced audio; the caller decides what that means
            // for the eval, so report it rather than failing the job.
            let end = match reason {
                EndReason::Timeout => StepEnd::Timeout,
                _ => StepEnd::Completed,
            };
            (format!("turn ended: {reason:?}"), end)
        }
        StepKind::AudioWaitForSpeechStart {
            timeout_ms,
            wait_after_start_ms,
            onset_duration_ms,
        } => {
            let started =
                io.wait_for_speech_start(*timeout_ms, *wait_after_start_ms, *onset_duration_ms)?;
            if started {
                (
                    format!("speech started, waited {wait_after_start_ms}ms"),
                    StepEnd::Completed,
                )
            } else {
                // Non-fatal by design: the far end never spoke, so this turn yields no
                // interrupt sample. Failing here would abandon a live call over a missing
                // datum.
                (
                    format!("no speech within {timeout_ms}ms"),
                    StepEnd::Timeout,
                )
            }
        }
        StepKind::CallDial { number } => {
            io.dial(number)?;
            (format!("dialed {number}"), StepEnd::Completed)
        }
        StepKind::CallWaitAnswered { timeout_ms } => {
            io.wait_for_answer(*timeout_ms)?;
            ("call answered".to_string(), StepEnd::Completed)
        }
        StepKind::CallAnswer => {
            io.answer()?;
            ("answered".to_string(), StepEnd::Completed)
        }
        StepKind::CallHangup => {
            io.hangup()?;
            ("hung up".to_string(), StepEnd::Completed)
        }
        StepKind::SmsSend { to, body } => {
            io.send_sms(to, body)?;
            (format!("sms -> {to} ({} chars)", body.len()), StepEnd::Completed)
        }
        StepKind::Wait { ms } => {
            io.sleep(*ms)?;
            (format!("waited {ms}ms"), StepEnd::Completed)
        }
        StepKind::Log { message } => {
            io.log(message);
            (format!("log: {message}"), StepEnd::Completed)
        }
    };
    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::jobs::schema;

    /// Records the steps the runner invoked; stands in for a phone + audio engine in tests.
    #[derive(Default)]
    struct MockIo {
        events: Vec<String>,
        speech_reason: Option<EndReason>,
        /// What `wait_for_speech_start` reports: `false` stands in for "the far end never
        /// spoke", which must not abort the job.
        speech_started: Option<bool>,
        /// Fake clock, advanced a tick per IO call so outcomes get distinguishable,
        /// monotonically increasing timestamps without any real waiting.
        clock_ms: u64,
    }

    impl MockIo {
        fn tick(&mut self) {
            self.clock_ms += 10;
        }
    }

    impl JobIo for MockIo {
        fn play(&mut self, file: &str) -> anyhow::Result<()> {
            self.tick();
            self.events.push(format!("play:{file}"));
            Ok(())
        }
        fn wait_for_speech(&mut self, _turn: TurnConfig) -> anyhow::Result<EndReason> {
            self.tick();
            self.events.push("wait".into());
            Ok(self.speech_reason.unwrap_or(EndReason::Silence))
        }
        fn wait_for_speech_start(
            &mut self,
            _timeout_ms: u64,
            wait_after_start_ms: u64,
            _onset_duration_ms: u64,
        ) -> anyhow::Result<bool> {
            self.tick();
            self.events.push(format!("wait_start:{wait_after_start_ms}"));
            Ok(self.speech_started.unwrap_or(true))
        }
        fn now_ms(&mut self) -> u64 {
            self.clock_ms
        }
        fn dial(&mut self, number: &str) -> anyhow::Result<()> {
            self.events.push(format!("dial:{number}"));
            Ok(())
        }
        fn wait_for_answer(&mut self, _timeout_ms: u64) -> anyhow::Result<()> {
            self.events.push("wait_answered".into());
            Ok(())
        }
        fn answer(&mut self) -> anyhow::Result<()> {
            self.events.push("answer".into());
            Ok(())
        }
        fn hangup(&mut self) -> anyhow::Result<()> {
            self.events.push("hangup".into());
            Ok(())
        }
        fn send_sms(&mut self, to: &str, body: &str) -> anyhow::Result<()> {
            self.events.push(format!("sms:{to}:{body}"));
            Ok(())
        }
        fn sleep(&mut self, ms: u64) -> anyhow::Result<()> {
            self.events.push(format!("sleep:{ms}"));
            Ok(())
        }
        fn log(&mut self, message: &str) {
            self.events.push(format!("log:{message}"));
        }
    }

    #[test]
    fn runs_sample_job_in_order() {
        let yaml = r#"
- type: call.answer
  description: answer
- type: audio.play
  file: q1.wav
- type: audio.wait_for_speech
  silence_duration_ms: 1000
- type: call.hangup
"#;
        let job = schema::parse(yaml).unwrap();
        let mut io = MockIo::default();
        let outcomes = run_job(&job, &mut io).unwrap();

        assert_eq!(outcomes.len(), 4);
        assert_eq!(
            io.events,
            vec![
                "log:answer", // description logged before the step
                "answer",
                "play:q1.wav",
                "wait",
                "hangup",
            ]
        );
    }

    #[test]
    fn outcomes_carry_the_libretto_envelope() {
        // Offline analysis reads these fields, not the prose summary: `id` ties a span of
        // recorded audio back to the scripted turn, and the timestamps place it on the
        // recording timeline.
        let steps: Vec<Step> = serde_yaml::from_str(
            "- type: audio.play\n  file: q.wav\n  id: rsp-001-ask\n  description: ask\n\
             - type: audio.wait_for_speech\n  id: rsp-001-answer\n",
        )
        .unwrap();
        let mut io = MockIo::default();
        let out = run_job(&steps, &mut io).unwrap();

        assert_eq!(out.len(), 2);
        assert_eq!(out[0].id.as_deref(), Some("rsp-001-ask"));
        assert_eq!(out[0].step_type, "audio.play");
        assert_eq!(out[0].description.as_deref(), Some("ask"));
        assert_eq!(out[0].end_reason, StepEnd::Completed);
        assert_eq!(out[1].id.as_deref(), Some("rsp-001-answer"));
        assert_eq!(out[1].step_type, "audio.wait_for_speech");

        // Timestamps must be usable as spans and must not go backwards between steps.
        for o in &out {
            assert!(o.t_start_ms <= o.t_end_ms, "{o:?}");
        }
        assert!(out[1].t_start_ms >= out[0].t_end_ms, "{out:?}");
    }

    #[test]
    fn a_step_without_an_id_simply_omits_it() {
        // `id` is optional; a job written before Libretto must not grow a null field.
        let steps: Vec<Step> = serde_yaml::from_str("- type: audio.play\n  file: q.wav\n").unwrap();
        let out = run_job(&steps, &mut MockIo::default()).unwrap();
        assert!(out[0].id.is_none());
        let json = serde_json::to_string(&out[0]).unwrap();
        assert!(!json.contains("\"id\""), "{json}");
        assert!(json.contains("\"type\":\"audio.play\""), "{json}");
    }

    #[test]
    fn a_timed_out_turn_is_reported_not_failed() {
        // The far end going quiet is a datum, not a broken job — the call may still be fine.
        let steps: Vec<Step> = serde_yaml::from_str("- type: audio.wait_for_speech\n").unwrap();
        let mut io = MockIo {
            speech_reason: Some(EndReason::Timeout),
            ..MockIo::default()
        };
        let out = run_job(&steps, &mut io).unwrap();
        assert_eq!(out[0].end_reason, StepEnd::Timeout);
    }

    #[test]
    fn wait_for_speech_start_timing_out_does_not_abort_the_job() {
        // The rule the whole barge-in design leans on: if the agent never speaks, that turn
        // yields no interrupt sample and the script carries on. Aborting a live PSTN call
        // over a missing datum would be far worse than a gap in the results.
        let steps: Vec<Step> = serde_yaml::from_str(
            "- type: audio.wait_for_speech_start\n  timeout_ms: 500\n\
             - type: audio.play\n  file: after.wav\n",
        )
        .unwrap();
        let mut io = MockIo {
            speech_started: Some(false),
            ..MockIo::default()
        };
        let out = run_job(&steps, &mut io).unwrap();

        assert_eq!(out[0].end_reason, StepEnd::Timeout);
        assert_eq!(out[0].step_type, "audio.wait_for_speech_start");
        assert_eq!(out[1].end_reason, StepEnd::Completed, "the job must continue");
        assert!(io.events.contains(&"play:after.wav".to_string()), "{:?}", io.events);
    }

    #[test]
    fn a_barge_in_script_runs_in_order() {
        // ask -> let the agent start -> talk over it -> capture the reaction.
        let steps: Vec<Step> = serde_yaml::from_str(
            "- type: audio.play\n  file: q.wav\n\
             - type: audio.wait_for_speech_start\n  wait_after_start_ms: 2000\n\
             - type: audio.play\n  file: interrupt.wav\n\
             - type: audio.wait_for_speech\n",
        )
        .unwrap();
        let mut io = MockIo::default();
        let out = run_job(&steps, &mut io).unwrap();

        assert_eq!(
            io.events,
            vec!["play:q.wav", "wait_start:2000", "play:interrupt.wav", "wait"]
        );
        assert!(out.iter().all(|o| o.end_reason == StepEnd::Completed), "{out:?}");
    }

    #[test]
    fn stops_on_error() {
        struct FailDial;
        impl JobIo for FailDial {
            fn play(&mut self, _: &str) -> anyhow::Result<()> {
                Ok(())
            }
            fn wait_for_speech_start(&mut self, _: u64, _: u64, _: u64) -> anyhow::Result<bool> {
                Ok(true)
            }
            fn wait_for_speech(&mut self, _: TurnConfig) -> anyhow::Result<EndReason> {
                Ok(EndReason::Silence)
            }
            fn dial(&mut self, _: &str) -> anyhow::Result<()> {
                anyhow::bail!("no device")
            }
            fn wait_for_answer(&mut self, _: u64) -> anyhow::Result<()> {
                Ok(())
            }
            fn answer(&mut self) -> anyhow::Result<()> {
                Ok(())
            }
            fn hangup(&mut self) -> anyhow::Result<()> {
                Ok(())
            }
            fn send_sms(&mut self, _: &str, _: &str) -> anyhow::Result<()> {
                Ok(())
            }
            fn sleep(&mut self, _: u64) -> anyhow::Result<()> {
                Ok(())
            }
            fn log(&mut self, _: &str) {}
        }
        let job = schema::parse("- type: call.dial\n  number: \"123\"\n").unwrap();
        let mut io = FailDial;
        assert!(run_job(&job, &mut io).is_err());
    }

    #[test]
    fn stops_and_marks_when_call_ends() {
        // call_ended() true after the first step: the job records the answer, then a synthetic
        // "caller hung up" outcome, and skips the remaining steps (no doomed play/hangup).
        #[derive(Default)]
        struct EndingIo {
            steps: usize,
        }
        impl JobIo for EndingIo {
            fn play(&mut self, _: &str) -> anyhow::Result<()> {
                Ok(())
            }
            fn wait_for_speech_start(&mut self, _: u64, _: u64, _: u64) -> anyhow::Result<bool> {
                Ok(true)
            }
            fn wait_for_speech(&mut self, _: TurnConfig) -> anyhow::Result<EndReason> {
                Ok(EndReason::Silence)
            }
            fn dial(&mut self, _: &str) -> anyhow::Result<()> {
                Ok(())
            }
            fn wait_for_answer(&mut self, _: u64) -> anyhow::Result<()> {
                Ok(())
            }
            fn answer(&mut self) -> anyhow::Result<()> {
                Ok(())
            }
            fn hangup(&mut self) -> anyhow::Result<()> {
                Ok(())
            }
            fn send_sms(&mut self, _: &str, _: &str) -> anyhow::Result<()> {
                Ok(())
            }
            fn sleep(&mut self, _: u64) -> anyhow::Result<()> {
                Ok(())
            }
            fn log(&mut self, _: &str) {}
            fn call_ended(&mut self) -> bool {
                self.steps += 1;
                self.steps >= 1 // ended right after the first step
            }
        }
        let yaml = "- type: call.answer\n- type: audio.play\n  file: x.wav\n- type: call.hangup\n";
        let job = schema::parse(yaml).unwrap();
        let mut io = EndingIo::default();
        let outcomes = run_job(&job, &mut io).unwrap();
        // answer ran; then the marker + one "skipped" line per remaining step.
        assert_eq!(outcomes.len(), 4);
        assert_eq!(outcomes[0].summary, "answered");
        assert_eq!(outcomes[1].summary, CALL_ENDED_SUMMARY);
        assert_eq!(outcomes[2].summary, "audio.play skipped");
        assert_eq!(outcomes[3].summary, "call.hangup skipped");
    }

    #[test]
    fn cancels_and_marks_remaining() {
        // cancelled() true (Ctrl+C on `dialf run`): after the first step the runner stops, records
        // a "cancelled" marker, and skips the rest — no more prompts / no held card.
        struct CancelIo;
        impl JobIo for CancelIo {
            fn play(&mut self, _: &str) -> anyhow::Result<()> {
                Ok(())
            }
            fn wait_for_speech_start(&mut self, _: u64, _: u64, _: u64) -> anyhow::Result<bool> {
                Ok(true)
            }
            fn wait_for_speech(&mut self, _: TurnConfig) -> anyhow::Result<EndReason> {
                Ok(EndReason::Silence)
            }
            fn dial(&mut self, _: &str) -> anyhow::Result<()> {
                Ok(())
            }
            fn wait_for_answer(&mut self, _: u64) -> anyhow::Result<()> {
                Ok(())
            }
            fn answer(&mut self) -> anyhow::Result<()> {
                Ok(())
            }
            fn hangup(&mut self) -> anyhow::Result<()> {
                Ok(())
            }
            fn send_sms(&mut self, _: &str, _: &str) -> anyhow::Result<()> {
                Ok(())
            }
            fn sleep(&mut self, _: u64) -> anyhow::Result<()> {
                Ok(())
            }
            fn log(&mut self, _: &str) {}
            fn cancelled(&self) -> bool {
                true
            }
        }
        let yaml =
            "- type: audio.play\n  file: a.wav\n- type: audio.wait_for_speech\n- type: log\n  message: done\n";
        let job = schema::parse(yaml).unwrap();
        let mut io = CancelIo;
        let outcomes = run_job(&job, &mut io).unwrap();
        // step 1 ran; then the marker + one "skipped" line per remaining step.
        assert_eq!(outcomes.len(), 4);
        assert_eq!(outcomes[0].summary, "played a.wav");
        assert_eq!(outcomes[1].summary, CANCELLED_SUMMARY);
        assert_eq!(outcomes[2].summary, "audio.wait_for_speech skipped");
        assert_eq!(outcomes[3].summary, "log skipped");
    }

    #[test]
    fn inbound_mode_skips_call_setup_steps() {
        // In auto-answer mode the daemon already answered, so call.answer/dial/wait_answered are
        // no-ops (warned); conversation steps still run.
        #[derive(Default)]
        struct InboundIo {
            ran: Vec<String>,
        }
        impl JobIo for InboundIo {
            fn play(&mut self, file: &str) -> anyhow::Result<()> {
                self.ran.push(format!("play:{file}"));
                Ok(())
            }
            fn wait_for_speech_start(&mut self, _: u64, _: u64, _: u64) -> anyhow::Result<bool> {
                Ok(true)
            }
            fn wait_for_speech(&mut self, _: TurnConfig) -> anyhow::Result<EndReason> {
                Ok(EndReason::Silence)
            }
            fn dial(&mut self, _: &str) -> anyhow::Result<()> {
                self.ran.push("dial".into());
                Ok(())
            }
            fn wait_for_answer(&mut self, _: u64) -> anyhow::Result<()> {
                self.ran.push("wait_answered".into());
                Ok(())
            }
            fn answer(&mut self) -> anyhow::Result<()> {
                self.ran.push("answer".into());
                Ok(())
            }
            fn hangup(&mut self) -> anyhow::Result<()> {
                self.ran.push("hangup".into());
                Ok(())
            }
            fn send_sms(&mut self, _: &str, _: &str) -> anyhow::Result<()> {
                Ok(())
            }
            fn sleep(&mut self, _: u64) -> anyhow::Result<()> {
                Ok(())
            }
            fn log(&mut self, _: &str) {}
            fn inbound_mode(&self) -> bool {
                true
            }
        }
        let yaml = "- type: call.answer\n- type: call.dial\n  number: \"1\"\n- type: audio.play\n  file: x.wav\n- type: call.hangup\n";
        let job = schema::parse(yaml).unwrap();
        let mut io = InboundIo::default();
        let outcomes = run_job(&job, &mut io).unwrap();
        // Setup steps were skipped (never invoked); only play + hangup actually ran.
        assert_eq!(io.ran, vec!["play:x.wav".to_string(), "hangup".to_string()]);
        assert_eq!(outcomes[0].summary, "call.answer skipped (auto-answer mode)");
        assert_eq!(outcomes[1].summary, "call.dial skipped (auto-answer mode)");
    }
}
