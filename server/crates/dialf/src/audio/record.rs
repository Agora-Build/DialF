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

/// Pad `sink` with silence from `have` **frames** up to `want` (no-op if `want <= have`).
/// `scratch` is a reusable all-zero buffer. Writes `channels` samples per frame, so a
/// multi-channel leg stays frame-aligned.
fn pad_to(sink: &mut WavFileSink, have: u64, want: u64, scratch: &mut Vec<i16>) -> io::Result<()> {
    if want <= have {
        return Ok(());
    }
    let channels = sink.channels().max(1) as usize;
    if scratch.is_empty() {
        scratch.resize(4096, 0);
    }
    let mut remaining = (want - have) as usize * channels;
    while remaining > 0 {
        let chunk = remaining.min(scratch.len());
        sink.write(&scratch[..chunk])?;
        remaining -= chunk;
    }
    Ok(())
}

/// Write a 2-channel mix at `out`, at the legs' own sample rate. With `tx_left` (the
/// default) the layout is **left = tx (local), right = rx (remote)**; otherwise the two are
/// swapped. Each leg is reduced to its call channel (channel 0) so the mix stays 2ch
/// whatever the legs' channel counts are — the full multi-channel capture lives in rx.wav.
/// The shorter leg is zero-padded; both legs share the recording clock, so left/right line
/// up frame-for-frame, keeping the two voices separated for per-side analysis.
///
/// Streams both legs rather than buffering them: at 48 kHz stereo a 30-minute call is
/// ~345 MB per leg.
fn mix_wavs(tx: &Path, rx: &Path, out: &Path, tx_left: bool) -> anyhow::Result<()> {
    let mut tx_r =
        hound::WavReader::open(tx).with_context(|| format!("open {}", tx.display()))?;
    let mut rx_r =
        hound::WavReader::open(rx).with_context(|| format!("open {}", rx.display()))?;
    let (tx_spec, rx_spec) = (tx_r.spec(), rx_r.spec());
    if tx_spec.sample_rate != rx_spec.sample_rate {
        anyhow::bail!(
            "cannot mix legs recorded at different rates (tx {} Hz, rx {} Hz)",
            tx_spec.sample_rate,
            rx_spec.sample_rate
        );
    }
    let (tx_ch, rx_ch) = (tx_spec.channels.max(1) as usize, rx_spec.channels.max(1) as usize);
    // Call channel of each leg, frame by frame.
    let mut tx_mono = tx_r.samples::<i16>().step_by(tx_ch);
    let mut rx_mono = rx_r.samples::<i16>().step_by(rx_ch);

    let spec = hound::WavSpec {
        channels: 2,
        sample_rate: rx_spec.sample_rate,
        bits_per_sample: 16,
        sample_format: hound::SampleFormat::Int,
    };
    let mut w =
        hound::WavWriter::create(out, spec).with_context(|| format!("create {}", out.display()))?;
    loop {
        let t = tx_mono.next().transpose()?;
        let r = rx_mono.next().transpose()?;
        if t.is_none() && r.is_none() {
            break;
        }
        let (t, r) = (t.unwrap_or(0), r.unwrap_or(0));
        let (l, rr) = if tx_left { (t, r) } else { (r, t) };
        w.write_sample(l)?;
        w.write_sample(rr)?;
    }
    w.finalize().context("finalize mix.wav")?;
    Ok(())
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
        let rx = WavFileSink::create(&rx_path, RECORD_RATE, 1)?;
        let tx = WavFileSink::create(&tx_path, RECORD_RATE, 1)?;
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

    /// Append captured samples (interleaved) to rx and advance the clock; returns the new
    /// rx length in frames.
    pub fn push_rx(&mut self, samples: &[i16]) -> io::Result<u64> {
        self.rx.write(samples)?;
        self.rx_len += (samples.len() / self.rx.channels().max(1) as usize) as u64;
        Ok(self.rx_len)
    }

    /// Place `samples` (interleaved) on the tx leg starting at frame `offset` (padding tx
    /// with silence up to `offset` first).
    pub fn push_tx_at(&mut self, offset: u64, samples: &[i16]) -> io::Result<()> {
        pad_to(&mut self.tx, self.tx_len, offset, &mut self.silence)?;
        self.tx_len = self.tx_len.max(offset);
        self.tx.write(samples)?;
        self.tx_len += (samples.len() / self.tx.channels().max(1) as usize) as u64;
        Ok(())
    }

    /// Current rx clock (frames).
    pub fn rx_len(&self) -> u64 {
        self.rx_len
    }

    /// Current tx length (frames).
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
        // These frames are produced by the capture thread's VAD branch, which always
        // resamples to the VAD rate. Deliberately not a record constant: rx.wav's rate is
        // now the card's, and tying the two together would silently double-resample.
        crate::audio::vad::VAD_SAMPLE_RATE
    }
}

/// Active, full-duplex recording session. A background thread records rx continuously; tx
/// is written from the job thread at the current rx offset. The rx sample count is the
/// master clock.
pub struct DuplexSession {
    tx: Option<WavFileSink>,
    tx_len: u64,
    /// Shape shared by both legs — the capture's own rate/channels.
    rate: u32,
    channels: u16,
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
        let src_channels = capture.channels().max(1) as usize;
        // The sink header must describe the stream byte-for-byte, or the file is silently
        // wrong (half speed / swapped channels) with nothing to show for it.
        if rx.channels().max(1) as usize != src_channels {
            anyhow::bail!(
                "rx sink has {} channel(s) but the capture yields {} — refusing to record a \
                 mislabeled file",
                rx.channels(),
                src_channels
            );
        }

        let join = std::thread::Builder::new()
            .name("dialf-capture".into())
            .spawn(move || -> io::Result<(WavFileSink, u64)> {
                // Best-effort: raise this thread's scheduling priority so the capture pipe is
                // drained promptly on a loaded host. Non-fatal if denied (Linux needs
                // privileges/rtprio; macOS raises the QoS class).
                let _ = thread_priority::set_current_thread_priority(
                    thread_priority::ThreadPriority::Max,
                );
                // rx is written straight through at the capture's own rate/channels — the
                // recording keeps everything the card gave us. The VAD, which only accepts
                // 16 kHz mono, gets its own reduced copy below.
                let mut rs = Resampler16k::new(src_rate);
                let mut buf = vec![0i16; 8192];
                let mut mono: Vec<i16> = Vec::new();
                let mut carry: Vec<i16> = Vec::new();
                let mut whole_buf: Vec<i16> = Vec::new();
                let mut total: u64 = 0;
                let mut was_active = false;
                loop {
                    let n = capture.read(&mut buf)?;
                    if n == 0 {
                        break; // EOF / capture stopped
                    }
                    // Write whole frames only. Sources are contracted to return whole
                    // frames, but a half frame reaching the sink would swap the channels of
                    // every frame after it — carry the remainder instead of trusting.
                    let chunk: &[i16] = if carry.is_empty() && n % src_channels == 0 {
                        &buf[..n]
                    } else {
                        carry.extend_from_slice(&buf[..n]);
                        let whole = (carry.len() / src_channels) * src_channels;
                        whole_buf.clear();
                        whole_buf.extend_from_slice(&carry[..whole]);
                        carry.drain(..whole);
                        &whole_buf
                    };
                    if chunk.is_empty() {
                        continue;
                    }
                    rx.write(chunk)?;
                    total += (chunk.len() / src_channels) as u64;
                    rx_len_bg.store(total, Ordering::Relaxed);
                    let active = vad_active_bg.load(Ordering::Relaxed);
                    if active && !was_active {
                        // New turn: the audio between turns never reached the resampler, so
                        // its history/phase are stale — start clean rather than convolving
                        // the gap into the first hops of this turn.
                        rs = Resampler16k::new(src_rate);
                    }
                    was_active = active;
                    if active {
                        // Reduce to the call channel, then to 16 kHz.
                        mono.clear();
                        mono.extend(chunk.iter().step_by(src_channels).copied());
                        let frame = rs.process(&mono);
                        if !frame.is_empty() {
                            // Non-blocking: never wedge on a full/absent consumer (which
                            // would also block finish()'s join). See VAD_CHANNEL_BOUND.
                            let _ = vad_tx.try_send(frame);
                        }
                    }
                }
                Ok((rx, total))
            })
            .context("spawn capture thread")?;

        Ok(Self {
            tx: Some(tx),
            tx_len: 0,
            rate: src_rate,
            channels: src_channels as u16,
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

    /// Sample rate of both legs (the capture's own rate).
    pub fn sample_rate(&self) -> u32 {
        self.rate
    }

    /// Channel count of both legs (the capture's own).
    pub fn channels(&self) -> u16 {
        self.channels
    }

    /// Current rx clock (frames captured so far).
    pub fn rx_len(&self) -> u64 {
        self.rx_len.load(Ordering::Relaxed)
    }

    /// Place `samples` (interleaved, tx's channel count) on the tx leg anchored at the
    /// current rx frame offset (pad tx with silence up to it first). Call once per prompt so
    /// the whole prompt lands at one offset. Both legs share the frame clock, which is only
    /// meaningful while they have the same sample rate — `start` enforces the shape.
    pub fn push_tx(&mut self, samples: &[i16]) -> io::Result<()> {
        let target = self.rx_len();
        let tx = self.tx.as_mut().expect("tx sink present until finish");
        let ch = tx.channels().max(1) as usize;
        pad_to(tx, self.tx_len, target, &mut self.silence)?;
        self.tx_len = self.tx_len.max(target);
        tx.write(samples)?;
        self.tx_len += (samples.len() / ch) as u64;
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
        // Both counters are FRAMES; pad_to scales by each sink's own channel count.
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
fn read_i16(path: &Path) -> anyhow::Result<Vec<i16>> {
    let mut r = hound::WavReader::open(path).with_context(|| format!("open {}", path.display()))?;
    Ok(r.samples::<i16>().collect::<Result<Vec<_>, _>>()?)
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
        let mut s = WavFileSink::create(&p, RECORD_RATE, 1).unwrap();
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

    /// Interleaved multi-channel fake at an arbitrary rate.
    struct FakeStereo {
        chunks: std::vec::IntoIter<Vec<i16>>,
        rate: u32,
        channels: u16,
    }
    impl CaptureSource for FakeStereo {
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
            self.rate
        }
        fn channels(&self) -> u16 {
            self.channels
        }
    }

    /// Read a WAV and return (samples, rate, channels).
    fn read_spec(path: &Path) -> (Vec<i16>, u32, u16) {
        let mut r = hound::WavReader::open(path).unwrap();
        let spec = r.spec();
        let s = r.samples::<i16>().collect::<Result<Vec<_>, _>>().unwrap();
        (s, spec.sample_rate, spec.channels)
    }

    /// Wait for the capture thread to reach `frames`, failing fast instead of hanging.
    fn await_frames(sess: &DuplexSession, frames: u64) {
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(5);
        while sess.rx_len() < frames {
            assert!(
                std::time::Instant::now() < deadline,
                "capture stalled at {} frames, wanted {frames}",
                sess.rx_len()
            );
            std::thread::sleep(std::time::Duration::from_millis(2));
        }
    }

    fn stereo_session(dir: &Path, rate: u32, chunks: Vec<Vec<i16>>, mix: bool) -> DuplexSession {
        std::fs::create_dir_all(dir).unwrap();
        let rx_path = dir.join("s-rx.wav");
        let tx_path = dir.join("s-tx.wav");
        let rx = WavFileSink::create(&rx_path, rate, 2).unwrap();
        let tx = WavFileSink::create(&tx_path, rate, 2).unwrap();
        let cap = FakeStereo { chunks: chunks.into_iter(), rate, channels: 2 };
        DuplexSession::start(
            cap,
            rx,
            tx,
            rx_path,
            tx_path,
            dir.to_path_buf(),
            "s".to_string(),
            mix,
            true,
            Box::new(|| {}),
        )
        .unwrap()
    }

    /// The sharpest guard against the samples-vs-frames slip: tx's leading silence must be
    /// measured in FRAMES of the rx clock. A samples-based counter doubles it at stereo.
    #[test]
    fn push_tx_offset_is_in_frames() {
        let dir = tmp().join("frames");
        // 100 stereo frames = 200 interleaved samples.
        let mut sess = stereo_session(&dir, 48_000, vec![vec![1i16; 200]], false);
        await_frames(&sess, 100);
        // One stereo prompt frame.
        sess.push_tx(&[9, 9]).unwrap();
        let out = sess.finish().unwrap();
        let (tx, _, ch) = read_spec(&out.tx);
        assert_eq!(ch, 2);
        // 100 frames of silence = 200 samples, THEN the prompt.
        assert_eq!(&tx[..200], vec![0i16; 200].as_slice(), "leading silence must be 100 frames");
        assert_eq!(tx[200], 9);
        assert_eq!(tx[201], 9);
    }

    /// Distinct L/R values — equal values would hide every interleave bug in the pipeline.
    #[test]
    fn stereo_capture_is_written_through_unswapped() {
        let dir = tmp().join("stereo-rt");
        let chunk: Vec<i16> = (0..50).flat_map(|_| [1000i16, -1000]).collect();
        let sess = stereo_session(&dir, 48_000, vec![chunk], false);
        await_frames(&sess, 50);
        let out = sess.finish().unwrap();
        let (rx, rate, ch) = read_spec(&out.rx);
        assert_eq!((rate, ch), (48_000, 2), "rx keeps the capture's rate and channels");
        assert_eq!(rx.len(), 100);
        assert!(rx.iter().step_by(2).all(|&s| s == 1000), "left channel");
        assert!(rx.iter().skip(1).step_by(2).all(|&s| s == -1000), "right channel");
    }

    /// Every leg must describe the same DURATION — catches a frames/samples slip anywhere,
    /// whichever of the nine arithmetic sites caused it.
    #[test]
    fn all_legs_have_equal_duration() {
        let dir = tmp().join("dur");
        let chunk: Vec<i16> = (0..80).flat_map(|_| [500i16, -500]).collect();
        let mut sess = stereo_session(&dir, 48_000, vec![chunk], true);
        await_frames(&sess, 80);
        sess.push_tx(&[7, 7, 7, 7]).unwrap(); // 2 stereo frames
        let out = sess.finish().unwrap();
        let secs = |p: &Path| {
            let (s, rate, ch) = read_spec(p);
            s.len() as f64 / ch as f64 / rate as f64
        };
        let (rx_s, tx_s) = (secs(&out.rx), secs(&out.tx));
        assert!((rx_s - tx_s).abs() < 1e-9, "rx {rx_s}s vs tx {tx_s}s");
        let mix_s = secs(out.mix.as_ref().unwrap());
        assert!((rx_s - mix_s).abs() < 1e-9, "rx {rx_s}s vs mix {mix_s}s");
    }

    /// The mix stays 2ch at the legs' native rate, one call channel per leg.
    #[test]
    fn mix_of_stereo_legs_is_two_channel_at_native_rate() {
        let dir = tmp().join("mix-stereo");
        let chunk: Vec<i16> = (0..30).flat_map(|_| [111i16, -999]).collect();
        let mut sess = stereo_session(&dir, 48_000, vec![chunk], true);
        await_frames(&sess, 30);
        sess.push_tx(&[222, -888]).unwrap(); // 1 stereo frame at the end
        let out = sess.finish().unwrap();
        let (mix, rate, ch) = read_spec(out.mix.as_ref().unwrap());
        assert_eq!((rate, ch), (48_000, 2), "mix follows the legs' rate, stays 2ch");
        // tx_left: left = tx call channel (silence, then 222), right = rx call channel (111).
        assert!(mix.iter().skip(1).step_by(2).take(30).all(|&s| s == 111), "right = rx ch0");
        assert_eq!(mix[0], 0, "tx silent before the prompt");
        assert_eq!(mix[30 * 2], 222, "tx prompt lands at the rx frame offset");
    }

    /// A chunk that ends mid-frame must not swap the channels for the rest of the file.
    #[test]
    fn odd_length_chunks_keep_channel_polarity() {
        let dir = tmp().join("odd");
        // 101 then 99 samples: the first chunk ends halfway through a frame, so the thread
        // must carry that sample rather than writing it (which would swap L/R from then on).
        // Interleaving is continuous across the split: even index = L (+1000), odd = R.
        let odd: Vec<i16> = (0..101).map(|i| if i % 2 == 0 { 1000 } else { -1000 }).collect();
        let rest: Vec<i16> = (0..99).map(|i| if (i + 101) % 2 == 0 { 1000 } else { -1000 }).collect();
        let sess = stereo_session(&dir, 48_000, vec![odd, rest], false);
        await_frames(&sess, 100);
        let out = sess.finish().unwrap();
        let (rx, _, ch) = read_spec(&out.rx);
        assert_eq!(ch, 2);
        // 200 samples in, all whole frames: L always +1000, R always -1000.
        assert_eq!(rx.len() % 2, 0, "file must hold whole frames");
        assert!(rx.iter().step_by(2).all(|&s| s == 1000), "left stayed left");
        assert!(rx.iter().skip(1).step_by(2).all(|&s| s == -1000), "right stayed right");
    }

    /// A sink whose header disagrees with the stream would be silently wrong audio.
    #[test]
    fn mismatched_sink_shape_is_rejected() {
        let dir = tmp().join("mismatch");
        std::fs::create_dir_all(&dir).unwrap();
        let rx_path = dir.join("m-rx.wav");
        let tx_path = dir.join("m-tx.wav");
        let rx = WavFileSink::create(&rx_path, 48_000, 1).unwrap(); // mono sink...
        let tx = WavFileSink::create(&tx_path, 48_000, 1).unwrap();
        let cap = FakeStereo { chunks: vec![vec![0i16; 8]].into_iter(), rate: 48_000, channels: 2 };
        let err = DuplexSession::start(
            cap, rx, tx, rx_path, tx_path, dir, "m".to_string(), false, true, Box::new(|| {}),
        )
        .err()
        .expect("a mono sink must refuse a stereo capture");
        assert!(err.to_string().contains("channel"), "{err}");
    }

    #[test]
    fn vad_frame_source_reports_the_vad_rate() {
        // Pinned: tying this to a record constant would silently double-resample once
        // rx.wav moved to the card's rate.
        let (_tx, mut rx) = sync_channel::<Vec<i16>>(1);
        let src = VadFrameSource::new(&mut rx);
        assert_eq!(src.sample_rate(), crate::audio::vad::VAD_SAMPLE_RATE);
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
        let rx = WavFileSink::create(&rx_path, RECORD_RATE, 1).unwrap();
        let tx = WavFileSink::create(&tx_path, RECORD_RATE, 1).unwrap();
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
