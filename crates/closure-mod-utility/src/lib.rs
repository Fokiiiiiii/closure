//! Commands that tell about the server, a role, a channel or a member; the names a member has
//! gone by; and finding a member in voice, or moving them.
//!
//! What the bot knows of a name or of voice is only what it has seen since it started to watch:
//! a member nobody saw change has no history, and a member who was already in voice before the bot
//! came has no place until they move.

use std::time::Duration;

use closure_core::action::{Action, InteractionReply, Outcome, Reason};
use closure_core::event::{Event, EventKind, UserInfo};
use closure_core::module::EventContext;
use closure_core::subscription::Flow;
use closure_sdk::{
    Args, ChannelId, Command, CommandArgs, CommandCx, ContextCommand, ContextCx, CoreError,
    DiscordError, EmbedBuilder, Intents, Level, Locale, ModuleBuilder, ModuleError, PermissionSpec,
    RoleId, SdkModule, Subscription, UserError, UserId,
};
use serde::{Deserialize, Serialize};
use twilight_model::guild::Member;

const EN: &str = include_str!("../locales/en.ftl");
const JA: &str = include_str!("../locales/ja.ftl");
/// How long the names and the voice place of a member are kept.
const KEEP: Duration = Duration::from_hours(8760);
/// The most names kept of one member.
const MAX_NAMES: usize = 20;
/// Discord's epoch, in Unix milliseconds.
const DISCORD_EPOCH_MS: u64 = 1_420_070_400_000;

#[derive(CommandArgs)]
struct OneUser {
    /// The member.
    user: UserId,
}

#[derive(CommandArgs)]
struct MaybeUser {
    /// The member; yourself when left out.
    user: Option<UserId>,
}

#[derive(CommandArgs)]
struct OneRole {
    /// The role.
    role: RoleId,
}

#[derive(CommandArgs)]
struct OneChannel {
    /// The channel.
    channel: ChannelId,
}

#[derive(CommandArgs)]
struct MoveArgs {
    /// The member, who has to be in voice.
    user: UserId,
    /// The voice channel to move them to.
    channel: ChannelId,
}

/// A name a member went by.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
struct Seen {
    /// Whether it is a nickname of this server rather than the name of the account.
    nick: bool,
    text: String,
    /// Unix seconds.
    at: i64,
}

fn names_key(user: u64) -> String {
    format!("names:{user}")
}

fn voice_key(user: u64) -> String {
    format!("voice:{user}")
}

fn stored(error: &closure_sdk::StorageError) -> ModuleError {
    ModuleError::other(error.to_string())
}

fn failed(error: &DiscordError) -> UserError {
    match error {
        DiscordError::NotFound => UserError::message("utility-not-found"),
        DiscordError::Forbidden(_) | DiscordError::HierarchyViolation(_) => {
            UserError::message("utility-cannot")
        }
        _ => UserError::message("utility-unavailable"),
    }
}

fn internal(text: &impl ToString) -> UserError {
    UserError::from(ModuleError::other(text.to_string()))
}

/// When a Discord id was made, in Unix seconds.
fn created_at(id: u64) -> i64 {
    i64::try_from(((id >> 22) + DISCORD_EPOCH_MS) / 1000).unwrap_or(0)
}

fn moment(seconds: i64) -> String {
    format!("<t:{seconds}:F> (<t:{seconds}:R>)")
}

fn reply_of(embed: EmbedBuilder) -> Result<InteractionReply, UserError> {
    Ok(InteractionReply {
        content: None,
        embeds: vec![embed.build().map_err(|e| internal(&e))?],
        ephemeral: false,
        components: Vec::new(),
    })
}

fn avatar_url(guild: u64, member: &Member) -> Option<String> {
    let animated = |hash: &twilight_model::util::ImageHash| {
        if hash.is_animated() { "gif" } else { "png" }
    };
    if let Some(hash) = &member.avatar {
        return Some(format!(
            "https://cdn.discordapp.com/guilds/{guild}/users/{}/avatars/{hash}.{}?size=1024",
            member.user.id,
            animated(hash)
        ));
    }
    let hash = member.user.avatar.as_ref()?;
    Some(format!(
        "https://cdn.discordapp.com/avatars/{}/{hash}.{}?size=1024",
        member.user.id,
        animated(hash)
    ))
}

type Say<'a> = &'a (dyn Fn(&str) -> String + Sync);

fn member_card(say: Say<'_>, member: &Member) -> EmbedBuilder {
    let user = &member.user;
    let mut card = EmbedBuilder::new()
        .title(user.name.clone())
        .field(say("utility-id"), user.id.get().to_string(), true)
        .field(
            say("utility-created"),
            moment(created_at(user.id.get())),
            false,
        );
    if let Some(nick) = &member.nick {
        card = card.field(say("utility-nick"), closure_sdk::embed::escape(nick), true);
    }
    if let Some(joined) = member.joined_at {
        card = card.field(say("utility-joined"), moment(joined.as_secs()), false);
    }
    if !member.roles.is_empty() {
        let roles: Vec<String> = member.roles.iter().map(|r| format!("<@&{r}>")).collect();
        card = card.field(
            say("utility-roles"),
            closure_sdk::embed::truncate(&roles.join(" "), 1024),
            false,
        );
    }
    if let Some(until) = member.communication_disabled_until {
        card = card.field(say("utility-timeout"), moment(until.as_secs()), false);
    }
    if user.bot {
        card = card.footer(say("utility-bot"));
    }
    card
}

async fn user_card(
    cx_api: &closure_core::client::ApiClient,
    guild: closure_sdk::GuildId,
    user: UserId,
    say: Say<'_>,
) -> Result<InteractionReply, UserError> {
    let member = cx_api
        .get_member(guild, user)
        .await
        .map_err(|e| failed(&e))?;
    reply_of(member_card(say, &member))
}

async fn userinfo_menu(cx: ContextCx) -> Result<(), UserError> {
    let Some(target) = cx.target_user().map(|u| u.id) else {
        return Err(UserError::message("utility-not-found"));
    };
    let say = |key: &str| cx.t(key, &Args::new());
    let reply = user_card(cx.api(), cx.guild_id(), target, &say).await?;
    cx.reply_with(reply).await
}

async fn avatar(cx: CommandCx, args: MaybeUser) -> Result<(), UserError> {
    let user = args.user.unwrap_or(cx.user().id);
    let guild = cx.guild_id();
    let member = cx
        .api()
        .get_member(guild, user)
        .await
        .map_err(|e| failed(&e))?;
    match avatar_url(guild.get(), &member) {
        Some(url) => cx.reply(url).await,
        None => cx.reply_private_t("utility-no-avatar", &Args::new()).await,
    }
}

async fn serverinfo(cx: CommandCx, (): ()) -> Result<(), UserError> {
    let guild = cx
        .api()
        .get_guild(cx.guild_id())
        .await
        .map_err(|e| failed(&e))?;
    let say = |key: &str| cx.t(key, &Args::new());
    let mut card = EmbedBuilder::new()
        .title(guild.name.clone())
        .field(say("utility-id"), guild.id.get().to_string(), true)
        .field(say("utility-owner"), format!("<@{}>", guild.owner_id), true)
        .field(
            say("utility-created"),
            moment(created_at(guild.id.get())),
            false,
        )
        .field(
            say("utility-roles-count"),
            guild.roles.len().to_string(),
            true,
        )
        .field(say("utility-emojis"), guild.emojis.len().to_string(), true);
    if let Some(members) = guild.member_count.or(guild.approximate_member_count) {
        card = card.field(say("utility-members"), members.to_string(), true);
    }
    if let Some(about) = &guild.description {
        card = card.description(closure_sdk::embed::escape(about));
    }
    cx.reply_with(reply_of(card)?).await
}

async fn roleinfo(cx: CommandCx, args: OneRole) -> Result<(), UserError> {
    let Outcome::Roles(roles) = cx
        .api()
        .execute(Action::ListRoles {
            guild_id: cx.guild_id(),
        })
        .await
        .map_err(|e| failed(&e))?
    else {
        return Err(UserError::message("utility-unavailable"));
    };
    let role = roles
        .iter()
        .find(|r| r.id.get() == args.role.get())
        .ok_or_else(|| UserError::message("utility-not-found"))?;
    let say = |key: &str| cx.t(key, &Args::new());
    let yes_no = |on: bool| say(if on { "utility-yes" } else { "utility-no" });
    let color = if role.colors.primary_color == 0 {
        say("utility-none")
    } else {
        format!("#{:06X}", role.colors.primary_color)
    };
    let card = EmbedBuilder::new()
        .title(role.name.clone())
        .field(say("utility-id"), role.id.get().to_string(), true)
        .field(say("utility-color"), color, true)
        .field(say("utility-position"), role.position.to_string(), true)
        .field(say("utility-hoist"), yes_no(role.hoist), true)
        .field(say("utility-mentionable"), yes_no(role.mentionable), true)
        .field(say("utility-managed"), yes_no(role.managed), true)
        .field(
            say("utility-created"),
            moment(created_at(role.id.get())),
            false,
        );
    cx.reply_with(reply_of(card)?).await
}

async fn channelinfo(cx: CommandCx, args: OneChannel) -> Result<(), UserError> {
    let Outcome::Channel(channel) = cx
        .api()
        .execute(Action::GetChannel {
            channel_id: args.channel,
        })
        .await
        .map_err(|e| failed(&e))?
    else {
        return Err(UserError::message("utility-unavailable"));
    };
    let say = |key: &str| cx.t(key, &Args::new());
    let mut card = EmbedBuilder::new()
        .title(channel.name.clone().unwrap_or_default())
        .field(say("utility-id"), channel.id.get().to_string(), true)
        .field(say("utility-kind"), format!("{:?}", channel.kind), true)
        .field(
            say("utility-created"),
            moment(created_at(channel.id.get())),
            false,
        );
    if let Some(parent) = channel.parent_id {
        card = card.field(say("utility-parent"), format!("<#{parent}>"), true);
    }
    if channel.nsfw == Some(true) {
        card = card.footer("NSFW");
    }
    if let Some(topic) = channel.topic.as_deref().filter(|t| !t.is_empty()) {
        card = card.description(closure_sdk::embed::escape(topic));
    }
    cx.reply_with(reply_of(card)?).await
}

async fn names(cx: CommandCx, args: OneUser) -> Result<(), UserError> {
    let seen = cx
        .store()
        .get::<Vec<Seen>>(&names_key(args.user.get()))
        .await
        .map_err(|e| internal(&e))?
        .unwrap_or_default();
    if seen.is_empty() {
        return cx.reply_private_t("utility-names-none", &Args::new()).await;
    }
    let lines: Vec<String> = seen
        .iter()
        .rev()
        .map(|s| {
            let kind = cx.t(
                if s.nick {
                    "utility-nick"
                } else {
                    "utility-account"
                },
                &Args::new(),
            );
            format!(
                "{kind}: {} (<t:{}:R>)",
                closure_sdk::embed::escape(&s.text),
                s.at
            )
        })
        .collect();
    cx.reply_private(closure_sdk::embed::truncate(&lines.join("\n"), 1900))
        .await
}

async fn locate(cx: CommandCx, args: OneUser) -> Result<(), UserError> {
    let place = cx
        .store()
        .get::<u64>(&voice_key(args.user.get()))
        .await
        .map_err(|e| internal(&e))?;
    let who = format!("<@{}>", args.user);
    match place {
        Some(channel) => {
            cx.reply_private_t(
                "utility-locate-in",
                &Args::new()
                    .with("user", who)
                    .with("channel", format!("<#{channel}>")),
            )
            .await
        }
        None => {
            cx.reply_private_t("utility-locate-out", &Args::new().with("user", who))
                .await
        }
    }
}

async fn vcmove(cx: CommandCx, args: MoveArgs) -> Result<(), UserError> {
    let reason = Reason::new(cx.t("utility-reason-move", &Args::new())).map_err(|e| failed(&e))?;
    cx.api()
        .move_member(cx.guild_id(), args.user, args.channel, reason)
        .await
        .map_err(|e| failed(&e))?;
    cx.reply_private_t(
        "utility-moved",
        &Args::new()
            .with("user", format!("<@{}>", args.user))
            .with("channel", format!("<#{}>", args.channel)),
    )
    .await
}

async fn vckick(cx: CommandCx, args: OneUser) -> Result<(), UserError> {
    let reason = Reason::new(cx.t("utility-reason-kick", &Args::new())).map_err(|e| failed(&e))?;
    cx.api()
        .disconnect_member(cx.guild_id(), args.user, reason)
        .await
        .map_err(|e| failed(&e))?;
    cx.reply_private_t(
        "utility-disconnected",
        &Args::new().with("user", format!("<@{}>", args.user)),
    )
    .await
}

/// Writes down the account name and the nickname of a member when they differ from the last ones.
async fn remember(
    ctx: &EventContext,
    user: &UserInfo,
    nick: Option<&str>,
) -> Result<(), ModuleError> {
    let key = names_key(user.id.get());
    let mut seen = ctx
        .store()
        .get::<Vec<Seen>>(&key)
        .await
        .map_err(|e| stored(&e))?
        .unwrap_or_default();
    let last = |seen: &[Seen], nick: bool| seen.iter().rev().find(|s| s.nick == nick).cloned();
    let now = ctx.now();
    let before = seen.len();
    for (is_nick, text) in [(false, Some(user.name.as_str())), (true, nick)] {
        if let Some(text) = text
            && last(&seen, is_nick).is_none_or(|s| s.text != text)
        {
            seen.push(Seen {
                nick: is_nick,
                text: text.to_owned(),
                at: now,
            });
        }
    }
    if seen.len() == before {
        return Ok(());
    }
    let excess = seen.len().saturating_sub(MAX_NAMES);
    seen.drain(..excess);
    ctx.store()
        .set_for(&key, &seen, KEEP)
        .await
        .map_err(|e| stored(&e))
}

async fn on_member(ctx: EventContext, event: Event) -> Result<Flow, ModuleError> {
    if let Event::MemberJoined(member) | Event::MemberUpdated(member) = &event
        && !member.user.bot
    {
        remember(&ctx, &member.user, member.nick.as_deref()).await?;
    }
    Ok(Flow::Continue)
}

async fn on_voice(ctx: EventContext, event: Event) -> Result<Flow, ModuleError> {
    if let Event::VoiceStateChanged { user, after, .. } = &event {
        let key = voice_key(user.id.get());
        if let Some(channel) = after {
            ctx.store()
                .set_for(&key, &channel.get(), KEEP)
                .await
                .map_err(|e| stored(&e))?;
        } else {
            ctx.store().delete(&key).await.map_err(|e| stored(&e))?;
        }
    }
    Ok(Flow::Continue)
}

#[allow(clippy::too_many_lines)]
pub fn builder() -> Result<ModuleBuilder, CoreError> {
    Ok(
        ModuleBuilder::new("utility", "Utility", env!("CARGO_PKG_VERSION"))?
            .capability(closure_sdk::Capability::ReadGuild)
            .capability(closure_sdk::Capability::MoveMembers)
            .capability(closure_sdk::Capability::Storage)
            .capability(closure_sdk::Capability::RespondToInteractions)
            .intents(Intents::GUILD_MEMBERS | Intents::GUILD_VOICE_STATES)
            .permission(PermissionSpec::new(
                "utility.info",
                Level::GENERAL,
                "permission-utility-info",
            )?)
            .permission(PermissionSpec::new(
                "utility.voice",
                Level::MODERATOR,
                "permission-utility-voice",
            )?)
            .command(
                Command::<()>::new("serverinfo")
                    .requires("utility.info")
                    .handler(serverinfo),
            )
            .command(
                Command::<OneRole>::new("roleinfo")
                    .requires("utility.info")
                    .handler(roleinfo),
            )
            .command(
                Command::<OneChannel>::new("channelinfo")
                    .requires("utility.info")
                    .handler(channelinfo),
            )
            .command(
                Command::<MaybeUser>::new("avatar")
                    .requires("utility.info")
                    .handler(avatar),
            )
            .command(
                Command::<OneUser>::new("names")
                    .requires("utility.info")
                    .handler(names),
            )
            .command(
                Command::<OneUser>::new("locate")
                    .requires("utility.info")
                    .handler(locate),
            )
            .command(
                Command::<MoveArgs>::new("vcmove")
                    .requires("utility.voice")
                    .handler(vcmove),
            )
            .command(
                Command::<OneUser>::new("vckick")
                    .requires("utility.voice")
                    .handler(vckick),
            )
            .context(
                ContextCommand::user("Info")
                    .requires("utility.info")
                    .handler(userinfo_menu),
            )
            .on(Subscription::new(EventKind::MemberJoined), on_member)
            .on(Subscription::new(EventKind::MemberUpdated), on_member)
            .on(Subscription::new(EventKind::VoiceStateChanged), on_voice)
            .catalog(Locale::En, EN)
            .catalog(Locale::Ja, JA)
            .enabled_by_default(),
    )
}

pub fn module() -> Result<SdkModule, CoreError> {
    builder()?.build()
}

#[cfg(test)]
mod tests;
