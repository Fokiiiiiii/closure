//! Role hierarchy checks performed before a request is sent.
//!
//! Discord refuses moderation of anyone whose highest role is not below the bot's own, and of the
//! server owner. Checking this from the cache turns a vague `403 Missing Permissions` into a
//! precise [`DiscordError::HierarchyViolation`] and saves the round trip. Whenever the cache does
//! not know enough, the check is skipped and Discord has the final say.

use std::cmp::Ordering;
use std::sync::Arc;

use closure_core::action::Action;
use closure_core::error::DiscordError;
use closure_core::event::{GuildId, RoleId, UserId};
use twilight_cache_inmemory::{CacheableRole, InMemoryCache};

/// What the hierarchy check needs to know. Implemented over Twilight's cache, and by fakes in
/// tests.
pub trait CacheView: Send + Sync {
    fn bot_user(&self) -> Option<UserId>;
    fn guild_owner(&self, guild: GuildId) -> Option<UserId>;
    /// Roles of a member, not including the implicit `@everyone` role.
    fn member_roles(&self, guild: GuildId, user: UserId) -> Option<Vec<RoleId>>;
    fn role_position(&self, role: RoleId) -> Option<i64>;
}

/// [`CacheView`] over Twilight's in-memory cache.
#[derive(Debug, Clone)]
pub struct InMemoryCacheView(pub Arc<InMemoryCache>);

impl CacheView for InMemoryCacheView {
    fn bot_user(&self) -> Option<UserId> {
        self.0.current_user().map(|user| user.id)
    }

    fn guild_owner(&self, guild: GuildId) -> Option<UserId> {
        self.0.guild(guild).map(|guild| guild.owner_id())
    }

    fn member_roles(&self, guild: GuildId, user: UserId) -> Option<Vec<RoleId>> {
        self.0
            .member(guild, user)
            .map(|member| member.roles().to_vec())
    }

    fn role_position(&self, role: RoleId) -> Option<i64> {
        self.0.role(role).map(|role| role.position())
    }
}

/// Position in the hierarchy: a higher `position` wins, and between equal positions the role
/// created earlier (smaller id) wins, as Discord orders them.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct Rank {
    position: i64,
    id: u64,
}

impl Ord for Rank {
    fn cmp(&self, other: &Self) -> Ordering {
        self.position
            .cmp(&other.position)
            .then_with(|| other.id.cmp(&self.id))
    }
}

impl PartialOrd for Rank {
    fn partial_cmp(&self, other: &Self) -> Option<Ordering> {
        Some(self.cmp(other))
    }
}

/// The highest role of `user`, or `None` when the cache does not know their roles. A member
/// with no roles ranks as `@everyone` (whose id is the guild id, at position 0).
fn highest_rank(view: &dyn CacheView, guild: GuildId, user: UserId) -> Option<Rank> {
    let everyone = Rank {
        position: 0,
        id: guild.get(),
    };
    let mut best = everyone;
    for role in view.member_roles(guild, user)? {
        let rank = Rank {
            position: view.role_position(role)?,
            id: role.get(),
        };
        best = best.max(rank);
    }
    Some(best)
}

fn violation(message: &str) -> DiscordError {
    DiscordError::HierarchyViolation(message.to_owned())
}

fn check_member_target(
    view: &dyn CacheView,
    guild: GuildId,
    target: UserId,
    forbid_self: bool,
) -> Result<(), DiscordError> {
    let bot = view.bot_user();
    if forbid_self && bot == Some(target) {
        return Err(DiscordError::Invalid("the bot cannot target itself".into()));
    }
    if view.guild_owner(guild) == Some(target) {
        return Err(violation("the server owner cannot be moderated"));
    }
    let Some(bot) = bot else { return Ok(()) };
    if view.guild_owner(guild) == Some(bot) {
        return Ok(());
    }
    if let (Some(bot_rank), Some(target_rank)) = (
        highest_rank(view, guild, bot),
        highest_rank(view, guild, target),
    ) && bot_rank <= target_rank
    {
        return Err(violation(
            "the target's highest role is not below the bot's highest role",
        ));
    }
    Ok(())
}

fn check_role_target(
    view: &dyn CacheView,
    guild: GuildId,
    role: RoleId,
) -> Result<(), DiscordError> {
    let Some(bot) = view.bot_user() else {
        return Ok(());
    };
    if view.guild_owner(guild) == Some(bot) {
        return Ok(());
    }
    if let (Some(bot_rank), Some(position)) =
        (highest_rank(view, guild, bot), view.role_position(role))
        && bot_rank
            <= (Rank {
                position,
                id: role.get(),
            })
    {
        return Err(violation("the role is not below the bot's highest role"));
    }
    Ok(())
}

/// Rejects `action` if the cache proves Discord would refuse it for hierarchy reasons.
pub fn preflight(view: &dyn CacheView, action: &Action) -> Result<(), DiscordError> {
    match action {
        Action::Kick {
            guild_id, user_id, ..
        }
        | Action::Ban {
            guild_id, user_id, ..
        }
        | Action::Timeout {
            guild_id, user_id, ..
        } => check_member_target(view, *guild_id, *user_id, true),
        Action::SetNickname {
            guild_id, user_id, ..
        } => check_member_target(view, *guild_id, *user_id, false),
        Action::AddRole {
            guild_id, role_id, ..
        }
        | Action::RemoveRole {
            guild_id, role_id, ..
        }
        | Action::EditRole {
            guild_id, role_id, ..
        }
        | Action::DeleteRole {
            guild_id, role_id, ..
        } => check_role_target(view, *guild_id, *role_id),
        _ => Ok(()),
    }
}

#[cfg(test)]
mod tests {
    use std::collections::HashMap;

    use closure_core::action::Reason;
    use twilight_model::id::Id;

    use super::*;

    const GUILD: u64 = 100;
    const OWNER: u64 = 1;
    const BOT: u64 = 2;
    const TARGET: u64 = 3;

    #[derive(Default)]
    struct Fake {
        owner: Option<u64>,
        bot: Option<u64>,
        members: HashMap<u64, Vec<u64>>,
        roles: HashMap<u64, i64>,
    }

    impl CacheView for Fake {
        fn bot_user(&self) -> Option<UserId> {
            self.bot.map(Id::new)
        }

        fn guild_owner(&self, _: GuildId) -> Option<UserId> {
            self.owner.map(Id::new)
        }

        fn member_roles(&self, _: GuildId, user: UserId) -> Option<Vec<RoleId>> {
            self.members
                .get(&user.get())
                .map(|roles| roles.iter().copied().map(Id::new).collect())
        }

        fn role_position(&self, role: RoleId) -> Option<i64> {
            self.roles.get(&role.get()).copied()
        }
    }

    fn world(bot_roles: &[u64], target_roles: &[u64]) -> Fake {
        Fake {
            owner: Some(OWNER),
            bot: Some(BOT),
            members: HashMap::from([(BOT, bot_roles.to_vec()), (TARGET, target_roles.to_vec())]),
            roles: HashMap::from([(10, 5), (11, 3), (12, 3), (13, 1)]),
        }
    }

    fn ban(user: u64) -> Action {
        Action::Ban {
            guild_id: Id::new(GUILD),
            user_id: Id::new(user),
            delete_message_seconds: 0,
            reason: Reason::new("test").unwrap(),
        }
    }

    fn add_role(role: u64) -> Action {
        Action::AddRole {
            guild_id: Id::new(GUILD),
            user_id: Id::new(TARGET),
            role_id: Id::new(role),
            reason: Reason::new("test").unwrap(),
        }
    }

    #[test]
    fn a_higher_bot_role_may_moderate() {
        assert_eq!(preflight(&world(&[10], &[11]), &ban(TARGET)), Ok(()));
    }

    #[test]
    fn a_lower_or_equal_bot_role_may_not() {
        let lower = preflight(&world(&[13], &[11]), &ban(TARGET));
        assert!(matches!(lower, Err(DiscordError::HierarchyViolation(_))));
        let equal = preflight(&world(&[10], &[10]), &ban(TARGET));
        assert!(matches!(equal, Err(DiscordError::HierarchyViolation(_))));
    }

    #[test]
    fn equal_positions_are_decided_by_the_older_role() {
        // Roles 11 and 12 share position 3; the smaller id outranks the larger.
        assert_eq!(preflight(&world(&[11], &[12]), &ban(TARGET)), Ok(()));
        assert!(preflight(&world(&[12], &[11]), &ban(TARGET)).is_err());
    }

    #[test]
    fn the_highest_of_several_roles_counts() {
        assert_eq!(
            preflight(&world(&[13, 10], &[11, 12]), &ban(TARGET)),
            Ok(())
        );
        assert!(preflight(&world(&[13, 11], &[10, 13]), &ban(TARGET)).is_err());
    }

    #[test]
    fn members_without_roles_rank_as_everyone() {
        assert_eq!(preflight(&world(&[13], &[]), &ban(TARGET)), Ok(()));
        assert!(preflight(&world(&[], &[]), &ban(TARGET)).is_err());
    }

    #[test]
    fn the_owner_is_never_a_valid_target_and_an_owner_bot_is_exempt() {
        let mut fake = world(&[10], &[]);
        fake.members.insert(OWNER, vec![13]);
        assert!(matches!(
            preflight(&fake, &ban(OWNER)),
            Err(DiscordError::HierarchyViolation(_))
        ));

        let mut owner_bot = world(&[13], &[10]);
        owner_bot.owner = Some(BOT);
        assert_eq!(preflight(&owner_bot, &ban(TARGET)), Ok(()));
    }

    #[test]
    fn the_bot_cannot_ban_itself() {
        assert!(matches!(
            preflight(&world(&[10], &[]), &ban(BOT)),
            Err(DiscordError::Invalid(_))
        ));
    }

    #[test]
    fn unknown_cache_data_defers_to_discord() {
        let mut fake = world(&[10], &[11]);
        fake.members.remove(&TARGET);
        assert_eq!(preflight(&fake, &ban(TARGET)), Ok(()));
        assert_eq!(preflight(&Fake::default(), &ban(TARGET)), Ok(()));
        let mut unknown_role = world(&[10], &[99]);
        unknown_role.roles.remove(&99);
        assert_eq!(preflight(&unknown_role, &ban(TARGET)), Ok(()));
    }

    #[test]
    fn assigning_a_role_needs_it_to_be_below_the_bot() {
        assert_eq!(preflight(&world(&[10], &[]), &add_role(11)), Ok(()));
        assert!(matches!(
            preflight(&world(&[11], &[]), &add_role(10)),
            Err(DiscordError::HierarchyViolation(_))
        ));
        assert!(preflight(&world(&[11], &[]), &add_role(11)).is_err());
    }

    #[test]
    fn actions_outside_moderation_are_not_checked() {
        let action = Action::GetGuild {
            guild_id: Id::new(GUILD),
        };
        assert_eq!(preflight(&Fake::default(), &action), Ok(()));
    }
}
