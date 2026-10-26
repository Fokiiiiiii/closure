//! Templates: the reason and the duration a moderator gets when a punishment command leaves them
//! out. The command then asks the moderator to pick one, and the pick carries the punishment out.
//!
//! The settings name the templates of each punishment. A template that cannot be used is skipped
//! rather than refusing the whole configuration: one whose duration does not read, a timeout
//! without a duration or with one Discord refuses, and a template whose id an earlier template of
//! the same punishment already has.

use std::collections::HashSet;
use std::time::Duration;

use closure_sdk::duration::{TimeStyle, parse as parse_duration, timestamp};
use closure_sdk::embed::truncate;
use closure_sdk::punish::Method;
use closure_sdk::ui::{self, CustomId};
use closure_sdk::{
    Args, Color, CommandCx, ComponentCx, Cx, EmbedBuilder, InteractionRef, InteractionReply,
    ModuleError, Reason, Span, Tone, UserError, UserId,
};
use serde::{Deserialize, Serialize};
use serde_json::json;
use twilight_model::channel::message::Component;

use crate::{
    Config, case_and_notes, delete_seconds, history, mention, punisher_for, timeout_seconds, undo,
    with_notes,
};

/// The route of the component that the picker's select menu sends its pick to.
pub(crate) const ROUTE: &str = "tpl";
/// How long a picker waits for its pick before the punishment it waits for lapses.
const PENDING_FOR: Duration = Duration::from_secs(600);
/// How many options a select menu holds.
const MAX_OPTIONS: usize = 25;
/// How many characters the description of an option shows.
const DESCRIPTION_CHARS: usize = 100;

/// The templates of each punishment, as the `templates` setting names them.
#[derive(Debug, Default, Deserialize)]
#[serde(deny_unknown_fields, default)]
pub(crate) struct Templates {
    timeout: Vec<Template>,
    kick: Vec<Template>,
    ban: Vec<Template>,
    vcmute: Vec<Template>,
}

/// One template of a punishment.
#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct Template {
    id: String,
    label: String,
    reason: String,
    /// How long: a timeout needs it, a ban or a voice mute may have it, a kick ignores it.
    duration: Option<String>,
}

/// The punishments a template can stand for.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub(crate) enum Kind {
    Timeout,
    Kick,
    Ban,
    Vcmute,
}

impl Kind {
    fn templates(self, templates: &Templates) -> &[Template] {
        match self {
            Self::Timeout => &templates.timeout,
            Self::Kick => &templates.kick,
            Self::Ban => &templates.ban,
            Self::Vcmute => &templates.vcmute,
        }
    }

    /// The permission that the command checks, and that the pick checks again.
    fn permission(self) -> &'static str {
        match self {
            Self::Timeout => "moderation.timeout",
            Self::Kick => "moderation.kick",
            Self::Ban => "moderation.ban",
            Self::Vcmute => "moderation.voice",
        }
    }

    fn pick_key(self) -> &'static str {
        match self {
            Self::Timeout => "template-pick-timeout",
            Self::Kick => "template-pick-kick",
            Self::Ban => "template-pick-ban",
            Self::Vcmute => "template-pick-vcmute",
        }
    }

    /// The undo button of the punishment, if it has something to undo.
    fn undo(self) -> Option<undo::Kind> {
        match self {
            Self::Timeout => Some(undo::Kind::Timeout),
            Self::Kick => None,
            Self::Ban => Some(undo::Kind::Ban),
            Self::Vcmute => Some(undo::Kind::Vcmute),
        }
    }

    /// The key of the reply to the punishment; `timed` tells whether it has a duration.
    fn reply_key(self, timed: bool) -> &'static str {
        match (self, timed) {
            (Self::Timeout, _) => "timed-out",
            (Self::Kick, _) => "kicked",
            (Self::Ban, true) => "banned-for",
            (Self::Ban, false) => "banned",
            (Self::Vcmute, true) => "voice-muted-for",
            (Self::Vcmute, false) => "voice-muted",
        }
    }
}

/// A punishment as far as it is known: what a command gave, or what a picked template filled in.
/// While a moderator picks a template, it waits in the store under [`pending_key`]. The duration
/// stays as typed, so a moment is resolved when the punishment runs, not when it waits.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub(crate) struct Punishment {
    kind: Kind,
    user: u64,
    reason: Option<String>,
    duration: Option<Span>,
    delete_days: Option<i64>,
}

impl Punishment {
    pub(crate) fn new(
        kind: Kind,
        user: UserId,
        reason: Option<String>,
        duration: Option<Span>,
        delete_days: Option<i64>,
    ) -> Self {
        Self {
            kind,
            user: user.get(),
            reason,
            duration,
            delete_days,
        }
    }

    /// The duration as it is now, or `None` when the punishment has none.
    fn duration(
        &self,
        cx: &Cx<impl InteractionRef + 'static>,
    ) -> Result<Option<Duration>, UserError> {
        self.duration
            .as_ref()
            .map(|span| span.resolve(cx))
            .transpose()
    }

    fn member(&self) -> Result<UserId, UserError> {
        UserId::new_checked(self.user)
            .ok_or_else(|| UserError::Module(ModuleError::other("a punishment names no member")))
    }

    /// The punishment with what the template gives: its reason where there is none, its duration
    /// where there is none.
    fn filled(&self, template: &Usable) -> Self {
        Self {
            kind: self.kind,
            user: self.user,
            reason: self.reason.clone().or(Some(template.reason.clone())),
            duration: self
                .duration
                .clone()
                .or(template.duration.map(Span::from_duration)),
            delete_days: self.delete_days,
        }
    }
}

/// A template a moderator may pick.
#[derive(Debug)]
struct Usable {
    id: String,
    label: String,
    reason: String,
    duration: Option<Duration>,
}

/// The templates of `kind` that a moderator may pick, in the order of the settings. A template is
/// left out when its duration does not read, when a timeout has none or one Discord refuses, or
/// when an earlier template of the same punishment has its id.
fn usable(config: &Config, kind: Kind) -> Vec<Usable> {
    let mut seen = HashSet::new();
    kind.templates(&config.templates)
        .iter()
        .filter(|template| seen.insert(template.id.as_str()))
        .filter_map(|template| {
            let duration = template
                .duration
                .as_deref()
                .map(parse_duration)
                .transpose()
                .ok()?;
            let fits = match (kind, duration) {
                (Kind::Timeout, None) => false,
                (Kind::Timeout, Some(duration)) => {
                    config.timeout_method == Method::Role || timeout_seconds(duration).is_ok()
                }
                _ => true,
            };
            fits.then(|| Usable {
                id: template.id.clone(),
                label: template.label.clone(),
                reason: template.reason.clone(),
                // A kick takes no duration, so a duration on one is not used.
                duration: duration.filter(|_| kind != Kind::Kick),
            })
        })
        .take(MAX_OPTIONS)
        .collect()
}

fn pending_key(token: &str) -> String {
    format!("pending:{token}")
}

/// A reply that shows `text` as the one embed of the Info tone, as the replies of this module do.
pub(crate) fn text_reply(text: String) -> Result<InteractionReply, UserError> {
    embed_reply(Color::Info, text)
}

/// The reply of an action that went through: the Success tone, with the guild's emoji in front.
pub(crate) fn success_reply<I: InteractionRef + 'static>(
    cx: &Cx<I>,
    text: String,
) -> Result<InteractionReply, UserError> {
    embed_reply(Color::Success, cx.tone_text(Tone::Success, text))
}

fn embed_reply(color: Color, text: String) -> Result<InteractionReply, UserError> {
    let embed = EmbedBuilder::new()
        .description(text)
        .color(color)
        .build()
        .map_err(|error| ModuleError::other(error.to_string()))?;
    Ok(InteractionReply {
        embeds: vec![embed],
        ..InteractionReply::default()
    })
}

/// What an option shows under its label: the reason, cut to fit, then the duration in brackets.
fn describe<I: InteractionRef + 'static>(cx: &Cx<I>, template: &Usable) -> String {
    let suffix = template.duration.map_or_else(String::new, |duration| {
        format!(" ({})", cx.format_duration(duration))
    });
    let room = DESCRIPTION_CHARS.saturating_sub(suffix.chars().count());
    format!("{}{suffix}", truncate(&template.reason, room))
}

/// Asks the moderator for a template when the command leaves out the reason, or a timeout's duration,
/// and a usable template can give it. The punishment waits in the store until the pick. Without a
/// template it runs as it is: without a reason, unless the server requires one.
async fn offer(
    cx: &CommandCx,
    config: &Config,
    punishment: Punishment,
) -> Result<Option<Punishment>, UserError> {
    let needs_duration = punishment.kind == Kind::Timeout && punishment.duration.is_none();
    if punishment.reason.is_some() && !needs_duration {
        return Ok(Some(punishment));
    }
    let templates = usable(config, punishment.kind);
    if templates.is_empty() {
        if needs_duration {
            return Err(UserError::message("duration-required"));
        }
        if config.require_reason {
            return Err(UserError::message("reason-required"));
        }
        return Ok(Some(punishment));
    }
    // The command's interaction is unique to it, so it names the waiting punishment.
    let token = cx.invocation().interaction_id.get().to_string();
    cx.store()
        .set_for(&pending_key(&token), &punishment, PENDING_FOR)
        .await?;

    let options = templates.iter().map(|template| {
        (
            template.label.clone(),
            template.id.clone(),
            Some(describe(cx, template)),
        )
    });
    let placeholder = cx.t("template-placeholder", &Args::new());
    let menu = ui::select_described(
        CustomId::new(ROUTE)
            .only_for(cx.user().id)
            .data(token)
            .encode(cx.module()),
        Some(placeholder.as_str()),
        options,
        1,
        1,
    );
    let text = cx.t(
        punishment.kind.pick_key(),
        &Args::new().with("user", mention(punishment.member()?)),
    );
    cx.reply_with(InteractionReply {
        components: vec![ui::row([menu])],
        ephemeral: true,
        ..text_reply(text)?
    })
    .await?;
    Ok(None)
}

/// Runs a punishment that a command asked for: a template is picked first when the command left
/// something out. The reply is private, as the replies of the commands are.
pub(crate) async fn punish(
    cx: &CommandCx,
    config: &Config,
    punishment: Punishment,
) -> Result<(), UserError> {
    let Some(punishment) = offer(cx, config, punishment).await? else {
        return Ok(());
    };
    let done = carry_out(cx, config, punishment).await?;
    cx.reply_with(InteractionReply {
        components: undo::row(done.undo),
        ephemeral: true,
        ..success_reply(cx, done.text)?
    })
    .await
}

/// A moderator picked a template. The punishment waiting for it runs, with the template's reason
/// and duration where the command left them out. The pick takes the waiting punishment, so a second
/// pick, or one after the time is up, finds nothing and is told so.
pub(crate) async fn pick(cx: ComponentCx) -> Result<(), UserError> {
    let key = pending_key(&cx.data());
    let Some(punishment) = cx.store().get::<Punishment>(&key).await? else {
        return Err(UserError::message("template-expired"));
    };
    cx.require(punishment.kind.permission()).await?;
    cx.check_target(punishment.member()?).await?;
    // The store deletes an entry once, so of two picks only one gets the punishment.
    if !cx.store().delete(&key).await? {
        return Err(UserError::message("template-expired"));
    }

    let config: Config = cx.config()?;
    let chosen = cx.values().first().map_or("", String::as_str);
    let template = usable(&config, punishment.kind)
        .into_iter()
        .find(|template| template.id == chosen)
        .ok_or_else(|| UserError::message("template-gone"))?;
    // The punishment calls Discord; the picker is replaced once it is done.
    cx.defer_update().await?;
    // A reason the command gave wins, so the template's is filled in only when it is used.
    let template = if punishment.reason.is_none() {
        filled_reason(&cx, punishment.member()?, template).await
    } else {
        template
    };
    let done = carry_out(&cx, &config, punishment.filled(&template)).await?;
    let mut reply = success_reply(&cx, done.text)?;
    reply.components = undo::row(done.undo);
    cx.update(reply).await
}

/// The template's reason with `{user}`, `{moderator}` and `{server}` filled in. The server's name is
/// asked for only when the reason names it; when Discord cannot say, the words for the server stand
/// in for it.
async fn filled_reason<I: InteractionRef + 'static>(
    cx: &Cx<I>,
    member: UserId,
    template: Usable,
) -> Usable {
    let mut values = vec![
        ("{user}", mention(member)),
        ("{moderator}", cx.user().name.clone()),
    ];
    if template.reason.contains("{server}") {
        let server = match cx.api().get_guild(cx.guild_id()).await {
            Ok(guild) => guild.name,
            Err(_) => cx.t("this-server", &Args::new()),
        };
        values.push(("{server}", server));
    }
    let reason = expand(&template.reason, &values);
    Usable {
        reason: truncate(&reason, Reason::MAX_CHARS),
        ..template
    }
}

/// Replaces each token of `values` in `text` with its value. The text is read once, so a value is
/// never expanded again, and a `{word}` that is no token stays as it is.
fn expand(text: &str, values: &[(&str, String)]) -> String {
    let mut out = String::with_capacity(text.len());
    let mut rest = text;
    while let Some(start) = rest.find('{') {
        out.push_str(&rest[..start]);
        let tail = &rest[start..];
        if let Some((token, value)) = values.iter().find(|(token, _)| tail.starts_with(*token)) {
            out.push_str(value);
            rest = &tail[token.len()..];
        } else {
            out.push('{');
            rest = &tail[1..];
        }
    }
    out.push_str(rest);
    out
}

/// A punishment's reply as text, and the undo button that lifts the punishment again.
pub(crate) struct Carried {
    pub(crate) text: String,
    pub(crate) undo: Option<Component>,
}

/// Carries out a punishment and returns the text of its reply. A command and a picked template
/// both answer with this text.
pub(crate) async fn carry_out<I: InteractionRef + 'static>(
    cx: &Cx<I>,
    config: &Config,
    punishment: Punishment,
) -> Result<Carried, UserError> {
    let user = punishment.member()?;
    let duration = punishment.duration(cx)?;
    // Counted before the action, so that the case of the action is left out of the history line.
    let before = history::before(cx, user).await;
    let punisher = punisher_for(cx, config);
    let done = match punishment.kind {
        Kind::Timeout => {
            let duration = duration.ok_or_else(|| UserError::message("duration-required"))?;
            punisher.timeout(user, duration, punishment.reason).await?
        }
        Kind::Kick => punisher.kick(user, punishment.reason).await?,
        Kind::Ban => {
            let days = punishment
                .delete_days
                .unwrap_or(i64::from(config.ban_delete_days));
            let delete = delete_seconds(days)?;
            punisher
                .ban(user, duration, delete, punishment.reason)
                .await?
        }
        Kind::Vcmute => {
            punisher
                .voice_mute(user, duration, punishment.reason)
                .await?
        }
    };
    let (case, notes) = case_and_notes(&punisher, done);
    let mut words = Args::new()
        .with("user", mention(user))
        .with("number", case.number);
    if let Some(duration) = duration {
        let mut shown = cx.format_duration(duration);
        // A moment given as a date shows its end too, as a Discord timestamp.
        if punishment.duration.as_ref().is_some_and(Span::is_moment) {
            let seconds = i64::try_from(duration.as_secs()).unwrap_or(i64::MAX);
            let end = timestamp(cx.now().saturating_add(seconds), TimeStyle::LongDateTime);
            shown = format!("{shown} ({end})");
        }
        words = words.with("duration", shown);
    }
    let mut text = with_notes(
        cx.t(punishment.kind.reply_key(duration.is_some()), &words),
        &notes,
    );
    if let Some(before) = before {
        text.push('\n');
        text.push_str(&history::line(cx, before));
    }
    Ok(Carried {
        text,
        undo: punishment
            .kind
            .undo()
            .map(|kind| undo::button(cx, kind, user, case.number)),
    })
}

/// The settings of the templates.
pub(crate) fn schema() -> serde_json::Value {
    json!({
        "type": "object",
        "description": "Templates a moderator picks from when a command leaves out the reason or the duration.",
        "properties": {
            "timeout": list("Templates for /timeout. Each needs a duration."),
            "kick": list("Templates for /kick. A duration is not used."),
            "ban": list("Templates for /ban."),
            "vcmute": list("Templates for /vcmute.")
        },
        "additionalProperties": false
    })
}

/// The schema of one list of templates.
fn list(description: &str) -> serde_json::Value {
    json!({
        "type": "array",
        "maxItems": 25,
        "description": description,
        "items": {
            "type": "object",
            "properties": {
                "id": {
                    "type": "string",
                    "pattern": "^[a-z0-9_-]{1,32}$",
                    "description": "The name of the template; unique within its list."
                },
                "label": {
                    "type": "string",
                    "minLength": 1,
                    "maxLength": 100,
                    "description": "The name the moderator sees."
                },
                "reason": {
                    "type": "string",
                    "minLength": 1,
                    "maxLength": 512,
                    "description": "The reason the punishment gets. {user} stands for the member, {moderator} for the moderator and {server} for the server."
                },
                "duration": {
                    "type": "string",
                    "pattern": "^([0-9]{1,10}[smhdw] ?)+$",
                    "description": "How long, such as 1h or 7d."
                }
            },
            "required": ["id", "label", "reason"],
            "additionalProperties": false
        }
    })
}
