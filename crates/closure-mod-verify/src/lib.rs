//! Verification of new members: a member who joins gets the unverified role (if the server has
//! one) and has to press a button, or answer a small sum, to get the verified role. A member who
//! has not done it within the time the server set is kicked.
//!
//! The roles are the server's own, but the module never hands out one that carries moderator
//! powers or belongs to a punishment, so a wrong setting cannot make the button a way to power.

use std::time::Duration;

use closure_core::action::{Action, Outcome, Reason};
use closure_core::error::DiscordError;
use closure_core::event::{Event, EventKind, RoleId, UserId};
use closure_core::module::EventContext;
use closure_core::permission::Actor;
use closure_core::subscription::Flow;
use closure_sdk::duration::parse as parse_duration;
use closure_sdk::punish::{self, Moderator, Punisher, Settings};
use closure_sdk::ui::{self, CustomId, text_input};
use closure_sdk::{
    Args, Capability, Command, ComponentCx, CoreError, Cx, Intents, InteractionRef,
    InteractionReply, JobCx, Level, Locale, ModalCx, ModuleBuilder, ModuleError, PermissionSpec,
    SdkModule, Subscription, UserError,
};
use serde::{Deserialize, Serialize};
use serde_json::json;
use twilight_model::channel::message::component::ButtonStyle;
use twilight_model::guild::Permissions;

const EN: &str = include_str!("../locales/en.ftl");
const JA: &str = include_str!("../locales/ja.ftl");

const PRESS: &str = "press";
const ANSWER: &str = "answer";
const EXPIRE: &str = "expire";
const QUESTION_LIFE: Duration = Duration::from_mins(10);
/// The longest time a member may have: Discord keeps a job for a year, this is a month.
const MAX_KICK_AFTER: Duration = Duration::from_hours(30 * 24);

/// The powers that make a role not one to hand out to whoever presses a button.
const BARRED: Permissions = Permissions::ADMINISTRATOR
    .union(Permissions::MANAGE_GUILD)
    .union(Permissions::MANAGE_ROLES)
    .union(Permissions::MANAGE_CHANNELS)
    .union(Permissions::MANAGE_WEBHOOKS)
    .union(Permissions::BAN_MEMBERS)
    .union(Permissions::KICK_MEMBERS)
    .union(Permissions::MODERATE_MEMBERS)
    .union(Permissions::MANAGE_MESSAGES)
    .union(Permissions::MENTION_EVERYONE);

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "snake_case")]
enum Mode {
    #[default]
    Button,
    /// Answer a sum, so that a script that only presses buttons is not enough.
    Question,
}

#[derive(Debug, Default, Deserialize)]
#[serde(default)]
struct Config {
    role: Option<String>,
    unverified_role: Option<String>,
    mode: Mode,
    /// How long a member has to verify; `None`: for ever.
    kick_after: Option<String>,
}

impl Config {
    fn verified(&self) -> Option<RoleId> {
        role_id(self.role.as_deref())
    }

    fn unverified(&self) -> Option<RoleId> {
        role_id(self.unverified_role.as_deref())
    }

    fn deadline(&self) -> Option<Duration> {
        self.kick_after
            .as_deref()
            .and_then(|text| parse_duration(text).ok())
            .map(|time| time.min(MAX_KICK_AFTER))
    }
}

fn role_id(text: Option<&str>) -> Option<RoleId> {
    text?.parse::<u64>().ok().and_then(RoleId::new_checked)
}

#[derive(Debug, Serialize, Deserialize)]
struct Expire {
    user: u64,
}

fn expire_key(user: UserId) -> String {
    format!("expire:{}", user.get())
}

fn question_key(user: UserId) -> String {
    format!("sum:{}", user.get())
}

fn failed(error: &DiscordError) -> UserError {
    match error {
        DiscordError::Forbidden(_) | DiscordError::HierarchyViolation(_) => {
            UserError::message("verify-cannot")
        }
        _ => UserError::message("verify-unavailable"),
    }
}

/// Whether the role may be handed out by a button: it has no moderator powers, no punishment owns
/// it, and it can be looked up at all.
async fn safe_to_give(ctx: &EventContext, role: RoleId) -> bool {
    let Ok(Outcome::Roles(known)) = ctx
        .api
        .execute(Action::ListRoles {
            guild_id: ctx.guild_id,
        })
        .await
    else {
        return false;
    };
    let powerful = known
        .iter()
        .find(|known| known.id == role)
        .is_none_or(|known| known.permissions.intersects(BARRED));
    !powerful
        && !is_moderator_role(ctx, role)
        && !closure_sdk::punish::is_restriction_role(ctx, role)
            .await
            .unwrap_or(true)
}

/// Whether the server's own settings make the role a moderator's, as its holders' powers in closure
/// do not show in the permissions of Discord.
fn is_moderator_role(ctx: &EventContext, role: RoleId) -> bool {
    holds_moderator_level(ctx, UserId::new(1), &[role])
}

/// Whether holders of these roles reach the moderator level by the server's settings.
fn holds_moderator_level(ctx: &EventContext, user: UserId, roles: &[RoleId]) -> bool {
    let actor = Actor {
        user,
        roles: roles.to_vec(),
        administrator: false,
        owner: false,
    };
    ctx.permissions().level_of(&actor) >= Level::MODERATOR
}

/// Whether one of the roles carries a power that makes its holder one of the staff in Discord's own
/// terms. A role that cannot be looked up counts as one: a member is not kicked on a guess.
async fn holds_powerful_role(ctx: &EventContext, roles: &[RoleId]) -> bool {
    let Ok(Outcome::Roles(known)) = ctx
        .api
        .execute(Action::ListRoles {
            guild_id: ctx.guild_id,
        })
        .await
    else {
        return true;
    };
    roles.iter().any(|held| {
        known
            .iter()
            .find(|role| role.id == *held)
            .is_some_and(|role| role.permissions.intersects(BARRED))
    })
}

/// Makes the member verified: the verified role on, the unverified role off, the expiry gone.
async fn grant<I: InteractionRef + 'static>(cx: &Cx<I>) -> Result<(), UserError> {
    let config: Config = cx.config()?;
    let Some(role) = config.verified() else {
        return Err(UserError::message("verify-not-set"));
    };
    if !safe_to_give(cx.ctx(), role).await {
        return Err(UserError::message("verify-unsafe-role"));
    }
    let (guild, user) = (cx.guild_id(), cx.user().id);
    let reason = || {
        Reason::new(cx.t("verify-reason", &Args::new()))
            .map_err(|_| UserError::message("verify-unavailable"))
    };
    cx.api()
        .add_role(guild, user, role, reason()?)
        .await
        .map_err(|error| failed(&error))?;
    if let Some(unverified) = config.unverified().filter(|unverified| *unverified != role) {
        // A member who never had it, or who lost it, is as good as done.
        let _ = cx
            .api()
            .remove_role(guild, user, unverified, reason()?)
            .await;
    }
    let _ = cx.scheduler().cancel(&expire_key(user)).await;
    cx.store().delete(&question_key(user)).await?;
    cx.reply_private_t("verify-done", &Args::new()).await
}

/// The button: a member who has the role already is told so; the others get it, or a sum first.
async fn press(cx: ComponentCx) -> Result<(), UserError> {
    let config: Config = cx.config()?;
    let Some(role) = config.verified() else {
        return Err(UserError::message("verify-not-set"));
    };
    if cx.click().roles.contains(&role) {
        // A member who is verified but kept the unverified role (taking it off failed once) is
        // set right by the press; it is the only way they have to ask.
        if let Some(unverified) = config.unverified().filter(|unverified| *unverified != role)
            && cx.click().roles.contains(&unverified)
        {
            let reason = Reason::new(cx.t("verify-reason", &Args::new()))
                .map_err(|_| UserError::message("verify-unavailable"))?;
            let _ = cx
                .api()
                .remove_role(cx.guild_id(), cx.user().id, unverified, reason)
                .await;
        }
        return cx.reply_private_t("verify-already", &Args::new()).await;
    }
    if config.mode == Mode::Button {
        return grant(&cx).await;
    }
    let user = cx.user().id;
    // The sum is made from the moment and the member, and kept: the answer is checked against
    // what was asked, never against what the form says.
    let seed = u64::try_from(cx.now()).unwrap_or(0) ^ user.get().rotate_left(17);
    let (left, right) = (seed % 9 + 1, (seed / 9) % 9 + 1);
    cx.store()
        .set_for(&question_key(user), &(left + right), QUESTION_LIFE)
        .await?;
    let form = ui::modal(
        CustomId::new(ANSWER).only_for(user).encode(cx.module()),
        cx.t("verify-modal-title", &Args::new()),
        [text_input(
            cx.module(),
            "answer",
            &cx.t(
                "verify-question",
                &Args::new().with("left", left).with("right", right),
            ),
            false,
            true,
            3,
        )],
    );
    cx.show_modal(form).await
}

/// The form: the sum is asked once; a wrong answer needs a new press, and a new sum.
async fn answer(cx: ModalCx) -> Result<(), UserError> {
    let user = cx.user().id;
    let key = question_key(user);
    let expected = cx.store().get::<u64>(&key).await?;
    // Taken in any case: a sum can be tried once.
    cx.store().delete(&key).await?;
    let given = cx
        .field("answer")
        .and_then(|text| text.trim().parse::<u64>().ok());
    if expected.is_none() || given != expected {
        return Err(UserError::message("verify-wrong"));
    }
    grant(&cx).await
}

/// A join: the unverified role on, and the expiry set if the server wants one.
async fn joined(ctx: EventContext, event: Event) -> Result<Flow, ModuleError> {
    let Event::MemberJoined(member) = event else {
        return Ok(Flow::Continue);
    };
    let config: Config = closure_sdk::parse_module_config(&ctx)?;
    let Some(role) = config.verified() else {
        return Ok(Flow::Continue);
    };
    // A bot is added by hand with a link of its own, not by a gate.
    if member.user.bot || member.roles.contains(&role) {
        return Ok(Flow::Continue);
    }
    if let Some(unverified) = config.unverified().filter(|unverified| *unverified != role) {
        if safe_to_give(&ctx, unverified).await {
            let reason = Reason::new(ctx.t_in(ctx.locale, "verify-reason-join", &Args::new()))?;
            if let Err(error) = ctx
                .api
                .add_role(ctx.guild_id, member.user.id, unverified, reason)
                .await
            {
                tracing::warn!(%error, "could not give the unverified role");
            }
        } else {
            // The time still runs: only the role is not given.
            tracing::warn!("the unverified role is not one to hand out: it is not given");
        }
    }
    if let Some(time) = config.deadline() {
        ctx.scheduler()
            .job(
                EXPIRE,
                &Expire {
                    user: member.user.id.get(),
                },
            )
            .map_err(|error| ModuleError::other(error.to_string()))?
            .key(&expire_key(member.user.id))
            .after(time)
            .await
            .map_err(|error| ModuleError::other(error.to_string()))?;
    }
    Ok(Flow::Continue)
}

/// The time is up: a member who still lacks the role is kicked.
async fn expire(cx: JobCx) -> Result<(), ModuleError> {
    let payload: Expire = cx.payload()?;
    let Some(user) = UserId::new_checked(payload.user) else {
        return Ok(());
    };
    let ctx = cx.ctx();
    let config: Config = closure_sdk::parse_module_config(ctx)?;
    let Some(role) = config.verified() else {
        return Ok(());
    };
    // The server may have turned the time off since the member joined.
    if config.deadline().is_none() {
        return Ok(());
    }
    let member = match ctx.api.get_member(ctx.guild_id, user).await {
        Ok(member) => member,
        // Gone already.
        Err(DiscordError::NotFound) => return Ok(()),
        Err(error) => return Err(ModuleError::Discord(error)),
    };
    if member.roles.contains(&role) {
        return Ok(());
    }
    // A member made a moderator since the join is staff, whatever role the gate asks for.
    if holds_moderator_level(ctx, user, &member.roles)
        || holds_powerful_role(ctx, &member.roles).await
    {
        return Ok(());
    }
    let settings = Settings {
        dm: false,
        ..Settings::default()
    };
    let reason = ctx.t_in(ctx.locale, "verify-kick-reason", &Args::new());
    Punisher::new(ctx, Moderator::bot(ctx, "verify")?, settings)
        .kick(user, Some(reason))
        .await
        .map_err(|error| ModuleError::other(format!("{error:?}")))?;
    Ok(())
}

/// Posts the panel with the button in the channel the command was used in.
async fn panel(cx: closure_sdk::CommandCx, (): ()) -> Result<(), UserError> {
    let config: Config = cx.config()?;
    if config.verified().is_none() {
        return Err(UserError::message("verify-not-set"));
    }
    let custom_id = CustomId::new(PRESS).encode(cx.module());
    cx.reply_with(InteractionReply {
        content: Some(cx.t("verify-panel", &Args::new())),
        components: vec![ui::row([ui::button(
            ButtonStyle::Success,
            cx.t("verify-button", &Args::new()),
            custom_id,
        )])],
        ephemeral: false,
        ..InteractionReply::default()
    })
    .await
}

fn snowflake() -> serde_json::Value {
    json!({ "type": "string", "pattern": "^[0-9]{1,20}$" })
}

/// The module before it is built, so that tests can run it on the SDK's pretend Discord.
pub fn builder() -> Result<ModuleBuilder, CoreError> {
    let schema = json!({
        "type": "object",
        "additionalProperties": false,
        "properties": {
            "role": {
                "anyOf": [snowflake(), { "type": "null" }],
                "description": "config-verify-role",
                "x-ui": { "level": "basic", "widget": "role" }
            },
            "unverified_role": {
                "anyOf": [snowflake(), { "type": "null" }],
                "description": "config-verify-unverified_role",
                "x-ui": { "level": "basic", "widget": "role" }
            },
            "mode": {
                "type": "string",
                "enum": ["button", "question"],
                "description": "config-verify-mode",
                "x-ui": { "level": "basic" }
            },
            "kick_after": {
                "anyOf": [
                    { "type": "string", "pattern": "^([0-9]{1,10}[smhdw] ?)+$" },
                    { "type": "null" }
                ],
                "description": "config-verify-kick_after",
                "x-ui": { "level": "basic" }
            }
        }
    });
    Ok(punish::jobs(
        ModuleBuilder::new("verify", "Verify", env!("CARGO_PKG_VERSION"))?
            .capability(Capability::ManageRoles)
            .capability(Capability::ReadGuild)
            .capability(Capability::RespondToInteractions)
            .capability(Capability::Schedule)
            .capability(Capability::Storage)
            // The kick of a member who did not verify is a case like any other.
            .capability(Capability::Cases)
            .capability(Capability::KickMembers)
            .capability(Capability::ReadBans)
            .capability(Capability::SendMessages)
            .intents(Intents::GUILD_MEMBERS)
            .permission(PermissionSpec::new(
                "verify.panel",
                Level::ADMIN,
                "permission-verify-panel",
            )?)
            .config(
                schema,
                json!({ "role": null, "unverified_role": null, "mode": "button", "kick_after": null }),
            )
            .command(
                Command::<()>::new("verify")
                    .requires("verify.panel")
                    .handler(panel),
            )
            .component(PRESS, press)
            .modal(ANSWER, answer)
            .job(EXPIRE, expire)
            .on(Subscription::new(EventKind::MemberJoined), joined)
            .catalog(Locale::En, EN)
            .catalog(Locale::Ja, JA),
    ))
}

pub fn module() -> Result<SdkModule, CoreError> {
    builder()?.build()
}

#[cfg(test)]
mod tests;
