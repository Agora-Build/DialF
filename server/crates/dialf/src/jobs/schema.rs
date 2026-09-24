//! Job step definitions, deserialized from YAML.
//!
//! Mirrors the spec's example and extends it with call/SMS/control steps. Each step
//! carries an optional human-readable `description` that the runner logs.
//!
//! ```yaml
//! - type: call.answer
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
/// Default for [`StepKind::AudioWaitForSpeechStart::timeout_ms`].
pub const DEFAULT_SPEECH_START_TIMEOUT_MS: u64 = 15_000;
/// Default for [`StepKind::AudioWaitForSpeechStart::wait_after_start_ms`].
pub const DEFAULT_WAIT_AFTER_START_MS: u64 = 2_000;

/// One job step: its kind plus optional envelope fields.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Step {
    #[serde(flatten)]
    pub kind: StepKind,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub description: Option<String>,
    /// Caller-supplied identifier, echoed back on the step's outcome and never interpreted
    /// here. An offline analyser uses it to tie a span of recorded audio to the scripted turn
    /// that produced it, which index alone cannot do once a script is regenerated.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub id: Option<String>,
}

/// The kinds of step the runner understands.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
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

    /// Wait for the far end to *start* speaking, then return while it still is.
    ///
    /// Unlike [`StepKind::AudioWaitForSpeech`], which waits for the turn to finish, this
    /// returns mid-speech — so the next `audio.play` lands on top of the far end and is
    /// itself the barge-in. That is the only way to script an interrupt, and the only way
    /// interrupt latency becomes measurable.
    #[serde(rename = "audio.wait_for_speech_start")]
    AudioWaitForSpeechStart {
        /// Give up if the far end never starts speaking. Non-fatal: the step reports
        /// `timeout` and the job continues, because a turn that yields no interrupt sample
        /// is a missing datum, not a broken call.
        #[serde(default = "default_speech_start_timeout")]
        timeout_ms: u64,
        /// How long to keep listening after onset before returning, so the interrupt lands
        /// some way into the far end's sentence rather than on its first syllable.
        #[serde(default = "default_wait_after_start")]
        wait_after_start_ms: u64,
        /// Continuous voiced run required to count as onset — same debounce as
        /// `audio.wait_for_speech`.
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
    #[serde(rename = "call.answer")]
    CallAnswer,

    /// Hang up the active call.
    #[serde(rename = "call.hangup")]
    CallHangup,

    /// Send a text message.
    #[serde(rename = "sms.send")]
    SmsSend { to: String, body: String },

    /// Sleep for a fixed duration.
    #[serde(rename = "wait", alias = "control.wait")]
    Wait { ms: u64 },

    /// Emit a log line.
    #[serde(rename = "log", alias = "control.log")]
    Log { message: String },
}

/// Libretto spec version this vocabulary implements.
pub const SPEC_VERSION: &str = "0.1";

/// Every step type DialF executes, in spec order.
///
/// The single source of truth behind `dialf manifest` / `server.manifest`. A test asserts it
/// matches [`StepKind::name`] exactly, so a step cannot be added to the engine without
/// appearing in the manifest — a manifest that overstates or understates what an engine runs
/// is worse than none, since Vox validates scripts against it before dispatch.
pub const IMPLEMENTED_STEPS: &[&str] = &[
    "call.dial",
    "call.wait_answered",
    "call.answer",
    "call.hangup",
    "audio.play",
    "audio.wait_for_speech",
    "audio.wait_for_speech_start",
    "sms.send",
    "control.wait",
    "control.log",
];

/// Orchestrated steps DialF is willing to run *inside* one of its session blocks, rather than
/// handing back to the orchestrator mid-call — leaving a call to sleep or log would be absurd.
pub const INLINE_ORCHESTRATED: &[&str] = &["sms.send", "control.wait", "control.log"];

/// The capability manifest (Libretto §7).
pub fn manifest() -> serde_json::Value {
    serde_json::json!({
        "executor": "dialf",
        "spec_version": SPEC_VERSION,
        "version": env!("CARGO_PKG_VERSION"),
        "steps": IMPLEMENTED_STEPS,
        "inline_orchestrated": INLINE_ORCHESTRATED,
        "extensions": [],
    })
}

impl StepKind {
    /// The step's wire name (matches the YAML `type:`), for logs and skip messages.
    pub fn name(&self) -> &'static str {
        match self {
            StepKind::AudioPlay { .. } => "audio.play",
            StepKind::AudioWaitForSpeech { .. } => "audio.wait_for_speech",
            StepKind::AudioWaitForSpeechStart { .. } => "audio.wait_for_speech_start",
            StepKind::CallDial { .. } => "call.dial",
            StepKind::CallWaitAnswered { .. } => "call.wait_answered",
            StepKind::CallAnswer => "call.answer",
            StepKind::CallHangup => "call.hangup",
            StepKind::SmsSend { .. } => "sms.send",
            StepKind::Wait { .. } => "wait",
            StepKind::Log { .. } => "log",
        }
    }
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

fn default_speech_start_timeout() -> u64 {
    DEFAULT_SPEECH_START_TIMEOUT_MS
}

fn default_wait_after_start() -> u64 {
    DEFAULT_WAIT_AFTER_START_MS
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
    #[test]
    fn control_prefixed_and_bare_forms_are_the_same_step() {
        // Libretto §10: engines accept both during 0.x, so a script written either way runs
        // here without a compiler shim.
        let bare: Job = serde_yaml::from_str("- type: wait\n  ms: 50\n- type: log\n  message: hi\n").unwrap();
        let spec: Job =
            serde_yaml::from_str("- type: control.wait\n  ms: 50\n- type: control.log\n  message: hi\n")
                .unwrap();
        assert!(matches!(bare[0].kind, StepKind::Wait { ms: 50 }));
        assert!(matches!(spec[0].kind, StepKind::Wait { ms: 50 }));
        assert!(matches!(bare[1].kind, StepKind::Log { .. }));
        assert!(matches!(spec[1].kind, StepKind::Log { .. }));
        // They report the canonical name either way, so outcomes are consistent.
        assert_eq!(bare[0].kind.name(), spec[0].kind.name());
    }

    #[test]
    fn wait_for_speech_start_has_spec_defaults() {
        let job: Job = serde_yaml::from_str("- type: audio.wait_for_speech_start\n").unwrap();
        match job[0].kind {
            StepKind::AudioWaitForSpeechStart {
                timeout_ms,
                wait_after_start_ms,
                onset_duration_ms,
            } => {
                assert_eq!(timeout_ms, 15_000);
                assert_eq!(wait_after_start_ms, 2_000);
                assert_eq!(onset_duration_ms, DEFAULT_ONSET_MS);
            }
            ref other => panic!("wrong step: {other:?}"),
        }
    }

    #[test]
    fn an_id_round_trips_and_is_optional() {
        let job: Job = serde_yaml::from_str("- type: audio.play\n  file: a.wav\n  id: turn-1\n").unwrap();
        assert_eq!(job[0].id.as_deref(), Some("turn-1"));
        let bare: Job = serde_yaml::from_str("- type: audio.play\n  file: a.wav\n").unwrap();
        assert!(bare[0].id.is_none());
    }

    #[test]
    fn the_manifest_lists_exactly_what_the_engine_runs() {
        // A manifest that overstates what DialF implements is worse than none: Vox validates
        // scripts against it and would dispatch a step that fails at run time. Build one of
        // every step and compare the two lists.
        let every: Vec<&str> = vec![
            StepKind::CallDial { number: "1".into() }.name(),
            StepKind::CallWaitAnswered { timeout_ms: 0 }.name(),
            StepKind::CallAnswer.name(),
            StepKind::CallHangup.name(),
            StepKind::AudioPlay { file: "a".into() }.name(),
            StepKind::AudioWaitForSpeech {
                end_timeout_ms: 0,
                silence_duration_ms: 0,
                onset_duration_ms: 0,
            }
            .name(),
            StepKind::AudioWaitForSpeechStart {
                timeout_ms: 0,
                wait_after_start_ms: 0,
                onset_duration_ms: 0,
            }
            .name(),
            StepKind::SmsSend { to: "1".into(), body: "b".into() }.name(),
            // `wait`/`log` are reported bare but declared under their spec names.
            "control.wait",
            "control.log",
        ];
        let mut declared: Vec<&str> = IMPLEMENTED_STEPS.to_vec();
        let mut actual = every;
        declared.sort_unstable();
        actual.sort_unstable();
        assert_eq!(declared, actual, "manifest has drifted from the implemented steps");

        let m = manifest();
        assert_eq!(m["executor"], "dialf");
        assert_eq!(m["spec_version"], SPEC_VERSION);
        // Everything declared inline-orchestrated must also be a step we implement.
        for s in INLINE_ORCHESTRATED {
            assert!(IMPLEMENTED_STEPS.contains(s), "{s} is not implemented");
        }
    }

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
