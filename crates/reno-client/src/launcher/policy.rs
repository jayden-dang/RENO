//! When the app or the CLI may start the daemon again: the restart backoff.
//!
//! Pure state, no clock and no timer: the caller passes the time of each event. A start is
//! allowed at once the first time; each start after that waits twice as long as the one before
//! (1 s, 2 s, 4 s, ... up to 60 s) counted from the last start, until the daemon has stayed
//! reachable for the stable period, which forgets the failures.

use std::time::{Duration, Instant};

/// The tuning of a [`RestartPolicy`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct RestartConfig {
    /// The wait after the first start.
    pub initial: Duration,
    /// The longest wait.
    pub max: Duration,
    /// How long the daemon must stay reachable before its failures are forgotten.
    pub stable_after: Duration,
}

impl Default for RestartConfig {
    fn default() -> Self {
        Self {
            initial: Duration::from_secs(1),
            max: Duration::from_secs(60),
            stable_after: Duration::from_secs(60),
        }
    }
}

/// What the policy says to do when the daemon is unreachable.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Verdict {
    /// Start it now.
    Start,
    /// Too soon since the last start: wait this long.
    Wait(Duration),
}

/// The restart backoff. See the module docs.
#[derive(Debug, Clone)]
pub struct RestartPolicy {
    config: RestartConfig,
    /// Starts since the daemon last stayed up.
    starts: u32,
    next_allowed: Option<Instant>,
    reachable_since: Option<Instant>,
}

impl RestartPolicy {
    /// A policy that has not started anything yet.
    pub fn new(config: RestartConfig) -> Self {
        Self {
            config,
            starts: 0,
            next_allowed: None,
            reachable_since: None,
        }
    }

    /// The daemon answered at `now`.
    pub fn reachable(&mut self, now: Instant) {
        self.reachable_since.get_or_insert(now);
    }

    /// The daemon did not answer at `now`: may it be started?
    pub fn unreachable(&mut self, now: Instant) -> Verdict {
        if let Some(since) = self.reachable_since.take()
            && now.saturating_duration_since(since) >= self.config.stable_after
        {
            self.starts = 0;
            self.next_allowed = None;
        }
        if let Some(allowed) = self.next_allowed
            && now < allowed
        {
            return Verdict::Wait(allowed - now);
        }
        let doublings = self.starts.min(20);
        let wait = self
            .config
            .initial
            .saturating_mul(1u32 << doublings)
            .min(self.config.max);
        self.starts = self.starts.saturating_add(1);
        self.next_allowed = Some(now + wait);
        Verdict::Start
    }

    /// Starts since the daemon last stayed up.
    pub fn starts(&self) -> u32 {
        self.starts
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const S: Duration = Duration::from_secs(1);

    #[test]
    fn starts_back_off_1_2_4_up_to_60_seconds() {
        let t0 = Instant::now();
        let mut policy = RestartPolicy::new(RestartConfig::default());
        let mut now = t0;
        let mut gaps = Vec::new();
        for _ in 0..9 {
            assert_eq!(policy.unreachable(now), Verdict::Start);
            let Verdict::Wait(gap) = policy.unreachable(now) else {
                panic!("a second start at the same instant");
            };
            gaps.push(gap.as_secs());
            now += gap;
        }
        assert_eq!(gaps, [1, 2, 4, 8, 16, 32, 60, 60, 60]);
    }

    #[test]
    fn asking_early_waits_out_the_rest_and_late_starts() {
        let t0 = Instant::now();
        let mut policy = RestartPolicy::new(RestartConfig::default());
        assert_eq!(policy.unreachable(t0), Verdict::Start);
        assert_eq!(policy.unreachable(t0 + S / 4), Verdict::Wait(S * 3 / 4));
        assert_eq!(policy.unreachable(t0 + S), Verdict::Start);
        assert_eq!(policy.starts(), 2);
    }

    #[test]
    fn a_daemon_that_stayed_up_starts_the_backoff_over_and_one_that_did_not_does_not() {
        let t0 = Instant::now();
        let mut policy = RestartPolicy::new(RestartConfig::default());
        let mut now = t0;
        for _ in 0..4 {
            assert_eq!(policy.unreachable(now), Verdict::Start);
            now += Duration::from_secs(60);
        }
        assert_eq!(policy.starts(), 4);

        // Up for 59 s, then gone: still counted.
        policy.reachable(now);
        now += Duration::from_secs(59);
        policy.reachable(now);
        assert_eq!(policy.unreachable(now), Verdict::Start);
        assert_eq!(policy.starts(), 5);
        let gap = match policy.unreachable(now) {
            Verdict::Wait(gap) => gap,
            Verdict::Start => panic!("too soon"),
        };
        assert_eq!(gap, Duration::from_secs(16));

        // Up for the stable period: the next loss is a first loss.
        now += gap;
        policy.reachable(now);
        now += Duration::from_secs(60);
        assert_eq!(policy.unreachable(now), Verdict::Start);
        assert_eq!(policy.starts(), 1);
        assert_eq!(policy.unreachable(now), Verdict::Wait(S));
    }
}
