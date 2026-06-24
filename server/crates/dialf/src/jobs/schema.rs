//! Job step definitions, deserialized from YAML.
//!
//! Mirrors the spec's example and extends it with call/SMS/control steps. Each step
//! carries an optional human-readable `description` that the runner logs.
//!
//! ```yaml
//! - type: call.pickup
//!   description: answer the inbound call
//! - type: audio.play
//!   file: samples/prompt-en-1.wav
//!   description: RSP_BASIC-001 question
//! - type: audio.wait_for_speech
//!   end_timeout_ms: 45000
//!   silence_duration_ms: 3000
//!   description: RSP_BASIC-001 response
//! ```

use serde::{Deserialize, Serialize};

/// A whole job: just an ordered list of steps.
pub type Job = Vec<Step>;

/// Default for [`StepKind::WaitForSpeech::end_timeout_ms`].
pub const DEFAULT_END_TIMEOUT_MS: u64 = 45_000;
/// Default for [`StepKind::WaitForSpeech::silence_duration_ms`].
pub const DEFAULT_SILENCE_MS: u64 = 3_000;
/// Default for [`StepKind::WaitForSpeech::onset_duration_ms`] — continuous voiced run
/// required to count as speech onset (debounces spurious noise/echo hops).
pub const DEFAULT_ONSET_MS: u64 = 100;
/// Default for [`StepKind::CallWaitAnswered::timeout_ms`].
pub const DEFAULT_ANSWER_TIMEOUT_MS: u64 = 30_000;

/// One job step: its kind plus an optional description.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Step {
    #[serde(flatten)]
    pub kind: StepKind,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub description: Option<String>,
}

/// The kinds of step the runner understands.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum StepKind {
    /// Play an audio file out the sound card.
    #[serde(rename = "audio.play")]
    AudioPlay { file: String },

    /// Capture from the sound card until the speaker finishes a turn (VAD).
    #[serde(rename = "audio.wait_for_speech")]
    AudioWaitForSpeech {
        /// Hard cap on the whole wait, in milliseconds.
        #[serde(default = "default_end_timeout")]
        end_timeout_ms: u64,
        /// Continuous trailing non-speech that marks end-of-turn, in milliseconds.
        #[serde(default = "default_silence")]
        silence_duration_ms: u64,
        /// Continuous voiced run required to count as speech onset, in milliseconds.
        /// Raise it if line noise/echo prematurely ends the turn; default 100 ms.
        #[serde(default = "default_onset")]
        onset_duration_ms: u64,
    },

    /// Place an outbound call on the controlled phone.
    #[serde(rename = "call.dial")]
    CallDial { number: String },

    /// Block until the outbound call is answered (active), or `timeout_ms` elapses.
    #[serde(rename = "call.wait_answered")]
    CallWaitAnswered {
        #[serde(default = "default_answer_timeout")]
        timeout_ms: u64,
    },

    /// Answer the ringing call.
    #[serde(rename = "call.pickup")]
    CallPickup,

    /// Hang up the active call.
    #[serde(rename = "call.hangup")]
    CallHangup,

    /// Send a text message.
    #[serde(rename = "sms.send")]
    SmsSend { to: String, body: String },

    /// Sleep for a fixed duration.
    #[serde(rename = "wait")]
    Wait { ms: u64 },

    /// Emit a log line.
    #[serde(rename = "log")]
    Log { message: String },
}

fn default_end_timeout() -> u64 {
    DEFAULT_END_TIMEOUT_MS
}

fn default_silence() -> u64 {
    DEFAULT_SILENCE_MS
}

fn default_onset() -> u64 {
    DEFAULT_ONSET_MS
}

fn default_answer_timeout() -> u64 {
    DEFAULT_ANSWER_TIMEOUT_MS
}

/// Parse a YAML job document.
pub fn parse(yaml: &str) -> Result<Job, serde_yaml::Error> {
    serde_yaml::from_str(yaml)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_spec_example() {
        let yaml = r#"
- type: audio.play
  file: samples/prompt-en-1.wav
  description: RSP_BASIC-001 question
- type: audio.wait_for_speech
  end_timeout_ms: 45000
  silence_duration_ms: 3000
  description: RSP_BASIC-001 response
"#;
        let job = parse(yaml).expect("parse");
        assert_eq!(job.len(), 2);
        match &job[0].kind {
            StepKind::AudioPlay { file } => assert!(file.ends_with("prompt-en-1.wav")),
            other => panic!("unexpected: {other:?}"),
        }
        match &job[1].kind {
            StepKind::AudioWaitForSpeech {
                end_timeout_ms,
                silence_duration_ms,
                ..
            } => {
                assert_eq!(*end_timeout_ms, 45_000);
                assert_eq!(*silence_duration_ms, 3_000);
            }
            other => panic!("unexpected: {other:?}"),
        }
    }

    #[test]
    fn parses_call_wait_answered_default() {
        let job = parse("- type: call.wait_answered\n").expect("parse");
        match &job[0].kind {
            StepKind::CallWaitAnswered { timeout_ms } => {
                assert_eq!(*timeout_ms, DEFAULT_ANSWER_TIMEOUT_MS)
            }
            other => panic!("unexpected: {other:?}"),
        }
    }

    #[test]
    fn wait_for_speech_defaults_apply() {
        let yaml = "- type: audio.wait_for_speech\n";
        let job = parse(yaml).expect("parse");
        match &job[0].kind {
            StepKind::AudioWaitForSpeech {
                end_timeout_ms,
                silence_duration_ms,
                onset_duration_ms,
            } => {
                assert_eq!(*end_timeout_ms, DEFAULT_END_TIMEOUT_MS);
                assert_eq!(*silence_duration_ms, DEFAULT_SILENCE_MS);
                assert_eq!(*onset_duration_ms, DEFAULT_ONSET_MS);
            }
            other => panic!("unexpected: {other:?}"),
        }
    }
}
