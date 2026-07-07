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
}

/// Outcome of a single executed step (for status reporting / streaming).
#[derive(Debug, Clone, Serialize)]
pub struct StepOutcome {
    pub index: usize,
    pub description: Option<String>,
    pub summary: String,
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
        // In auto-answer mode the daemon already set up (answered) the call, so call-setup steps
        // are meaningless — skip them with a warning rather than, e.g., placing a second call.
        if io.inbound_mode()
            && matches!(
                step.kind,
                StepKind::CallDial { .. } | StepKind::CallWaitAnswered { .. } | StepKind::CallAnswer
            )
        {
            tracing::warn!(target: "job", "{} skipped — auto-answer mode (the call is already inbound)", step.kind.name());
            outcomes.push(StepOutcome {
                index,
                description: step.description.clone(),
                summary: format!("{} skipped (auto-answer mode)", step.kind.name()),
            });
            continue;
        }
        if let Some(d) = &step.description {
            io.log(d);
        }
        let summary = run_step(&step.kind, io)?;
        outcomes.push(StepOutcome {
            index,
            description: step.description.clone(),
            summary,
        });
        // Cancelled (Ctrl+C on `dialf run`) — stop now; don't play more prompts or hold the card.
        if io.cancelled() {
            tracing::info!(target: "job", "job cancelled — stopping after step {index}");
            outcomes.push(StepOutcome {
                index: index + 1,
                description: Some("cancelled".to_string()),
                summary: CANCELLED_SUMMARY.to_string(),
            });
            for (j, skipped) in steps.iter().enumerate().skip(index + 1) {
                outcomes.push(StepOutcome {
                    index: j,
                    description: skipped.description.clone(),
                    summary: format!("{} skipped", skipped.kind.name()),
                });
            }
            break;
        }
        // The far end hung up — stop here rather than run the remaining steps (more prompts, a
        // doomed hangup) against a call that no longer exists. Record it as a visible outcome so
        // it shows up in `dialf run` output / the serve stream, not just the daemon log.
        if io.call_ended() {
            tracing::info!(target: "job", "call ended (far end hung up) — stopping after step {index}");
            outcomes.push(StepOutcome {
                index: index + 1,
                description: Some("call ended".to_string()),
                summary: CALL_ENDED_SUMMARY.to_string(),
            });
            // Record each remaining step as skipped, so it's clear what didn't run.
            for (j, skipped) in steps.iter().enumerate().skip(index + 1) {
                outcomes.push(StepOutcome {
                    index: j,
                    description: skipped.description.clone(),
                    summary: format!("{} skipped", skipped.kind.name()),
                });
            }
            break;
        }
    }
    Ok(outcomes)
}

fn run_step(kind: &StepKind, io: &mut dyn JobIo) -> anyhow::Result<String> {
    let summary = match kind {
        StepKind::AudioPlay { file } => {
            io.play(file)?;
            format!("played {file}")
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
            format!("turn ended: {reason:?}")
        }
        StepKind::CallDial { number } => {
            io.dial(number)?;
            format!("dialed {number}")
        }
        StepKind::CallWaitAnswered { timeout_ms } => {
            io.wait_for_answer(*timeout_ms)?;
            "call answered".to_string()
        }
        StepKind::CallAnswer => {
            io.answer()?;
            "answered".to_string()
        }
        StepKind::CallHangup => {
            io.hangup()?;
            "hung up".to_string()
        }
        StepKind::SmsSend { to, body } => {
            io.send_sms(to, body)?;
            format!("sms -> {to} ({} chars)", body.len())
        }
        StepKind::Wait { ms } => {
            io.sleep(*ms)?;
            format!("waited {ms}ms")
        }
        StepKind::Log { message } => {
            io.log(message);
            format!("log: {message}")
        }
    };
    Ok(summary)
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
    }

    impl JobIo for MockIo {
        fn play(&mut self, file: &str) -> anyhow::Result<()> {
            self.events.push(format!("play:{file}"));
            Ok(())
        }
        fn wait_for_speech(&mut self, _turn: TurnConfig) -> anyhow::Result<EndReason> {
            self.events.push("wait".into());
            Ok(self.speech_reason.unwrap_or(EndReason::Silence))
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
    fn stops_on_error() {
        struct FailDial;
        impl JobIo for FailDial {
            fn play(&mut self, _: &str) -> anyhow::Result<()> {
                Ok(())
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
