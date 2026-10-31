//! The guilds whose configuration changed, until their modules are told. A guild that changes again
//! before it is told waits once, so the queue never holds more entries than there are guilds.

use std::collections::{HashSet, VecDeque};
use std::sync::{Mutex, PoisonError};

use closure_core::event::GuildId;
use tokio::sync::Notify;

#[derive(Default)]
pub struct ConfigChanges {
    pending: Mutex<Pending>,
    woken: Notify,
}

#[derive(Default)]
struct Pending {
    order: VecDeque<GuildId>,
    waiting: HashSet<GuildId>,
}

impl ConfigChanges {
    /// Queues `guild` behind the guilds that wait already, unless it waits.
    pub fn push(&self, guild: GuildId) {
        let mut pending = self.pending.lock().unwrap_or_else(PoisonError::into_inner);
        if pending.waiting.insert(guild) {
            pending.order.push_back(guild);
        }
        drop(pending);
        self.woken.notify_one();
    }

    /// The guild that waited longest, or `None` when no guild waits.
    pub fn try_next(&self) -> Option<GuildId> {
        let mut pending = self.pending.lock().unwrap_or_else(PoisonError::into_inner);
        let guild = pending.order.pop_front()?;
        pending.waiting.remove(&guild);
        Some(guild)
    }

    /// Waits for the guild that has waited longest.
    pub async fn next(&self) -> GuildId {
        loop {
            if let Some(guild) = self.try_next() {
                return guild;
            }
            self.woken.notified().await;
        }
    }
}

#[cfg(test)]
mod tests {
    use twilight_model::id::Id;

    use super::*;

    #[test]
    fn a_guild_that_changes_again_while_it_waits_is_told_once_in_turn() {
        let changes = ConfigChanges::default();
        changes.push(Id::new(1));
        changes.push(Id::new(2));
        changes.push(Id::new(1));
        assert_eq!(changes.try_next(), Some(Id::new(1)));
        assert_eq!(changes.try_next(), Some(Id::new(2)));
        assert_eq!(changes.try_next(), None);
    }
}
