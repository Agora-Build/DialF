//! End-to-end audio pipeline test: WAV capture source -> resample -> ten-vad segmenter,
//! plus the full-duplex recorder driven from a WAV "card".
//!
//! The VAD test skips when ten-vad isn't linked (stub build). Uses the speech fixture
//! vendored in the ten-vad-sys crate.

use std::path::Path;
use std::sync::atomic::AtomicBool;
use std::sync::mpsc::sync_channel;

use dialf::audio::backend::{CaptureSource, WavFileSink, WavFileSource};
use dialf::audio::engine::run_wait_for_speech;
use dialf::audio::record::{DuplexSession, VadFrameSource, RECORD_RATE};
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

    let reason = run_wait_for_speech(&mut src, turn, &AtomicBool::new(false)).expect("pipeline ran");
    eprintln!("pipeline end reason: {reason:?}");

    // The clip is ~7.6s and short, so it must not hit the 60s cap.
    assert_ne!(reason, EndReason::Timeout, "unexpected timeout on a 7.6s clip");
}

/// The duplex VAD path: frames pushed through the session's channel and consumed via
/// `VadFrameSource` must drive the segmenter exactly like a direct capture — speech onset
/// followed by trailing silence ends the turn with `Silence` (not `Timeout`/`EndOfStream`).
/// This exercises the path `wait_for_speech` uses while a recording session is active.
#[test]
fn wait_for_speech_over_vad_frame_channel() {
    if !dialf::vad_linked() {
        eprintln!("ten-vad not linked; skipping duplex VAD path test");
        return;
    }
    let (tx, mut rx) = sync_channel::<Vec<i16>>(10_000);

    // Push the speech fixture (16 kHz) as ~100 ms frames, then ~1 s of trailing silence.
    let mut fixture = WavFileSource::open(Path::new(FIXTURE)).expect("open fixture");
    let mut buf = vec![0i16; 1_600];
    loop {
        let n = fixture.read(&mut buf).expect("read fixture");
        if n == 0 {
            break;
        }
        tx.send(buf[..n].to_vec()).expect("send speech frame");
    }
    for _ in 0..10 {
        tx.send(vec![0i16; 1_600]).expect("send silence frame"); // 10 * 100 ms = 1 s
    }
    drop(tx); // disconnect after the data so a stuck run would EndOfStream, not hang

    let turn = TurnConfig {
        silence_duration_ms: 300, // ends well within the appended 1 s of silence
        end_timeout_ms: 60_000,
        ..TurnConfig::default()
    };
    let mut src = VadFrameSource::new(&mut rx);
    let reason =
        run_wait_for_speech(&mut src, turn, &AtomicBool::new(false)).expect("vad ran over channel");
    assert_eq!(
        reason,
        EndReason::Silence,
        "duplex VAD path should end the turn on trailing silence"
    );
}

/// A dead/stalled capture (channel open but no frames) must make `wait_for_speech` fail
/// with a clear error instead of hanging forever — the hop-based timeout can't fire without
/// frames, so the wall-clock stall guard has to.
#[test]
fn wait_for_speech_bails_on_dead_capture() {
    if !dialf::vad_linked() {
        eprintln!("ten-vad not linked; skipping dead-capture test");
        return;
    }
    let (tx, mut rx) = sync_channel::<Vec<i16>>(1); // keep tx alive: recv times out, not disconnects
    let mut src = VadFrameSource::new(&mut rx);
    let turn = TurnConfig {
        silence_duration_ms: 300,
        end_timeout_ms: 60_000,
        ..TurnConfig::default()
    };
    let err = run_wait_for_speech(&mut src, turn, &AtomicBool::new(false))
        .expect_err("dead capture must error, not hang");
    assert!(
        err.to_string().contains("no audio"),
        "expected a clear capture error, got: {err}"
    );
    drop(tx);
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
    let rx = WavFileSink::create(&rx_path, RECORD_RATE, 1).expect("rx sink");
    let tx = WavFileSink::create(&tx_path, RECORD_RATE, 1).expect("tx sink");
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
        true, // mix_tx_left: default layout (left = tx, right = rx)
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

    assert!(rx_len > 50_000, "rx too short: {rx_len}");
    assert!(rx_peak > 1_000, "rx should contain speech, peak={rx_peak}");
    assert_eq!(tx_len, rx_len, "legs must be aligned/equal length");
    assert_eq!(tx_peak, 0, "tx should be silence when nothing is injected");

    // mix is stereo (left = tx, right = rx). WavFileSource downmixes to the left channel, which is
    // tx here — silent, since nothing was injected. The full L/R layout is covered by the
    // record.rs unit tests (deterministic + live session).
    let (mix_len, mix_peak) = peak(out.mix.as_ref().unwrap());
    assert_eq!(mix_len, rx_len, "mix frame count matches the aligned legs");
    assert_eq!(mix_peak, 0, "mix left channel = tx (silent when nothing injected)");

    let _ = std::fs::remove_dir_all(&dir);
}

/// End-to-end through the REAL capture subprocess: a stereo byte stream on a pipe must land
/// in rx.wav at the card's own rate and channel count, channels un-swapped. Uses `cat` so it
/// runs anywhere (no sox/ffmpeg, no microphone).
#[test]
fn records_stereo_at_native_rate_through_a_real_pipe() {
    use dialf::audio::backend::WavFileSink;
    use dialf::audio::command_backend::CommandCaptureSource;
    use dialf::audio::record::DuplexSession;
    use dialf::audio::tool_detect::CaptureCommand;

    let dir = std::env::temp_dir().join(format!("dialf-stereo-it-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();

    // 1000 stereo frames: left = +4000, right = -4000, as raw little-endian s16.
    let raw = dir.join("cap.raw");
    let mut bytes = Vec::new();
    for _ in 0..1000 {
        bytes.extend_from_slice(&4000i16.to_le_bytes());
        bytes.extend_from_slice(&(-4000i16).to_le_bytes());
    }
    std::fs::write(&raw, &bytes).unwrap();

    let cmd = CaptureCommand {
        argv: vec!["cat".into(), raw.to_string_lossy().into_owned()],
    };
    let src = CommandCaptureSource::spawn(&cmd, 48_000, 2).expect("spawn capture");
    let rx_path = dir.join("s-rx.wav");
    let tx_path = dir.join("s-tx.wav");
    let rx = WavFileSink::create(&rx_path, 48_000, 2).unwrap();
    let tx = WavFileSink::create(&tx_path, 48_000, 2).unwrap();
    let sess = DuplexSession::start(
        src,
        rx,
        tx,
        rx_path.clone(),
        tx_path,
        dir.clone(),
        "s".to_string(),
        true,
        true,
        Box::new(|| {}),
    )
    .expect("session");

    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(5);
    while sess.rx_len() < 1000 {
        assert!(std::time::Instant::now() < deadline, "stalled at {} frames", sess.rx_len());
        std::thread::sleep(std::time::Duration::from_millis(5));
    }
    let out = sess.finish().expect("finish");

    let mut r = hound::WavReader::open(&out.rx).unwrap();
    let spec = r.spec();
    assert_eq!(
        (spec.sample_rate, spec.channels),
        (48_000, 2),
        "rx.wav must keep the capture's native shape"
    );
    let s: Vec<i16> = r.samples::<i16>().map(|x| x.unwrap()).collect();
    assert_eq!(s.len(), 2000, "1000 stereo frames");
    assert!(s.iter().step_by(2).all(|&v| v == 4000), "left channel intact");
    assert!(s.iter().skip(1).step_by(2).all(|&v| v == -4000), "right channel intact");

    // The mix collapses each leg to its call channel, at the same rate.
    let mix = hound::WavReader::open(out.mix.as_ref().unwrap()).unwrap();
    assert_eq!((mix.spec().sample_rate, mix.spec().channels), (48_000, 2));
    std::fs::remove_dir_all(&dir).ok();
}

/// The crux of the split: rx keeps the card's 48 kHz stereo, while the VAD branch still
/// receives 16 kHz MONO. Counts what reaches the VAD — a missing downmix would triple it
/// (interleaved read as mono) and a missing resample would triple it again, so every turn
/// timeout would fire at the wrong wall time.
#[test]
fn vad_gets_16k_mono_from_a_48k_stereo_capture() {
    use dialf::audio::backend::{CaptureSource, WavFileSink};
    use dialf::audio::command_backend::CommandCaptureSource;
    use dialf::audio::record::{DuplexSession, VadFrameSource};
    use dialf::audio::tool_detect::CaptureCommand;

    let dir = std::env::temp_dir().join(format!("dialf-vadmix-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();

    // 0.5 s of 48 kHz stereo = 24000 frames. Left carries a tone, right is silent, so a
    // downmix that averaged instead of picking the call channel would halve the amplitude.
    let raw = dir.join("cap.raw");
    let mut bytes = Vec::new();
    for i in 0..24_000 {
        let v = if (i / 24) % 2 == 0 { 6000i16 } else { -6000 };
        bytes.extend_from_slice(&v.to_le_bytes()); // left = tone
        bytes.extend_from_slice(&0i16.to_le_bytes()); // right = silence
    }
    std::fs::write(&raw, &bytes).unwrap();

    let cmd = CaptureCommand {
        argv: vec!["cat".into(), raw.to_string_lossy().into_owned()],
    };
    let src = CommandCaptureSource::spawn(&cmd, 48_000, 2).expect("spawn");
    let rx_path = dir.join("v-rx.wav");
    let tx_path = dir.join("v-tx.wav");
    let rx = WavFileSink::create(&rx_path, 48_000, 2).unwrap();
    let tx = WavFileSink::create(&tx_path, 48_000, 2).unwrap();
    let mut sess = DuplexSession::start(
        src, rx, tx, rx_path, tx_path, dir.clone(), "v".to_string(), false, true, Box::new(|| {}),
    )
    .expect("session");

    sess.vad_begin();
    let mut got = 0usize;
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(5);
    {
        let mut vsrc = VadFrameSource::new(sess.vad_receiver_mut());
        assert_eq!(vsrc.sample_rate(), 16_000, "the VAD branch must report 16 kHz");
        let mut buf = vec![0i16; 4096];
        while got < 6_000 && std::time::Instant::now() < deadline {
            match vsrc.read(&mut buf) {
                Ok(0) => break,
                Ok(n) => got += n,
                Err(e) if e.kind() == std::io::ErrorKind::WouldBlock => {}
                Err(e) => panic!("vad read: {e}"),
            }
        }
    }
    sess.vad_end();
    let out = sess.finish().expect("finish");

    // 24000 stereo frames at 48k = 0.5 s -> ~8000 mono samples at 16 kHz. Allow slack for
    // frames produced before vad_begin armed, but the ORDER OF MAGNITUDE is the assertion:
    // no downmix would give ~16000+, no resample ~24000+.
    assert!(
        (4_000..=9_000).contains(&got),
        "expected ~8000 16k-mono samples from 0.5s of 48k stereo, got {got}"
    );
    let rxr = hound::WavReader::open(&out.rx).unwrap();
    assert_eq!((rxr.spec().sample_rate, rxr.spec().channels), (48_000, 2), "rx stays native");
    std::fs::remove_dir_all(&dir).ok();
}
