use std::time::Duration;

use closure_core::action::{Action, ChannelKind};
use closure_core::event::Event;
use closure_sdk::Level;
use closure_sdk::testing::{Harness, Invocation};
use serde_json::{Value, json};
use twilight_model::channel::Channel;
use twilight_model::guild::Role;
use twilight_model::id::Id;

use crate::builder;

const GUILD: u64 = 1;
const ADMIN_ROLE: u64 = 300;
const STAFF: u64 = 400;
const CATEGORY: u64 = 10;
const RULES: u64 = 11;

fn role(id: u64, managed: bool) -> Role {
    serde_json::from_value(json!({
        "id": id.to_string(), "name": format!("role-{id}"), "color": 255, "colors": {
            "primary_color": 255, "secondary_color": null, "tertiary_color": null },
        "hoist": true, "managed": managed, "mentionable": false,
        "permissions": "1024", "position": 1, "flags": 0,
    }))
    .unwrap()
}

fn channel(id: u64, kind: u8, name: &str, parent: Option<u64>, overwrites: &Value) -> Channel {
    serde_json::from_value(json!({
        "id": id.to_string(), "type": kind, "guild_id": GUILD.to_string(), "name": name,
        "position": 0, "parent_id": parent.map(|id| id.to_string()),
        "permission_overwrites": overwrites, "topic": "read first", "nsfw": false,
        "rate_limit_per_user": 5,
    }))
    .unwrap()
}

fn harness() -> Harness {
    let harness = Harness::new(builder().unwrap()).unwrap();
    harness.permissions(|permissions| {
        permissions
            .role_levels
            .insert(Id::new(ADMIN_ROLE), Level::ADMIN);
    });
    harness.api().set_roles(
        Id::new(GUILD),
        vec![role(GUILD, false), role(STAFF, false), role(900, true)],
    );
    harness.api().set_channels(
        Id::new(GUILD),
        vec![
            channel(CATEGORY, 4, "Info", None, &json!([])),
            channel(
                RULES,
                0,
                "rules",
                Some(CATEGORY),
                &json!([{ "id": STAFF.to_string(), "type": 0, "allow": "0", "deny": "1024" }]),
            ),
        ],
    );
    harness
}

fn admin_runs(sub: &str) -> Invocation {
    Invocation::new("backup")
        .subcommand(sub)
        .user(7)
        .roles([ADMIN_ROLE])
}

fn gone(harness: &Harness) {
    harness
        .api()
        .set_roles(Id::new(GUILD), vec![role(GUILD, false)]);
    harness.api().set_channels(Id::new(GUILD), Vec::new());
}

#[tokio::test]
async fn a_backup_holds_the_roles_and_channels_but_not_everyone_or_a_bots_role() {
    let harness = harness();
    let replies = harness.invoke(admin_runs("create")).await.unwrap();
    assert!(replies.last().unwrap().content.contains("1 role(s)"));
    assert!(replies.last().unwrap().content.contains("2 channel(s)"));
    let info = harness.invoke(admin_runs("info")).await.unwrap();
    assert!(info.last().unwrap().content.contains("1 role(s)"));
}

#[tokio::test]
async fn restore_creates_the_roles_and_channels_again_in_order_and_is_safe_to_repeat() {
    let harness = harness();
    harness.invoke(admin_runs("create")).await.unwrap();
    gone(&harness);

    let replies = harness.invoke(admin_runs("restore")).await.unwrap();
    assert!(replies.last().unwrap().content.contains("1 role(s)"));
    let actions = harness.actions();
    let channels: Vec<_> = actions
        .iter()
        .filter_map(|action| match action {
            Action::CreateChannel { spec, .. } => Some(spec),
            _ => None,
        })
        .collect();
    assert_eq!(channels.len(), 2);
    assert_eq!(
        channels[0].kind,
        ChannelKind::Category,
        "the category first"
    );
    assert_eq!(channels[1].name, "rules");
    assert_eq!(channels[1].slowmode_seconds, 5);
    assert_eq!(channels[1].topic.as_deref(), Some("read first"));
    assert!(
        channels[1]
            .parent_id
            .is_some_and(|parent| parent.get() != CATEGORY)
    );
    assert_eq!(channels[1].overwrites.len(), 1);
    assert!(matches!(
        channels[1].overwrites[0].target,
        closure_core::action::OverwriteTarget::Role(role) if role.get() != STAFF
    ));

    let before = harness.actions().len();
    harness.invoke(admin_runs("restore")).await.unwrap();
    let created = harness.actions()[before..]
        .iter()
        .filter(|action| {
            matches!(
                action,
                Action::CreateChannel { .. } | Action::CreateRole { .. }
            )
        })
        .count();
    assert_eq!(created, 0, "everything is there the second time");
}

#[tokio::test]
async fn the_daily_backup_does_not_replace_a_good_one_with_a_nuked_server() {
    let harness = harness();
    harness.invoke(admin_runs("create")).await.unwrap();
    gone(&harness);

    harness
        .emit(Event::GuildAvailable {
            guild_id: Id::new(GUILD),
            name: "server".into(),
            preferred_locale: None,
        })
        .await
        .unwrap();
    assert_eq!(harness.jobs(), 1);
    harness.advance(Duration::from_hours(25));
    let ran = harness.run_due_jobs().await;
    assert!(ran.iter().all(Result::is_ok), "{ran:?}");

    let info = harness.invoke(admin_runs("info")).await.unwrap();
    assert!(info.last().unwrap().content.contains("2 channel(s)"));
}

#[tokio::test]
async fn only_an_admin_may_use_it() {
    let harness = harness();
    harness.invoke(admin_runs("create")).await.unwrap();
    gone(&harness);
    let _ = harness
        .invoke(Invocation::new("backup").subcommand("restore").user(8))
        .await;
    assert!(!harness.actions().iter().any(|action| matches!(
        action,
        Action::CreateChannel { .. } | Action::CreateRole { .. }
    )));
}

fn made(harness: &Harness) -> (usize, usize) {
    let actions = harness.actions();
    let count = |pick: fn(&Action) -> bool| actions.iter().filter(|action| pick(action)).count();
    (
        count(|a| matches!(a, Action::CreateRole { .. })),
        count(|a| matches!(a, Action::CreateChannel { .. })),
    )
}

#[tokio::test]
async fn a_restore_that_fails_for_one_channel_keeps_its_record_and_makes_only_that_one_next_time() {
    let harness = harness();
    harness.invoke(admin_runs("create")).await.unwrap();
    gone(&harness);
    // The calls of the restore, in order: the answer, the roles, the channels, the role, the
    // category and then `rules`, which is the sixth.
    harness
        .api()
        .fail_nth(6, closure_core::error::DiscordError::Forbidden("no".into()));
    let replies = harness.invoke(admin_runs("restore")).await.unwrap();
    assert!(
        replies
            .last()
            .unwrap()
            .content
            .contains("Could not create: 1"),
        "{replies:?}"
    );
    assert_eq!(made(&harness), (1, 1), "the role and the category");

    // Nothing of the backup is lost: both channels are still named by it.
    let info = harness.invoke(admin_runs("info")).await.unwrap();
    assert!(info.last().unwrap().content.contains("2 channel(s)"));

    // The second restore makes `rules` and nothing else, and puts it in the new category.
    let replies = harness.invoke(admin_runs("restore")).await.unwrap();
    assert!(
        replies
            .last()
            .unwrap()
            .content
            .contains("Could not create: 0")
    );
    assert_eq!(made(&harness), (1, 2));
    let rules = harness
        .actions()
        .iter()
        .find_map(|action| match action {
            Action::CreateChannel { spec, .. } if spec.name == "rules" => Some(spec.clone()),
            _ => None,
        })
        .expect("rules is made");
    assert!(
        rules
            .parent_id
            .is_some_and(|parent| parent.get() != CATEGORY),
        "it goes into the category made the first time: {rules:?}"
    );
    assert_eq!(rules.overwrites.len(), 1);

    // And a third restore finds everything there.
    harness.invoke(admin_runs("restore")).await.unwrap();
    assert_eq!(made(&harness), (1, 2));
}

#[tokio::test]
async fn a_channel_whose_category_could_not_be_made_waits_and_is_made_inside_it_next_time() {
    let harness = harness();
    harness.invoke(admin_runs("create")).await.unwrap();
    gone(&harness);
    // The fifth call is the category.
    harness
        .api()
        .fail_nth(5, closure_core::error::DiscordError::Forbidden("no".into()));
    let replies = harness.invoke(admin_runs("restore")).await.unwrap();
    assert!(
        replies
            .last()
            .unwrap()
            .content
            .contains("Could not create: 2"),
        "the category and what is in it: {replies:?}"
    );
    assert_eq!(
        made(&harness),
        (1, 0),
        "no channel is made at the top and left there"
    );

    harness.invoke(admin_runs("restore")).await.unwrap();
    assert_eq!(made(&harness), (1, 2));
    let rules = harness
        .actions()
        .iter()
        .find_map(|action| match action {
            Action::CreateChannel { spec, .. } if spec.name == "rules" => Some(spec.clone()),
            _ => None,
        })
        .expect("rules is made");
    assert!(
        rules
            .parent_id
            .is_some_and(|parent| parent.get() != CATEGORY)
    );
}

#[tokio::test]
async fn the_daily_backup_is_set_going_when_it_is_turned_on_while_the_bot_runs() {
    let harness = harness();
    harness.set_config(json!({ "daily": false }));
    harness
        .emit(Event::GuildAvailable {
            guild_id: Id::new(GUILD),
            name: "server".into(),
            preferred_locale: None,
        })
        .await
        .unwrap();
    assert_eq!(harness.jobs(), 0);
    harness.set_config(json!({ "daily": true }));
    harness
        .emit(Event::ConfigChanged {
            guild_id: Id::new(GUILD),
        })
        .await
        .unwrap();
    assert_eq!(harness.jobs(), 1);
}

#[tokio::test]
async fn a_new_backup_replaces_the_old_one_whole_and_leaves_no_row_of_it() {
    let harness = harness();
    harness.invoke(admin_runs("create")).await.unwrap();
    harness.api().set_roles(
        Id::new(GUILD),
        vec![role(GUILD, false), role(STAFF, false), role(401, false)],
    );
    let replies = harness.invoke(admin_runs("create")).await.unwrap();
    assert!(replies.last().unwrap().content.contains("2 role(s)"));
    let store = harness.store();
    assert!(store.list("g1:", 1000).await.unwrap().is_empty());
    assert_eq!(store.list("g2:role:", 1000).await.unwrap().len(), 2);
    gone(&harness);
    harness.invoke(admin_runs("restore")).await.unwrap();
    assert_eq!(made(&harness).0, 2, "both roles of the second backup");
}

#[tokio::test]
async fn a_save_or_restore_is_refused_while_another_is_under_way() {
    let harness = harness();
    harness.invoke(admin_runs("create")).await.unwrap();
    harness.store().set("lock", &0).await.unwrap();
    for sub in ["create", "restore"] {
        let replies = harness.invoke(admin_runs(sub)).await.unwrap();
        assert!(
            replies.last().unwrap().content.contains("under way"),
            "{sub}: {replies:?}"
        );
    }
    assert_eq!(made(&harness), (0, 0));
}

#[tokio::test]
async fn a_restore_that_stopped_after_making_things_carries_on_instead_of_making_them_again() {
    let harness = harness();
    harness.invoke(admin_runs("create")).await.unwrap();
    gone(&harness);
    // The earlier restore made the role as 777 and the category as 778, then stopped.
    harness
        .api()
        .set_roles(Id::new(GUILD), vec![role(GUILD, false), role(777, false)]);
    harness.api().set_channels(
        Id::new(GUILD),
        vec![channel(778, 4, "Info", None, &json!([]))],
    );
    harness
        .store()
        .set(
            "progress",
            &json!({ "roles": { STAFF.to_string(): 777 }, "channels": { CATEGORY.to_string(): 778 } }),
        )
        .await
        .unwrap();
    harness.invoke(admin_runs("restore")).await.unwrap();
    assert_eq!(made(&harness), (0, 1), "only `rules` is made");
    let rules = harness
        .actions()
        .iter()
        .find_map(|action| match action {
            Action::CreateChannel { spec, .. } => Some(spec.clone()),
            _ => None,
        })
        .unwrap();
    assert_eq!(rules.parent_id.map(twilight_model::id::Id::get), Some(778));
}
