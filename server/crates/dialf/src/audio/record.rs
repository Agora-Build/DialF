//! Call recording: full-duplex, on a single clock, for latency measurement.
//!
//! - **rx** — audio captured from the sound card (the phone's earpiece / far end).
//! - **tx** — audio we injected into the card (our prompts / TTS).
//!
//! rx is recorded **continuously** for the whole job by a background capture thread —
//! including while we play tx and during `wait`/dial gaps. tx is written at its true
//! offset on the same timeline (silence elsewhere). The **master clock is the rx sample
//! count** (driven by the capture card, so there is no wall-clock drift). On
//! [`DuplexSession::finish`] both legs are padded to the same length and an optional stereo
//! `*-mix.wav` is produced with **left = tx, right = rx**. rx, tx, and the mix are the same
//! length and sample-aligned, so a tx↔rx cross-correlation yields round-trip (echo) latency,
//! and the gap between a tx prompt and the rx reply yields response latency.
//!
//! Note on scheduling: the real capture timing is owned by the external recording tool +
//! the OS audio driver; the background thread here only drains that tool's stdout pipe and
//! writes the WAV. It does not set a real-time thread priority (that would need extra
//! privileges / a dependency); to harden a heavily loaded host, raise `dialfd`'s priority
//! at the OS level (`nice` / launchd QoS).

use std::io;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::mpsc::{sync_channel, Receiver, RecvTimeoutError};
use std::sync::Arc;
use std::thread::JoinHandle;
use std::time::Duration;

use anyhow::{anyhow, Context};

use crate::audio::backend::{CaptureSource, PlaybackSink, WavFileSink};
use crate::audio::resample::Resampler16k;

/// Recording sample rate (matches the VAD path).
pub const RECORD_RATE: u32 = 16_000;

/// Bound on the VAD frame channel. The bg thread forwards with `try_send` (never blocks),
/// so it can never wedge waiting on a stalled/absent consumer. During an active turn the
/// consumer (resample-passthrough + native VAD) far outpaces real time, so the channel
/// stays near-empty and nothing is dropped; a drop can only happen once we've stopped
/// consuming (turn ended / between turns), where the frame is irrelevant. rx is written
/// before the send attempt, so recording is unaffected regardless.
const VAD_CHANNEL_BOUND: usize = 64;

/// How long [`VadFrameSource::read`] waits for a frame before reporting `WouldBlock`, so the
/// caller can apply its own stall deadline instead of blocking forever on a dead capture.
const VAD_RECV_POLL: Duration = Duration::from_millis(200);

/// Paths produced by a finished recording.
#[derive(Debug, Clone)]
pub struct RecordOutput {
    pub rx: PathBuf,
    pub tx: PathBuf,
    pub mix: Option<PathBuf>,
}

/// Pad `sink` with silence from `have` samples up to `want` (no-op if `want <= have`).
/// `scratch` is a reusable all-zero buffer.
fn pad_to(sink: &mut WavFileSink, have: u64, want: u64, scratch: &mut Vec<i16>) -> io::Result<()> {
    if want <= have {
        return Ok(());
    }
    if scratch.is_empty() {
        scratch.resize(4096, 0);
    }
    let mut remaining = (want - have) as usize;
    while remaining > 0 {
        let chunk = remaining.min(scratch.len());
        sink.write(&scratch[..chunk])?;
        remaining -= chunk;
    }
    Ok(())
}

/// Write a stereo 16 kHz mix at `out`. With `tx_left` (the default) the layout is
/// **left = tx (local), right = rx (remote)**; otherwise the two channels are swapped. The
/// shorter leg is zero-padded. Both legs share the recording clock, so left/right line up
/// sample-for-sample — keeping the two voices separated for per-side analysis.
fn mix_wavs(tx: &Path, rx: &Path, out: &Path, tx_left: bool) -> anyhow::Result<()> {
    let tx_s = read_i16(tx)?;
    let rx_s = read_i16(rx)?;
    let (left, right): (&[i16], &[i16]) = if tx_left {
        (&tx_s, &rx_s)
    } else {
        (&rx_s, &tx_s)
    };
    let n = left.len().max(right.len());
    let spec = hound::WavSpec {
        channels: 2,
        sample_rate: RECORD_RATE,
        bits_per_sample: 16,
        sample_format: hound::SampleFormat::Int,
    };
    let mut w =
        hound::WavWriter::create(out, spec).with_context(|| format!("create {}", out.display()))?;
    for i in 0..n {
        w.write_sample(*left.get(i).unwrap_or(&0))?;
        w.write_sample(*right.get(i).unwrap_or(&0))?;
    }
    w.finalize().context("finalize mix.wav")?;
    Ok(())
}

fn read_i16(path: &Path) -> anyhow::Result<Vec<i16>> {
    let mut r = hound::WavReader::open(path).with_context(|| format!("open {}", path.display()))?;
    Ok(r.samples::<i16>().collect::<Result<Vec<_>, _>>()?)
}

/// Passive, single-threaded, deterministic recorder. Used for tests and as the shared
/// padding/mix logic; the live path uses [`DuplexSession`].
pub struct DuplexRecorder {
    rx: WavFileSink,
    tx: WavFileSink,
    rx_path: PathBuf,
    tx_path: PathBuf,
    dir: PathBuf,
    session: String,
    mix: bool,
    mix_tx_left: bool,
    rx_len: u64,
    tx_len: u64,
    silence: Vec<i16>,
}

impl DuplexRecorder {
    /// Create a recorder writing `<dir>/<session>-rx.wav` and `-tx.wav` (and, if `mix`,
    /// `-mix.wav` on finish).
    pub fn new(
        dir: impl Into<PathBuf>,
        session: impl Into<String>,
        mix: bool,
        mix_tx_left: bool,
    ) -> anyhow::Result<Self> {
        let dir = dir.into();
        let session = session.into();
        std::fs::create_dir_all(&dir)
            .with_context(|| format!("create record dir {}", dir.display()))?;
        let rx_path = dir.join(format!("{session}-rx.wav"));
        let tx_path = dir.join(format!("{session}-tx.wav"));
        let rx = WavFileSink::create(&rx_path, RECORD_RATE)?;
        let tx = WavFileSink::create(&tx_path, RECORD_RATE)?;
        Ok(Self {
            rx,
            tx,
            rx_path,
            tx_path,
            dir,
            session,
            mix,
            mix_tx_left,
            rx_len: 0,
            tx_len: 0,
            silence: vec![0i16; 4096],
        })
    }

    /// Append captured samples to rx and advance the clock; returns the new rx length.
    pub fn push_rx(&mut self, samples: &[i16]) -> io::Result<u64> {
        self.rx.write(samples)?;
        self.rx_len += samples.len() as u64;
        Ok(self.rx_len)
    }

    /// Place `samples` on the tx leg starting at sample `offset` (padding tx with silence
    /// up to `offset` first).
    pub fn push_tx_at(&mut self, offset: u64, samples: &[i16]) -> io::Result<()> {
        pad_to(&mut self.tx, self.tx_len, offset, &mut self.silence)?;
        self.tx_len = self.tx_len.max(offset);
        self.tx.write(samples)?;
        self.tx_len += samples.len() as u64;
        Ok(())
    }

    /// Current rx clock (samples).
    pub fn rx_len(&self) -> u64 {
        self.rx_len
    }

    /// Current tx length (samples).
    pub fn tx_len(&self) -> u64 {
        self.tx_len
    }

    /// Pad both legs to equal length, finalize, and (if enabled) write the mix.
    pub fn finish(self) -> anyhow::Result<RecordOutput> {
        let DuplexRecorder {
            mut rx,
            mut tx,
            rx_path,
            tx_path,
            dir,
            session,
            mix,
            mix_tx_left,
            rx_len,
            tx_len,
            mut silence,
        } = self;
        let total = rx_len.max(tx_len);
        pad_to(&mut rx, rx_len, total, &mut silence)?;
        pad_to(&mut tx, tx_len, total, &mut silence)?;
        rx.finalize().context("finalize rx.wav")?;
        tx.finalize().context("finalize tx.wav")?;
        let mix_path = if mix {
            let p = dir.join(format!("{session}-mix.wav"));
            mix_wavs(&tx_path, &rx_path, &p, mix_tx_left).context("write mix.wav")?;
            Some(p)
        } else {
            None
        };
        Ok(RecordOutput {
            rx: rx_path,
            tx: tx_path,
            mix: mix_path,
        })
    }
}

/// A [`CaptureSource`] that yields the background thread's 16 kHz frames over a channel,
/// so [`crate::audio::engine::run_wait_for_speech`] can drive the VAD from the live
/// duplex capture. `read` blocks for the next frame; a disconnected channel (capture ended)
/// reports end-of-stream.
pub struct VadFrameSource<'a> {
    rx: &'a mut Receiver<Vec<i16>>,
    leftover: Vec<i16>,
    pos: usize,
}

impl<'a> VadFrameSource<'a> {
    /// Borrow the session's VAD frame receiver for one wait.
    pub fn new(rx: &'a mut Receiver<Vec<i16>>) -> Self {
        Self {
            rx,
            leftover: Vec::new(),
            pos: 0,
        }
    }
}

impl CaptureSource for VadFrameSource<'_> {
    fn read(&mut self, out: &mut [i16]) -> io::Result<usize> {
        if out.is_empty() {
            return Ok(0);
        }
        while self.pos >= self.leftover.len() {
            match self.rx.recv_timeout(VAD_RECV_POLL) {
                Ok(f) => {
                    self.leftover = f;
                    self.pos = 0;
                }
                // No frame yet: report `WouldBlock` so the caller can enforce a stall
                // deadline rather than block forever if the capture is dead.
                Err(RecvTimeoutError::Timeout) => {
                    return Err(io::Error::new(io::ErrorKind::WouldBlock, "no capture frame yet"));
                }
                // Capture ended (bg thread exited / sender dropped) -> end of stream.
                Err(RecvTimeoutError::Disconnected) => return Ok(0),
            }
        }
        let avail = self.leftover.len() - self.pos;
        let n = avail.min(out.len());
        out[..n].copy_from_slice(&self.leftover[self.pos..self.pos + n]);
        self.pos += n;
        Ok(n)
    }

    fn sample_rate(&self) -> u32 {
        RECORD_RATE
    }
}

/// Active, full-duplex recording session. A background thread records rx continuously; tx
/// is written from the job thread at the current rx offset. The rx sample count is the
/// master clock.
pub struct DuplexSession {
    tx: Option<WavFileSink>,
    tx_len: u64,
    rx_path: PathBuf,
    tx_path: PathBuf,
    dir: PathBuf,
    session: String,
    mix: bool,
    mix_tx_left: bool,
    rx_len: Arc<AtomicU64>,
    vad_active: Arc<AtomicBool>,
    vad_rx: Receiver<Vec<i16>>,
    /// Stops the capture (kills the child) so the bg thread's blocked read returns EOF.
    unblock: Box<dyn Fn() + Send>,
    join: Option<JoinHandle<io::Result<(WavFileSink, u64)>>>,
    silence: Vec<i16>,
}

impl DuplexSession {
    /// Spawn the background capture thread and start recording rx immediately. Generic over
    /// the capture source so tests can pass a fake (no hardware). `unblock` must stop the
    /// capture so the bg thread exits (kill the child for the real source; a no-op for a
    /// source that EOFs on its own).
    #[allow(clippy::too_many_arguments)]
    pub fn start<C: CaptureSource + Send + 'static>(
        mut capture: C,
        mut rx: WavFileSink,
        tx: WavFileSink,
        rx_path: PathBuf,
        tx_path: PathBuf,
        dir: PathBuf,
        session: String,
        mix: bool,
        mix_tx_left: bool,
        unblock: Box<dyn Fn() + Send>,
    ) -> anyhow::Result<Self> {
        let rx_len = Arc::new(AtomicU64::new(0));
        let vad_active = Arc::new(AtomicBool::new(false));
        let (vad_tx, vad_rx) = sync_channel::<Vec<i16>>(VAD_CHANNEL_BOUND);
        let rx_len_bg = rx_len.clone();
        let vad_active_bg = vad_active.clone();
        let src_rate = capture.sample_rate();

        let join = std::thread::Builder::new()
            .name("dialf-capture".into())
            .spawn(move || -> io::Result<(WavFileSink, u64)> {
                // Best-effort: raise this thread's scheduling priority so the capture pipe is
                // drained promptly on a loaded host. Non-fatal if denied (Linux needs
                // privileges/rtprio; macOS raises the QoS class).
                let _ = thread_priority::set_current_thread_priority(
                    thread_priority::ThreadPriority::Max,
                );
                let mut rs = Resampler16k::new(src_rate);
                let mut buf = vec![0i16; 8192];
                let mut total: u64 = 0;
                loop {
                    let n = capture.read(&mut buf)?;
                    if n == 0 {
                        break; // EOF / capture stopped
                    }
                    let frame = rs.process(&buf[..n]);
                    if frame.is_empty() {
                        continue;
                    }
                    rx.write(&frame)?;
                    total += frame.len() as u64;
                    rx_len_bg.store(total, Ordering::Relaxed);
                    if vad_active_bg.load(Ordering::Relaxed) {
                        // Non-blocking: never wedge on a full/absent consumer (which would
                        // also block finish()'s join). See VAD_CHANNEL_BOUND.
                        let _ = vad_tx.try_send(frame);
                    }
                }
                Ok((rx, total))
            })
            .context("spawn capture thread")?;

        Ok(Self {
            tx: Some(tx),
            tx_len: 0,
            rx_path,
            tx_path,
            dir,
            session,
            mix,
            mix_tx_left,
            rx_len,
            vad_active,
            vad_rx,
            unblock,
            join: Some(join),
            silence: vec![0i16; 4096],
        })
    }

    /// Current rx clock (samples captured so far).
    pub fn rx_len(&self) -> u64 {
        self.rx_len.load(Ordering::Relaxed)
    }

    /// Place `samples` on the tx leg anchored at the current rx offset (pad tx with silence
    /// up to it first). Call once per prompt so the whole prompt lands at one offset.
    pub fn push_tx(&mut self, samples: &[i16]) -> io::Result<()> {
        let target = self.rx_len();
        let tx = self.tx.as_mut().expect("tx sink present until finish");
        pad_to(tx, self.tx_len, target, &mut self.silence)?;
        self.tx_len = self.tx_len.max(target);
        tx.write(samples)?;
        self.tx_len += samples.len() as u64;
        Ok(())
    }

    /// Begin forwarding captured frames to the VAD consumer: drop any stale frames, then arm.
    pub fn vad_begin(&self) {
        while self.vad_rx.try_recv().is_ok() {}
        self.vad_active.store(true, Ordering::Relaxed);
    }

    /// Stop forwarding captured frames to the VAD consumer.
    pub fn vad_end(&self) {
        self.vad_active.store(false, Ordering::Relaxed);
    }

    /// The VAD frame receiver, for one [`VadFrameSource`] per wait.
    pub fn vad_receiver_mut(&mut self) -> &mut Receiver<Vec<i16>> {
        &mut self.vad_rx
    }

    /// Stop capture, pad both legs to equal length, finalize, and (if enabled) write the mix.
    pub fn finish(mut self) -> anyhow::Result<RecordOutput> {
        self.vad_active.store(false, Ordering::Relaxed);
        (self.unblock)(); // kill the capture so the bg read returns EOF
        let (mut rx, rx_len) = match self.join.take() {
            Some(h) => h
                .join()
                .map_err(|_| anyhow!("capture thread panicked"))??,
            None => anyhow::bail!("recording session already finished"),
        };
        let mut tx = self.tx.take().expect("tx sink present until finish");
        let total = rx_len.max(self.tx_len);
        let mut scratch = std::mem::take(&mut self.silence);
        pad_to(&mut rx, rx_len, total, &mut scratch)?;
        pad_to(&mut tx, self.tx_len, total, &mut scratch)?;
        rx.finalize().context("finalize rx.wav")?;
        tx.finalize().context("finalize tx.wav")?;
        let mix_path = if self.mix {
            let p = self.dir.join(format!("{}-mix.wav", self.session));
            mix_wavs(&self.tx_path, &self.rx_path, &p, self.mix_tx_left).context("write mix.wav")?;
            Some(p)
        } else {
            None
        };
        Ok(RecordOutput {
            rx: self.rx_path.clone(),
            tx: self.tx_path.clone(),
            mix: mix_path,
        })
    }
}

impl Drop for DuplexSession {
    fn drop(&mut self) {
        // Only runs if finish() wasn't called (e.g. a panic) — reap the capture child.
        if let Some(h) = self.join.take() {
            (self.unblock)();
            let _ = h.join();
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::Duration;

    fn tmp() -> PathBuf {
        std::env::temp_dir().join(format!("dialf-rectest-{}", std::process::id()))
    }

    #[test]
    fn tx_placed_at_offset_and_equal_length() {
        let dir = tmp().join("dr1");
        let mut r = DuplexRecorder::new(&dir, "s1", true, true).unwrap();
        r.push_rx(&[10i16; 100]).unwrap();
        r.push_tx_at(40, &[100i16; 20]).unwrap();
        r.push_rx(&[20i16; 50]).unwrap();
        let out = r.finish().unwrap();

        let rx = read_i16(&out.rx).unwrap();
        let tx = read_i16(&out.tx).unwrap();
        let mix = read_i16(out.mix.as_ref().unwrap()).unwrap();

        assert_eq!(rx.len(), 150);
        assert_eq!(tx.len(), 150);
        // rx = [10x100][20x50]
        assert_eq!(&rx[..100], &[10i16; 100][..]);
        assert_eq!(&rx[100..], &[20i16; 50][..]);
        // tx = [0x40][100x20][0x90]
        assert_eq!(&tx[..40], &[0i16; 40][..]);
        assert_eq!(&tx[40..60], &[100i16; 20][..]);
        assert_eq!(&tx[60..], &[0i16; 90][..]);
        // mix is stereo, interleaved: default layout is left = tx (local), right = rx (remote)
        let spec = hound::WavReader::open(out.mix.as_ref().unwrap())
            .unwrap()
            .spec();
        assert_eq!(spec.channels, 2);
        assert_eq!(mix.len(), 300); // 150 frames x 2 channels
        let left: Vec<i16> = mix.iter().step_by(2).copied().collect();
        let right: Vec<i16> = mix.iter().skip(1).step_by(2).copied().collect();
        assert_eq!(left, tx); // left  = tx
        assert_eq!(right, rx); // right = rx

        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn mix_channels_swap_puts_rx_left_tx_right() {
        let dir = tmp().join("dr_swap");
        // mix = true, mix_tx_left = false -> left = rx (remote), right = tx (local)
        let mut r = DuplexRecorder::new(&dir, "sw", true, false).unwrap();
        r.push_rx(&[11i16; 60]).unwrap();
        r.push_tx_at(0, &[99i16; 60]).unwrap();
        let out = r.finish().unwrap();

        let mix = read_i16(out.mix.as_ref().unwrap()).unwrap();
        let spec = hound::WavReader::open(out.mix.as_ref().unwrap())
            .unwrap()
            .spec();
        assert_eq!(spec.channels, 2);
        assert_eq!(mix.len(), 120); // 60 frames x 2 channels
        let left: Vec<i16> = mix.iter().step_by(2).copied().collect();
        let right: Vec<i16> = mix.iter().skip(1).step_by(2).copied().collect();
        assert_eq!(left, vec![11i16; 60]); // left  = rx (remote)
        assert_eq!(right, vec![99i16; 60]); // right = tx (local)

        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn tx_after_rx_pads_rx_to_max() {
        let dir = tmp().join("dr2");
        let mut r = DuplexRecorder::new(&dir, "s2", false, true).unwrap();
        r.push_rx(&[1i16; 100]).unwrap();
        r.push_tx_at(100, &[5i16; 30]).unwrap();
        let out = r.finish().unwrap();
        assert!(out.mix.is_none());
        let rx = read_i16(&out.rx).unwrap();
        let tx = read_i16(&out.tx).unwrap();
        assert_eq!(rx.len(), 130);
        assert_eq!(tx.len(), 130);
        assert_eq!(&rx[100..], &[0i16; 30][..]); // rx padded up to tx end
        assert_eq!(&tx[..100], &[0i16; 100][..]);
        assert_eq!(&tx[100..], &[5i16; 30][..]);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn empty_session_finishes() {
        let dir = tmp().join("dr3");
        let r = DuplexRecorder::new(&dir, "s3", true, true).unwrap();
        let out = r.finish().unwrap();
        assert_eq!(read_i16(&out.rx).unwrap().len(), 0);
        assert_eq!(read_i16(&out.tx).unwrap().len(), 0);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn pad_to_writes_zeros_and_is_noop_when_full() {
        let dir = tmp().join("dr4");
        std::fs::create_dir_all(&dir).unwrap();
        let p = dir.join("p.wav");
        let mut s = WavFileSink::create(&p, RECORD_RATE).unwrap();
        let mut scratch = Vec::new();
        pad_to(&mut s, 0, 5, &mut scratch).unwrap();
        pad_to(&mut s, 5, 5, &mut scratch).unwrap(); // no-op
        pad_to(&mut s, 5, 3, &mut scratch).unwrap(); // want < have: no-op
        s.finalize().unwrap();
        assert_eq!(read_i16(&p).unwrap(), vec![0i16; 5]);
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// A capture source that yields fixed chunks then EOFs — no hardware, no blocking.
    struct FakeCapture {
        chunks: std::vec::IntoIter<Vec<i16>>,
    }
    impl CaptureSource for FakeCapture {
        fn read(&mut self, out: &mut [i16]) -> io::Result<usize> {
            match self.chunks.next() {
                Some(c) => {
                    let n = c.len().min(out.len());
                    out[..n].copy_from_slice(&c[..n]);
                    Ok(n)
                }
                None => Ok(0),
            }
        }
        fn sample_rate(&self) -> u32 {
            RECORD_RATE
        }
    }

    #[test]
    fn vad_frame_source_would_block_then_delivers_then_eof() {
        let (tx, mut rx) = sync_channel::<Vec<i16>>(4);
        let mut src = VadFrameSource::new(&mut rx);
        let mut buf = [0i16; 256];
        // No frame yet but the sender is alive -> WouldBlock (not EOF).
        let err = src.read(&mut buf).unwrap_err();
        assert_eq!(err.kind(), io::ErrorKind::WouldBlock);
        // A frame arrives -> delivered.
        tx.send(vec![5i16; 100]).unwrap();
        assert_eq!(src.read(&mut buf).unwrap(), 100);
        // Sender dropped (capture ended) -> end of stream.
        drop(tx);
        assert_eq!(src.read(&mut buf).unwrap(), 0);
    }

    #[test]
    fn session_records_rx_and_aligns_tx() {
        let dir = tmp().join("sess");
        std::fs::create_dir_all(&dir).unwrap();
        let rx_path = dir.join("s-rx.wav");
        let tx_path = dir.join("s-tx.wav");
        let rx = WavFileSink::create(&rx_path, RECORD_RATE).unwrap();
        let tx = WavFileSink::create(&tx_path, RECORD_RATE).unwrap();
        let cap = FakeCapture {
            chunks: vec![vec![7i16; 100], vec![7i16; 100]].into_iter(),
        };
        let mut sess = DuplexSession::start(
            cap,
            rx,
            tx,
            rx_path.clone(),
            tx_path.clone(),
            dir.clone(),
            "s".into(),
            true,
            true,
            Box::new(|| {}),
        )
        .unwrap();

        // Wait for the fake capture to be fully consumed (rx clock reaches 200).
        let mut waited = 0;
        while sess.rx_len() < 200 && waited < 5000 {
            std::thread::sleep(Duration::from_millis(1));
            waited += 1;
        }
        assert_eq!(sess.rx_len(), 200);

        sess.push_tx(&[9i16; 50]).unwrap();
        let out = sess.finish().unwrap();

        let rx = read_i16(&out.rx).unwrap();
        let tx = read_i16(&out.tx).unwrap();
        assert_eq!(rx.len(), tx.len());
        assert_eq!(rx.len(), 250); // max(rx=200, tx=200+50)
        assert_eq!(&rx[..200], &[7i16; 200][..]);
        assert_eq!(&rx[200..], &[0i16; 50][..]);
        assert_eq!(&tx[..200], &[0i16; 200][..]); // tx anchored at rx offset 200
        assert_eq!(&tx[200..250], &[9i16; 50][..]);

        // the live session's mix is stereo, default layout left = tx / right = rx
        let mix = read_i16(out.mix.as_ref().unwrap()).unwrap();
        let spec = hound::WavReader::open(out.mix.as_ref().unwrap())
            .unwrap()
            .spec();
        assert_eq!(spec.channels, 2);
        assert_eq!(mix.len(), 500); // 250 frames x 2 channels
        let left: Vec<i16> = mix.iter().step_by(2).copied().collect();
        let right: Vec<i16> = mix.iter().skip(1).step_by(2).copied().collect();
        assert_eq!(left, tx); // left  = tx
        assert_eq!(right, rx); // right = rx

        let _ = std::fs::remove_dir_all(&dir);
    }
}
