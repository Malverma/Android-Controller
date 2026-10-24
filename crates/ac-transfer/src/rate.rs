//! Token bucket in front of the SYNC writer (spec §3.6): one second of
//! burst, refilled continuously.

use std::time::{Duration, Instant};

#[derive(Debug, Clone)]
pub struct TokenBucket {
    /// Bytes per second; 0 = unlimited.
    rate: u64,
    tokens: f64,
    last: Instant,
}

impl TokenBucket {
    pub fn new(rate_bytes_per_s: u64, now: Instant) -> Self {
        Self { rate: rate_bytes_per_s, tokens: rate_bytes_per_s as f64, last: now }
    }

    /// From a `*_mbps` setting (megabits per second).
    pub fn from_mbps(mbps: u32, now: Instant) -> Self {
        Self::new(mbps as u64 * 1_000_000 / 8, now)
    }

    pub fn rate(&self) -> u64 {
        self.rate
    }

    /// Changes the rate (e.g. when mirroring starts or stops), keeping at
    /// most one second of burst.
    pub fn set_rate(&mut self, rate_bytes_per_s: u64, now: Instant) {
        if rate_bytes_per_s != self.rate {
            self.refill(now);
            self.rate = rate_bytes_per_s;
            self.tokens = self.tokens.min(rate_bytes_per_s as f64);
        }
    }

    fn refill(&mut self, now: Instant) {
        let dt = now.saturating_duration_since(self.last).as_secs_f64();
        self.last = now;
        self.tokens = (self.tokens + dt * self.rate as f64).min(self.rate as f64);
    }

    /// Takes `n` bytes and returns how long to wait before sending them.
    pub fn take(&mut self, n: u64, now: Instant) -> Duration {
        if self.rate == 0 {
            return Duration::ZERO;
        }
        self.refill(now);
        self.tokens -= n as f64;
        if self.tokens >= 0.0 {
            Duration::ZERO
        } else {
            Duration::from_secs_f64(-self.tokens / self.rate as f64)
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn unlimited_never_waits() {
        let t0 = Instant::now();
        let mut b = TokenBucket::new(0, t0);
        assert_eq!(b.take(u64::MAX / 2, t0), Duration::ZERO);
    }

    #[test]
    fn holds_the_long_run_rate() {
        let t0 = Instant::now();
        let mut b = TokenBucket::from_mbps(40, t0); // 5 MB/s
        assert_eq!(b.rate(), 5_000_000);
        // The first second's worth goes out as burst.
        assert_eq!(b.take(5_000_000, t0), Duration::ZERO);
        // Then 10 MB more must take 2 s.
        let mut now = t0;
        let mut waited = Duration::ZERO;
        for _ in 0..160 {
            let w = b.take(65_536 * 1_000_000 / 1_048_576, now); // 62 500 bytes
            now += w;
            waited += w;
        }
        assert!((waited.as_secs_f64() - 2.0).abs() < 0.01, "{waited:?}");
    }

    #[test]
    fn rate_change_caps_burst() {
        let t0 = Instant::now();
        let mut b = TokenBucket::new(10_000_000, t0);
        b.set_rate(1_000_000, t0);
        assert_eq!(b.take(1_000_000, t0), Duration::ZERO);
        assert_eq!(b.take(500_000, t0), Duration::from_millis(500));
        b.set_rate(0, t0);
        assert_eq!(b.take(1 << 30, t0), Duration::ZERO);
    }
}
