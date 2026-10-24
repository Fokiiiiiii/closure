//! Keeps a limit that is hit over and over from filling the log.

use std::sync::{Mutex, PoisonError};
use std::time::{Duration, Instant};

/// Lets one message through per interval and counts what it held back, so a limit that is
/// reached thousands of times a second is one line of log a few seconds apart, not thousands.
#[derive(Debug)]
pub struct LogGate {
    interval: Duration,
    state: Mutex<State>,
}

#[derive(Debug, Default)]
struct State {
    last: Option<Instant>,
    held_back: u64,
}

impl LogGate {
    #[must_use]
    pub const fn new(interval: Duration) -> Self {
        Self {
            interval,
            state: Mutex::new(State {
                last: None,
                held_back: 0,
            }),
        }
    }

    /// `Some(n)` when this message is to be logged now, `n` being how many were held back since
    /// the last one that was; `None` when it is to be left out.
    pub fn pass(&self) -> Option<u64> {
        self.pass_at(Instant::now())
    }

    fn pass_at(&self, now: Instant) -> Option<u64> {
        let mut state = self.state.lock().unwrap_or_else(PoisonError::into_inner);
        let due = state
            .last
            .is_none_or(|last| now.saturating_duration_since(last) >= self.interval);
        if due {
            state.last = Some(now);
            Some(std::mem::take(&mut state.held_back))
        } else {
            state.held_back += 1;
            None
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn one_message_per_interval_with_the_count_of_those_held_back() {
        let gate = LogGate::new(Duration::from_secs(10));
        let start = Instant::now();
        assert_eq!(gate.pass_at(start), Some(0));
        for second in 1..10 {
            assert_eq!(gate.pass_at(start + Duration::from_secs(second)), None);
        }
        assert_eq!(gate.pass_at(start + Duration::from_secs(10)), Some(9));
        // The count starts over.
        assert_eq!(gate.pass_at(start + Duration::from_secs(11)), None);
        assert_eq!(gate.pass_at(start + Duration::from_secs(20)), Some(1));
    }

    #[test]
    fn a_clock_that_goes_back_holds_the_message_back() {
        let gate = LogGate::new(Duration::from_secs(10));
        let start = Instant::now();
        assert_eq!(gate.pass_at(start + Duration::from_secs(100)), Some(0));
        assert_eq!(gate.pass_at(start + Duration::from_secs(50)), None);
    }
}
