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
    /// The last report of a missing permission, per guild and rule.
    reported: Mutex<HashMap<String, i64>>,
    /// The slowmode last set for a channel, and when.
    slowmodes: Mutex<HashMap<ChannelId, (u16, i64)>>,
}

fn lock<T>(mutex: &Mutex<T>) -> MutexGuard<'_, T> {
    // Each of these maps is only ever left whole, so a poisoned lock is still usable.
    mutex.lock().unwrap_or_else(PoisonError::into_inner)
}

impl Memory {
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

    /// Remembers that the slowmode `seconds` was set on `channel` now.
    pub fn remember_slowmode(&self, channel: ChannelId, seconds: u16, now: i64) {
        let mut slowmodes = lock(&self.slowmodes);
        if slowmodes.len() >= MAX_REMEMBERED {
            slowmodes.retain(|_, (_, at)| now - *at < SLOWMODE_REPEAT_SECONDS);
        }
        slowmodes.insert(channel, (seconds, now));
    }
}
