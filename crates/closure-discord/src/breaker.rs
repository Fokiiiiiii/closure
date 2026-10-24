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

/// Proof that a call was admitted, handed back with its result. It says which period of the
/// breaker the call belongs to and whether the call is the recovery trial, so that a result is
/// never mistaken for another call's.
#[derive(Debug, Clone, Copy)]
pub(crate) struct Ticket {
    generation: u64,
    trial: bool,
}

/// A circuit breaker for one module in one guild.
///
/// A module that keeps timing out or panicking would otherwise burn a task and a timeout's worth
/// of latency on every event. After `threshold` consecutive failures its calls are skipped for
/// `cooldown`; then a single trial call decides whether it recovered.
///
/// Calls can overlap and handlers may run for a minute, so a call admitted before the breaker
/// opened can finish while a trial is running. Results of such calls (an older `generation`)
/// are ignored: they say nothing about whether the module has recovered.
#[derive(Debug, Default)]
pub(crate) struct Breaker {
    consecutive_failures: u32,
    open_until: Option<Instant>,
    /// A recovery attempt was let through and has not reported yet.
    trial_in_flight: bool,
    /// Counts how often the breaker opened.
    generation: u64,
}

impl Breaker {
    /// Admits a call now, or refuses it. After the cooldown, the first caller is admitted as the
    /// trial and the breaker closes the door again until that trial reports back.
    pub(crate) fn allow(&mut self, now: Instant) -> Option<Ticket> {
        let trial = match self.open_until {
            None => false,
            Some(until) if now < until => return None,
            // The cooldown is over. One call at a time may test whether the module recovered,
            // however long it takes: others stay out until it reports.
            Some(_) if self.trial_in_flight => return None,
            Some(_) => {
                self.trial_in_flight = true;
                true
            }
        };
        Some(Ticket {
            generation: self.generation,
            trial,
        })
    }

    pub(crate) fn record_success(&mut self, ticket: Ticket) {
        if ticket.generation != self.generation {
            return;
        }
        if ticket.trial {
            self.trial_in_flight = false;
        }
        self.consecutive_failures = 0;
        self.open_until = None;
    }

    pub(crate) fn record_failure(&mut self, ticket: Ticket, now: Instant, config: &BreakerConfig) {
        if ticket.generation != self.generation {
            return;
        }
        if ticket.trial {
            self.trial_in_flight = false;
        }
        self.consecutive_failures = self.consecutive_failures.saturating_add(1);
        if self.consecutive_failures >= config.threshold {
            self.open_until = Some(now + config.cooldown);
            self.generation += 1;
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

    /// Admits a call (the breaker must allow it) and fails it.
    fn fail(breaker: &mut Breaker, now: Instant) {
        let ticket = breaker.allow(now).expect("admitted");
        breaker.record_failure(ticket, now, &CONFIG);
    }

    /// Fails calls until the breaker is open.
    fn open(breaker: &mut Breaker, now: Instant) {
        for _ in 0..CONFIG.threshold {
            fail(breaker, now);
        }
        assert!(breaker.is_open(now));
    }

    #[test]
    fn opens_after_the_threshold_of_consecutive_failures() {
        let now = Instant::now();
        let mut breaker = Breaker::default();
        for _ in 0..2 {
            fail(&mut breaker, now);
        }
        assert!(
            breaker.allow(now).is_some(),
            "still closed after two failures"
        );
        fail(&mut breaker, now);
        assert!(breaker.allow(now).is_none());
        assert!(breaker.is_open(now));
    }

    #[test]
    fn a_success_resets_the_count() {
        let now = Instant::now();
        let mut breaker = Breaker::default();
        fail(&mut breaker, now);
        fail(&mut breaker, now);
        let ticket = breaker.allow(now).unwrap();
        breaker.record_success(ticket);
        fail(&mut breaker, now);
        fail(&mut breaker, now);
        assert!(breaker.allow(now).is_some());
    }

    #[test]
    fn after_the_cooldown_exactly_one_trial_is_admitted() {
        let start = Instant::now();
        let mut breaker = Breaker::default();
        open(&mut breaker, start);
        let later = start + CONFIG.cooldown;
        assert!(breaker.allow(later).is_some(), "the trial");
        assert!(
            breaker.allow(later).is_none(),
            "others wait for the trial's verdict"
        );
    }

    #[test]
    fn a_slow_trial_keeps_everyone_else_out_until_it_reports() {
        let start = Instant::now();
        let mut breaker = Breaker::default();
        open(&mut breaker, start);
        let first = start + CONFIG.cooldown;
        let trial = breaker.allow(first).expect("the trial");
        // The trial runs for several cooldowns; nobody else may be admitted meanwhile.
        for waited in 1..=5 {
            assert!(
                breaker.allow(first + CONFIG.cooldown * waited).is_none(),
                "{waited}"
            );
        }
        breaker.record_success(trial);
        assert!(
            breaker.allow(first).is_some(),
            "closed again after the trial succeeded"
        );
    }

    #[test]
    fn a_successful_trial_closes_the_breaker_and_a_failed_one_reopens_it() {
        let start = Instant::now();
        let later = start + CONFIG.cooldown;

        let mut recovering = Breaker::default();
        open(&mut recovering, start);
        let trial = recovering.allow(later).unwrap();
        recovering.record_success(trial);
        assert!(recovering.allow(later).is_some());
        assert!(!recovering.is_open(later));

        let mut relapsing = Breaker::default();
        open(&mut relapsing, start);
        let trial = relapsing.allow(later).unwrap();
        relapsing.record_failure(trial, later, &CONFIG);
        assert!(relapsing.allow(later + CONFIG.cooldown / 2).is_none());
        assert!(relapsing.allow(later + CONFIG.cooldown).is_some());
    }

    #[test]
    fn results_of_calls_admitted_before_the_breaker_opened_do_not_disturb_the_trial() {
        let start = Instant::now();
        let mut breaker = Breaker::default();
        // Slow calls admitted while the breaker was still closed.
        let slow_failure = breaker.allow(start).unwrap();
        let slow_success = breaker.allow(start).unwrap();
        open(&mut breaker, start);

        let trial_at = start + CONFIG.cooldown;
        let trial = breaker.allow(trial_at).expect("the trial");

        // They finish while the trial is running.
        breaker.record_failure(slow_failure, trial_at, &CONFIG);
        breaker.record_success(slow_success);

        assert!(
            breaker.allow(trial_at + CONFIG.cooldown * 3).is_none(),
            "the trial is still the only call under test"
        );
        assert!(breaker.is_open(trial_at - Duration::from_secs(1)));
        breaker.record_success(trial);
        assert!(
            breaker.allow(trial_at).is_some(),
            "closed by the trial alone"
        );
    }
}
