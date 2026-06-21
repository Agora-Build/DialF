//! End-to-end audio pipeline test: WAV capture source -> resample -> ten-vad segmenter.
//!
//! Skips when ten-vad isn't linked (stub build). Uses the speech fixture vendored in the
//! ten-vad-sys crate.

use dialf::audio::backend::{CaptureSource, WavFileSource};
use dialf::audio::engine::run_wait_for_speech;
use dialf::audio::record::Recorder;
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

    let reason = run_wait_for_speech(&mut src, turn, None).expect("pipeline ran");
    eprintln!("pipeline end reason: {reason:?}");

    // The clip is ~7.6s and short, so it must not hit the 60s cap.
    assert_ne!(reason, EndReason::Timeout, "unexpected timeout on a 7.6s clip");
}

/// Recording: capturing the fixture writes the speech to rx.wav, silence to tx.wav, and a
/// mix; all three the same length. (No sound card needed — WavFileSource stands in.)
#[test]
fn records_rx_tx_and_mix() {
    if !dialf::vad_linked() {
        eprintln!("ten-vad not linked; skipping recording test");
        return;
    }
    let dir = std::env::temp_dir().join(format!("dialf-rec-it-{}", std::process::id()));
    let mut rec = Recorder::new(&dir, "call", true).expect("recorder");

    let mut src = WavFileSource::open(std::path::Path::new(FIXTURE)).expect("open fixture");
    let turn = TurnConfig {
        silence_duration_ms: 300,
        end_timeout_ms: 60_000,
        ..TurnConfig::default()
    };
    run_wait_for_speech(&mut src, turn, Some(&mut rec)).expect("pipeline ran");
    let out = rec.finish().expect("finish recording");

    let peak = |p: &std::path::Path| -> (usize, i32) {
        let mut s = WavFileSource::open(p).unwrap();
        let mut buf = vec![0i16; 8192];
        let (mut total, mut peak) = (0usize, 0i32);
        loop {
            let n = s.read(&mut buf).unwrap();
            if n == 0 {
                break;
            }
            total += n;
            for &x in &buf[..n] {
                peak = peak.max((x as i32).abs());
            }
        }
        (total, peak)
    };

    let (rx_len, rx_peak) = peak(&out.rx);
    let (tx_len, tx_peak) = peak(&out.tx);
    let (mix_len, mix_peak) = peak(out.mix.as_ref().unwrap());

    assert!(rx_len > 50_000, "rx too short: {rx_len}");
    assert!(rx_peak > 1_000, "rx should contain speech, peak={rx_peak}");
    assert_eq!(tx_len, rx_len, "legs must be aligned/equal length");
    assert_eq!(tx_peak, 0, "tx should be silence during a capture-only turn");
    assert_eq!(mix_len, rx_len);
    assert_eq!(mix_peak, rx_peak, "mix == rx when tx is silent");

    let _ = std::fs::remove_dir_all(&dir);
}
