//! What the module remembers between events, besides the recent actions and the rules: the role
//! changes it made itself, the missing permissions it reported, and the slowmodes it set.

use std::collections::{HashMap, VecDeque};
use std::sync::{Mutex, MutexGuard, PoisonError};

use closure_sdk::{ChannelId, GuildId, RoleId, UserId};

/// A role change the bot made is ignored for this long; Discord's gateway reports it within seconds.
const OWN_ROLE_CHANGE_SECONDS: i64 = 10;
const MAX_OWN_ROLE_CHANGES: usize = 256;
/// A missing permission is reported once a minute per rule.
const REPORT_GAP_SECONDS: i64 = 60;
/// The same slowmode is not set again within this time: Discord limits how often a channel is edited.
const SLOWMODE_REPEAT_SECONDS: i64 = 600;
const MAX_REMEMBERED: usize = 1_024;
/// How many threads are followed, for all the guilds together.
const MAX_THREADS_REMEMBERED: usize = 100_000;

/// The quarantine role found for a guild, the role the server configured when it was found, and
/// the time after which it is looked up again.
#[derive(Debug, Clone, Copy)]
struct FoundRole {
    role: RoleId,
    configured: Option<RoleId>,
    until: i64,
}

/// How long a joiner may be given the quarantine role found last, before the role is looked up again.
const QUARANTINE_ROLE_SECONDS: i64 = 60;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct OwnRoleChange {
    guild: GuildId,
    user: UserId,
    role: RoleId,
    at: i64,
}

#[derive(Debug, Default)]
pub struct Memory {
    /// Oldest first; bounded, and pruned by age.
    own_roles: Mutex<VecDeque<OwnRoleChange>>,
    reported: Mutex<HashMap<String, i64>>,
    slowmodes: Mutex<HashMap<ChannelId, (u16, i64)>>,
    /// The parent of each thread seen, by guild and thread: a lookup, not a scan.
    thread_parents: Mutex<HashMap<(GuildId, ChannelId), ChannelId>>,
    /// The quarantine role of each guild with the role that was configured and the time it is
    /// asked about again.
    quarantine_roles: Mutex<HashMap<GuildId, FoundRole>>,
}

fn lock<T>(mutex: &Mutex<T>) -> MutexGuard<'_, T> {
    // Each of these maps is only ever left whole, so a poisoned lock is still usable.
    mutex.lock().unwrap_or_else(PoisonError::into_inner)
}

impl Memory {
    pub fn remember_thread(&self, guild: GuildId, thread: ChannelId, parent: Option<ChannelId>) {
        let mut threads = lock(&self.thread_parents);
        let Some(parent) = parent else {
            threads.remove(&(guild, thread));
            return;
        };
        // Past the bound the table starts again, and a thread it forgot is learned at its next event.
        if threads.len() >= MAX_THREADS_REMEMBERED && !threads.contains_key(&(guild, thread)) {
            threads.clear();
        }
        threads.insert((guild, thread), parent);
    }

    pub fn parent_or_channel(&self, guild: GuildId, channel: ChannelId) -> ChannelId {
        lock(&self.thread_parents)
            .get(&(guild, channel))
            .copied()
            .unwrap_or(channel)
    }

    /// The quarantine role of the guild when it was looked up lately for the same configured role.
    pub fn quarantine_role(
        &self,
        guild: GuildId,
        configured: Option<RoleId>,
        now: i64,
    ) -> Option<RoleId> {
        lock(&self.quarantine_roles)
            .get(&guild)
            .filter(|found| found.configured == configured && now < found.until)
            .map(|found| found.role)
    }

    pub fn remember_quarantine_role(
        &self,
        guild: GuildId,
        configured: Option<RoleId>,
        role: RoleId,
        now: i64,
    ) {
        lock(&self.quarantine_roles).insert(
            guild,
            FoundRole {
                role,
                configured,
                until: now + QUARANTINE_ROLE_SECONDS,
            },
        );
    }

    /// The role could not be given: the next join looks the role up again.
    pub fn forget_quarantine_role(&self, guild: GuildId) {
        lock(&self.quarantine_roles).remove(&guild);
    }

    /// Remembers that the bot is about to give or take `role` from `user`.
    pub fn ignore_role(&self, guild: GuildId, user: UserId, role: RoleId, now: i64) {
        let mut changes = lock(&self.own_roles);
        changes.retain(|change| now - change.at < OWN_ROLE_CHANGE_SECONDS);
        if changes.len() >= MAX_OWN_ROLE_CHANGES {
            changes.pop_front();
        }
        changes.push_back(OwnRoleChange {
            guild,
            user,
            role,
            at: now,
        });
    }

    /// Whether a role change of `user` in `role` is the bot's own, and so is ignored. Each change the
    /// bot made is matched once.
    pub fn take_own_role(&self, guild: GuildId, user: UserId, role: RoleId, now: i64) -> bool {
        let mut changes = lock(&self.own_roles);
        changes.retain(|change| now - change.at < OWN_ROLE_CHANGE_SECONDS);
        let found = changes
            .iter()
            .position(|change| change.guild == guild && change.user == user && change.role == role);
        found.map(|index| changes.remove(index)).is_some()
    }

    /// Whether a missing permission of `rule` may be reported now; at most once a minute.
    pub fn should_report(&self, guild: GuildId, rule: &str, now: i64) -> bool {
        let mut reported = lock(&self.reported);
        let key = format!("{}:{rule}", guild.get());
        if reported
            .get(&key)
            .is_some_and(|last| now - last < REPORT_GAP_SECONDS)
        {
            return false;
        }
        if reported.len() >= MAX_REMEMBERED {
            reported.retain(|_, last| now - *last < REPORT_GAP_SECONDS);
        }
        reported.insert(key, now);
        true
    }

    /// Whether the slowmode `seconds` was set on `channel` a moment ago, so that it need not be set
    /// again.
    pub fn is_repeat_slowmode(&self, channel: ChannelId, seconds: u16, now: i64) -> bool {
        lock(&self.slowmodes)
            .get(&channel)
            .is_some_and(|(set, at)| *set == seconds && now - at < SLOWMODE_REPEAT_SECONDS)
    }

    pub fn remember_slowmode(&self, channel: ChannelId, seconds: u16, now: i64) {
        let mut slowmodes = lock(&self.slowmodes);
        if slowmodes.len() >= MAX_REMEMBERED {
            slowmodes.retain(|_, (_, at)| now - *at < SLOWMODE_REPEAT_SECONDS);
        }
        slowmodes.insert(channel, (seconds, now));
    }
}
