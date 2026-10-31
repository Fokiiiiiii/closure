//! Permission changes: the overwrites of a channel and the permissions of a role. Both name the
//! permissions as Discord does, marked `+` when they were added and `-` when they were removed.

use closure_core::event::{OverwriteInfo, OverwriteTarget};
use closure_core::module::EventContext;
use twilight_model::guild::Permissions;

use crate::{code_block, label};

/// An overwrite of a channel that differs between two states of the channel.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct OverwriteChange {
    pub(crate) target: OverwriteTarget,
    pub(crate) before: OverwriteInfo,
    pub(crate) after: OverwriteInfo,
}

/// The overwrites that differ between two states of a channel: one for each role or member whose
/// allowed or denied permissions changed. A target without an overwrite allows and denies nothing.
pub(crate) fn overwrite_changes(
    before: &[OverwriteInfo],
    after: &[OverwriteInfo],
) -> Vec<OverwriteChange> {
    let mut targets: Vec<OverwriteTarget> = before
        .iter()
        .chain(after)
        .map(|overwrite| overwrite.target)
        .collect();
    targets.sort_unstable();
    targets.dedup();
    targets
        .into_iter()
        .filter_map(|target| {
            let before_overwrite = overwrite_of(before, target);
            let after_overwrite = overwrite_of(after, target);
            let changed = before_overwrite.allow != after_overwrite.allow
                || before_overwrite.deny != after_overwrite.deny;
            changed.then_some(OverwriteChange {
                target,
                before: before_overwrite,
                after: after_overwrite,
            })
        })
        .collect()
}

/// The overwrite of `target` among `overwrites`, or one that allows and denies nothing.
fn overwrite_of(overwrites: &[OverwriteInfo], target: OverwriteTarget) -> OverwriteInfo {
    overwrites
        .iter()
        .find(|overwrite| overwrite.target == target)
        .cloned()
        .unwrap_or(OverwriteInfo {
            target,
            allow: Permissions::empty(),
            deny: Permissions::empty(),
        })
}

/// The permissions that `after` adds to `before`, marked `+`, then those it removes, marked `-`.
/// Empty when the two are the same.
pub(crate) fn permission_diff(before: Permissions, after: Permissions) -> String {
    let added = after & !before;
    let removed = before & !after;
    added
        .iter_names()
        .map(|(name, _)| format!("+{name}"))
        .chain(removed.iter_names().map(|(name, _)| format!("-{name}")))
        .collect::<Vec<_>>()
        .join(" ")
}

/// A changed overwrite as a field value: who it applies to, then what it allows and denies.
pub(crate) fn overwrite_value(ctx: &EventContext, change: &OverwriteChange) -> String {
    let mut lines = Vec::new();
    let allowed = permission_diff(change.before.allow, change.after.allow);
    if !allowed.is_empty() {
        lines.push(format!("{}: {allowed}", label(ctx, "logs-allowed")));
    }
    let denied = permission_diff(change.before.deny, change.after.deny);
    if !denied.is_empty() {
        lines.push(format!("{}: {denied}", label(ctx, "logs-denied")));
    }
    format!(
        "{}\n{}",
        target_mention(change.target),
        code_block(&lines.join("\n"))
    )
}

/// The role or member an overwrite applies to, as a mention.
fn target_mention(target: OverwriteTarget) -> String {
    match target {
        OverwriteTarget::Role(role) => format!("<@&{}>", role.get()),
        OverwriteTarget::Member(user) => format!("<@{}>", user.get()),
    }
}

/// The id of the role or member an overwrite applies to, as the audit log names it.
pub(crate) fn target_id(target: OverwriteTarget) -> u64 {
    match target {
        OverwriteTarget::Role(role) => role.get(),
        OverwriteTarget::Member(user) => user.get(),
    }
}

#[cfg(test)]
mod tests {
    use closure_core::event::{OverwriteInfo, OverwriteTarget};
    use twilight_model::guild::Permissions;
    use twilight_model::id::Id;

    use super::{overwrite_changes, permission_diff};

    fn role_overwrite(allow: Permissions, deny: Permissions) -> OverwriteInfo {
        OverwriteInfo {
            target: OverwriteTarget::Role(Id::new(9)),
            allow,
            deny,
        }
    }

    #[test]
    fn a_permission_diff_marks_what_was_added_and_what_was_removed() {
        let before = Permissions::VIEW_CHANNEL | Permissions::SEND_MESSAGES;
        let after = Permissions::VIEW_CHANNEL | Permissions::ADD_REACTIONS;
        assert_eq!(
            permission_diff(before, after),
            "+ADD_REACTIONS -SEND_MESSAGES"
        );
    }

    #[test]
    fn a_permission_diff_is_empty_when_nothing_changed() {
        let same = Permissions::VIEW_CHANNEL;
        assert_eq!(permission_diff(same, same), "");
    }

    #[test]
    fn an_overwrite_that_is_the_same_is_not_a_change() {
        let overwrite = role_overwrite(Permissions::VIEW_CHANNEL, Permissions::empty());
        let same = std::slice::from_ref(&overwrite);
        assert!(overwrite_changes(same, same).is_empty());
    }

    #[test]
    fn an_overwrite_that_appears_or_disappears_is_a_change_from_or_to_nothing() {
        let overwrite = role_overwrite(Permissions::VIEW_CHANNEL, Permissions::empty());
        let added = overwrite_changes(&[], std::slice::from_ref(&overwrite));
        assert_eq!(added.len(), 1);
        assert_eq!(added[0].before.allow, Permissions::empty());
        let removed = overwrite_changes(std::slice::from_ref(&overwrite), &[]);
        assert_eq!(removed.len(), 1);
        assert_eq!(removed[0].after.allow, Permissions::empty());
    }
}
