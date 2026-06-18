//! Smoke test for the real ten-vad library.
//!
//! Skips automatically when the crate was built without a linked lib (stub mode), so it
//! stays green on a clean checkout. When linked (vendor/lib present or TEN_VAD_LIB_DIR
//! set), it runs VAD over a real 16 kHz speech clip and asserts a sane mix of voiced and
//! non-voiced frames.

#[test]
fn smoke_detects_speech_and_silence() {
    if !ten_vad_sys::is_linked() {
        eprintln!("ten-vad not linked (stub build); skipping smoke test");
        return;
    }

    let path = concat!(env!("CARGO_MANIFEST_DIR"), "/tests/fixtures/speech_16k.wav");
    let mut reader = hound::WavReader::open(path).expect("open fixture wav");
    let spec = reader.spec();
    assert_eq!(spec.sample_rate, 16_000, "fixture must be 16 kHz");
    assert_eq!(spec.channels, 1, "fixture must be mono");

    let samples: Vec<i16> = reader
        .samples::<i16>()
        .collect::<Result<_, _>>()
        .expect("read samples");

    let hop = ten_vad_sys::HOP_256;
    let mut vad = ten_vad_sys::TenVad::new(hop, 0.5).expect("create ten-vad");

    let mut total = 0usize;
    let mut voiced = 0usize;
    let mut prob_sum = 0.0f32;
    for frame in samples.chunks_exact(hop) {
        let r = vad.process(frame).expect("process frame");
        total += 1;
        prob_sum += r.probability;
        if r.voiced {
            voiced += 1;
        }
    }

    eprintln!(
        "ten-vad smoke: frames={total} voiced={voiced} ({:.0}%) avg_prob={:.3}",
        100.0 * voiced as f32 / total as f32,
        prob_sum / total as f32
    );

    assert!(total > 100, "expected many frames, got {total}");
    assert!(voiced > 0, "expected some voiced frames");
    assert!(voiced < total, "expected some non-voiced frames too");
}
