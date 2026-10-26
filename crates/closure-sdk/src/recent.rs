//! Recent actions: what members did in the last moments, kept in memory so a module can count a
//! burst (messages, mentions, joins) and act once per burst. Nothing is persisted: a restart
//! starts empty, as in Zeppelin.
//!
//! A window starts inclusively: an action exactly `within` seconds old still counts. The
//! filters gate a window: when the selected actions do not span enough distinct groups or
//! subjects, [`RecentActions::sum`] is 0 and [`RecentActions::take`] takes nothing.

use std::collections::{BTreeSet, HashMap, HashSet, VecDeque};
use std::sync::{Arc, Mutex, MutexGuard, PoisonError};
use std::time::Duration;

use closure_core::event::{ChannelId, GuildId, MessageId};

/// The most handled keys kept per guild; the one expiring soonest goes first.
const MAX_HANDLED_PER_GUILD: usize = 10_000;

/// How much is kept per guild.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RecentLimits {
    pub max_entries_per_guild: usize,
    /// Actions older than this, counted from the newest recorded one, are dropped.
    pub max_age: Duration,
}

impl Default for RecentLimits {
    fn default() -> Self {
        Self {
            max_entries_per_guild: 20_000,
            max_age: Duration::from_secs(60 * 60),
        }
    }
}

/// One action, such as a message or a join. `count` is how many actions it stands for.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Recent {
    pub kind: String,
    /// A user id, or any other key the module counts by.
    pub subject: u64,
    /// What the action is grouped by, such as its channel or a hash of its content.
    pub group: Option<String>,
    pub count: u32,
    /// Unix seconds.
    pub at: i64,
    pub messages: Vec<(ChannelId, MessageId)>,
    pub tag: Option<String>,
}

/// Narrows a window to one group and sets how many distinct groups and subjects it must span.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct RecentFilter {
    pub group: Option<String>,
    pub distinct_groups_at_least: Option<usize>,
    pub distinct_subjects_at_least: Option<usize>,
}

/// A window that a burst is checked against, and the key that marks the burst handled once it fires.
#[derive(Debug, Clone, Copy)]
pub struct Burst<'a> {
    pub kind: &'a str,
    /// The subject whose actions count; `None` counts every subject.
    pub subject: Option<u64>,
    pub within: Duration,
    pub filter: &'a RecentFilter,
    pub handled_key: &'a str,
    /// Unix second the key stops being handled, set when a burst is claimed.
    pub handled_until: i64,
}

/// What a check of a [`Burst`] found.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Claim {
    /// The key is handled: the burst has already fired.
    Handled,
    /// The window does not pass its gates, or the count it was given.
    Short,
    /// The burst passes. `entries` are the actions it counted, oldest first, and `count` is the
    /// value the count gave them.
    Counted { count: u64, entries: Vec<Recent> },
}

#[derive(Debug, Default)]
struct GuildLog {
    /// Oldest first.
    entries: VecDeque<Recent>,
    /// Handled key to the Unix second it stops being handled.
    handled: HashMap<String, i64>,
    /// The same keys ordered by expiry, so expired ones leave without a scan.
    expiring: BTreeSet<(i64, String)>,
}

impl GuildLog {
    fn prune_handled(&mut self, now: i64) {
        while let Some((until, key)) = self.expiring.first().cloned() {
            if until > now {
                break;
            }
            self.expiring.pop_first();
            self.handled.remove(&key);
        }
    }
}

#[derive(Debug)]
struct Inner {
    limits: RecentLimits,
    guilds: Mutex<HashMap<GuildId, GuildLog>>,
}

/// The recent actions of every guild. Cloning shares the same store.
#[derive(Debug, Clone)]
pub struct RecentActions {
    inner: Arc<Inner>,
}

/// The part of a window that an action must be in: kind, subject, group, and the start of the
/// window (`cutoff`, Unix seconds).
#[derive(Debug, Clone, Copy)]
struct Query<'a> {
    kind: &'a str,
    subject: Option<u64>,
    group: Option<&'a str>,
    cutoff: i64,
}

impl Query<'_> {
    fn matches(self, entry: &Recent) -> bool {
        entry.at >= self.cutoff
            && entry.kind == self.kind
            && self.subject.is_none_or(|subject| entry.subject == subject)
            && self
                .group
                .is_none_or(|group| entry.group.as_deref() == Some(group))
    }
}

/// The actions of `log` that match, oldest first.
fn select<'l>(log: &'l GuildLog, query: Query<'_>) -> Vec<&'l Recent> {
    let start = log.entries.partition_point(|entry| entry.at < query.cutoff);
    log.entries
        .range(start..)
        .filter(|entry| query.matches(entry))
        .collect()
}

/// Whether the selected actions span enough distinct groups and subjects.
fn spans_enough(filter: &RecentFilter, selected: &[&Recent]) -> bool {
    if let Some(min) = filter.distinct_groups_at_least {
        let groups: HashSet<Option<&str>> = selected.iter().map(|e| e.group.as_deref()).collect();
        if groups.len() < min {
            return false;
        }
    }
    if let Some(min) = filter.distinct_subjects_at_least {
        let subjects: HashSet<u64> = selected.iter().map(|e| e.subject).collect();
        if subjects.len() < min {
            return false;
        }
    }
    true
}

impl RecentActions {
    #[must_use]
    pub fn new(limits: RecentLimits) -> Self {
        Self {
            inner: Arc::new(Inner {
                limits,
                guilds: Mutex::new(HashMap::new()),
            }),
        }
    }

    fn guilds(&self) -> MutexGuard<'_, HashMap<GuildId, GuildLog>> {
        // The map is only ever left whole by the methods below, so a poisoned lock is still usable.
        self.inner
            .guilds
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
    }

    /// Stores an action. Actions older than `max_age` before it, and the oldest ones past
    /// `max_entries_per_guild`, are dropped.
    pub fn record(&self, guild: GuildId, entry: Recent) {
        let max_age = i64::try_from(self.inner.limits.max_age.as_secs()).unwrap_or(i64::MAX);
        let max_entries = self.inner.limits.max_entries_per_guild;
        let at = entry.at;
        let oldest = at.saturating_sub(max_age);
        let mut guilds = self.guilds();
        let log = guilds.entry(guild).or_default();
        log.prune_handled(at);
        while log.entries.front().is_some_and(|front| front.at < oldest) {
            log.entries.pop_front();
        }
        // Keeps the deque sorted by time even when a clock steps back.
        let position = log.entries.partition_point(|e| e.at <= at);
        log.entries.insert(position, entry);
        while log.entries.len() > max_entries {
            log.entries.pop_front();
        }
    }

    /// How many actions of `kind` (of `subject`, or of any subject) happened in the last
    /// `within`, up to `now`, and pass `filter`.
    #[must_use]
    pub fn sum(
        &self,
        guild: GuildId,
        kind: &str,
        subject: Option<u64>,
        within: Duration,
        now: i64,
        filter: &RecentFilter,
    ) -> u32 {
        let guilds = self.guilds();
        let Some(log) = guilds.get(&guild) else {
            return 0;
        };
        let selected = select(log, query(kind, subject, within, now, filter));
        if !spans_enough(filter, &selected) {
            return 0;
        }
        selected
            .iter()
            .fold(0, |total: u32, entry| total.saturating_add(entry.count))
    }

    /// The actions [`Self::sum`] counts, oldest first.
    #[must_use]
    pub fn entries(
        &self,
        guild: GuildId,
        kind: &str,
        subject: Option<u64>,
        within: Duration,
        now: i64,
        filter: &RecentFilter,
    ) -> Vec<Recent> {
        let guilds = self.guilds();
        let Some(log) = guilds.get(&guild) else {
            return Vec::new();
        };
        let selected = select(log, query(kind, subject, within, now, filter));
        if !spans_enough(filter, &selected) {
            return Vec::new();
        }
        selected.into_iter().cloned().collect()
    }

    /// Removes and returns the actions [`Self::sum`] counts, so one burst trips a rule once.
    #[must_use]
    pub fn take(
        &self,
        guild: GuildId,
        kind: &str,
        subject: Option<u64>,
        within: Duration,
        now: i64,
        filter: &RecentFilter,
    ) -> Vec<Recent> {
        let mut guilds = self.guilds();
        let Some(log) = guilds.get_mut(&guild) else {
            return Vec::new();
        };
        let wanted = query(kind, subject, within, now, filter);
        if !spans_enough(filter, &select(log, wanted)) {
            return Vec::new();
        }
        let mut taken = Vec::new();
        let mut kept = VecDeque::with_capacity(log.entries.len());
        for entry in log.entries.drain(..) {
            if wanted.matches(&entry) {
                taken.push(entry);
            } else {
                kept.push_back(entry);
            }
        }
        log.entries = kept;
        taken
    }

    /// Whether `key` was marked handled and has not expired at `now`.
    #[must_use]
    pub fn already_handled(&self, guild: GuildId, key: &str, now: i64) -> bool {
        let mut guilds = self.guilds();
        let Some(log) = guilds.get_mut(&guild) else {
            return false;
        };
        log.prune_handled(now);
        log.handled.contains_key(key)
    }

    /// Marks `key` handled until the Unix second `until`; later actions of the same burst then
    /// do not trigger again.
    pub fn mark_handled(&self, guild: GuildId, key: &str, until: i64) {
        let mut guilds = self.guilds();
        mark(guilds.entry(guild).or_default(), key, until);
    }

    /// Checks a burst and, when it passes, takes the actions it counts out of the window and marks
    /// its key handled. The check and the taking happen under one lock, so two checks of the same
    /// burst cannot both pass. `counts` gives the count of the window, or `None` when it does not pass.
    #[must_use]
    pub fn claim_burst(
        &self,
        guild: GuildId,
        now: i64,
        burst: &Burst<'_>,
        counts: impl FnOnce(&[&Recent]) -> Option<u64>,
    ) -> Claim {
        self.burst(guild, now, burst, true, counts)
    }

    /// The check of [`Self::claim_burst`], without taking or marking anything.
    #[must_use]
    pub fn inspect_burst(
        &self,
        guild: GuildId,
        now: i64,
        burst: &Burst<'_>,
        counts: impl FnOnce(&[&Recent]) -> Option<u64>,
    ) -> Claim {
        self.burst(guild, now, burst, false, counts)
    }

    fn burst(
        &self,
        guild: GuildId,
        now: i64,
        burst: &Burst<'_>,
        commit: bool,
        counts: impl FnOnce(&[&Recent]) -> Option<u64>,
    ) -> Claim {
        let mut guilds = self.guilds();
        let log = guilds.entry(guild).or_default();
        log.prune_handled(now);
        if log.handled.contains_key(burst.handled_key) {
            return Claim::Handled;
        }
        let wanted = query(burst.kind, burst.subject, burst.within, now, burst.filter);
        let (count, entries) = {
            let selected = select(log, wanted);
            if !spans_enough(burst.filter, &selected) {
                return Claim::Short;
            }
            let Some(count) = counts(&selected) else {
                return Claim::Short;
            };
            (count, selected.into_iter().cloned().collect::<Vec<_>>())
        };
        if commit {
            log.entries.retain(|entry| !wanted.matches(entry));
            mark(log, burst.handled_key, burst.handled_until);
        }
        Claim::Counted { count, entries }
    }
}

fn mark(log: &mut GuildLog, key: &str, until: i64) {
    if let Some(previous) = log.handled.insert(key.to_owned(), until) {
        log.expiring.remove(&(previous, key.to_owned()));
    }
    log.expiring.insert((until, key.to_owned()));
    while log.handled.len() > MAX_HANDLED_PER_GUILD {
        let Some((_, oldest)) = log.expiring.pop_first() else {
            break;
        };
        log.handled.remove(&oldest);
    }
}

fn query<'a>(
    kind: &'a str,
    subject: Option<u64>,
    within: Duration,
    now: i64,
    filter: &'a RecentFilter,
) -> Query<'a> {
    let within = i64::try_from(within.as_secs()).unwrap_or(i64::MAX);
    Query {
        kind,
        subject,
        group: filter.group.as_deref(),
        cutoff: now.saturating_sub(within),
    }
}

#[cfg(test)]
mod tests {
    use twilight_model::id::Id;

    use super::*;

    const WITHIN: Duration = Duration::from_secs(10);

    fn guild(id: u64) -> GuildId {
        Id::new(id)
    }

    fn action(kind: &str, subject: u64, group: Option<&str>, at: i64) -> Recent {
        Recent {
            kind: kind.to_owned(),
            subject,
            group: group.map(str::to_owned),
            count: 1,
            at,
            messages: Vec::new(),
            tag: None,
        }
    }

    fn recent_actions() -> RecentActions {
        RecentActions::new(RecentLimits::default())
    }

    fn any() -> RecentFilter {
        RecentFilter::default()
    }

    #[test]
    fn a_window_includes_its_start_and_nothing_older() {
        let actions = recent_actions();
        for at in [99, 100, 105] {
            actions.record(guild(1), action("msg", 1, None, at));
        }
        assert_eq!(actions.sum(guild(1), "msg", None, WITHIN, 110, &any()), 2);
    }

    #[test]
    fn sums_are_per_kind_and_subject_and_count_each_entry_by_its_count() {
        let actions = recent_actions();
        actions.record(guild(1), action("msg", 1, None, 100));
        actions.record(guild(1), action("msg", 2, None, 100));
        actions.record(guild(1), action("join", 1, None, 100));
        let mut burst = action("msg", 1, None, 101);
        burst.count = 3;
        actions.record(guild(1), burst);
        assert_eq!(
            actions.sum(guild(1), "msg", Some(1), WITHIN, 105, &any()),
            4
        );
        assert_eq!(actions.sum(guild(1), "msg", None, WITHIN, 105, &any()), 5);
        assert_eq!(
            actions.sum(guild(1), "join", Some(1), WITHIN, 105, &any()),
            1
        );
        assert_eq!(actions.sum(guild(2), "msg", None, WITHIN, 105, &any()), 0);
    }

    #[test]
    fn group_filter_and_distinct_gates_decide_what_counts() {
        let actions = recent_actions();
        actions.record(guild(1), action("msg", 1, Some("a"), 100));
        actions.record(guild(1), action("msg", 2, Some("a"), 100));
        actions.record(guild(1), action("msg", 3, Some("b"), 100));
        let at_least = |groups, subjects| RecentFilter {
            group: None,
            distinct_groups_at_least: groups,
            distinct_subjects_at_least: subjects,
        };
        let in_a = RecentFilter {
            group: Some("a".to_owned()),
            ..RecentFilter::default()
        };
        assert_eq!(actions.sum(guild(1), "msg", None, WITHIN, 101, &in_a), 2);
        assert_eq!(
            actions.sum(guild(1), "msg", None, WITHIN, 101, &at_least(Some(2), None)),
            3
        );
        assert_eq!(
            actions.sum(guild(1), "msg", None, WITHIN, 101, &at_least(Some(3), None)),
            0
        );
        assert_eq!(
            actions.sum(guild(1), "msg", None, WITHIN, 101, &at_least(None, Some(3))),
            3
        );
        assert_eq!(
            actions.sum(guild(1), "msg", None, WITHIN, 101, &at_least(None, Some(4))),
            0
        );
    }

    #[test]
    fn entries_are_oldest_first_and_take_removes_only_what_it_returns() {
        let actions = recent_actions();
        actions.record(guild(1), action("msg", 1, Some("a"), 1));
        actions.record(guild(1), action("msg", 2, Some("b"), 2));
        actions.record(guild(1), action("msg", 3, Some("a"), 3));
        let times = |entries: &[Recent]| entries.iter().map(|e| e.at).collect::<Vec<_>>();
        assert_eq!(
            times(&actions.entries(guild(1), "msg", None, WITHIN, 5, &any())),
            vec![1, 2, 3]
        );
        let in_a = RecentFilter {
            group: Some("a".to_owned()),
            ..RecentFilter::default()
        };
        assert_eq!(
            times(&actions.take(guild(1), "msg", None, WITHIN, 5, &in_a)),
            vec![1, 3]
        );
        assert_eq!(actions.sum(guild(1), "msg", None, WITHIN, 5, &any()), 1);
    }

    #[test]
    fn take_does_nothing_when_the_gate_is_not_passed() {
        let actions = recent_actions();
        actions.record(guild(1), action("msg", 1, Some("a"), 1));
        let needs_three = RecentFilter {
            distinct_groups_at_least: Some(3),
            ..RecentFilter::default()
        };
        assert!(
            actions
                .take(guild(1), "msg", None, WITHIN, 5, &needs_three)
                .is_empty()
        );
        assert_eq!(actions.sum(guild(1), "msg", None, WITHIN, 5, &any()), 1);
    }

    #[test]
    fn handled_keys_stop_being_handled_at_their_expiry() {
        let actions = recent_actions();
        actions.mark_handled(guild(1), "burst", 110);
        assert!(actions.already_handled(guild(1), "burst", 109));
        assert!(!actions.already_handled(guild(1), "burst", 110));
        assert!(!actions.already_handled(guild(2), "burst", 105));
    }

    #[test]
    fn the_oldest_entries_go_first_past_the_entry_limit() {
        let actions = RecentActions::new(RecentLimits {
            max_entries_per_guild: 3,
            max_age: Duration::from_secs(3600),
        });
        for at in 1..=5 {
            actions.record(guild(1), action("msg", 1, None, at));
        }
        let times: Vec<i64> = actions
            .entries(guild(1), "msg", None, Duration::from_secs(100), 5, &any())
            .iter()
            .map(|e| e.at)
            .collect();
        assert_eq!(times, vec![3, 4, 5]);
    }

    #[test]
    fn entries_past_the_maximum_age_go_when_a_newer_one_is_recorded() {
        let actions = RecentActions::new(RecentLimits {
            max_entries_per_guild: 100,
            max_age: Duration::from_secs(10),
        });
        actions.record(guild(1), action("msg", 1, None, 0));
        actions.record(guild(1), action("msg", 1, None, 100));
        assert_eq!(
            actions.sum(
                guild(1),
                "msg",
                None,
                Duration::from_secs(1000),
                100,
                &any()
            ),
            1
        );
    }

    #[test]
    fn guilds_do_not_see_each_other() {
        let actions = recent_actions();
        actions.record(guild(1), action("msg", 1, None, 100));
        actions.record(guild(2), action("msg", 1, None, 100));
        actions.record(guild(2), action("msg", 1, None, 100));
        assert_eq!(actions.sum(guild(1), "msg", None, WITHIN, 101, &any()), 1);
        assert_eq!(actions.sum(guild(2), "msg", None, WITHIN, 101, &any()), 2);
    }

    #[test]
    fn a_hundred_thousand_records_stay_within_the_entry_limit() {
        // The age limit is wide, so the entry limit is what bounds the store here.
        let limits = RecentLimits {
            max_entries_per_guild: 20_000,
            max_age: Duration::from_secs(1_000_000),
        };
        let actions = RecentActions::new(limits);
        for at in 0..100_000 {
            actions.record(guild(1), action("msg", 1, None, at));
        }
        assert_eq!(
            actions.sum(
                guild(1),
                "msg",
                None,
                Duration::from_secs(1_000_000),
                100_000,
                &any()
            ),
            20_000
        );
    }

    #[test]
    fn handled_keys_are_capped_and_the_soonest_to_expire_go_first() {
        let actions = recent_actions();
        for n in 0..=MAX_HANDLED_PER_GUILD {
            actions.mark_handled(
                guild(1),
                &format!("key{n}"),
                1_000 + i64::try_from(n).unwrap(),
            );
        }
        assert!(!actions.already_handled(guild(1), "key0", 0));
        assert!(actions.already_handled(guild(1), "key1", 0));
    }

    fn burst_of<'a>(filter: &'a RecentFilter, handled_key: &'a str) -> Burst<'a> {
        Burst {
            kind: "msg",
            subject: Some(1),
            within: WITHIN,
            filter,
            handled_key,
            handled_until: 200,
        }
    }

    /// Counts the entries of a window when they add up to at least three.
    fn three_or_more(entries: &[&Recent]) -> Option<u64> {
        let total: u64 = entries.iter().map(|entry| u64::from(entry.count)).sum();
        (total >= 3).then_some(total)
    }

    #[test]
    fn a_claim_returns_what_it_took_and_keeps_the_rest() {
        let actions = recent_actions();
        actions.record(guild(1), action("msg", 1, Some("a"), 100));
        actions.record(guild(1), action("msg", 1, Some("b"), 100));
        actions.record(guild(1), action("msg", 1, Some("a"), 101));
        let in_a = RecentFilter {
            group: Some("a".to_owned()),
            ..RecentFilter::default()
        };
        let claim = actions.claim_burst(guild(1), 102, &burst_of(&in_a, "k"), |entries| {
            u64::try_from(entries.len()).ok()
        });
        assert_eq!(
            claim,
            Claim::Counted {
                count: 2,
                entries: vec![
                    action("msg", 1, Some("a"), 100),
                    action("msg", 1, Some("a"), 101)
                ],
            }
        );
        assert_eq!(actions.sum(guild(1), "msg", None, WITHIN, 102, &any()), 1);
        assert!(actions.already_handled(guild(1), "k", 102));
    }

    #[test]
    fn a_claim_that_is_too_small_takes_and_marks_nothing() {
        let actions = recent_actions();
        actions.record(guild(1), action("msg", 1, None, 100));
        let filter = any();
        let claim = actions.claim_burst(guild(1), 101, &burst_of(&filter, "k"), three_or_more);
        assert_eq!(claim, Claim::Short);
        assert_eq!(actions.sum(guild(1), "msg", None, WITHIN, 101, &any()), 1);
        assert!(!actions.already_handled(guild(1), "k", 101));
    }

    #[test]
    fn inspecting_a_burst_changes_nothing() {
        let actions = recent_actions();
        for at in 100..103 {
            actions.record(guild(1), action("msg", 1, None, at));
        }
        let filter = any();
        let burst = burst_of(&filter, "k");
        let inspected = actions.inspect_burst(guild(1), 103, &burst, three_or_more);
        assert!(matches!(inspected, Claim::Counted { count: 3, .. }));
        assert_eq!(actions.sum(guild(1), "msg", None, WITHIN, 103, &any()), 3);
        assert!(!actions.already_handled(guild(1), "k", 103));
    }

    #[test]
    fn exactly_one_of_many_threads_claims_a_burst() {
        let actions = recent_actions();
        for at in 100..103 {
            actions.record(guild(1), action("msg", 1, None, at));
        }
        let filter = any();
        let burst = burst_of(&filter, "k");
        let claims: Vec<Claim> = std::thread::scope(|scope| {
            let handles: Vec<_> = (0..8)
                .map(|_| scope.spawn(|| actions.claim_burst(guild(1), 103, &burst, three_or_more)))
                .collect();
            handles
                .into_iter()
                .map(|handle| handle.join().unwrap())
                .collect()
        });
        let counted = claims
            .iter()
            .filter(|claim| matches!(claim, Claim::Counted { .. }))
            .count();
        let handled = claims
            .iter()
            .filter(|claim| **claim == Claim::Handled)
            .count();
        assert_eq!((counted, handled), (1, 7));
    }
}
