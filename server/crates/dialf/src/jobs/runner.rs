//! Job runner: executes a list of [`Step`]s against a [`JobIo`] implementation.
//!
//! The runner is pure control flow — all side effects go through [`JobIo`]. The real
//! implementation (added with the daemon) ties the audio engine to a connected phone;
//! the [`tests`] mock here doubles as the loopback used for hardware-free M1 runs.

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
    /// Answer the ringing call.
    fn pickup(&mut self) -> anyhow::Result<()>;
    /// Hang up the active call.
    fn hangup(&mut self) -> anyhow::Result<()>;
    /// Send a text message.
    fn send_sms(&mut self, to: &str, body: &str) -> anyhow::Result<()>;
    /// Sleep for `ms` milliseconds.
    fn sleep(&mut self, ms: u64) -> anyhow::Result<()>;
    /// Emit a log line.
    fn log(&mut self, message: &str);
}

/// Outcome of a single executed step (for status reporting / streaming).
#[derive(Debug, Clone, Serialize)]
pub struct StepOutcome {
    pub index: usize,
    pub description: Option<String>,
    pub summary: String,
}

/// Run all `steps`, stopping at the first error. Returns per-step outcomes.
pub fn run_job(steps: &[Step], io: &mut dyn JobIo) -> anyhow::Result<Vec<StepOutcome>> {
    let mut outcomes = Vec::with_capacity(steps.len());
    for (index, step) in steps.iter().enumerate() {
        if let Some(d) = &step.description {
            io.log(d);
        }
        let summary = run_step(&step.kind, io)?;
        outcomes.push(StepOutcome {
            index,
            description: step.description.clone(),
            summary,
        });
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
        } => {
            let turn = TurnConfig {
                silence_duration_ms: *silence_duration_ms,
                end_timeout_ms: *end_timeout_ms,
                ..TurnConfig::default()
            };
            let reason = io.wait_for_speech(turn)?;
            format!("turn ended: {reason:?}")
        }
        StepKind::CallDial { number } => {
            io.dial(number)?;
            format!("dialed {number}")
        }
        StepKind::CallPickup => {
            io.pickup()?;
            "picked up".to_string()
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

    /// Records calls; stands in for the loopback phone + a no-op audio engine.
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
        fn pickup(&mut self) -> anyhow::Result<()> {
            self.events.push("pickup".into());
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
- type: call.pickup
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
                "pickup",
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
            fn pickup(&mut self) -> anyhow::Result<()> {
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
}
