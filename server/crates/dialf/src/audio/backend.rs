//! Audio backend abstraction.
//!
//! A [`CaptureSource`] yields mono i16 PCM at a known rate; a [`PlaybackSink`] consumes
//! it. The real backend (added next) spawns an external CLI tool. A WAV file backend is
//! provided here so the pipeline (resample + VAD + job runner) can be exercised end-to-end
//! with no sound card — used by tests and the `--no-card` dev path.

use std::io;
use std::path::Path;

/// A source of mono i16 PCM samples.
pub trait CaptureSource: Send {
    /// Fill `buf` with up to its capacity worth of samples; returns the count read.
    /// `0` means end-of-stream.
    fn read(&mut self, buf: &mut [i16]) -> io::Result<usize>;
    /// Native sample rate of this source.
    fn sample_rate(&self) -> u32;
}

/// A sink that plays mono i16 PCM.
pub trait PlaybackSink: Send {
    /// Play all of `samples`.
    fn write(&mut self, samples: &[i16]) -> io::Result<()>;
    /// Block until playback has drained.
    fn flush(&mut self) -> io::Result<()>;
}

/// Reads a WAV file as a capture source (mono; multi-channel is downmixed to ch 0).
pub struct WavFileSource {
    samples: std::vec::IntoIter<i16>,
    sample_rate: u32,
}

impl WavFileSource {
    /// Open a WAV file for reading.
    pub fn open(path: &Path) -> anyhow::Result<Self> {
        let reader = hound::WavReader::open(path)?;
        let spec = reader.spec();
        let channels = spec.channels.max(1) as usize;
        let mut reader = reader;
        let all: Vec<i16> = reader
            .samples::<i16>()
            .collect::<Result<Vec<_>, _>>()
            .map_err(|e| anyhow::anyhow!("read wav samples: {e}"))?;
        // Downmix to channel 0 if needed.
        let mono: Vec<i16> = if channels <= 1 {
            all
        } else {
            all.into_iter().step_by(channels).collect()
        };
        Ok(Self {
            samples: mono.into_iter(),
            sample_rate: spec.sample_rate,
        })
    }
}

impl CaptureSource for WavFileSource {
    fn read(&mut self, buf: &mut [i16]) -> io::Result<usize> {
        let mut n = 0;
        for slot in buf.iter_mut() {
            match self.samples.next() {
                Some(s) => {
                    *slot = s;
                    n += 1;
                }
                None => break,
            }
        }
        Ok(n)
    }

    fn sample_rate(&self) -> u32 {
        self.sample_rate
    }
}

/// Writes played PCM to a WAV file (for the no-card path / recordings).
pub struct WavFileSink {
    writer: hound::WavWriter<io::BufWriter<std::fs::File>>,
}

impl WavFileSink {
    /// Create a mono 16-bit WAV at `path` with the given `sample_rate`.
    pub fn create(path: &Path, sample_rate: u32) -> anyhow::Result<Self> {
        let spec = hound::WavSpec {
            channels: 1,
            sample_rate,
            bits_per_sample: 16,
            sample_format: hound::SampleFormat::Int,
        };
        Ok(Self {
            writer: hound::WavWriter::create(path, spec)?,
        })
    }
}

impl PlaybackSink for WavFileSink {
    fn write(&mut self, samples: &[i16]) -> io::Result<()> {
        for &s in samples {
            self.writer
                .write_sample(s)
                .map_err(|e| io::Error::new(io::ErrorKind::Other, e))?;
        }
        Ok(())
    }

    fn flush(&mut self) -> io::Result<()> {
        self.writer
            .flush()
            .map_err(|e| io::Error::new(io::ErrorKind::Other, e))
    }
}
