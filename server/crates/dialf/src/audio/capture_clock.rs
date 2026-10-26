//! Wall time against the audio a capture actually delivers.
//!
//! rx is the recording's master clock: tx is placed by rx frame count. Audio the capture never
//! delivers therefore shortens rx without leaving a mark — every reply lands early against
//! its prompt and the run's latency figures are wrong, while both files look fine.

use std::time::{Duration, Instant};

/// A tool's first buffers arrive in a burst, or late while it opens the device; counting from
/// the first frame misreads a healthy capture either way, so measuring starts after this.
pub const SETTLE: Duration = Duration::from_millis(600);

/// A gap between wall time and delivered audio is reported once it is at least this long…
pub const REPORT_MIN: Duration = Duration::from_secs(1);
/// …and at least this share of the run, so buffering jitter never trips it.
pub const REPORT_SHARE: f64 = 0.02;

#[derive(Debug, Clone)]
pub struct CaptureClock {
    rate: u32,
    first: Option<Instant>,
    /// Start of the measured span: the first chunk to arrive after [`SETTLE`]. Its own frames
    /// are not counted — they accumulated before it arrived.
    from: Option<Instant>,
    last: Option<Instant>,
    frames: u64,
}

impl CaptureClock {
    pub fn new(rate: u32) -> Self {
        Self { rate: rate.max(1), first: None, from: None, last: None, frames: 0 }
    }

    /// Note `frames` arriving now.
    pub fn observe(&mut self, frames: u64) {
        self.observe_at(Instant::now(), frames);
    }

    pub fn observe_at(&mut self, now: Instant, frames: u64) {
        let first = *self.first.get_or_insert(now);
        match self.from {
            Some(_) => self.frames += frames,
            None if now.duration_since(first) >= SETTLE => self.from = Some(now),
            None => {}
        }
        self.last = Some(now);
    }

    /// Wall time measured so far.
    pub fn elapsed(&self) -> Duration {
        match (self.from, self.last) {
            (Some(from), Some(last)) => last.duration_since(from),
            _ => Duration::ZERO,
        }
    }

    /// Frames per second actually delivered; `None` until something has been measured.
    pub fn rate(&self) -> Option<u32> {
        let secs = self.elapsed().as_secs_f64();
        (secs > 0.0 && self.frames > 0).then(|| (self.frames as f64 / secs).round() as u32)
    }

    /// Wall time minus the audio delivered, in seconds: positive when audio is missing,
    /// negative when the tool delivers more than time allows (it runs faster than configured).
    pub fn drift_secs(&self) -> f64 {
        self.elapsed().as_secs_f64() - self.frames as f64 / self.rate as f64
    }

    /// [`drift_secs`](Self::drift_secs), once it is big enough to be worth saying.
    pub fn reportable_drift(&self) -> Option<f64> {
        let drift = self.drift_secs();
        let span = self.elapsed().as_secs_f64();
        (drift.abs() >= REPORT_MIN.as_secs_f64() && drift.abs() > REPORT_SHARE * span)
            .then_some(drift)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Feed `secs` of wall time in 100 ms chunks, delivering `share` of the audio each time.
    fn run(rate: u32, secs: f64, share: f64) -> CaptureClock {
        let mut c = CaptureClock::new(rate);
        let t0 = Instant::now();
        let chunks = (secs * 10.0) as u64;
        for i in 0..=chunks {
            let frames = (rate as f64 * 0.1 * share).round() as u64;
            c.observe_at(t0 + Duration::from_millis(i * 100), frames);
        }
        c
    }

    #[test]
    fn a_healthy_capture_keeps_time() {
        let c = run(48_000, 60.0, 1.0);
        assert_eq!(c.rate(), Some(48_000));
        assert!(c.drift_secs().abs() < 0.01, "{}", c.drift_secs());
        assert_eq!(c.reportable_drift(), None);
    }

    #[test]
    fn the_opening_burst_and_a_slow_start_are_not_measured() {
        let mut c = CaptureClock::new(48_000);
        let t0 = Instant::now();
        // A late first buffer, then a burst holding everything since — the case that made
        // the old check report ~41 kHz on every healthy run.
        c.observe_at(t0, 4_800);
        c.observe_at(t0 + Duration::from_millis(50), 48_000);
        for i in 7..=40u64 {
            c.observe_at(t0 + Duration::from_millis(i * 100), 4_800);
        }
        assert_eq!(c.rate(), Some(48_000));
        assert_eq!(c.reportable_drift(), None);
    }

    #[test]
    fn missing_audio_is_measured_in_seconds() {
        // The WebDuT001 run: about 17% of rx never arrived over ~280 s.
        let c = run(44_100, 280.0, 0.83);
        let lost = c.reportable_drift().expect("reported");
        assert!((lost - 47.0).abs() < 1.5, "{lost}");
    }

    #[test]
    fn a_tool_running_fast_drifts_the_other_way() {
        // Card at 48k, config says 44100, the tool ignores the request.
        let c = run(44_100, 30.0, 48_000.0 / 44_100.0);
        assert!(c.reportable_drift().expect("reported") < -2.0);
        assert_eq!(c.rate(), Some(48_000));
    }

    #[test]
    fn small_or_early_gaps_are_not_reported() {
        // 1.5% short: under the share threshold however long it runs.
        assert_eq!(run(48_000, 300.0, 0.985).reportable_drift(), None);
        // 10% short but only 5 s in: under a second missing.
        assert_eq!(run(48_000, 5.0, 0.9).reportable_drift(), None);
        // Nothing measured yet.
        assert_eq!(CaptureClock::new(48_000).reportable_drift(), None);
        assert_eq!(CaptureClock::new(48_000).rate(), None);
    }
}
