//! Limits how often a command may be used.

use std::collections::HashMap;
use std::sync::{Mutex, PoisonError};
use std::time::{Duration, Instant};

use closure_core::event::{GuildId, InteractionRef};

/// Whose use of the command counts against the limit.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CooldownScope {
    /// Each user has the limit to themselves.
    User,
    /// Everyone in the guild shares it.
    Guild,
    /// Everyone in a channel shares it.
    Channel,
}

/// At most one use of a command per `period` and scope.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Cooldown {
    pub scope: CooldownScope,
    pub period: Duration,
}

impl Cooldown {
    #[must_use]
    pub fn per_user(period: Duration) -> Self {
        Self {
            scope: CooldownScope::User,
            period,
        }
    }

    #[must_use]
    pub fn per_guild(period: Duration) -> Self {
        Self {
            scope: CooldownScope::Guild,
            period,
        }
    }

    #[must_use]
    pub fn per_channel(period: Duration) -> Self {
        Self {
            scope: CooldownScope::Channel,
            period,
        }
    }
}

/// How many cooldowns are followed at once. Past it, expired ones are dropped; if all are still
/// running, a new one is not recorded (the command is not limited for that user) rather than
/// let memory grow.
const MAX_ENTRIES: usize = 50_000;

type Key = (String, GuildId, u64);

#[derive(Debug, Default)]
pub(crate) struct Cooldowns {
    until: Mutex<HashMap<Key, Instant>>,
}

impl Cooldowns {
    /// Records a use of `route`, or says how long to wait.
    pub(crate) fn take(
        &self,
        route: &str,
        invocation: &(impl InteractionRef + ?Sized),
        cooldown: &Cooldown,
        now: Instant,
    ) -> Result<(), Duration> {
        let subject = match cooldown.scope {
            CooldownScope::User => invocation.user().id.get(),
            CooldownScope::Guild => 0,
            CooldownScope::Channel => invocation.channel_id().get(),
        };
        let key = (route.to_owned(), invocation.guild_id(), subject);
        let mut until = self.until.lock().unwrap_or_else(PoisonError::into_inner);
        if let Some(end) = until.get(&key)
            && *end > now
        {
            return Err(*end - now);
        }
        if until.len() >= MAX_ENTRIES {
            until.retain(|_, end| *end > now);
        }
        if until.len() < MAX_ENTRIES || until.contains_key(&key) {
            until.insert(key, now + cooldown.period);
        }
        Ok(())
    }

    #[cfg(test)]
    fn len(&self) -> usize {
        self.until
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .len()
    }
}

#[cfg(test)]
mod tests {
    use closure_core::event::{CommandInvocation, InteractionToken, UserInfo};
    use twilight_model::guild::Permissions;
    use twilight_model::id::Id;

    use super::*;

    fn invocation(guild: u64, channel: u64, user: u64) -> CommandInvocation {
        CommandInvocation {
            interaction_id: Id::new(1),
            application_id: Id::new(1),
            token: InteractionToken::new("t"),
            guild_id: Id::new(guild),
            channel_id: Id::new(channel),
            user: UserInfo {
                id: Id::new(user),
                name: "u".into(),
                bot: false,
            },
            roles: vec![],
            invoker_permissions: Permissions::empty(),
            locale: None,
            command: "c".into(),
            subcommand: None,
            options: vec![],
        }
    }

    #[test]
    fn a_use_blocks_the_next_one_of_the_same_scope_until_the_period_is_over() {
        let cooldowns = Cooldowns::default();
        let start = Instant::now();
        let limit = Cooldown::per_user(Duration::from_secs(10));
        let alice = invocation(1, 5, 100);

        assert_eq!(cooldowns.take("warn", &alice, &limit, start), Ok(()));
        assert_eq!(
            cooldowns.take("warn", &alice, &limit, start + Duration::from_secs(4)),
            Err(Duration::from_secs(6))
        );
        // Others, other commands and other guilds are not held up.
        assert_eq!(
            cooldowns.take("warn", &invocation(1, 5, 101), &limit, start),
            Ok(())
        );
        assert_eq!(cooldowns.take("mute", &alice, &limit, start), Ok(()));
        assert_eq!(
            cooldowns.take("warn", &invocation(2, 5, 100), &limit, start),
            Ok(())
        );
        assert_eq!(
            cooldowns.take("warn", &alice, &limit, start + Duration::from_secs(10)),
            Ok(())
        );
    }

    #[test]
    fn guild_and_channel_scopes_are_shared_by_everyone_in_them() {
        let cooldowns = Cooldowns::default();
        let start = Instant::now();
        let guild = Cooldown::per_guild(Duration::from_secs(5));
        assert_eq!(
            cooldowns.take("a", &invocation(1, 5, 100), &guild, start),
            Ok(())
        );
        assert!(
            cooldowns
                .take("a", &invocation(1, 6, 101), &guild, start)
                .is_err()
        );

        let channel = Cooldown::per_channel(Duration::from_secs(5));
        assert_eq!(
            cooldowns.take("b", &invocation(1, 5, 100), &channel, start),
            Ok(())
        );
        assert!(
            cooldowns
                .take("b", &invocation(1, 5, 101), &channel, start)
                .is_err()
        );
        assert_eq!(
            cooldowns.take("b", &invocation(1, 6, 100), &channel, start),
            Ok(())
        );
    }

    #[test]
    fn finished_cooldowns_are_dropped_when_the_table_is_full() {
        let cooldowns = Cooldowns::default();
        let start = Instant::now();
        let short = Cooldown::per_user(Duration::from_secs(1));
        let long = Cooldown::per_user(Duration::from_secs(3600));
        for user in 1..=MAX_ENTRIES as u64 {
            cooldowns
                .take("a", &invocation(1, 5, user), &short, start)
                .unwrap();
        }
        assert_eq!(cooldowns.len(), MAX_ENTRIES);
        // Everything has ended by now, so there is room again.
        let later = start + Duration::from_secs(2);
        cooldowns
            .take("a", &invocation(1, 5, 999_999), &long, later)
            .unwrap();
        assert_eq!(cooldowns.len(), 1);

        // While all are running, a new one is not recorded: the table does not grow.
        for user in 1..MAX_ENTRIES as u64 {
            cooldowns
                .take("a", &invocation(1, 5, user), &long, later)
                .unwrap();
        }
        assert_eq!(cooldowns.len(), MAX_ENTRIES);
        cooldowns
            .take("a", &invocation(1, 5, 1_000_000), &long, later)
            .unwrap();
        assert_eq!(cooldowns.len(), MAX_ENTRIES);
        assert!(
            cooldowns
                .take("a", &invocation(1, 5, 1_000_000), &long, later)
                .is_ok()
        );
    }
}
