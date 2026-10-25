//! Adaptive bitrate controller, one per slot (spec §3.6). Pure: the manager
//! feeds it one sample a second and restarts the stream when it says so.

use std::time::{Duration, Instant};

/// Lag above the downgrade threshold this many seconds in a row → lower.
const DOWNGRADE_AFTER: u32 = 3;
/// Lag below the upgrade threshold this many seconds in a row → raise.
const UPGRADE_AFTER: u32 = 30;
/// At most one adaptive restart per slot in this window.
const MIN_GAP: Duration = Duration::from_secs(10);
const FLOOR_KBPS: u32 = 1000;
const FLOOR_FPS: u32 = 15;
const FPS_STEP: u32 = 10;

/// What the running stream asked for, and what the profile would ask for.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Rate {
    pub kbps: u32,
    pub fps: u32,
}

#[derive(Debug, Default, Clone)]
pub struct Adaptive {
    /// `None` while the profile's own values run.
    current: Option<Rate>,
    high: u32,
    low: u32,
    last_restart: Option<Instant>,
}

impl Adaptive {
    /// The rate to launch with: the adapted one, else the profile's.
    pub fn rate(&self, profile: Rate) -> Rate {
        self.current.unwrap_or(profile)
    }

    /// True while the stream runs below the profile's bitrate.
    pub fn reduced(&self, profile: Rate) -> bool {
        self.current.is_some_and(|c| c != profile)
    }

    /// Back to the profile's values (profile switch, phone gone).
    pub fn reset(&mut self) {
        *self = Self::default();
    }

    /// A second without video (still screen): it tells nothing about the link.
    pub fn idle(&mut self) {
        self.high = 0;
        self.low = 0;
    }

    /// One second of a live stream with `lag_ms` p95 lag. Returns the rate to
    /// restart with when it should change. `grid` streams also lose frame
    /// rate on a downgrade; focus keeps its smoothness and only loses detail.
    pub fn sample(&mut self, lag_ms: u32, now: Instant, profile: Rate, grid: bool, down_ms: u32, up_ms: u32) -> Option<Rate> {
        let cur = self.rate(profile);
        if lag_ms > down_ms {
            self.high += 1;
            self.low = 0;
        } else if lag_ms < up_ms {
            self.low += 1;
            self.high = 0;
        } else {
            self.high = 0;
            self.low = 0;
        }
        if self.last_restart.is_some_and(|t| now.duration_since(t) < MIN_GAP) {
            return None;
        }
        let next = if self.high >= DOWNGRADE_AFTER {
            Rate {
                kbps: ((cur.kbps as f64 * 0.6) as u32).max(FLOOR_KBPS).min(cur.kbps),
                fps: if grid { cur.fps.saturating_sub(FPS_STEP).max(FLOOR_FPS).min(cur.fps) } else { cur.fps },
            }
        } else if self.low >= UPGRADE_AFTER && (cur.kbps < profile.kbps || cur.fps < profile.fps) {
            Rate {
                kbps: ((cur.kbps as f64 * 1.25) as u32).min(profile.kbps).max(cur.kbps),
                fps: (cur.fps + FPS_STEP).min(profile.fps).max(cur.fps),
            }
        } else {
            return None;
        };
        if next == cur {
            return None; // already at the floor (or the ceiling)
        }
        self.high = 0;
        self.low = 0;
        self.last_restart = Some(now);
        self.current = (next != profile).then_some(next);
        Some(next)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const GRID: Rate = Rate { kbps: 3000, fps: 30 };
    const S: Duration = Duration::from_secs(1);

    fn run(a: &mut Adaptive, t: &mut Instant, lag: u32, secs: u32, grid: bool) -> Vec<Rate> {
        let mut out = Vec::new();
        for _ in 0..secs {
            *t += S;
            out.extend(a.sample(lag, *t, GRID, grid, 250, 80));
        }
        out
    }

    #[test]
    fn three_laggy_seconds_lower_the_bitrate_and_grid_fps() {
        let (mut a, mut t) = (Adaptive::default(), Instant::now());
        assert!(run(&mut a, &mut t, 300, 2, true).is_empty(), "two seconds are not enough");
        assert_eq!(run(&mut a, &mut t, 300, 1, true), vec![Rate { kbps: 1800, fps: 20 }]);
        assert!(a.reduced(GRID));
        assert_eq!(a.rate(GRID), Rate { kbps: 1800, fps: 20 });
    }

    #[test]
    fn focus_keeps_its_frame_rate() {
        let focus = Rate { kbps: 12000, fps: 60 };
        let (mut a, t) = (Adaptive::default(), Instant::now());
        let mut got = None;
        for i in 1..=3 {
            got = a.sample(400, t + S * i, focus, false, 250, 80);
        }
        assert_eq!(got, Some(Rate { kbps: 7200, fps: 60 }));
    }

    #[test]
    fn one_restart_per_ten_seconds_and_a_floor() {
        let (mut a, mut t) = (Adaptive::default(), Instant::now());
        let steps = run(&mut a, &mut t, 900, 60, true);
        assert_eq!(
            steps,
            vec![
                Rate { kbps: 1800, fps: 20 },
                Rate { kbps: 1080, fps: 15 },
                Rate { kbps: 1000, fps: 15 },
            ],
            "10 s apart, never below 1000 kbps / 15 fps"
        );
    }

    #[test]
    fn thirty_calm_seconds_climb_back_to_the_profile() {
        let (mut a, mut t) = (Adaptive::default(), Instant::now());
        run(&mut a, &mut t, 300, 3, true);
        assert!(run(&mut a, &mut t, 20, 29, true).is_empty());
        assert_eq!(run(&mut a, &mut t, 20, 1, true), vec![Rate { kbps: 2250, fps: 30 }]);
        let rest = run(&mut a, &mut t, 20, 60, true);
        assert_eq!(rest, vec![Rate { kbps: 2812, fps: 30 }, Rate { kbps: 3000, fps: 30 }]);
        assert!(!a.reduced(GRID), "back at the profile");
        assert!(run(&mut a, &mut t, 20, 60, true).is_empty(), "never above the profile");
    }

    #[test]
    fn middling_or_idle_seconds_reset_the_count() {
        let (mut a, mut t) = (Adaptive::default(), Instant::now());
        run(&mut a, &mut t, 300, 2, true);
        a.idle();
        assert!(run(&mut a, &mut t, 300, 2, true).is_empty());
        run(&mut a, &mut t, 150, 1, true);
        assert!(run(&mut a, &mut t, 300, 2, true).is_empty());
        a.reset();
        assert_eq!(a.rate(GRID), GRID);
    }
}
