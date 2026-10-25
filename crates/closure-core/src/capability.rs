use std::collections::BTreeSet;
use std::fmt;

use serde::{Deserialize, Serialize};

/// A coarse permission a module must hold to perform a class of Discord actions.
///
/// Native modules declare what they need and receive it by default; modules loaded from
/// scripts or WASM receive only what the guild administrator explicitly grants.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Capability {
    SendMessages,
    AddReactions,
    ManageMessages,
    ReadGuild,
    ReadBans,
    KickMembers,
    BanMembers,
    TimeoutMembers,
    ManageNicknames,
    ManageRoles,
    ManageChannels,
    ManageAutoMod,
    ReadAuditLog,
    RespondToInteractions,
    /// Keep data of its own: entries and counters (see [`crate::storage`]).
    Storage,
    /// Have work done later, once or again and again (see [`crate::schedule`]).
    Schedule,
    /// Record moderation cases, numbered per guild and shared by modules (see [`crate::cases`]).
    Cases,
}

impl Capability {
    pub const ALL: [Self; 17] = [
        Self::SendMessages,
        Self::AddReactions,
        Self::ManageMessages,
        Self::ReadGuild,
        Self::ReadBans,
        Self::KickMembers,
        Self::BanMembers,
        Self::TimeoutMembers,
        Self::ManageNicknames,
        Self::ManageRoles,
        Self::ManageChannels,
        Self::ManageAutoMod,
        Self::ReadAuditLog,
        Self::RespondToInteractions,
        Self::Storage,
        Self::Schedule,
        Self::Cases,
    ];

    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::SendMessages => "send_messages",
            Self::AddReactions => "add_reactions",
            Self::ManageMessages => "manage_messages",
            Self::ReadGuild => "read_guild",
            Self::ReadBans => "read_bans",
            Self::KickMembers => "kick_members",
            Self::BanMembers => "ban_members",
            Self::TimeoutMembers => "timeout_members",
            Self::ManageNicknames => "manage_nicknames",
            Self::ManageRoles => "manage_roles",
            Self::ManageChannels => "manage_channels",
            Self::ManageAutoMod => "manage_auto_mod",
            Self::ReadAuditLog => "read_audit_log",
            Self::RespondToInteractions => "respond_to_interactions",
            Self::Storage => "storage",
            Self::Schedule => "schedule",
            Self::Cases => "cases",
        }
    }
}

impl fmt::Display for Capability {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(transparent)]
pub struct CapabilitySet(BTreeSet<Capability>);

impl CapabilitySet {
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    #[must_use]
    pub fn all() -> Self {
        Capability::ALL.into_iter().collect()
    }

    #[must_use]
    pub fn contains(&self, capability: Capability) -> bool {
        self.0.contains(&capability)
    }

    pub fn insert(&mut self, capability: Capability) -> bool {
        self.0.insert(capability)
    }

    pub fn iter(&self) -> impl Iterator<Item = Capability> + '_ {
        self.0.iter().copied()
    }

    #[must_use]
    pub fn len(&self) -> usize {
        self.0.len()
    }

    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.0.is_empty()
    }

    /// The capabilities present in both sets.
    #[must_use]
    pub fn intersection(&self, other: &Self) -> Self {
        self.0.intersection(&other.0).copied().collect()
    }

    /// The capabilities in `self` that `granted` does not contain.
    #[must_use]
    pub fn missing_from(&self, granted: &Self) -> Vec<Capability> {
        self.0.difference(&granted.0).copied().collect()
    }
}

impl FromIterator<Capability> for CapabilitySet {
    fn from_iter<I: IntoIterator<Item = Capability>>(iter: I) -> Self {
        Self(iter.into_iter().collect())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn all_lists_every_variant_once() {
        assert_eq!(CapabilitySet::all().len(), Capability::ALL.len());
    }

    #[test]
    fn serde_uses_snake_case_names_matching_as_str() {
        for capability in Capability::ALL {
            let json = serde_json::to_string(&capability).unwrap();
            assert_eq!(json, format!("\"{}\"", capability.as_str()));
        }
    }

    #[test]
    fn missing_from_reports_only_ungranted() {
        let wanted: CapabilitySet = [Capability::BanMembers, Capability::SendMessages]
            .into_iter()
            .collect();
        let granted: CapabilitySet = [Capability::SendMessages].into_iter().collect();
        assert_eq!(wanted.missing_from(&granted), vec![Capability::BanMembers]);
        assert_eq!(wanted.intersection(&granted), granted);
    }
}
