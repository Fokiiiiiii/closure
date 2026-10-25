//! Who may do what.
//!
//! A *level* from 1 to 100 says how far a member is trusted in a guild. A *permission key* such
//! as `moderation.ban` names one thing a module does. The module declares a default level for it;
//! a guild may change that level and may allow or deny single roles and users.

use std::collections::BTreeMap;
use std::fmt;

use serde::{Deserialize, Serialize};

use crate::error::CoreError;
use crate::event::{RoleId, UserId};

/// How far a member is trusted: 1 (anyone) to 100 (the owner of the server).
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(try_from = "u8", into = "u8")]
pub struct Level(u8);

impl Level {
    pub const GENERAL: Self = Self(1);
    pub const TRUSTED: Self = Self(25);
    pub const MODERATOR: Self = Self(50);
    pub const ADMIN: Self = Self(75);
    pub const OWNER: Self = Self(100);

    pub fn new(value: u8) -> Result<Self, CoreError> {
        if (1..=100).contains(&value) {
            Ok(Self(value))
        } else {
            Err(CoreError::InvalidLevel(value))
        }
    }

    #[must_use]
    pub const fn get(self) -> u8 {
        self.0
    }

    /// The level a name stands for: `general`, `trusted`, `moderator`, `admin` or `owner`.
    #[must_use]
    pub fn named(name: &str) -> Option<Self> {
        match name {
            "general" => Some(Self::GENERAL),
            "trusted" => Some(Self::TRUSTED),
            "moderator" => Some(Self::MODERATOR),
            "admin" => Some(Self::ADMIN),
            "owner" => Some(Self::OWNER),
            _ => None,
        }
    }
}

impl TryFrom<u8> for Level {
    type Error = CoreError;

    fn try_from(value: u8) -> Result<Self, CoreError> {
        Self::new(value)
    }
}

impl From<Level> for u8 {
    fn from(level: Level) -> Self {
        level.0
    }
}

impl fmt::Display for Level {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        self.0.fmt(f)
    }
}

/// Names one thing a module does, like `moderation.ban`: two to four dot-separated parts of
/// `[a-z][a-z0-9_-]*`. The first part is the id of the module that declares it, so keys of
/// different modules never collide and a guild can tell where one comes from.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(try_from = "String", into = "String")]
pub struct PermissionKey(String);

impl PermissionKey {
    pub fn new(key: impl Into<String>) -> Result<Self, CoreError> {
        let key = key.into();
        let parts: Vec<&str> = key.split('.').collect();
        let valid = key.len() <= 64
            && (2..=4).contains(&parts.len())
            && parts.iter().all(|part| {
                part.starts_with(|c: char| c.is_ascii_lowercase())
                    && part.chars().all(|c| {
                        c.is_ascii_lowercase() || c.is_ascii_digit() || c == '_' || c == '-'
                    })
            });
        if valid {
            Ok(Self(key))
        } else {
            Err(CoreError::InvalidPermissionKey(key))
        }
    }

    #[must_use]
    pub fn as_str(&self) -> &str {
        &self.0
    }

    /// The first part: the module the key belongs to.
    #[must_use]
    pub fn module(&self) -> &str {
        self.0.split('.').next().unwrap_or_default()
    }
}

impl TryFrom<String> for PermissionKey {
    type Error = CoreError;

    fn try_from(key: String) -> Result<Self, CoreError> {
        Self::new(key)
    }
}

impl From<PermissionKey> for String {
    fn from(key: PermissionKey) -> Self {
        key.0
    }
}

impl fmt::Display for PermissionKey {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.0)
    }
}

/// A permission a module declares in its manifest.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct PermissionSpec {
    pub key: PermissionKey,
    /// The level a member needs unless the guild decided otherwise.
    pub default_level: Level,
    pub description: String,
}

impl PermissionSpec {
    pub fn new(
        key: impl Into<String>,
        default_level: Level,
        description: impl Into<String>,
    ) -> Result<Self, CoreError> {
        Ok(Self {
            key: PermissionKey::new(key)?,
            default_level,
            description: description.into(),
        })
    }
}

/// The member whose permission is asked about.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Actor {
    pub user: UserId,
    pub roles: Vec<RoleId>,
    /// Holds Discord's Administrator permission.
    pub administrator: bool,
    /// Owns the server.
    pub owner: bool,
}

/// What a guild decided about one permission key.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct KeyRule {
    /// Replaces the level the module declared.
    pub min_level: Option<Level>,
    pub allow_roles: Vec<RoleId>,
    pub allow_users: Vec<UserId>,
    pub deny_roles: Vec<RoleId>,
    pub deny_users: Vec<UserId>,
}

/// Why a permission was granted or refused.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Reason {
    /// The owner of the server is never refused.
    Owner,
    DeniedUser,
    DeniedRole(RoleId),
    AllowedUser,
    AllowedRole(RoleId),
    Level {
        actual: Level,
        required: Level,
    },
    /// No module declares the key, so nobody but the owner has it.
    UnknownKey,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Decision {
    pub allowed: bool,
    pub reason: Reason,
}

/// A guild's decisions about levels and permission keys.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct GuildPermissions {
    /// The level of a member who holds Discord's Administrator permission.
    pub administrator_level: Level,
    pub role_levels: BTreeMap<RoleId, Level>,
    pub user_levels: BTreeMap<UserId, Level>,
    /// What the guild decided about single keys.
    pub keys: BTreeMap<PermissionKey, KeyRule>,
    /// The level every declared key needs unless [`KeyRule::min_level`] says otherwise.
    pub defaults: BTreeMap<PermissionKey, Level>,
}

impl Default for GuildPermissions {
    fn default() -> Self {
        Self {
            administrator_level: Level::ADMIN,
            role_levels: BTreeMap::new(),
            user_levels: BTreeMap::new(),
            keys: BTreeMap::new(),
            defaults: BTreeMap::new(),
        }
    }
}

impl GuildPermissions {
    /// The highest level any of the member's attributes gives: the owner is at the top, then the
    /// roles, the member as a person, and Administrator; everyone is at least general.
    #[must_use]
    pub fn level_of(&self, actor: &Actor) -> Level {
        if actor.owner {
            return Level::OWNER;
        }
        let from_roles = actor
            .roles
            .iter()
            .filter_map(|role| self.role_levels.get(role));
        let administrator = actor.administrator.then_some(&self.administrator_level);
        from_roles
            .chain(self.user_levels.get(&actor.user))
            .chain(administrator)
            .copied()
            .max()
            .unwrap_or(Level::GENERAL)
    }

    /// Whether `actor` may use `key`. Refusals come first, except for the owner; then explicit
    /// allowances; then the level.
    #[must_use]
    pub fn check(&self, actor: &Actor, key: &PermissionKey) -> Decision {
        let decision = |allowed, reason| Decision { allowed, reason };
        if actor.owner {
            return decision(true, Reason::Owner);
        }
        let Some(default_level) = self.defaults.get(key).copied() else {
            return decision(false, Reason::UnknownKey);
        };
        let rule = self.keys.get(key);
        if let Some(rule) = rule {
            if rule.deny_users.contains(&actor.user) {
                return decision(false, Reason::DeniedUser);
            }
            if let Some(role) = actor
                .roles
                .iter()
                .find(|role| rule.deny_roles.contains(role))
            {
                return decision(false, Reason::DeniedRole(*role));
            }
            if rule.allow_users.contains(&actor.user) {
                return decision(true, Reason::AllowedUser);
            }
            if let Some(role) = actor
                .roles
                .iter()
                .find(|role| rule.allow_roles.contains(role))
            {
                return decision(true, Reason::AllowedRole(*role));
            }
        }
        let required = rule
            .and_then(|rule| rule.min_level)
            .unwrap_or(default_level);
        let actual = self.level_of(actor);
        decision(actual >= required, Reason::Level { actual, required })
    }
}

#[cfg(test)]
mod tests {
    use twilight_model::id::Id;

    use super::*;

    fn key(text: &str) -> PermissionKey {
        PermissionKey::new(text).unwrap()
    }

    fn member(roles: &[u64]) -> Actor {
        Actor {
            user: Id::new(1),
            roles: roles.iter().map(|id| Id::new(*id)).collect(),
            administrator: false,
            owner: false,
        }
    }

    #[test]
    fn levels_run_from_one_to_one_hundred() {
        assert!(Level::new(0).is_err());
        assert!(Level::new(101).is_err());
        assert_eq!(Level::new(1).unwrap(), Level::GENERAL);
        assert_eq!(Level::new(100).unwrap(), Level::OWNER);
        assert!(Level::GENERAL < Level::TRUSTED && Level::MODERATOR < Level::ADMIN);
        assert_eq!(Level::named("moderator"), Some(Level::MODERATOR));
        assert_eq!(Level::named("Moderator"), None);
        assert!(serde_json::from_str::<Level>("0").is_err());
        assert_eq!(serde_json::to_string(&Level::ADMIN).unwrap(), "75");
    }

    #[test]
    fn keys_are_dotted_lowercase_names() {
        for good in ["moderation.ban", "warn.add_all", "a-b.c.d.e", "x1.y2"] {
            assert!(PermissionKey::new(good).is_ok(), "{good}");
        }
        for bad in [
            "",
            "ban",
            "a.b.c.d.e",
            "Moderation.ban",
            "moderation.",
            ".ban",
            "1a.b",
            "a..b",
            "a.b c",
            &format!("a.{}", "b".repeat(70)),
        ] {
            assert!(PermissionKey::new(bad).is_err(), "{bad}");
        }
        assert_eq!(key("moderation.ban").module(), "moderation");
    }

    #[test]
    fn a_members_level_is_the_highest_any_of_their_attributes_gives() {
        let mut permissions = GuildPermissions::default();
        permissions.role_levels.insert(Id::new(10), Level::TRUSTED);
        permissions
            .role_levels
            .insert(Id::new(11), Level::MODERATOR);
        permissions
            .user_levels
            .insert(Id::new(1), Level::new(60).unwrap());

        assert_eq!(permissions.level_of(&member(&[])), Level::new(60).unwrap());
        let mut other = member(&[10, 11, 12]);
        other.user = Id::new(2);
        assert_eq!(permissions.level_of(&other), Level::MODERATOR);
        other.roles.clear();
        assert_eq!(permissions.level_of(&other), Level::GENERAL);
        other.administrator = true;
        assert_eq!(permissions.level_of(&other), Level::ADMIN);
        permissions.administrator_level = Level::GENERAL;
        assert_eq!(permissions.level_of(&other), Level::GENERAL);
        other.owner = true;
        assert_eq!(permissions.level_of(&other), Level::OWNER);
    }

    #[test]
    fn the_level_a_module_declares_decides_unless_the_guild_changed_it() {
        let mut permissions = GuildPermissions::default();
        permissions
            .role_levels
            .insert(Id::new(10), Level::MODERATOR);
        let ban = key("moderation.ban");
        permissions.defaults.insert(ban.clone(), Level::MODERATOR);
        let moderator = member(&[10]);

        let decision = permissions.check(&moderator, &ban);
        assert!(decision.allowed);
        assert_eq!(
            decision.reason,
            Reason::Level {
                actual: Level::MODERATOR,
                required: Level::MODERATOR
            }
        );
        assert!(!permissions.check(&member(&[]), &ban).allowed);

        let rule = KeyRule {
            min_level: Some(Level::ADMIN),
            ..KeyRule::default()
        };
        permissions.keys.insert(ban.clone(), rule);
        let decision = permissions.check(&moderator, &ban);
        assert!(!decision.allowed);
        assert_eq!(
            decision.reason,
            Reason::Level {
                actual: Level::MODERATOR,
                required: Level::ADMIN
            }
        );
    }

    #[test]
    fn refusals_beat_allowances_and_allowances_beat_the_level() {
        let mut permissions = GuildPermissions::default();
        let ban = key("moderation.ban");
        permissions.defaults.insert(ban.clone(), Level::ADMIN);
        permissions.keys.insert(
            ban.clone(),
            KeyRule {
                allow_roles: vec![Id::new(20)],
                deny_roles: vec![Id::new(21)],
                // 6 is allowed by name and refused by name: the refusal wins.
                allow_users: vec![Id::new(5), Id::new(6)],
                deny_users: vec![Id::new(6)],
                ..KeyRule::default()
            },
        );
        // No level at all, yet allowed by role or by name.
        let by_role = permissions.check(&member(&[20]), &ban);
        assert_eq!(by_role.reason, Reason::AllowedRole(Id::new(20)));
        assert!(by_role.allowed);
        let mut friend = member(&[]);
        friend.user = Id::new(5);
        assert_eq!(permissions.check(&friend, &ban).reason, Reason::AllowedUser);

        // A refusal wins over an allowance and over a high level.
        let both = permissions.check(&member(&[20, 21]), &ban);
        assert_eq!(both.reason, Reason::DeniedRole(Id::new(21)));
        assert!(!both.allowed);
        let mut banned = member(&[20]);
        banned.user = Id::new(6);
        banned.administrator = true;
        assert_eq!(permissions.check(&banned, &ban).reason, Reason::DeniedUser);
    }

    #[test]
    fn the_owner_is_never_refused() {
        let mut permissions = GuildPermissions::default();
        let ban = key("moderation.ban");
        permissions.defaults.insert(ban.clone(), Level::OWNER);
        permissions.keys.insert(
            ban.clone(),
            KeyRule {
                deny_users: vec![Id::new(1)],
                ..KeyRule::default()
            },
        );
        let mut owner = member(&[]);
        owner.owner = true;
        let decision = permissions.check(&owner, &ban);
        assert!(decision.allowed);
        assert_eq!(decision.reason, Reason::Owner);
        // A key nobody declared is refused to everyone else.
        let unknown = key("moderation.unheard");
        assert_eq!(
            permissions.check(&member(&[]), &unknown).reason,
            Reason::UnknownKey
        );
        assert!(permissions.check(&owner, &unknown).allowed);
    }
}
