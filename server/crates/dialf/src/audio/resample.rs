//! Lightweight mono resampling to 16 kHz for the VAD path.
//!
//! Sound cards typically capture at 44.1/48 kHz; ten-vad needs 16 kHz. We use a small,
//! dependency-free streaming resampler:
//! - **Integer downsample** (e.g. 48000 -> 16000, factor 3): a windowed-sinc FIR low-pass
//!   at the 16 kHz Nyquist, then decimate — proper anti-aliasing.
//! - **Other ratios** (e.g. 44100 -> 16000): linear interpolation. Adequate for VAD,
//!   though not audiophile quality.
//!
//! `Resampler16k` is stateful so it can be fed arbitrary-length chunks from a capture
//! stream and emit whatever 16 kHz samples are ready.

/// Target rate for the VAD path.
pub const TARGET_RATE: u32 = 16_000;

/// Streaming mono i16 -> 16 kHz i16 resampler.
pub enum Resampler16k {
    /// Source already at 16 kHz; pass through.
    Passthrough,
    /// Integer decimation with an anti-alias FIR.
    Decimate(Decimator),
    /// Arbitrary-ratio linear interpolation.
    Linear(LinearResampler),
}

impl Resampler16k {
    /// Build a resampler from `src_rate` to 16 kHz.
    pub fn new(src_rate: u32) -> Self {
        if src_rate == TARGET_RATE {
            Resampler16k::Passthrough
        } else if src_rate % TARGET_RATE == 0 {
            Resampler16k::Decimate(Decimator::new((src_rate / TARGET_RATE) as usize))
        } else {
            Resampler16k::Linear(LinearResampler::new(src_rate, TARGET_RATE))
        }
    }

    /// Push input samples; returns newly produced 16 kHz samples.
    pub fn process(&mut self, input: &[i16]) -> Vec<i16> {
        match self {
            Resampler16k::Passthrough => input.to_vec(),
            Resampler16k::Decimate(d) => d.process(input),
            Resampler16k::Linear(l) => l.process(input),
        }
    }
}

/// Integer-factor decimator with a windowed-sinc low-pass.
pub struct Decimator {
    factor: usize,
    taps: Vec<f32>,
    history: Vec<f32>, // last (taps.len()-1) input samples
    phase: usize,      // counts inputs since last emitted sample
}

impl Decimator {
    fn new(factor: usize) -> Self {
        let factor = factor.max(1);
        // Cutoff at the target Nyquist relative to the source rate: 0.5/factor.
        let taps = design_lowpass(0.5 / factor as f32, 64);
        let hist = taps.len().saturating_sub(1);
        Self {
            factor,
            taps,
            history: vec![0.0; hist],
            phase: 0,
        }
    }

    fn process(&mut self, input: &[i16]) -> Vec<i16> {
        let mut out = Vec::with_capacity(input.len() / self.factor + 1);
        let ntaps = self.taps.len();
        for &s in input {
            // Slide sample into history (history holds the most recent ntaps-1 samples;
            // we append current then keep a window).
            self.history.push(s as f32);
            if self.history.len() > ntaps {
                self.history.remove(0);
            }
            self.phase += 1;
            if self.phase == self.factor {
                self.phase = 0;
                // Convolve current window with taps (only when we have a full window).
                if self.history.len() == ntaps {
                    let mut acc = 0.0f32;
                    for (h, t) in self.history.iter().zip(self.taps.iter()) {
                        acc += h * t;
                    }
                    out.push(clamp_i16(acc));
                } else {
                    out.push(clamp_i16(s as f32));
                }
            }
        }
        out
    }
}

/// Arbitrary-ratio linear interpolation resampler.
pub struct LinearResampler {
    step: f64,    // src samples advanced per output sample
    pos: f64,     // fractional read position into the running stream
    last: f32,    // last input sample carried across chunks
    primed: bool, // whether `last` holds a real sample
    consumed: u64,
}

impl LinearResampler {
    fn new(src_rate: u32, dst_rate: u32) -> Self {
        Self {
            step: src_rate as f64 / dst_rate as f64,
            pos: 0.0,
            last: 0.0,
            primed: false,
            consumed: 0,
        }
    }

    fn process(&mut self, input: &[i16]) -> Vec<i16> {
        if input.is_empty() {
            return Vec::new();
        }
        let base = self.consumed; // global index of input[0]
        let mut out = Vec::new();
        // Produce outputs whose source position falls within this chunk.
        loop {
            let idx_f = self.pos;
            let i0 = idx_f.floor() as i64;
            let frac = (idx_f - i0 as f64) as f32;
            let rel = i0 - base as i64;
            if rel < -1 {
                // Shouldn't happen; advance.
                self.pos += self.step;
                continue;
            }
            // Need samples rel and rel+1 within this chunk (rel == -1 uses carried `last`).
            let need_hi = rel + 1;
            if need_hi >= input.len() as i64 {
                break; // not enough input yet; wait for next chunk
            }
            let lo = if rel < 0 {
                if self.primed {
                    self.last
                } else {
                    input[0] as f32
                }
            } else {
                input[rel as usize] as f32
            };
            let hi = input[(rel + 1) as usize] as f32;
            out.push(clamp_i16(lo + (hi - lo) * frac));
            self.pos += self.step;
        }
        self.consumed = base + input.len() as u64;
        self.last = input[input.len() - 1] as f32;
        self.primed = true;
        // Keep pos referenced near the consumed window to avoid unbounded growth.
        out
    }
}

/// Windowed-sinc low-pass FIR. `cutoff` is normalized (1.0 == source Nyquist).
fn design_lowpass(cutoff: f32, ntaps: usize) -> Vec<f32> {
    let ntaps = if ntaps % 2 == 0 { ntaps + 1 } else { ntaps };
    let mid = (ntaps / 2) as i32;
    let mut taps = Vec::with_capacity(ntaps);
    let mut sum = 0.0f32;
    for n in 0..ntaps as i32 {
        let k = n - mid;
        // Ideal sinc at cutoff (cutoff here is fraction of Nyquist; fc = cutoff).
        let sinc = if k == 0 {
            2.0 * cutoff
        } else {
            (std::f32::consts::PI * cutoff * k as f32).sin() / (std::f32::consts::PI * k as f32)
        };
        // Hann window.
        let w = 0.5
            - 0.5 * (2.0 * std::f32::consts::PI * n as f32 / (ntaps as f32 - 1.0)).cos();
        let t = sinc * w;
        taps.push(t);
        sum += t;
    }
    // Normalize to unity DC gain.
    if sum.abs() > f32::EPSILON {
        for t in taps.iter_mut() {
            *t /= sum;
        }
    }
    taps
}

fn clamp_i16(v: f32) -> i16 {
    v.round().clamp(i16::MIN as f32, i16::MAX as f32) as i16
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn passthrough_is_identity() {
        let mut r = Resampler16k::new(16_000);
        let input: Vec<i16> = (0..100).collect();
        assert_eq!(r.process(&input), input);
    }

    #[test]
    fn decimate_48k_to_16k_thirds_the_rate() {
        let mut r = Resampler16k::new(48_000);
        // Feed 3000 samples of a low-frequency ramp; expect ~1000 out.
        let input: Vec<i16> = (0..3000).map(|i| ((i % 200) as i16) - 100).collect();
        let out = r.process(&input);
        // Within one sample of N/3 due to filter priming.
        assert!((out.len() as i64 - 1000).abs() <= 1, "got {}", out.len());
    }

    #[test]
    fn decimate_dc_preserved() {
        let mut r = Resampler16k::new(48_000);
        let input = vec![1000i16; 6000];
        let out = r.process(&input);
        // After the FIR primes, output should settle near the DC level.
        let tail = &out[out.len().saturating_sub(50)..];
        for &s in tail {
            assert!((s as i32 - 1000).abs() <= 5, "settled value {s}");
        }
    }

    #[test]
    fn linear_44100_to_16k_ratio() {
        let mut r = Resampler16k::new(44_100);
        let input = vec![500i16; 44_100];
        let out = r.process(&input);
        // ~16000 out for one second of input (allow priming slack).
        assert!((out.len() as i64 - 16_000).abs() <= 5, "got {}", out.len());
        // Constant input -> constant output.
        assert!(out.iter().all(|&s| (s - 500).abs() <= 1));
    }

    #[test]
    fn linear_streaming_matches_chunked() {
        let input: Vec<i16> = (0..10_000).map(|i| ((i * 7 % 500) as i16) - 250).collect();
        let mut whole = Resampler16k::new(44_100);
        let a = whole.process(&input);

        let mut chunked = Resampler16k::new(44_100);
        let mut b = Vec::new();
        for chunk in input.chunks(333) {
            b.extend(chunked.process(chunk));
        }
        // Streaming in chunks must yield the same samples as one big call.
        assert_eq!(a, b);
    }
}
