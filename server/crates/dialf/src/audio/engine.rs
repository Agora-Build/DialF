//! Audio engine: ties playback, capture, resampling, and VAD together.
//!
//! `audio.play` -> [`AudioEngine::play_file`]. `audio.wait_for_speech` ->
//! [`AudioEngine::wait_for_speech`], which captures from the card, resamples to 16 kHz,
//! frames into hops, and runs the [`Segmenter`] until the turn ends.
//!
//! All methods are synchronous; the daemon calls them on a blocking task.

use std::path::{Path, PathBuf};

use crate::config::AudioConfig;

use super::backend::{CaptureSource, WavFileSink, WavFileSource};
use super::command_backend::{self, CommandCaptureSource};
use super::record::{DuplexSession, VadFrameSource, RECORD_RATE};
use super::resample::Resampler16k;
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
    /// file's audio (resampled to 16 kHz) is also written to the tx leg, anchored at the
    /// current rx clock so it aligns with what the continuous capture records.
    pub fn play_file(&self, file: &Path, sess: Option<&mut DuplexSession>) -> anyhow::Result<()> {
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
        command_backend::play_file_blocking(&cmd)?;
        Ok(())
    }

    /// Open the configured sound-card capture source.
    pub fn open_capture(&self) -> anyhow::Result<CommandCaptureSource> {
        let cmd = tool_detect::resolve_capture(&self.capture_params(), self.cfg.capture_cmd.as_deref())?;
        let src = CommandCaptureSource::spawn(&cmd, self.cfg.sample_rate)?;
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
        let rx = WavFileSink::create(&rx_path, RECORD_RATE)?;
        let tx = WavFileSink::create(&tx_path, RECORD_RATE)?;
        DuplexSession::start(
            source, rx, tx, rx_path, tx_path, dir, session_name, mix, unblock,
        )
    }

    /// Capture from the card until the current speaking turn ends. With a recording session
    /// the turn is driven from the continuous capture (rx is already being written by the
    /// session's bg thread); without one, a one-shot capture is opened just for the VAD.
    pub fn wait_for_speech(
        &self,
        turn: TurnConfig,
        sess: Option<&mut DuplexSession>,
    ) -> anyhow::Result<EndReason> {
        match sess {
            Some(s) => {
                s.vad_begin();
                let reason = {
                    let mut src = VadFrameSource::new(s.vad_receiver_mut());
                    run_wait_for_speech(&mut src, turn)
                };
                s.vad_end();
                reason
            }
            None => {
                let mut src = self.open_capture()?;
                run_wait_for_speech(&mut src, turn)
            }
        }
    }
}

/// Read `file`, resample to 16 kHz, and append it to the session's tx leg as one block
/// anchored at the current rx clock.
fn tee_tx(sess: &mut DuplexSession, file: &Path) -> anyhow::Result<()> {
    let mut src = WavFileSource::open(file)?;
    let mut rs = Resampler16k::new(src.sample_rate());
    let mut buf = vec![0i16; 4096];
    let mut prompt: Vec<i16> = Vec::new();
    loop {
        let n = src.read(&mut buf)?;
        if n == 0 {
            break;
        }
        prompt.extend(rs.process(&buf[..n]));
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
) -> anyhow::Result<EndReason> {
    let mut seg = Segmenter::new(turn)?;
    let hop = seg.hop_size();
    let mut resampler = Resampler16k::new(src.sample_rate());

    let mut read_buf = vec![0i16; 4096];
    let mut pending: Vec<i16> = Vec::with_capacity(hop * 4);

    let reason = 'outer: loop {
        let n = src.read(&mut read_buf)?;
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
