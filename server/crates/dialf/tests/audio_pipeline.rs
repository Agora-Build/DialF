//! End-to-end audio pipeline test: WAV capture source -> resample -> ten-vad segmenter.
//!
//! Skips when ten-vad isn't linked (stub build). Uses the speech fixture vendored in the
//! ten-vad-sys crate.

use dialf::audio::backend::WavFileSource;
use dialf::audio::engine::run_wait_for_speech;
use dialf::audio::vad::{EndReason, TurnConfig};

const FIXTURE: &str = concat!(
    env!("CARGO_MANIFEST_DIR"),
    "/../ten-vad-sys/tests/fixtures/speech_16k.wav"
);

#[test]
fn wait_for_speech_runs_over_real_clip() {
    if !dialf::vad_linked() {
        eprintln!("ten-vad not linked (stub build); skipping pipeline test");
        return;
    }

    let mut src = WavFileSource::open(std::path::Path::new(FIXTURE)).expect("open fixture");

    // Short trailing-silence threshold so internal gaps or EOF end the turn quickly.
    let turn = TurnConfig {
        silence_duration_ms: 300,
        end_timeout_ms: 60_000,
        ..TurnConfig::default()
    };

    let reason = run_wait_for_speech(&mut src, turn).expect("pipeline ran");
    eprintln!("pipeline end reason: {reason:?}");

    // The clip is ~7.6s and short, so it must not hit the 60s cap.
    assert_ne!(reason, EndReason::Timeout, "unexpected timeout on a 7.6s clip");
}
