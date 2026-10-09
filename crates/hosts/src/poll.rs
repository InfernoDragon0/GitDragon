//! When to refresh host data and auto-fetch: a configurable interval, stretched while the
//! window is unfocused, refreshed on focus, doubled after failures and paused while the host's
//! rate limit is spent. Pure timing logic; the app owns the timers.

use std::time::{Duration, Instant, SystemTime};

use serde::{Deserialize, Serialize};

/// User settings (persisted with the app settings).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct PollConfig {
    /// PR / issue / CI refresh, seconds; 0 = only on demand.
    pub refresh_secs: u32,
    /// Background `git fetch`, seconds; 0 = off.
    pub fetch_secs: u32,
    /// Intervals are multiplied by this while the window is unfocused.
    pub unfocused_factor: u32,
}

impl Default for PollConfig {
    fn default() -> Self {
        Self {
            refresh_secs: 120,
            fetch_secs: 600,
            unfocused_factor: 5,
        }
    }
}

/// Shortest gap between two focus-triggered refreshes (alt-tabbing should not spam).
const FOCUS_MIN_GAP: Duration = Duration::from_secs(30);
/// Failures double the wait up to 2^4 = 16 times the interval.
const MAX_BACKOFF_SHIFT: u32 = 4;

/// One periodic job (host refresh or auto-fetch).
#[derive(Debug, Clone)]
pub struct PollSchedule {
    interval: Option<Duration>,
    unfocused_factor: u32,
    focused: bool,
    last: Option<Instant>,
    failures: u32,
}

impl PollSchedule {
    /// `interval_secs` 0 disables it.
    pub fn new(interval_secs: u32, unfocused_factor: u32) -> Self {
        Self {
            interval: (interval_secs > 0).then(|| Duration::from_secs(interval_secs.into())),
            unfocused_factor: unfocused_factor.max(1),
            focused: true,
            last: None,
            failures: 0,
        }
    }

    pub fn host_refresh(config: &PollConfig) -> Self {
        Self::new(config.refresh_secs, config.unfocused_factor)
    }

    pub fn auto_fetch(config: &PollConfig) -> Self {
        Self::new(config.fetch_secs, config.unfocused_factor)
    }

    /// Focus changed; `true` = refresh now (regained focus and the data is not brand new).
    pub fn set_focused(&mut self, focused: bool, now: Instant) -> bool {
        let regained = focused && !self.focused;
        self.focused = focused;
        regained
            && self.interval.is_some()
            && self
                .last
                .is_none_or(|last| now.duration_since(last) >= FOCUS_MIN_GAP)
    }

    /// The current wait between runs, `None` when disabled.
    pub fn period(&self) -> Option<Duration> {
        let mut period = self.interval?;
        if !self.focused {
            period *= self.unfocused_factor;
        }
        Some(period * (1u32 << self.failures.min(MAX_BACKOFF_SHIFT)))
    }

    /// When the next run is due; `blocked_until` is the host's rate-limit block, if any.
    pub fn next_due(&self, blocked_until: Option<SystemTime>) -> Option<Instant> {
        let period = self.period()?;
        let due = self.last.map_or_else(Instant::now, |last| last + period);
        let wait = blocked_until
            .and_then(|t| t.duration_since(SystemTime::now()).ok())
            .map_or(due, |w| due.max(Instant::now() + w));
        Some(wait)
    }

    pub fn is_due(&self, now: Instant, blocked_until: Option<SystemTime>) -> bool {
        self.next_due(blocked_until).is_some_and(|due| due <= now)
    }

    /// A run finished; failures back off, success resets.
    pub fn record(&mut self, now: Instant, ok: bool) {
        self.last = Some(now);
        self.failures = if ok {
            0
        } else {
            self.failures.saturating_add(1)
        };
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const MIN: Duration = Duration::from_secs(60);

    #[test]
    fn interval_focus_and_backoff() {
        let mut s = PollSchedule::new(60, 5);
        let t0 = Instant::now();
        assert!(
            s.is_due(t0 + Duration::from_millis(1), None),
            "first run is immediate"
        );
        s.record(t0, true);
        assert!(!s.is_due(t0 + MIN / 2, None));
        assert!(s.is_due(t0 + MIN, None));

        assert!(!s.set_focused(false, t0));
        assert_eq!(s.period(), Some(MIN * 5));
        assert!(!s.is_due(t0 + MIN * 2, None));
        // Back within 30 s of the last run: no extra refresh.
        assert!(!s.set_focused(true, t0 + Duration::from_secs(10)));
        assert!(!s.set_focused(false, t0 + Duration::from_secs(11)));
        assert!(s.set_focused(true, t0 + MIN));
        assert_eq!(s.period(), Some(MIN));

        s.record(t0, false);
        s.record(t0, false);
        assert_eq!(s.period(), Some(MIN * 4));
        for _ in 0..10 {
            s.record(t0, false);
        }
        assert_eq!(s.period(), Some(MIN * 16));
        s.record(t0, true);
        assert_eq!(s.period(), Some(MIN));
    }

    #[test]
    fn rate_limit_pushes_the_next_run() {
        let mut s = PollSchedule::new(60, 5);
        let t0 = Instant::now();
        s.record(t0, true);
        let blocked = Some(SystemTime::now() + MIN * 10);
        assert!(!s.is_due(t0 + MIN * 2, blocked));
        assert!(s.is_due(t0 + MIN * 11, blocked));
        assert!(s.is_due(t0 + MIN * 2, Some(SystemTime::now() - MIN)));
    }

    #[test]
    fn disabled() {
        let mut s = PollSchedule::new(0, 5);
        assert_eq!(s.next_due(None), None);
        assert!(!s.set_focused(false, Instant::now()));
        assert!(!s.set_focused(true, Instant::now()));
        let config: PollConfig = serde_json::from_str(r#"{"fetch_secs": 0}"#).unwrap();
        assert_eq!(config.refresh_secs, 120);
        assert_eq!(PollSchedule::auto_fetch(&config).period(), None);
    }
}
