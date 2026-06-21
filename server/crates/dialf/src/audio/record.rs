//! Call recording: two legs written time-aligned at 16 kHz mono.
//!
//! - **rx** — audio captured from the sound card (the phone's earpiece / far end).
//! - **tx** — audio we injected into the card (our prompts / TTS).
//!
//! Steps in a job run sequentially, so the two legs are kept aligned trivially: while we
//! play (tx), rx gets matching silence; while we capture (rx), tx gets matching silence.
//! On [`Recorder::finish`] an optional summed `*-mix.wav` is produced. All three files are
//! the same length and sample-aligned.

use std::path::PathBuf;

use anyhow::Context;

use crate::audio::backend::{PlaybackSink, WavFileSink};

/// Recording sample rate (matches the VAD path).
pub const RECORD_RATE: u32 = 16_000;

/// Paths produced by a finished recording.
#[derive(Debug, Clone)]
pub struct RecordOutput {
    pub rx: PathBuf,
    pub tx: PathBuf,
    pub mix: Option<PathBuf>,
}

/// Records the rx + tx legs of a session to WAV.
pub struct Recorder {
    rx: WavFileSink,
    tx: WavFileSink,
    rx_path: PathBuf,
    tx_path: PathBuf,
    dir: PathBuf,
    session: String,
    mix: bool,
    silence: Vec<i16>,
}

impl Recorder {
    /// Create a recorder writing `<dir>/<session>-rx.wav` and `-tx.wav` (and, if `mix`,
    /// `-mix.wav` on finish).
    pub fn new(dir: impl Into<PathBuf>, session: impl Into<String>, mix: bool) -> anyhow::Result<Self> {
        let dir = dir.into();
        let session = session.into();
        std::fs::create_dir_all(&dir).with_context(|| format!("create record dir {}", dir.display()))?;
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
            silence: vec![0i16; 4096],
        })
    }

    /// Captured-from-card samples (16 kHz mono): append to rx, pad tx with silence.
    pub fn push_rx(&mut self, samples: &[i16]) -> std::io::Result<()> {
        self.rx.write(samples)?;
        self.pad_silence_tx(samples.len())
    }

    /// Injected-to-card samples (16 kHz mono): append to tx, pad rx with silence.
    pub fn push_tx(&mut self, samples: &[i16]) -> std::io::Result<()> {
        self.tx.write(samples)?;
        self.pad_silence_rx(samples.len())
    }

    fn pad_silence_tx(&mut self, n: usize) -> std::io::Result<()> {
        if self.silence.len() < n {
            self.silence.resize(n, 0);
        }
        self.tx.write(&self.silence[..n])
    }

    fn pad_silence_rx(&mut self, n: usize) -> std::io::Result<()> {
        if self.silence.len() < n {
            self.silence.resize(n, 0);
        }
        self.rx.write(&self.silence[..n])
    }

    /// Finalize the legs and, if requested, write the summed mix. Returns the paths.
    pub fn finish(self) -> anyhow::Result<RecordOutput> {
        let Recorder {
            rx,
            tx,
            rx_path,
            tx_path,
            dir,
            session,
            mix,
            ..
        } = self;
        // Finalize headers so the files are readable.
        rx.finalize().context("finalize rx.wav")?;
        tx.finalize().context("finalize tx.wav")?;

        let mix_path = if mix {
            let p = dir.join(format!("{session}-mix.wav"));
            mix_wavs(&rx_path, &tx_path, &p).context("write mix.wav")?;
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

/// Sum two mono 16 kHz WAVs into `out` (saturating). Shorter leg is zero-padded.
fn mix_wavs(a: &std::path::Path, b: &std::path::Path, out: &std::path::Path) -> anyhow::Result<()> {
    let sa = read_i16(a)?;
    let sb = read_i16(b)?;
    let n = sa.len().max(sb.len());
    let mut sink = WavFileSink::create(out, RECORD_RATE)?;
    let mut buf = Vec::with_capacity(n);
    for i in 0..n {
        let x = *sa.get(i).unwrap_or(&0) as i32 + *sb.get(i).unwrap_or(&0) as i32;
        buf.push(x.clamp(i16::MIN as i32, i16::MAX as i32) as i16);
    }
    sink.write(&buf)?;
    sink.finalize()?;
    Ok(())
}

fn read_i16(path: &std::path::Path) -> anyhow::Result<Vec<i16>> {
    let mut r = hound::WavReader::open(path).with_context(|| format!("open {}", path.display()))?;
    Ok(r.samples::<i16>().collect::<Result<Vec<_>, _>>()?)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn tmp() -> PathBuf {
        std::env::temp_dir().join(format!("dialf-rectest-{}", std::process::id()))
    }

    #[test]
    fn legs_are_aligned_and_mixed() {
        let dir = tmp();
        let mut rec = Recorder::new(&dir, "s1", true).unwrap();
        // tx prompt of 10, then rx response of 10.
        rec.push_tx(&[100i16; 10]).unwrap();
        rec.push_rx(&[50i16; 10]).unwrap();
        let out = rec.finish().unwrap();

        let rx = read_i16(&out.rx).unwrap();
        let tx = read_i16(&out.tx).unwrap();
        let mix = read_i16(out.mix.as_ref().unwrap()).unwrap();

        // Both legs 20 samples, aligned: tx = [100x10, 0x10], rx = [0x10, 50x10].
        assert_eq!(tx.len(), 20);
        assert_eq!(rx.len(), 20);
        assert_eq!(&tx[..10], &[100i16; 10]);
        assert_eq!(&tx[10..], &[0i16; 10]);
        assert_eq!(&rx[..10], &[0i16; 10]);
        assert_eq!(&rx[10..], &[50i16; 10]);
        // Mix = sum.
        assert_eq!(&mix[..10], &[100i16; 10]);
        assert_eq!(&mix[10..], &[50i16; 10]);

        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn no_mix_when_disabled() {
        let dir = tmp().join("nomix");
        let mut rec = Recorder::new(&dir, "s2", false).unwrap();
        rec.push_tx(&[1i16; 4]).unwrap();
        let out = rec.finish().unwrap();
        assert!(out.mix.is_none());
        assert!(out.rx.exists() && out.tx.exists());
        let _ = std::fs::remove_dir_all(&dir);
    }
}
