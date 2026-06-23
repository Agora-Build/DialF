//! End-to-end audio pipeline test: WAV capture source -> resample -> ten-vad segmenter,
//! plus the full-duplex recorder driven from a WAV "card".
//!
//! The VAD test skips when ten-vad isn't linked (stub build). Uses the speech fixture
//! vendored in the ten-vad-sys crate.

use std::path::Path;

use dialf::audio::backend::{CaptureSource, WavFileSink, WavFileSource};
use dialf::audio::engine::run_wait_for_speech;
use dialf::audio::record::{DuplexSession, RECORD_RATE};
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

    let mut src = WavFileSource::open(Path::new(FIXTURE)).expect("open fixture");

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

/// Full-duplex recording: a WAV "card" (the fixture) is captured continuously to rx.wav,
/// tx.wav stays silent (we inject nothing), and the mix equals rx. All three the same
/// length. No sound card and no VAD needed — recording is independent of VAD.
#[test]
fn records_rx_tx_and_mix() {
    let dir = std::env::temp_dir().join(format!("dialf-rec-it-{}", std::process::id()));
    std::fs::create_dir_all(&dir).expect("mkdir");
    let rx_path = dir.join("call-rx.wav");
    let tx_path = dir.join("call-tx.wav");
    let rx = WavFileSink::create(&rx_path, RECORD_RATE).expect("rx sink");
    let tx = WavFileSink::create(&tx_path, RECORD_RATE).expect("tx sink");
    let cap = WavFileSource::open(Path::new(FIXTURE)).expect("open fixture");

    // The fixture EOFs on its own, so finish() simply joins the capture thread after the
    // whole clip has been recorded (unblock is a no-op).
    let sess = DuplexSession::start(
        cap,
        rx,
        tx,
        rx_path.clone(),
        tx_path.clone(),
        dir.clone(),
        "call".into(),
        true,
        Box::new(|| {}),
    )
    .expect("start session");
    let out = sess.finish().expect("finish recording");

    let peak = |p: &Path| -> (usize, i32) {
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
    assert_eq!(tx_peak, 0, "tx should be silence when nothing is injected");
    assert_eq!(mix_len, rx_len);
    assert_eq!(mix_peak, rx_peak, "mix == rx when tx is silent");

    let _ = std::fs::remove_dir_all(&dir);
}
