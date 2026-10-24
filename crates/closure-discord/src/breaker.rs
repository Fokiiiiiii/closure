use std::time::Duration;

use tokio::time::Instant;

/// When a module is cut off after repeated failures, and for how long.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct BreakerConfig {
    /// Consecutive failures that open the breaker.
    pub threshold: u32,
    /// How long calls are skipped before one trial call is let through.
    pub cooldown: Duration,
}

impl Default for BreakerConfig {
    fn default() -> Self {
        Self {
            threshold: 5,
            cooldown: Duration::from_secs(30),
        }
    }
}

/// A circuit breaker for one module in one guild.
///
/// A module that keeps timing out or panicking would otherwise burn a task and a timeout's worth
/// of latency on every event. After `threshold` consecutive failures its calls are skipped for
/// `cooldown`; then a single trial call decides whether it recovered.
#[derive(Debug, Default)]
pub(crate) struct Breaker {
    consecutive_failures: u32,
    open_until: Option<Instant>,
    /// A recovery attempt was let through and has not reported yet.
    trial_in_flight: bool,
}

impl Breaker {
    /// Whether a call may go ahead now. After the cooldown, the first caller is admitted as the
    /// trial and the breaker closes the door again until that trial reports back.
    pub(crate) fn allow(&mut self, now: Instant) -> bool {
        match self.open_until {
            None => true,
            Some(until) if now < until => false,
            // The cooldown is over. One call at a time may test whether the module recovered,
            // however long it takes: others stay out until it reports.
            Some(_) if self.trial_in_flight => false,
            Some(_) => {
                self.trial_in_flight = true;
                true
            }
        }
    }

    pub(crate) fn record_success(&mut self) {
        self.consecutive_failures = 0;
        self.open_until = None;
        self.trial_in_flight = false;
    }

    pub(crate) fn record_failure(&mut self, now: Instant, config: &BreakerConfig) {
        self.trial_in_flight = false;
        self.consecutive_failures = self.consecutive_failures.saturating_add(1);
        if self.consecutive_failures >= config.threshold {
            self.open_until = Some(now + config.cooldown);
        }
    }

    pub(crate) fn is_open(&self, now: Instant) -> bool {
        self.open_until.is_some_and(|until| now < until)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const CONFIG: BreakerConfig = BreakerConfig {
        threshold: 3,
        cooldown: Duration::from_secs(10),
    };

    #[test]
    fn opens_after_the_threshold_of_consecutive_failures() {
        let now = Instant::now();
        let mut breaker = Breaker::default();
        for _ in 0..2 {
            assert!(breaker.allow(now));
            breaker.record_failure(now, &CONFIG);
        }
        assert!(breaker.allow(now), "still closed after two failures");
        breaker.record_failure(now, &CONFIG);
        assert!(!breaker.allow(now));
        assert!(breaker.is_open(now));
    }

    #[test]
    fn a_success_resets_the_count() {
        let now = Instant::now();
        let mut breaker = Breaker::default();
        breaker.record_failure(now, &CONFIG);
        breaker.record_failure(now, &CONFIG);
        breaker.record_success();
        breaker.record_failure(now, &CONFIG);
        breaker.record_failure(now, &CONFIG);
        assert!(breaker.allow(now));
    }

    #[test]
    fn after_the_cooldown_exactly_one_trial_is_admitted() {
        let start = Instant::now();
        let mut breaker = Breaker::default();
        for _ in 0..3 {
            breaker.record_failure(start, &CONFIG);
        }
        let later = start + CONFIG.cooldown;
        assert!(breaker.allow(later), "the trial");
        assert!(!breaker.allow(later), "others wait for the trial's verdict");
    }

    #[test]
    fn a_slow_trial_keeps_everyone_else_out_until_it_reports() {
        let start = Instant::now();
        let mut breaker = Breaker::default();
        for _ in 0..3 {
            breaker.record_failure(start, &CONFIG);
        }
        let first = start + CONFIG.cooldown;
        assert!(breaker.allow(first), "the trial");
        // The trial runs for several cooldowns; nobody else may be admitted meanwhile.
        for waited in 1..=5 {
            assert!(!breaker.allow(first + CONFIG.cooldown * waited), "{waited}");
        }
        breaker.record_success();
        assert!(
            breaker.allow(first),
            "closed again after the trial succeeded"
        );
    }

    #[test]
    fn a_successful_trial_closes_the_breaker_and_a_failed_one_reopens_it() {
        let start = Instant::now();
        let later = start + CONFIG.cooldown;

        let mut recovering = Breaker::default();
        for _ in 0..3 {
            recovering.record_failure(start, &CONFIG);
        }
        assert!(recovering.allow(later));
        recovering.record_success();
        assert!(recovering.allow(later));
        assert!(!recovering.is_open(later));

        let mut relapsing = Breaker::default();
        for _ in 0..3 {
            relapsing.record_failure(start, &CONFIG);
        }
        assert!(relapsing.allow(later));
        relapsing.record_failure(later, &CONFIG);
        assert!(!relapsing.allow(later + CONFIG.cooldown / 2));
        assert!(relapsing.allow(later + CONFIG.cooldown));
    }
}
