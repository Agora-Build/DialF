//! Audio backend abstraction.
//!
//! A [`CaptureSource`] yields interleaved i16 PCM frames at a known rate and channel
//! count; a [`PlaybackSink`] consumes samples. The real backend spawns an external CLI
//! tool. A WAV file backend is provided here so the pipeline (resample + VAD + job runner)
//! can be exercised end-to-end with no sound card — used by tests and the `--no-card` path.

use std::io;
use std::path::Path;

/// A source of interleaved i16 PCM samples.
pub trait CaptureSource: Send {
    /// Fill `buf` with up to its capacity worth of samples; returns the count read.
    /// `0` means end-of-stream. Implementations with `channels() > 1` return **whole
    /// frames only**, so a caller can always split the result into frames without carrying
    /// a remainder.
    fn read(&mut self, buf: &mut [i16]) -> io::Result<usize>;
    /// Native sample rate of this source.
    fn sample_rate(&self) -> u32;
    /// Interleaved channel count. Mono unless overridden.
    fn channels(&self) -> u16 {
        1
    }
}

/// Reduces an interleaved multi-channel source to one channel.
///
/// Picks a single channel rather than averaging: on the bridge hardware the extra capture
/// channels can be a loopback of our own playback (see docs/HARDWARE.md), so mixing them in
/// would feed our own prompt to the VAD. Channel 0 is the call channel by convention.
pub struct DownmixMono<C: CaptureSource> {
    inner: C,
    channel: usize,
    buf: Vec<i16>,
}

impl<C: CaptureSource> DownmixMono<C> {
    /// Wrap `inner`, keeping channel 0.
    pub fn new(inner: C) -> Self {
        Self::channel(inner, 0)
    }

    /// Wrap `inner`, keeping `channel` (clamped into range).
    pub fn channel(inner: C, channel: usize) -> Self {
        Self {
            inner,
            channel,
            buf: Vec::new(),
        }
    }
}

impl<C: CaptureSource> CaptureSource for DownmixMono<C> {
    fn read(&mut self, out: &mut [i16]) -> io::Result<usize> {
        let ch = self.inner.channels().max(1) as usize;
        if ch == 1 {
            return self.inner.read(out);
        }
        let pick = self.channel.min(ch - 1);
        // Read up to `out.len()` frames' worth of interleaved samples.
        self.buf.resize(out.len().saturating_mul(ch).max(ch), 0);
        let n = self.inner.read(&mut self.buf)?;
        let frames = n / ch; // the source guarantees whole frames; this is belt-and-braces
        for (slot, f) in out.iter_mut().zip(0..frames) {
            *slot = self.buf[f * ch + pick];
        }
        Ok(frames.min(out.len()))
    }

    fn sample_rate(&self) -> u32 {
        self.inner.sample_rate()
    }
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

/// Writes PCM to a WAV file (for the no-card path / recordings).
pub struct WavFileSink {
    writer: hound::WavWriter<io::BufWriter<std::fs::File>>,
    channels: u16,
}

impl WavFileSink {
    /// Create a 16-bit WAV at `path` with the given `sample_rate` and channel count.
    /// Multi-channel writes are interleaved; `hound` refuses to finalize a file whose
    /// sample count isn't a whole number of frames.
    pub fn create(path: &Path, sample_rate: u32, channels: u16) -> anyhow::Result<Self> {
        let channels = channels.max(1);
        let spec = hound::WavSpec {
            channels,
            sample_rate,
            bits_per_sample: 16,
            sample_format: hound::SampleFormat::Int,
        };
        Ok(Self {
            writer: hound::WavWriter::create(path, spec)?,
            channels,
        })
    }

    /// Interleaved channel count this sink was created with.
    pub fn channels(&self) -> u16 {
        self.channels
    }

    /// Finalize the WAV (writes the correct header lengths). Required before reading the
    /// file back; `hound` does not finalize on drop.
    pub fn finalize(self) -> io::Result<()> {
        self.writer
            .finalize()
            .map_err(|e| io::Error::new(io::ErrorKind::Other, e))
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
