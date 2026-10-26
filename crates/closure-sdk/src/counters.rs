//! Counters: named integers kept per guild, and per channel or per user as the definition says,
//! that decay over time and fire triggers when they cross a condition. The module's config gives
//! the definitions; this keeps the values (Zeppelin's Counters plugin).
//!
//! Every change is one compare-and-set write of the scope's entry, so two handlers changing the
//! same counter cannot lose an update. A conflict is retried a few times, then reported as
//! [`CounterError::Conflict`]. Decay is lazy: a value is brought up to date from the time of its
//! last write whenever it is read or changed, so no job runs in the background.
//!
//! Triggers look at each change as a step from the value as last written (before decay) to the new
//! one. A trigger fires when the step takes the value from not matching its condition to matching
//! it. Its reverse fires when the step takes the value from matching the condition to matching the
//! reverse.
//! So a value that jumps over the reverse's range fires nothing until a step crosses it.
//!
//! Shortcut: a trigger that only decay crosses fires on the next change of that scope, not at
//! decay time, and a value nobody has touched stays at its initial value; a sweep job over the
//! scopes would make both exact.

use std::collections::HashSet;
use std::time::Duration;

use closure_core::event::{ChannelId, UserId};
use closure_core::storage::{Expect, StorageError, Store, check_key};
use serde::{Deserialize, Serialize};
use thiserror::Error;

/// The largest value a counter holds, as in Zeppelin. Values are clamped to `0..=MAX`.
pub const MAX_COUNTER_VALUE: i64 = 2_147_483_647;
/// The longest counter name. The store key `counter:<name>:<channel>:<user>` must fit its limit.
const MAX_NAME_CHARS: usize = 64;
/// Compare-and-set attempts before a change is reported as a conflict.
const MAX_ATTEMPTS: usize = 5;
/// Entries read per page by [`Counters::reset_all`].
const PAGE: usize = 1_000;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Op {
    Eq,
    Ne,
    Gt,
    Ge,
    Lt,
    Le,
}

/// A comparison of a counter's value with a number, such as `>= 5`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Condition {
    op: Op,
    value: i64,
}

#[derive(Debug, Clone, PartialEq, Eq, Error)]
#[error("`{0}` is not a condition such as `>= 5`")]
pub struct ConditionError(String);

impl Condition {
    /// Reads `>= 5`, `> 10`, `= 0`, `!= 3`, `< 2` or `<= 7`. Spaces around the operator are
    /// optional; the value is `0` to `2^31 - 1`.
    pub fn parse(text: &str) -> Result<Self, ConditionError> {
        let trimmed = text.trim();
        let (op, rest) = [
            (">=", Op::Ge),
            ("<=", Op::Le),
            ("!=", Op::Ne),
            (">", Op::Gt),
            ("<", Op::Lt),
            ("=", Op::Eq),
        ]
        .into_iter()
        .find_map(|(symbol, op)| trimmed.strip_prefix(symbol).map(|rest| (op, rest)))
        .ok_or_else(|| ConditionError(text.to_owned()))?;
        let digits = rest.trim_start();
        let value = digits
            .bytes()
            .all(|b| b.is_ascii_digit())
            .then(|| digits.parse::<i64>().ok())
            .flatten()
            .filter(|value| *value <= MAX_COUNTER_VALUE)
            .ok_or_else(|| ConditionError(text.to_owned()))?;
        Ok(Self { op, value })
    }

    /// Whether `value` satisfies the comparison.
    #[must_use]
    pub fn matches(self, value: i64) -> bool {
        match self.op {
            Op::Eq => value == self.value,
            Op::Ne => value != self.value,
            Op::Gt => value > self.value,
            Op::Ge => value >= self.value,
            Op::Lt => value < self.value,
            Op::Le => value <= self.value,
        }
    }

    /// The opposite comparison on the same number: `>= 5` gives `< 5`.
    #[must_use]
    pub fn reverse(self) -> Self {
        let op = match self.op {
            Op::Eq => Op::Ne,
            Op::Ne => Op::Eq,
            Op::Gt => Op::Le,
            Op::Ge => Op::Lt,
            Op::Lt => Op::Ge,
            Op::Le => Op::Gt,
        };
        Self {
            op,
            value: self.value,
        }
    }
}

/// Changes the counter by `amount` every `every`, as Zeppelin's `decay`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Decay {
    pub amount: i64,
    pub every: Duration,
}

/// A named condition that fires when it newly holds, and its reverse, which fires when the
/// condition has held and the reverse holds, so that a burst fires once.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TriggerDef {
    pub name: String,
    pub condition: Condition,
    pub reverse: Condition,
}

/// One counter of the module's config.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CounterDef {
    pub name: String,
    pub per_channel: bool,
    pub per_user: bool,
    pub initial_value: i64,
    pub decay: Option<Decay>,
    pub triggers: Vec<TriggerDef>,
}

/// A trigger that fired during a change, or its reverse.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Fired {
    pub trigger: String,
    pub reverse: bool,
}

/// The result of a change: the value before and after it, and the triggers it fired.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Change {
    pub old: i64,
    pub new: i64,
    pub fired: Vec<Fired>,
}

#[derive(Debug, Clone, PartialEq, Eq, Error)]
pub enum CounterError {
    #[error("there is no counter named `{0}`")]
    Unknown(String),
    #[error("the counter is per channel, so a channel is needed")]
    MissingChannel,
    #[error("the counter is per user, so a user is needed")]
    MissingUser,
    #[error("the counter definition is invalid: {0}")]
    Invalid(String),
    #[error("the counter kept changing; try again")]
    Conflict,
    #[error(transparent)]
    Storage(#[from] StorageError),
}

/// What is stored for one scope: the value and the decay time it has been brought up to.
#[derive(Debug, Clone, Serialize, Deserialize)]
struct Stored {
    value: i64,
    at: i64,
}

enum Write {
    Add(i64),
    Set(i64),
}

/// The counters of one module in one guild.
#[derive(Debug, Clone)]
pub struct Counters {
    store: Store,
    defs: Vec<CounterDef>,
}

impl Counters {
    /// Checks the definitions and binds them to the module's `store` in one guild.
    pub fn new(store: &Store, defs: &[CounterDef]) -> Result<Self, CounterError> {
        validate(defs)?;
        Ok(Self {
            store: store.clone(),
            defs: defs.to_vec(),
        })
    }

    /// The value of a counter at `now` (Unix seconds), after the decay that has passed.
    pub async fn get(
        &self,
        name: &str,
        channel: Option<ChannelId>,
        user: Option<UserId>,
        now: i64,
    ) -> Result<i64, CounterError> {
        let (def, key) = self.locate(name, channel, user)?;
        Ok(match self.store.get::<Stored>(&key).await? {
            Some(stored) => catch_up(def.decay, stored.value, stored.at, now).0,
            None => def.initial_value,
        })
    }

    /// Adds `delta` (clamped to the counter's range) and reports the triggers that fired.
    pub async fn add(
        &self,
        name: &str,
        channel: Option<ChannelId>,
        user: Option<UserId>,
        delta: i64,
        now: i64,
    ) -> Result<Change, CounterError> {
        let delta = delta.clamp(-MAX_COUNTER_VALUE, MAX_COUNTER_VALUE);
        self.write(name, channel, user, &Write::Add(delta), now)
            .await
    }

    /// Sets the value (clamped to the counter's range) and reports the triggers that fired.
    pub async fn set(
        &self,
        name: &str,
        channel: Option<ChannelId>,
        user: Option<UserId>,
        value: i64,
        now: i64,
    ) -> Result<Change, CounterError> {
        self.write(name, channel, user, &Write::Set(value), now)
            .await
    }

    /// Puts one scope back to its initial value. No trigger fires.
    pub async fn reset(
        &self,
        name: &str,
        channel: Option<ChannelId>,
        user: Option<UserId>,
    ) -> Result<(), CounterError> {
        let (_, key) = self.locate(name, channel, user)?;
        self.store.delete(&key).await?;
        Ok(())
    }

    /// Puts every scope of a counter back to its initial value. No trigger fires.
    pub async fn reset_all(&self, name: &str) -> Result<(), CounterError> {
        self.def(name)?;
        // Names contain no colon, so this prefix belongs to this counter alone.
        let prefix = format!("counter:{name}:");
        loop {
            let page = self.store.list(&prefix, PAGE).await?;
            if page.is_empty() {
                return Ok(());
            }
            for (key, _) in page {
                self.store.delete(&key).await?;
            }
        }
    }

    async fn write(
        &self,
        name: &str,
        channel: Option<ChannelId>,
        user: Option<UserId>,
        write: &Write,
        now: i64,
    ) -> Result<Change, CounterError> {
        let (def, key) = self.locate(name, channel, user)?;
        for _ in 0..MAX_ATTEMPTS {
            let (current, expect) = match self.store.entry(&key).await? {
                Some(entry) => {
                    let stored = serde_json::from_value::<Stored>(entry.value)
                        .map_err(|error| StorageError::Value(error.to_string()))?;
                    (Some(stored), Expect::Revision(entry.revision))
                }
                None => (None, Expect::Missing),
            };
            // Triggers judge the stored value, so a crossing that only decay made is part of this step.
            let (stored_value, old, at) = match current {
                Some(stored) => {
                    let (old, at) = catch_up(def.decay, stored.value, stored.at, now);
                    (stored.value, old, at)
                }
                None => (def.initial_value, def.initial_value, now),
            };
            let new = match write {
                Write::Add(delta) => old.saturating_add(*delta).clamp(0, MAX_COUNTER_VALUE),
                Write::Set(value) => (*value).clamp(0, MAX_COUNTER_VALUE),
            };
            let fired_now = advance_triggers(&def.triggers, stored_value, new);
            let stored = Stored { value: new, at };
            match self.store.replace(&key, &stored, expect, None).await {
                Ok(_) => {
                    return Ok(Change {
                        old,
                        new,
                        fired: fired_now,
                    });
                }
                Err(StorageError::Conflict) => {}
                Err(error) => return Err(error.into()),
            }
        }
        Err(CounterError::Conflict)
    }

    fn def(&self, name: &str) -> Result<&CounterDef, CounterError> {
        self.defs
            .iter()
            .find(|def| def.name == name)
            .ok_or_else(|| CounterError::Unknown(name.to_owned()))
    }

    /// The definition and the store key of one scope. Scopes the definition does not use are
    /// left out of the key, as Zeppelin does.
    fn locate(
        &self,
        name: &str,
        channel: Option<ChannelId>,
        user: Option<UserId>,
    ) -> Result<(&CounterDef, String), CounterError> {
        let def = self.def(name)?;
        if def.per_channel && channel.is_none() {
            return Err(CounterError::MissingChannel);
        }
        if def.per_user && user.is_none() {
            return Err(CounterError::MissingUser);
        }
        let channel = channel
            .filter(|_| def.per_channel)
            .map_or("_".to_owned(), |c| c.get().to_string());
        let user = user
            .filter(|_| def.per_user)
            .map_or("_".to_owned(), |u| u.get().to_string());
        Ok((def, format!("counter:{name}:{channel}:{user}")))
    }
}

/// The triggers a change from `stored`, the value as last written, to `new` fires: a trigger when the
/// condition goes from not holding to holding, its reverse when it goes from holding to the reverse.
fn advance_triggers(triggers: &[TriggerDef], stored: i64, new: i64) -> Vec<Fired> {
    let mut events = Vec::new();
    for trigger in triggers {
        let was = trigger.condition.matches(stored);
        if !was && trigger.condition.matches(new) {
            events.push(Fired {
                trigger: trigger.name.clone(),
                reverse: false,
            });
        } else if was && trigger.reverse.matches(new) {
            events.push(Fired {
                trigger: trigger.name.clone(),
                reverse: true,
            });
        }
    }
    events
}

/// The value and decay time after the whole decay periods since `at`. The part of a period that
/// has not passed yet is kept, so reading often gives the same value as reading rarely.
fn catch_up(decay: Option<Decay>, value: i64, at: i64, now: i64) -> (i64, i64) {
    let Some(decay) = decay else {
        return (value, at);
    };
    // Validated to be at least one second.
    let period = i64::try_from(decay.every.as_secs()).unwrap_or(i64::MAX);
    let periods = now.saturating_sub(at).max(0) / period;
    if periods == 0 {
        return (value, at);
    }
    let value = value
        .saturating_sub(periods.saturating_mul(decay.amount))
        .clamp(0, MAX_COUNTER_VALUE);
    (value, at.saturating_add(periods.saturating_mul(period)))
}

fn validate(defs: &[CounterDef]) -> Result<(), CounterError> {
    let invalid = |reason: String| Err(CounterError::Invalid(reason));
    let mut names = HashSet::new();
    for def in defs {
        if !valid_name(&def.name) {
            return invalid(format!(
                "`{}` is not 1 to {MAX_NAME_CHARS} characters without spaces or colons",
                def.name
            ));
        }
        if !names.insert(def.name.as_str()) {
            return invalid(format!("the counter `{}` is defined twice", def.name));
        }
        if !(0..=MAX_COUNTER_VALUE).contains(&def.initial_value) {
            return invalid(format!(
                "the initial value of `{}` is out of range",
                def.name
            ));
        }
        if let Some(decay) = def.decay {
            if decay.every.as_secs() == 0 {
                return invalid(format!(
                    "the decay of `{}` is shorter than a second",
                    def.name
                ));
            }
            if !(-MAX_COUNTER_VALUE..=MAX_COUNTER_VALUE).contains(&decay.amount) {
                return invalid(format!(
                    "the decay amount of `{}` is out of range",
                    def.name
                ));
            }
        }
        let mut trigger_names = HashSet::new();
        for trigger in &def.triggers {
            if trigger.name.is_empty() || !trigger_names.insert(trigger.name.as_str()) {
                return invalid(format!("the triggers of `{}` need unique names", def.name));
            }
        }
    }
    Ok(())
}

fn valid_name(name: &str) -> bool {
    !name.is_empty()
        && name.len() <= MAX_NAME_CHARS
        && !name.contains(':')
        && check_key(name).is_ok()
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;

    use async_trait::async_trait;
    use closure_core::capability::{Capability, CapabilitySet};
    use closure_core::event::GuildId;
    use closure_core::module::ModuleId;
    use closure_core::storage::{Entry, MemoryStorage, StorageBackend};
    use twilight_model::id::Id;

    use super::*;

    const NOW: i64 = 1_700_000_000;

    /// Yields inside every read, so concurrent changes interleave and must pass the revision check.
    struct Interleaved(MemoryStorage);

    #[async_trait]
    impl StorageBackend for Interleaved {
        async fn get(
            &self,
            module: &ModuleId,
            guild: GuildId,
            key: &str,
        ) -> Result<Option<Entry>, StorageError> {
            let entry = StorageBackend::get(&self.0, module, guild, key).await;
            tokio::task::yield_now().await;
            entry
        }

        async fn put(
            &self,
            module: &ModuleId,
            guild: GuildId,
            key: &str,
            value: serde_json::Value,
            ttl: Option<Duration>,
            expect: Expect,
        ) -> Result<i64, StorageError> {
            StorageBackend::put(&self.0, module, guild, key, value, ttl, expect).await
        }

        async fn delete(
            &self,
            module: &ModuleId,
            guild: GuildId,
            key: &str,
        ) -> Result<bool, StorageError> {
            StorageBackend::delete(&self.0, module, guild, key).await
        }

        async fn list(
            &self,
            module: &ModuleId,
            guild: GuildId,
            prefix: &str,
            limit: usize,
        ) -> Result<Vec<(String, Entry)>, StorageError> {
            StorageBackend::list(&self.0, module, guild, prefix, limit).await
        }

        async fn counter_add(
            &self,
            module: &ModuleId,
            guild: GuildId,
            counter: &str,
            subject: u64,
            delta: i64,
        ) -> Result<i64, StorageError> {
            StorageBackend::counter_add(&self.0, module, guild, counter, subject, delta).await
        }

        async fn counter_get(
            &self,
            module: &ModuleId,
            guild: GuildId,
            counter: &str,
            subject: u64,
            window: Option<Duration>,
        ) -> Result<i64, StorageError> {
            StorageBackend::counter_get(&self.0, module, guild, counter, subject, window).await
        }

        async fn counter_reset(
            &self,
            module: &ModuleId,
            guild: GuildId,
            counter: &str,
            subject: u64,
        ) -> Result<(), StorageError> {
            StorageBackend::counter_reset(&self.0, module, guild, counter, subject).await
        }
    }

    fn store_on(backend: Arc<dyn StorageBackend>) -> Store {
        let granted: CapabilitySet = [Capability::Storage].into_iter().collect();
        Store::new(
            backend,
            &granted,
            ModuleId::new("counters-test").unwrap(),
            Id::new(1),
        )
    }

    fn counters(defs: &[CounterDef]) -> Counters {
        Counters::new(&store_on(Arc::new(MemoryStorage::default())), defs).unwrap()
    }

    fn plain(name: &str) -> CounterDef {
        CounterDef {
            name: name.to_owned(),
            per_channel: false,
            per_user: false,
            initial_value: 0,
            decay: None,
            triggers: Vec::new(),
        }
    }

    // Optional so that call sites read like the command's arguments, which are optional too.
    #[allow(clippy::unnecessary_wraps)]
    fn channel(id: u64) -> Option<ChannelId> {
        Some(Id::new(id))
    }

    #[allow(clippy::unnecessary_wraps)]
    fn user(id: u64) -> Option<UserId> {
        Some(Id::new(id))
    }

    fn fired(trigger: &str, reverse: bool) -> Fired {
        Fired {
            trigger: trigger.to_owned(),
            reverse,
        }
    }

    #[test]
    fn condition_parses_the_six_operators_and_reverses_them() {
        for (text, op_value, reversed) in [
            (">= 5", (Op::Ge, 5), (Op::Lt, 5)),
            ("> 10", (Op::Gt, 10), (Op::Le, 10)),
            ("= 0", (Op::Eq, 0), (Op::Ne, 0)),
            ("!= 3", (Op::Ne, 3), (Op::Eq, 3)),
            ("< 2", (Op::Lt, 2), (Op::Ge, 2)),
            ("<=7", (Op::Le, 7), (Op::Gt, 7)),
        ] {
            let condition = Condition::parse(text).unwrap();
            assert_eq!((condition.op, condition.value), op_value, "{text:?}");
            assert_eq!(
                (condition.reverse().op, condition.reverse().value),
                reversed,
                "{text:?}"
            );
        }
        assert!(Condition::parse(">= 5").unwrap().matches(5));
        assert!(!Condition::parse("> 5").unwrap().matches(5));
    }

    #[test]
    fn condition_rejects_what_is_not_a_condition() {
        for text in [
            "",
            "5",
            "== 5",
            ">= -1",
            ">= 5x",
            ">= 2147483648",
            "+5",
            "=",
            "10 m",
        ] {
            assert!(Condition::parse(text).is_err(), "{text:?}");
        }
    }

    #[test]
    fn invalid_definitions_are_refused() {
        let store = store_on(Arc::new(MemoryStorage::default()));
        let mut bad_initial = plain("a");
        bad_initial.initial_value = -1;
        let mut zero_decay = plain("a");
        zero_decay.decay = Some(Decay {
            amount: 1,
            every: Duration::from_millis(500),
        });
        for defs in [
            vec![plain("")],
            vec![plain("a:b")],
            vec![plain("a b")],
            vec![plain("a"), plain("a")],
            vec![bad_initial],
            vec![zero_decay],
        ] {
            assert!(matches!(
                Counters::new(&store, &defs),
                Err(CounterError::Invalid(_))
            ));
        }
    }

    #[tokio::test]
    async fn scopes_keep_global_channel_user_and_both_apart() {
        let mut per_channel = plain("c");
        per_channel.per_channel = true;
        let mut per_user = plain("u");
        per_user.per_user = true;
        let mut both = plain("cu");
        both.per_channel = true;
        both.per_user = true;
        let counters = counters(&[plain("g"), per_channel, per_user, both]);

        counters
            .add("g", channel(1), user(9), 1, NOW)
            .await
            .unwrap();
        counters.add("g", channel(2), None, 2, NOW).await.unwrap();
        assert_eq!(counters.get("g", None, None, NOW).await.unwrap(), 3);

        counters.add("c", channel(1), None, 1, NOW).await.unwrap();
        counters.add("c", channel(2), None, 5, NOW).await.unwrap();
        assert_eq!(counters.get("c", channel(1), None, NOW).await.unwrap(), 1);
        assert_eq!(counters.get("c", channel(2), None, NOW).await.unwrap(), 5);

        counters.add("u", None, user(7), 4, NOW).await.unwrap();
        assert_eq!(counters.get("u", None, user(7), NOW).await.unwrap(), 4);
        assert_eq!(counters.get("u", None, user(8), NOW).await.unwrap(), 0);

        counters
            .add("cu", channel(1), user(7), 2, NOW)
            .await
            .unwrap();
        assert_eq!(
            counters.get("cu", channel(1), user(7), NOW).await.unwrap(),
            2
        );
        assert_eq!(
            counters.get("cu", channel(1), user(8), NOW).await.unwrap(),
            0
        );
        assert_eq!(
            counters.get("cu", channel(2), user(7), NOW).await.unwrap(),
            0
        );
    }

    #[tokio::test]
    async fn a_scope_the_counter_needs_is_an_error() {
        let mut per_channel = plain("c");
        per_channel.per_channel = true;
        let counters = counters(&[per_channel, plain("g")]);
        assert_eq!(
            counters.add("c", None, None, 1, NOW).await,
            Err(CounterError::MissingChannel)
        );
        assert_eq!(counters.get("g", channel(3), None, NOW).await.unwrap(), 0);
        assert_eq!(
            counters.get("missing", None, None, NOW).await,
            Err(CounterError::Unknown("missing".to_owned()))
        );
    }

    #[tokio::test]
    async fn values_are_clamped_to_zero_and_the_maximum() {
        let counters = counters(&[plain("g")]);
        assert_eq!(counters.add("g", None, None, -5, NOW).await.unwrap().new, 0);
        assert_eq!(
            counters
                .add("g", None, None, i64::MAX, NOW)
                .await
                .unwrap()
                .new,
            MAX_COUNTER_VALUE
        );
        assert_eq!(
            counters.add("g", None, None, 1, NOW).await.unwrap().new,
            MAX_COUNTER_VALUE
        );
        assert_eq!(counters.set("g", None, None, -9, NOW).await.unwrap().new, 0);
    }

    #[tokio::test]
    async fn initial_value_is_where_an_untouched_counter_starts() {
        let mut def = plain("g");
        def.initial_value = 7;
        let counters = counters(&[def]);
        assert_eq!(counters.get("g", None, None, NOW).await.unwrap(), 7);
        let change = counters.add("g", None, None, 1, NOW).await.unwrap();
        assert_eq!((change.old, change.new), (7, 8));
    }

    #[tokio::test]
    async fn decay_moves_the_value_by_whole_periods_and_keeps_the_rest() {
        let mut def = plain("g");
        def.decay = Some(Decay {
            amount: -2,
            every: Duration::from_secs(60),
        });
        let counters = counters(&[def]);
        counters.set("g", None, None, 10, NOW).await.unwrap();
        // Negative decay grows the value by 2 per minute, up to the maximum.
        assert_eq!(counters.get("g", None, None, NOW + 59).await.unwrap(), 10);
        assert_eq!(counters.get("g", None, None, NOW + 179).await.unwrap(), 14);
        def = plain("d");
        def.decay = Some(Decay {
            amount: 2,
            every: Duration::from_secs(60),
        });
        let counters = self::counters(&[def]);
        counters.set("d", None, None, 10, NOW).await.unwrap();
        assert_eq!(counters.get("d", None, None, NOW + 59).await.unwrap(), 10);
        assert_eq!(counters.get("d", None, None, NOW + 179).await.unwrap(), 6);
        assert_eq!(counters.get("d", None, None, NOW + 600).await.unwrap(), 0);
        // Reading at 130 s applies two periods and keeps the 10 s left over, so 180 s is three.
        let change = counters.add("d", None, None, 0, NOW + 130).await.unwrap();
        assert_eq!(change.old, 6);
        assert_eq!(counters.get("d", None, None, NOW + 179).await.unwrap(), 6);
        assert_eq!(counters.get("d", None, None, NOW + 180).await.unwrap(), 4);
    }

    #[tokio::test]
    async fn a_trigger_fires_once_per_crossing_and_its_reverse_once_back() {
        let mut def = plain("g");
        def.triggers = vec![TriggerDef {
            name: "high".to_owned(),
            condition: Condition::parse(">= 5").unwrap(),
            reverse: Condition::parse("< 5").unwrap(),
        }];
        let counters = counters(&[def]);
        assert!(
            counters
                .add("g", None, None, 4, NOW)
                .await
                .unwrap()
                .fired
                .is_empty()
        );
        assert_eq!(
            counters.add("g", None, None, 1, NOW).await.unwrap().fired,
            vec![fired("high", false)]
        );
        assert!(
            counters
                .add("g", None, None, 1, NOW)
                .await
                .unwrap()
                .fired
                .is_empty()
        );
        assert_eq!(
            counters.add("g", None, None, -3, NOW).await.unwrap().fired,
            vec![fired("high", true)]
        );
        assert!(
            counters
                .add("g", None, None, 0, NOW)
                .await
                .unwrap()
                .fired
                .is_empty()
        );
        assert_eq!(
            counters.add("g", None, None, 2, NOW).await.unwrap().fired,
            vec![fired("high", false)]
        );
    }

    #[tokio::test]
    async fn a_reverse_fires_only_for_a_step_from_the_condition_to_the_reverse() {
        let mut def = plain("g");
        def.triggers = vec![TriggerDef {
            name: "big".to_owned(),
            condition: Condition::parse(">= 10").unwrap(),
            reverse: Condition::parse("<= 5").unwrap(),
        }];
        let counters = counters(&[def]);
        assert_eq!(
            counters.set("g", None, None, 12, NOW).await.unwrap().fired,
            vec![fired("big", false)]
        );
        // 7 is neither the condition nor the reverse: nothing fires.
        assert!(
            counters
                .set("g", None, None, 7, NOW)
                .await
                .unwrap()
                .fired
                .is_empty()
        );
        // 7 to 3 does not start from the condition, so the reverse does not fire.
        assert!(
            counters
                .set("g", None, None, 3, NOW)
                .await
                .unwrap()
                .fired
                .is_empty()
        );
        assert_eq!(
            counters.set("g", None, None, 15, NOW).await.unwrap().fired,
            vec![fired("big", false)]
        );
        assert_eq!(
            counters.set("g", None, None, 4, NOW).await.unwrap().fired,
            vec![fired("big", true)]
        );
    }

    #[tokio::test]
    async fn set_reports_the_old_and_new_value_and_reset_fires_nothing() {
        let mut def = plain("g");
        def.triggers = vec![TriggerDef {
            name: "any".to_owned(),
            condition: Condition::parse(">= 1").unwrap(),
            reverse: Condition::parse("= 0").unwrap(),
        }];
        let counters = counters(&[def]);
        let change = counters.set("g", None, None, 3, NOW).await.unwrap();
        assert_eq!((change.old, change.new), (0, 3));
        assert_eq!(change.fired, vec![fired("any", false)]);
        counters.reset("g", None, None).await.unwrap();
        assert_eq!(counters.get("g", None, None, NOW).await.unwrap(), 0);
        // The reset is no event, but the value rises from 0 again, so the trigger fires again.
        let change = counters.add("g", None, None, 1, NOW).await.unwrap();
        assert_eq!(change.fired, vec![fired("any", false)]);
    }

    #[tokio::test]
    async fn reset_all_clears_one_counter_and_leaves_similar_names() {
        let mut per_channel = plain("a");
        per_channel.per_channel = true;
        let counters = counters(&[per_channel, plain("ab")]);
        counters.add("a", channel(1), None, 3, NOW).await.unwrap();
        counters.add("a", channel(2), None, 4, NOW).await.unwrap();
        counters.add("ab", None, None, 5, NOW).await.unwrap();
        counters.reset_all("a").await.unwrap();
        assert_eq!(counters.get("a", channel(1), None, NOW).await.unwrap(), 0);
        assert_eq!(counters.get("a", channel(2), None, NOW).await.unwrap(), 0);
        assert_eq!(counters.get("ab", None, None, NOW).await.unwrap(), 5);
    }

    #[tokio::test]
    async fn long_names_fit_the_store_key_with_both_scopes() {
        let mut def = plain(&"n".repeat(MAX_NAME_CHARS));
        def.per_channel = true;
        def.per_user = true;
        let name = def.name.clone();
        let counters = counters(&[def]);
        counters
            .add(&name, channel(u64::MAX), user(u64::MAX), 1, NOW)
            .await
            .unwrap();
        assert_eq!(
            counters
                .get(&name, channel(u64::MAX), user(u64::MAX), NOW)
                .await
                .unwrap(),
            1
        );
    }

    #[tokio::test]
    async fn concurrent_changes_are_not_lost() {
        let backend = Arc::new(Interleaved(MemoryStorage::default()));
        let counters = Counters::new(&store_on(backend), &[plain("g")]).unwrap();
        let results =
            futures_util::future::join_all((0..4).map(|_| counters.add("g", None, None, 1, NOW)))
                .await;
        assert!(results.iter().all(Result::is_ok), "{results:?}");
        assert_eq!(counters.get("g", None, None, NOW).await.unwrap(), 4);
    }

    /// A counter that moves by `amount` every second, with a `>= 5` trigger and its `< 5` reverse.
    fn decaying_high(amount: i64) -> CounterDef {
        let mut def = plain("g");
        def.decay = Some(Decay {
            amount,
            every: Duration::from_secs(1),
        });
        def.triggers = vec![TriggerDef {
            name: "high".to_owned(),
            condition: Condition::parse(">= 5").unwrap(),
            reverse: Condition::parse("< 5").unwrap(),
        }];
        def
    }

    #[tokio::test]
    async fn a_reverse_crossing_made_by_decay_fires_at_the_next_change() {
        let counters = counters(&[decaying_high(1)]);
        counters.set("g", None, None, 5, NOW).await.unwrap();
        // Decayed to 4 a second later, the value has left the condition: the reverse fires now.
        let change = counters.add("g", None, None, 0, NOW + 1).await.unwrap();
        assert_eq!((change.old, change.new), (4, 4));
        assert_eq!(change.fired, vec![fired("high", true)]);
        assert!(
            counters
                .add("g", None, None, 0, NOW + 1)
                .await
                .unwrap()
                .fired
                .is_empty()
        );
    }

    #[tokio::test]
    async fn a_crossing_made_by_growth_fires_at_the_next_change() {
        let counters = counters(&[decaying_high(-1)]);
        counters.set("g", None, None, 4, NOW).await.unwrap();
        let change = counters.add("g", None, None, 0, NOW + 1).await.unwrap();
        assert_eq!(change.new, 5);
        assert_eq!(change.fired, vec![fired("high", false)]);
    }

    #[tokio::test]
    async fn a_crossing_made_by_decay_and_a_delta_together_fires_once() {
        let counters = counters(&[decaying_high(-1)]);
        counters.set("g", None, None, 4, NOW).await.unwrap();
        // Growth takes 4 to 5 and the delta to 6: one step from 4, so one fire.
        let change = counters.add("g", None, None, 1, NOW + 1).await.unwrap();
        assert_eq!(change.new, 6);
        assert_eq!(change.fired, vec![fired("high", false)]);
    }

    #[tokio::test]
    async fn a_set_to_the_value_decay_left_does_not_fire_again() {
        let counters = counters(&[decaying_high(1)]);
        counters.set("g", None, None, 5, NOW).await.unwrap();
        // The stored 5 already holds the condition, so setting 5 again is no crossing.
        let change = counters.set("g", None, None, 5, NOW + 1).await.unwrap();
        assert_eq!(change.old, 4);
        assert!(change.fired.is_empty());
    }

    #[tokio::test]
    async fn a_crossing_is_reported_once_when_changes_race() {
        let mut def = plain("g");
        def.triggers = vec![TriggerDef {
            name: "two".to_owned(),
            condition: Condition::parse(">= 2").unwrap(),
            reverse: Condition::parse("< 2").unwrap(),
        }];
        let backend = Arc::new(Interleaved(MemoryStorage::default()));
        let counters = Counters::new(&store_on(backend), &[def]).unwrap();
        let results =
            futures_util::future::join_all((0..4).map(|_| counters.add("g", None, None, 1, NOW)))
                .await;
        assert!(results.iter().all(Result::is_ok), "{results:?}");
        let fires: usize = results
            .iter()
            .filter_map(|result| result.as_ref().ok())
            .map(|change| change.fired.len())
            .sum();
        assert_eq!(fires, 1);
    }
}
