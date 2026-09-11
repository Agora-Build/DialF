//! Audio engine: ties playback, capture, resampling, and VAD together.
//!
//! `audio.play` -> [`AudioEngine::play_file`]. `audio.wait_for_speech` ->
//! [`AudioEngine::wait_for_speech`], which captures from the card, resamples to 16 kHz,
//! frames into hops, and runs the [`Segmenter`] until the turn ends.
//!
//! All methods are synchronous; the daemon calls them on a blocking task.

use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::{Duration, Instant};

use crate::config::AudioConfig;

use super::backend::{CaptureSource, DownmixMono, WavFileSink, WavFileSource};
use super::command_backend::{self, CommandCaptureSource};
use super::record::{DuplexSession, VadFrameSource};
use super::resample::{Resampler, Resampler16k};
use super::tool_detect::{self, AudioParams};
use super::vad::{EndReason, Segmenter, TurnConfig, TurnEvent};

/// Owns audio configuration and resolves tools on demand.
pub struct AudioEngine {
    cfg: AudioConfig,
}

impl AudioEngine {
    /// Create an engine from audio config.
    pub fn new(cfg: AudioConfig) -> Self {
        Self { cfg }
    }

    fn capture_params(&self) -> AudioParams {
        AudioParams {
            rate: self.cfg.sample_rate,
            channels: self.cfg.channels,
            device: self.cfg.capture_device.clone(),
        }
    }

    fn playback_params(&self) -> AudioParams {
        AudioParams {
            rate: self.cfg.sample_rate,
            channels: self.cfg.channels,
            device: self.cfg.playback_device.clone(),
        }
    }

    /// Play an audio file out the sound card (blocking until done). If `sess` is set, the
    /// file's audio (converted to the tx leg's shape) is also written to the tx leg,
    /// anchored at the current rx clock so it aligns with what the capture records.
    pub fn play_file(
        &self,
        file: &Path,
        sess: Option<&mut DuplexSession>,
        force: &AtomicBool,
    ) -> anyhow::Result<()> {
        tracing::info!(target: "job", "audio.play: {}", file.display());
        if let Some(s) = sess {
            tee_tx(s, file)?;
        }
        let file_str = file.to_string_lossy().to_string();
        let cmd = tool_detect::resolve_playback_file(
            &file_str,
            &self.playback_params(),
            self.cfg.playback_cmd.as_deref(),
        )?;
        if cmd.via_stdin {
            anyhow::bail!("configured playback_cmd reads stdin; use a {{file}} template for audio.play");
        }
        command_backend::play_file_blocking(&cmd, force)?;
        Ok(())
    }

    /// Open the configured sound-card capture source.
    pub fn open_capture(&self) -> anyhow::Result<CommandCaptureSource> {
        // macOS: settle mic consent BEFORE spawning the tool — shows the dialog and waits
        // for the click if never asked; fails fast with the fix if denied. The tool's own
        // implicit access would just be silently denied (empty capture, vague timeout).
        super::mic_permission::ensure_consent()?;
        let cmd = tool_detect::resolve_capture(&self.capture_params(), self.cfg.capture_cmd.as_deref())?;
        let src = CommandCaptureSource::spawn(&cmd, self.cfg.sample_rate, self.cfg.channels)?;
        Ok(src)
    }

    /// Start a full-duplex recording session: spawn the capture tool, create the rx/tx
    /// sinks, and begin recording rx continuously. The bg thread is stopped (capture child
    /// killed) on [`DuplexSession::finish`].
    pub fn start_duplex(
        &self,
        dir: PathBuf,
        session_name: String,
        mix: bool,
        mix_tx_left: bool,
    ) -> anyhow::Result<DuplexSession> {
        std::fs::create_dir_all(&dir)
            .map_err(|e| anyhow::anyhow!("create record dir {}: {e}", dir.display()))?;
        let source = self.open_capture()?;
        let killer = source.kill_handle();
        let unblock = Box::new(move || {
            if let Ok(mut child) = killer.lock() {
                let _ = child.kill();
            }
        });
        let rx_path = dir.join(format!("{session_name}-rx.wav"));
        let tx_path = dir.join(format!("{session_name}-tx.wav"));
        // Record exactly what the card gives us — no resampling, nothing discarded. Both
        // legs share the shape so they share one frame clock and can be mixed; the VAD gets
        // its own 16 kHz mono copy inside the capture thread.
        let (rate, channels) = (source.sample_rate(), source.channels());
        let rx = WavFileSink::create(&rx_path, rate, channels)?;
        let tx = WavFileSink::create(&tx_path, rate, channels)?;
        let session = DuplexSession::start(
            source, rx, tx, rx_path, tx_path, dir, session_name, mix, mix_tx_left, unblock,
        )?;

        // The capture tool opens even when the mic is denied (macOS) or the card is dead,
        // but then yields no frames — which would otherwise stall playback on the same
        // device for minutes. Verify the first frame arrives quickly; a healthy capture
        // produces audio within a few hundred ms, so 3s with no data means it's dead.
        let deadline = Instant::now() + Duration::from_millis(3_000);
        while session.rx_len() == 0 {
            if Instant::now() >= deadline {
                // Permission problems are caught upfront by mic_permission::ensure_consent,
                // so a silent capture here is almost always the device itself.
                tracing::error!(
                    "capture produced no audio — the capture device delivered no frames: check \
                     the sound card is connected and `audio.capture_device` matches its EXACT \
                     name (see the capture tool's error output above)"
                );
                let _ = session.finish(); // kills the capture child, finalizes empty files
                anyhow::bail!(
                    "capture produced no audio (3s) — the capture device delivered no frames: \
                     check the sound card is connected and `audio.capture_device` matches its \
                     EXACT name (the daemon log has the capture tool's error output)"
                );
            }
            std::thread::sleep(Duration::from_millis(50));
        }
        Ok(session)
    }

    /// Capture from the card until the current speaking turn ends. With a recording session
    /// the turn is driven from the continuous capture (rx is already being written by the
    /// session's bg thread); without one, a one-shot capture is opened just for the VAD.
    pub fn wait_for_speech(
        &self,
        turn: TurnConfig,
        sess: Option<&mut DuplexSession>,
        cancel: &AtomicBool,
    ) -> anyhow::Result<EndReason> {
        match sess {
            Some(s) => {
                s.vad_begin();
                let reason = {
                    let mut src = VadFrameSource::new(s.vad_receiver_mut());
                    run_wait_for_speech(&mut src, turn, cancel)
                };
                s.vad_end();
                reason
            }
            None => {
                // No recording session: capture straight for the VAD. Reduce to the call
                // channel first — feeding interleaved audio to the segmenter would double
                // the apparent rate and halve every turn timeout.
                let mut src = DownmixMono::new(self.open_capture()?);
                run_wait_for_speech(&mut src, turn, cancel)
            }
        }
    }
}

/// Read `file`, convert it to the tx leg's shape (the card's rate and channel count), and
/// append it as one block anchored at the current rx clock.
///
/// The prompt is read as mono (`WavFileSource` keeps channel 0) and duplicated across the
/// leg's channels: tx is a reference copy of what we injected, and matching rx's shape is
/// what lets the two legs share one frame clock and be mixed. Resampling here is never in
/// the path of audio the far end hears — the card is fed the original file by the playback
/// tool.
fn tee_tx(sess: &mut DuplexSession, file: &Path) -> anyhow::Result<()> {
    let mut src = WavFileSource::open(file)?;
    let mut rs = Resampler::new(src.sample_rate(), sess.sample_rate());
    let channels = sess.channels().max(1) as usize;
    let mut buf = vec![0i16; 4096];
    let mut prompt: Vec<i16> = Vec::new();
    loop {
        let n = src.read(&mut buf)?;
        if n == 0 {
            break;
        }
        for s in rs.process(&buf[..n]) {
            for _ in 0..channels {
                prompt.push(s);
            }
        }
    }
    sess.push_tx(&prompt)?;
    Ok(())
}

/// Drive a [`Segmenter`] from any capture source: resample to 16 kHz, frame into hops,
/// and return why the turn ended. Generic so tests can feed a WAV file source and the
/// live path can feed the duplex session's frames. Recording (rx) is handled separately by
/// the [`DuplexSession`]; this only does VAD.
pub fn run_wait_for_speech<S: CaptureSource>(
    src: &mut S,
    turn: TurnConfig,
    cancel: &AtomicBool,
) -> anyhow::Result<EndReason> {
    let mut seg = Segmenter::new(turn)?;
    let hop = seg.hop_size();
    let mut resampler = Resampler16k::new(src.sample_rate());

    let mut read_buf = vec![0i16; 4096];
    let mut pending: Vec<i16> = Vec::with_capacity(hop * 4);

    // Wall-clock stall guard: a live capture delivers frames continuously (silence is still
    // frames of zeros), so if no data arrives for this long the capture is dead — fail with
    // a clear error instead of blocking forever (the hop-based timeout can't fire with no
    // frames to advance it). Only sources that report `WouldBlock` (the duplex VAD stream)
    // are subject to this; blocking sources are unaffected.
    let stall_limit = Duration::from_millis(3_000);
    let mut last_data = Instant::now();

    let reason = 'outer: loop {
        // Cancelled (Ctrl+C on `dialf run`) — bail out of a long wait promptly.
        if cancel.load(Ordering::Relaxed) {
            break 'outer EndReason::EndOfStream;
        }
        let n = match src.read(&mut read_buf) {
            Ok(n) => n,
            Err(e) if e.kind() == std::io::ErrorKind::WouldBlock => {
                if last_data.elapsed() >= stall_limit {
                    anyhow::bail!(
                        "capture stalled — no audio for {} ms. Please grant Microphone permission \
                         to the daemon (macOS) and check the sound card is connected",
                        stall_limit.as_millis()
                    );
                }
                continue;
            }
            Err(e) => return Err(e.into()),
        };
        last_data = Instant::now();
        if n == 0 {
            // Drain any final whole hop, then signal end-of-stream.
            if pending.len() >= hop {
                if let Some(TurnEvent::Ended(r)) = seg.push_hop(&pending[..hop])? {
                    break 'outer r;
                }
            }
            break 'outer match seg.finish() {
                Some(TurnEvent::Ended(r)) => r,
                _ => EndReason::EndOfStream,
            };
        }

        let out = resampler.process(&read_buf[..n]);
        pending.extend_from_slice(&out);

        // Consume whole hops.
        let mut start = 0;
        while pending.len() - start >= hop {
            let frame = &pending[start..start + hop];
            if let Some(TurnEvent::Ended(r)) = seg.push_hop(frame)? {
                break 'outer r;
            }
            start += hop;
        }
        if start > 0 {
            pending.drain(0..start);
        }
    };

    let (total, voiced, mean_prob) = seg.stats();
    tracing::info!(
        ?reason,
        total_hops = total,
        voiced_hops = voiced,
        mean_prob = format!("{mean_prob:.3}"),
        src_rate = src.sample_rate(),
        "wait_for_speech finished"
    );
    Ok(reason)
}
