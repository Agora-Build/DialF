//! Audio engine: ties playback, capture, resampling, and VAD together.
//!
//! `audio.play` -> [`AudioEngine::play_file`]. `audio.wait_for_speech` ->
//! [`AudioEngine::wait_for_speech`], which captures from the card, resamples to 16 kHz,
//! frames into hops, and runs the [`Segmenter`] until the turn ends.
//!
//! All methods are synchronous; the daemon calls them on a blocking task.

use std::path::Path;

use crate::config::AudioConfig;

use super::backend::{CaptureSource, WavFileSource};
use super::command_backend::{self, CommandCaptureSource};
use super::record::Recorder;
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

    /// Play an audio file out the sound card (blocking until done). If `rec` is set, the
    /// file's audio (resampled to 16 kHz) is also written to the tx leg.
    pub fn play_file(&self, file: &Path, rec: Option<&mut Recorder>) -> anyhow::Result<()> {
        if let Some(r) = rec {
            tee_tx(r, file)?;
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

    /// Capture from the card until the current speaking turn ends. If `rec` is set, the
    /// captured audio (16 kHz) is also written to the rx leg.
    pub fn wait_for_speech(
        &self,
        turn: TurnConfig,
        rec: Option<&mut Recorder>,
    ) -> anyhow::Result<EndReason> {
        let mut src = self.open_capture()?;
        run_wait_for_speech(&mut src, turn, rec)
    }
}

/// Read `file`, resample to 16 kHz, and append it to the recorder's tx leg.
fn tee_tx(rec: &mut Recorder, file: &Path) -> anyhow::Result<()> {
    let mut src = WavFileSource::open(file)?;
    let mut rs = Resampler16k::new(src.sample_rate());
    let mut buf = vec![0i16; 4096];
    loop {
        let n = src.read(&mut buf)?;
        if n == 0 {
            break;
        }
        let out = rs.process(&buf[..n]);
        rec.push_tx(&out)?;
    }
    Ok(())
}

/// Drive a [`Segmenter`] from any capture source: resample to 16 kHz, frame into hops,
/// and return why the turn ended. Generic so tests can feed a WAV file source.
pub fn run_wait_for_speech<S: CaptureSource>(
    src: &mut S,
    turn: TurnConfig,
    mut rec: Option<&mut Recorder>,
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
        if let Some(r) = rec.as_deref_mut() {
            r.push_rx(&out)?;
        }
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
