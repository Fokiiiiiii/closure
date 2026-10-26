//! Heartbeat round trips of the Gateway connection, shared with the modules.

use std::collections::{HashMap, VecDeque};
use std::sync::{Mutex, MutexGuard, PoisonError};
use std::time::Duration;

/// How many recent round trips each shard keeps.
pub const SAMPLES_PER_SHARD: usize = 5;

/// What a set of round trips adds up to.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct LatencySummary {
    pub min: Duration,
    pub mean: Duration,
    pub max: Duration,
    /// How many round trips the summary covers.
    pub samples: usize,
}

impl LatencySummary {
    /// The summary of `samples`, or `None` when there are none.
    #[must_use]
    pub fn of(samples: &[Duration]) -> Option<Self> {
        let (Some(&min), Some(&max)) = (samples.iter().min(), samples.iter().max()) else {
            return None;
        };
        let count = u32::try_from(samples.len()).ok()?;
        let total: Duration = samples.iter().sum();
        Some(Self {
            min,
            mean: total / count,
            max,
            samples: samples.len(),
        })
    }
}

/// The latest heartbeat round trips of each Gateway shard. The Gateway task writes them and the
/// dispatcher hands them to the modules; the lock is only held for one update or read.
#[derive(Debug, Default)]
pub struct GatewayLatency {
    shards: Mutex<HashMap<u64, VecDeque<Duration>>>,
}

impl GatewayLatency {
    /// Keeps `latency` as the newest round trip of `shard`, dropping the oldest beyond
    /// [`SAMPLES_PER_SHARD`].
    pub fn record(&self, shard: u64, latency: Duration) {
        let mut shards = self.lock();
        let samples = shards.entry(shard).or_default();
        if samples.len() == SAMPLES_PER_SHARD {
            samples.pop_front();
        }
        samples.push_back(latency);
    }

    /// The mean of every retained round trip, or `None` before the first one.
    #[must_use]
    pub fn mean(&self) -> Option<Duration> {
        self.summary().map(|summary| summary.mean)
    }

    /// The minimum, mean and maximum of every retained round trip of every shard.
    #[must_use]
    pub fn summary(&self) -> Option<LatencySummary> {
        let all: Vec<Duration> = self.lock().values().flatten().copied().collect();
        LatencySummary::of(&all)
    }

    fn lock(&self) -> MutexGuard<'_, HashMap<u64, VecDeque<Duration>>> {
        self.shards.lock().unwrap_or_else(PoisonError::into_inner)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn ms(value: u64) -> Duration {
        Duration::from_millis(value)
    }

    #[test]
    fn nothing_is_measured_before_the_first_round_trip() {
        let latency = GatewayLatency::default();
        assert_eq!(latency.mean(), None);
        assert_eq!(latency.summary(), None);
    }

    #[test]
    fn the_mean_covers_every_shard() {
        let latency = GatewayLatency::default();
        latency.record(0, ms(10));
        latency.record(0, ms(20));
        latency.record(1, ms(60));
        assert_eq!(latency.mean(), Some(ms(30)));
    }

    #[test]
    fn the_summary_gives_the_minimum_mean_and_maximum() {
        let latency = GatewayLatency::default();
        latency.record(0, ms(12));
        latency.record(1, ms(30));
        latency.record(2, ms(6));
        assert_eq!(
            latency.summary(),
            Some(LatencySummary {
                min: ms(6),
                mean: ms(16),
                max: ms(30),
                samples: 3,
            })
        );
    }

    #[test]
    fn each_shard_keeps_only_its_newest_round_trips() {
        let latency = GatewayLatency::default();
        for value in 1..=(SAMPLES_PER_SHARD as u64 + 2) {
            latency.record(7, ms(value * 100));
        }
        latency.record(8, ms(1000));
        let summary = latency.summary().unwrap_or_else(|| panic!("measured"));
        // Shard 7 keeps 300..=700 ms, shard 8 keeps its single 1000 ms.
        assert_eq!(summary.samples, SAMPLES_PER_SHARD + 1);
        assert_eq!(summary.min, ms(300));
        assert_eq!(summary.max, ms(1000));
    }

    #[test]
    fn a_summary_of_no_samples_is_none() {
        assert_eq!(LatencySummary::of(&[]), None);
    }
}
