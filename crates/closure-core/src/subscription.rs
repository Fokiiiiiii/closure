//! Which events a module receives, and in which order relative to other modules.

use serde::{Deserialize, Serialize};

use crate::event::{ChannelId, Event, EventKind, RoleId};

/// The most entries each list of a filter may hold.
pub const MAX_FILTER_ENTRIES: usize = 100;

/// Conditions an event must meet to be delivered; all that are set must hold. It is plain data,
/// so a script module can declare it the same way.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct EventFilter {
    /// Only events in these channels (an event that is not about a channel does not match).
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub channels: Vec<ChannelId>,
    /// Not events in these channels.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub exclude_channels: Vec<ChannelId>,
    /// Not events caused by bots.
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub ignore_bots: bool,
    /// Not events about a member who holds one of these roles.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub ignore_roles: Vec<RoleId>,
}

impl EventFilter {
    #[must_use]
    pub fn matches(&self, event: &Event) -> bool {
        let channel = event.channel_id();
        if !self.channels.is_empty() && !channel.is_some_and(|c| self.channels.contains(&c)) {
            return false;
        }
        if channel.is_some_and(|c| self.exclude_channels.contains(&c)) {
            return false;
        }
        if self.ignore_bots && event.caused_by_bot() {
            return false;
        }
        !event
            .member_roles()
            .iter()
            .any(|role| self.ignore_roles.contains(role))
    }

    pub(crate) fn fits_limits(&self) -> bool {
        [
            self.channels.len(),
            self.exclude_channels.len(),
            self.ignore_roles.len(),
        ]
        .iter()
        .all(|len| *len <= MAX_FILTER_ENTRIES)
    }
}

/// A module's wish to receive one kind of event: only those that pass the filter, and after the
/// modules with a higher `priority` (it defaults to 0; equal priorities run together).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Subscription {
    pub kind: EventKind,
    #[serde(default)]
    pub filter: EventFilter,
    #[serde(default)]
    pub priority: i16,
}

impl Subscription {
    #[must_use]
    pub fn new(kind: EventKind) -> Self {
        Self {
            kind,
            filter: EventFilter::default(),
            priority: 0,
        }
    }

    /// Modules with a higher priority handle an event first.
    #[must_use]
    pub fn priority(mut self, priority: i16) -> Self {
        self.priority = priority;
        self
    }

    #[must_use]
    pub fn filter(mut self, filter: EventFilter) -> Self {
        self.filter = filter;
        self
    }

    #[must_use]
    pub fn ignore_bots(mut self) -> Self {
        self.filter.ignore_bots = true;
        self
    }

    #[must_use]
    pub fn in_channels(mut self, channels: impl IntoIterator<Item = ChannelId>) -> Self {
        self.filter.channels = channels.into_iter().collect();
        self
    }

    #[must_use]
    pub fn not_in_channels(mut self, channels: impl IntoIterator<Item = ChannelId>) -> Self {
        self.filter.exclude_channels = channels.into_iter().collect();
        self
    }

    #[must_use]
    pub fn ignore_roles(mut self, roles: impl IntoIterator<Item = RoleId>) -> Self {
        self.filter.ignore_roles = roles.into_iter().collect();
        self
    }
}

/// What a module says about an event it has handled.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub enum Flow {
    /// Let the modules with a lower priority see it too.
    #[default]
    Continue,
    /// It is dealt with: modules with a lower priority do not get it.
    Stop,
}

#[cfg(test)]
mod tests {
    use twilight_model::id::Id;

    use super::*;
    use crate::event::{MessageInfo, ThreadInfo, UserInfo};

    fn message(channel: u64, bot: bool, roles: &[u64]) -> Event {
        Event::MessageCreated(MessageInfo {
            removed_attachments: Vec::new(),
            id: Id::new(1),
            channel_id: Id::new(channel),
            guild_id: Id::new(1),
            author: UserInfo {
                id: Id::new(7),
                name: "u".into(),
                bot,
            },
            author_roles: roles.iter().map(|id| Id::new(*id)).collect(),
            content: String::new(),
            mentioned_users: vec![],
            mentions_everyone: false,
            mentioned_roles: Vec::new(),
            attachments: Vec::new(),
            embed_count: 0,
            sticker_count: 0,
            reply_to: None,
            from_webhook: false,
            created_at: 0,
            edited_at: None,
        })
    }

    #[test]
    fn an_empty_filter_lets_everything_through() {
        let filter = EventFilter::default();
        assert!(filter.matches(&message(3, true, &[1])));
        assert!(filter.matches(&Event::GuildRemoved {
            guild_id: Id::new(1)
        }));
    }

    #[test]
    fn every_condition_that_is_set_has_to_hold() {
        let channels = Subscription::new(EventKind::MessageCreated)
            .in_channels([Id::new(3)])
            .filter;
        assert!(channels.matches(&message(3, false, &[])));
        assert!(!channels.matches(&message(4, false, &[])));
        // An event that is not about a channel cannot be in one.
        assert!(!channels.matches(&Event::GuildRemoved {
            guild_id: Id::new(1)
        }));

        let outside = Subscription::new(EventKind::MessageCreated)
            .not_in_channels([Id::new(3)])
            .filter;
        assert!(!outside.matches(&message(3, false, &[])));
        assert!(outside.matches(&message(4, false, &[])));
        assert!(outside.matches(&Event::GuildRemoved {
            guild_id: Id::new(1)
        }));

        let humans = Subscription::new(EventKind::MessageCreated)
            .ignore_bots()
            .filter;
        assert!(humans.matches(&message(3, false, &[])));
        assert!(!humans.matches(&message(3, true, &[])));

        let no_staff = Subscription::new(EventKind::MessageCreated)
            .ignore_roles([Id::new(50)])
            .filter;
        assert!(no_staff.matches(&message(3, false, &[1, 2])));
        assert!(!no_staff.matches(&message(3, false, &[1, 50])));

        let all = EventFilter {
            channels: vec![Id::new(3)],
            ignore_bots: true,
            ignore_roles: vec![Id::new(50)],
            ..EventFilter::default()
        };
        assert!(all.matches(&message(3, false, &[])));
        assert!(!all.matches(&message(3, true, &[])));
        assert!(!all.matches(&message(3, false, &[50])));
        assert!(!all.matches(&message(4, false, &[])));
    }

    #[test]
    fn a_thread_is_in_its_parent_channel_for_the_channel_filters() {
        let thread = |parent: Option<u64>| {
            Event::ThreadCreated(ThreadInfo {
                id: Id::new(20),
                guild_id: Id::new(1),
                parent_id: parent.map(Id::new),
                name: "t".into(),
                owner_id: None,
                archived: false,
                locked: false,
                created_at: None,
            })
        };
        let in_parent = Subscription::new(EventKind::ThreadCreated)
            .in_channels([Id::new(3)])
            .filter;
        assert!(in_parent.matches(&thread(Some(3))));
        assert!(!in_parent.matches(&thread(Some(4))));

        let outside = Subscription::new(EventKind::ThreadCreated)
            .not_in_channels([Id::new(3)])
            .filter;
        assert!(!outside.matches(&thread(Some(3))));
        assert!(outside.matches(&thread(Some(4))));
    }

    #[test]
    fn a_subscription_round_trips_as_plain_data_with_defaults() {
        let text = r#"{ "kind": "message_created" }"#;
        let subscription: Subscription = serde_json::from_str(text).unwrap();
        assert_eq!(subscription, Subscription::new(EventKind::MessageCreated));
        let filtered = Subscription::new(EventKind::MessageCreated)
            .priority(3)
            .ignore_bots();
        let json = serde_json::to_string(&filtered).unwrap();
        assert_eq!(
            serde_json::from_str::<Subscription>(&json).unwrap(),
            filtered
        );
    }
}
