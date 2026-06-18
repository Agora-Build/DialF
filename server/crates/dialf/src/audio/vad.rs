//! Turn-detection on top of ten-vad.
//!
//! [`TurnDetector`] is a pure state machine fed one voiced/unvoiced decision per hop.
//! It is decoupled from the native VAD so it can be unit-tested without the library:
//! - waits for speech onset,
//! - ends the turn after `silence_duration_ms` of continuous non-speech,
//! - or ends on the overall `end_timeout_ms` cap.
//!
//! [`Segmenter`] wires a real [`ten_vad_sys::TenVad`] to the detector, consuming 16 kHz
//! mono i16 hops.

use ten_vad_sys::{TenVad, VadFrame};

/// ten-vad operates at 16 kHz.
pub const VAD_SAMPLE_RATE: u32 = 16_000;

/// Configuration for one `audio.wait_for_speech` turn.
#[derive(Debug, Clone, Copy)]
pub struct TurnConfig {
    /// Hop size in samples (e.g. 256 = 16 ms at 16 kHz).
    pub hop_size: usize,
    /// Continuous trailing non-speech that ends the turn.
    pub silence_duration_ms: u64,
    /// Overall cap on the wait.
    pub end_timeout_ms: u64,
    /// Voiced decision threshold passed to ten-vad.
    pub threshold: f32,
}

impl Default for TurnConfig {
    fn default() -> Self {
        Self {
            hop_size: ten_vad_sys::HOP_256,
            silence_duration_ms: 3_000,
            end_timeout_ms: 45_000,
            threshold: 0.5,
        }
    }
}

/// Why a turn ended.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum EndReason {
    /// `silence_duration_ms` of continuous non-speech elapsed after speech started.
    Silence,
    /// `end_timeout_ms` elapsed.
    Timeout,
    /// The capture stream ended before a normal turn boundary.
    EndOfStream,
}

/// Events emitted by [`TurnDetector::push`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TurnEvent {
    /// First voiced hop after the wait began.
    SpeechStarted,
    /// The turn is over.
    Ended(EndReason),
}

/// Pure turn-detection state machine. Feed one voiced/unvoiced decision per hop.
#[derive(Debug)]
pub struct TurnDetector {
    hop_ms: f64,
    silence_hops_needed: usize,
    timeout_hops: usize,
    started: bool,
    silence_run: usize,
    elapsed_hops: usize,
    finished: bool,
}

impl TurnDetector {
    /// Build a detector from `cfg`.
    pub fn new(cfg: &TurnConfig) -> Self {
        let hop_ms = (cfg.hop_size as f64) * 1000.0 / (VAD_SAMPLE_RATE as f64);
        Self {
            hop_ms,
            silence_hops_needed: ms_to_hops(cfg.silence_duration_ms, hop_ms),
            timeout_hops: ms_to_hops(cfg.end_timeout_ms, hop_ms),
            started: false,
            silence_run: 0,
            elapsed_hops: 0,
            finished: false,
        }
    }

    /// Whether a terminal event has already been emitted.
    pub fn is_finished(&self) -> bool {
        self.finished
    }

    /// Milliseconds elapsed since the wait began.
    pub fn elapsed_ms(&self) -> f64 {
        self.elapsed_hops as f64 * self.hop_ms
    }

    /// Feed one hop's voiced decision. Returns at most one event per hop; once a
    /// terminal `Ended` is returned, subsequent calls return `None`.
    pub fn push(&mut self, voiced: bool) -> Option<TurnEvent> {
        if self.finished {
            return None;
        }
        self.elapsed_hops += 1;

        // Onset.
        if !self.started {
            if voiced {
                self.started = true;
                // Onset and timeout can't both fire meaningfully on the same hop;
                // prefer reporting onset, timeout will be caught next hop.
                return Some(TurnEvent::SpeechStarted);
            }
        } else if voiced {
            self.silence_run = 0;
        } else {
            self.silence_run += 1;
            if self.silence_run >= self.silence_hops_needed {
                self.finished = true;
                return Some(TurnEvent::Ended(EndReason::Silence));
            }
        }

        // Overall cap.
        if self.elapsed_hops >= self.timeout_hops {
            self.finished = true;
            return Some(TurnEvent::Ended(EndReason::Timeout));
        }
        None
    }

    /// Signal that the capture stream ended. Returns a terminal event unless one was
    /// already emitted.
    pub fn finish(&mut self) -> Option<TurnEvent> {
        if self.finished {
            return None;
        }
        self.finished = true;
        Some(TurnEvent::Ended(EndReason::EndOfStream))
    }
}

fn ms_to_hops(ms: u64, hop_ms: f64) -> usize {
    if hop_ms <= 0.0 {
        return usize::MAX;
    }
    (ms as f64 / hop_ms).ceil() as usize
}

/// Couples a native ten-vad instance with a [`TurnDetector`].
pub struct Segmenter {
    vad: TenVad,
    detector: TurnDetector,
    cfg: TurnConfig,
}

impl Segmenter {
    /// Create a segmenter, allocating the native VAD.
    pub fn new(cfg: TurnConfig) -> Result<Self, ten_vad_sys::Error> {
        let vad = TenVad::new(cfg.hop_size, cfg.threshold)?;
        let detector = TurnDetector::new(&cfg);
        Ok(Self { vad, detector, cfg })
    }

    /// The hop size this segmenter expects.
    pub fn hop_size(&self) -> usize {
        self.cfg.hop_size
    }

    /// Process one hop of exactly `hop_size` i16 samples (16 kHz mono).
    pub fn push_hop(&mut self, hop: &[i16]) -> Result<Option<TurnEvent>, ten_vad_sys::Error> {
        let VadFrame { voiced, .. } = self.vad.process(hop)?;
        Ok(self.detector.push(voiced))
    }

    /// Flush at end-of-stream.
    pub fn finish(&mut self) -> Option<TurnEvent> {
        self.detector.finish()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn cfg(hop: usize, silence_ms: u64, timeout_ms: u64) -> TurnConfig {
        TurnConfig {
            hop_size: hop,
            silence_duration_ms: silence_ms,
            end_timeout_ms: timeout_ms,
            threshold: 0.5,
        }
    }

    #[test]
    fn ms_to_hops_rounds_up() {
        // 256 samples @16k = 16ms/hop.
        let hop_ms = 256.0 * 1000.0 / 16_000.0;
        assert_eq!(ms_to_hops(3000, hop_ms), 188); // 3000/16 = 187.5 -> 188
        assert_eq!(ms_to_hops(16, hop_ms), 1);
        assert_eq!(ms_to_hops(17, hop_ms), 2);
    }

    #[test]
    fn detects_onset_then_silence_end() {
        // hop=160 -> 10ms; silence 50ms -> 5 hops; timeout 100s (won't fire).
        let mut d = TurnDetector::new(&cfg(160, 50, 100_000));

        // 3 hops of silence before speech: no event, no onset yet.
        for _ in 0..3 {
            assert_eq!(d.push(false), None);
        }
        // First voiced hop -> onset.
        assert_eq!(d.push(true), Some(TurnEvent::SpeechStarted));
        // More speech.
        for _ in 0..4 {
            assert_eq!(d.push(true), None);
        }
        // 4 hops of silence: not enough (need 5).
        for _ in 0..4 {
            assert_eq!(d.push(false), None);
        }
        // 5th silent hop ends the turn.
        assert_eq!(d.push(false), Some(TurnEvent::Ended(EndReason::Silence)));
        // Idempotent afterwards.
        assert_eq!(d.push(false), None);
        assert!(d.is_finished());
    }

    #[test]
    fn silence_run_resets_on_speech() {
        let mut d = TurnDetector::new(&cfg(160, 50, 100_000)); // 5-hop silence
        assert_eq!(d.push(true), Some(TurnEvent::SpeechStarted));
        for _ in 0..4 {
            assert_eq!(d.push(false), None);
        }
        // Speech resets the silence counter.
        assert_eq!(d.push(true), None);
        for _ in 0..4 {
            assert_eq!(d.push(false), None);
        }
        assert_eq!(d.push(false), Some(TurnEvent::Ended(EndReason::Silence)));
    }

    #[test]
    fn timeout_fires_without_speech() {
        // hop=160 -> 10ms; timeout 50ms -> 5 hops.
        let mut d = TurnDetector::new(&cfg(160, 5_000, 50));
        for _ in 0..4 {
            assert_eq!(d.push(false), None);
        }
        assert_eq!(d.push(false), Some(TurnEvent::Ended(EndReason::Timeout)));
    }

    #[test]
    fn end_of_stream_is_terminal() {
        let mut d = TurnDetector::new(&cfg(160, 5_000, 100_000));
        assert_eq!(d.push(true), Some(TurnEvent::SpeechStarted));
        assert_eq!(d.finish(), Some(TurnEvent::Ended(EndReason::EndOfStream)));
        assert_eq!(d.finish(), None);
    }
}
