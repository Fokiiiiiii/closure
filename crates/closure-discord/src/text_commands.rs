//! Text commands: a message that starts with the guild's prefix runs the slash command of the same
//! name, through the same handler and the same checks. The words after the command word fill the
//! options by their type.

use std::sync::Arc;
use std::time::Duration;

use async_trait::async_trait;
use closure_core::action::{Action, InteractionResponse, MentionPolicy, Outcome, Reason};
use closure_core::client::DiscordApi;
use closure_core::error::DiscordError;
use closure_core::event::{
    ApplicationId, AttachmentInfo, ChannelId, CommandInvocation, CommandOption, CommandOptionValue,
    GuildId, InteractionToken, Mentionable, MessageId, MessageInfo, UserId,
};
use closure_core::interaction::InteractionGuilds;
use closure_core::locale::{Args, Locale, Translator};
use closure_core::module::{ChoiceValue, CommandSpec, OptionKind, OptionSpec, SubcommandSpec};
use closure_core::settings::SettingsProvider;
use closure_i18n::CORE_NAMESPACE;
use tokio::sync::{OwnedSemaphorePermit, Semaphore};
use twilight_cache_inmemory::InMemoryCache;
use twilight_model::channel::permission_overwrite::PermissionOverwrite;
use twilight_model::guild::Permissions;
use twilight_model::id::Id;
use twilight_model::id::marker::RoleMarker;
use twilight_util::permission_calculator::PermissionCalculator;

use crate::notice;

/// What a message means as a text command.
#[derive(Debug, PartialEq)]
pub(crate) enum Reading {
    /// Not a command this guild can run: plain chat.
    Ignore,
    /// Run the command, as the slash command with these options would be.
    Run(CommandInvocation),
    /// The words do not fit the command: answer with its usage line and run nothing.
    Usage {
        invocation: CommandInvocation,
        text: String,
    },
}

/// Everything a message is read against: the guild's prefix and language, and the commands of the
/// modules enabled in the guild.
pub(crate) struct Reader<'a> {
    pub prefix: &'a str,
    pub application: ApplicationId,
    pub locale: Locale,
    pub commands: &'a [&'a CommandSpec],
    pub translator: &'a dyn Translator,
}

impl Reader<'_> {
    /// `permissions` is asked for only when a command runs, since computing them takes the cache.
    pub(crate) fn read(
        &self,
        message: &MessageInfo,
        permissions: impl FnOnce(&MessageInfo) -> Permissions,
    ) -> Reading {
        if self.prefix.is_empty() || message.author.bot || message.from_webhook {
            return Reading::Ignore;
        }
        let Some(line) = message.content.strip_prefix(self.prefix) else {
            return Reading::Ignore;
        };
        let words = words(line);
        let Some((first, rest)) = words.split_first() else {
            return Reading::Ignore;
        };
        let Some(spec) = self
            .commands
            .iter()
            .copied()
            .find(|spec| spec.name.eq_ignore_ascii_case(&first.text))
        else {
            return Reading::Ignore;
        };

        let target = match resolve(spec, rest) {
            Ok(target) => target,
            Err(path) => return self.usage(message, spec, format!("{}{path}", self.prefix)),
        };
        let Some(options) = Self::fill(message, target.options, &rest[target.consumed..], line)
        else {
            let usage = usage_line(self.prefix, &target.path, target.options);
            return self.usage(message, spec, usage);
        };
        Reading::Run(self.invocation(
            message,
            &spec.name,
            target.subcommand,
            options,
            permissions(message),
        ))
    }

    /// Reads the options from `words`, or `None` when they do not fit.
    /// `line` is the text after the prefix: the words index into it.
    fn fill(
        message: &MessageInfo,
        options: &[OptionSpec],
        words: &[Word],
        line: &str,
    ) -> Option<Vec<CommandOption>> {
        let mut values = vec![None; options.len()];
        let mut files = message.attachments.iter();
        for (index, option) in options.iter().enumerate() {
            if option.kind == OptionKind::Attachment {
                match files.next() {
                    Some(file) => values[index] = Some(attachment(option, file)),
                    None if option.required => return None,
                    None => {}
                }
            }
        }
        let free = options.iter().enumerate().find_map(|(i, option)| {
            (option.kind == OptionKind::String
                && (option.name == "reason"
                    || (i + 1 == options.len() && option.name != "duration")))
                .then_some(i)
        });
        for word in words {
            let found = options.iter().enumerate().find_map(|(i, option)| {
                if values[i].is_some()
                    || (Some(i) == free && option.name != "duration")
                    || option.kind == OptionKind::Attachment
                    || (word.quoted && (Some(i) != free || option.name == "duration"))
                {
                    return None;
                }
                if option.kind == OptionKind::String
                    && option.name == "duration"
                    && closure_sdk::Span::parse(&word.text).is_err()
                    && !(word.quoted && values[..i].iter().all(Option::is_some))
                {
                    return None;
                }
                convert(option, &word.text).map(|value| (i, value))
            });
            if let Some((i, value)) = found {
                values[i] = Some(value);
                continue;
            }
            let i = free.filter(|&i| values[i].is_none())?;
            let raw = if word.quoted {
                &word.text
            } else {
                line[word.start..].trim_end()
            };
            values[i] = Some(convert(&options[i], raw)?);
            if !word.quoted {
                break;
            }
        }
        if options
            .iter()
            .zip(&values)
            .any(|(option, value)| option.required && value.is_none())
        {
            return None;
        }
        Some(values.into_iter().flatten().collect())
    }

    fn usage(&self, message: &MessageInfo, spec: &CommandSpec, usage: String) -> Reading {
        let text = self.translator.translate(
            self.locale,
            CORE_NAMESPACE,
            "text-command-usage",
            &Args::new().with("usage", usage),
        );
        Reading::Usage {
            invocation: self.invocation(
                message,
                &spec.name,
                None,
                Vec::new(),
                Permissions::empty(),
            ),
            text,
        }
    }

    /// The slash command this text command stands for. Its token is the message itself, so a
    /// reply goes to the message that asked (see [`InteractionToken::for_text_command`]).
    fn invocation(
        &self,
        message: &MessageInfo,
        command: &str,
        subcommand: Option<String>,
        options: Vec<CommandOption>,
        invoker_permissions: Permissions,
    ) -> CommandInvocation {
        CommandInvocation {
            interaction_id: Id::new(message.id.get()),
            application_id: self.application,
            token: InteractionToken::for_text_command(message.channel_id, message.id),
            guild_id: message.guild_id,
            channel_id: message.channel_id,
            user: message.author.clone(),
            roles: message.author_roles.clone(),
            invoker_permissions,
            // The guild's language applies, as it does for a slash command without one.
            locale: None,
            command: command.to_owned(),
            subcommand,
            options,
        }
    }
}

/// A word after the prefix: its text with quotes and escapes taken out, whether any part of it
/// was quoted, and where it starts in the line.
#[derive(Debug, PartialEq, Eq)]
struct Word {
    text: String,
    quoted: bool,
    start: usize,
}

/// Splits `line` as a shell would: whitespace separates words, `"..."` keeps its spaces, and a
/// backslash takes the next character literally. An unclosed quote runs to the end of the line.
fn words(line: &str) -> Vec<Word> {
    let mut words = Vec::new();
    let mut chars = line.char_indices().peekable();
    loop {
        while chars.next_if(|(_, c)| c.is_whitespace()).is_some() {}
        let Some(&(start, _)) = chars.peek() else {
            return words;
        };
        let mut word = Word {
            text: String::new(),
            quoted: false,
            start,
        };
        let mut quoting = false;
        while let Some((_, c)) = chars.next_if(|(_, c)| quoting || !c.is_whitespace()) {
            match c {
                '"' => {
                    quoting = !quoting;
                    word.quoted = true;
                }
                '\\' => {
                    if let Some((_, escaped)) = chars.next() {
                        word.text.push(escaped);
                    }
                }
                _ => word.text.push(c),
            }
        }
        words.push(word);
    }
}

/// Where the words lead: the options to fill, and the path of the subcommand that has them.
struct Target<'s> {
    /// The command and subcommands, as the usage line shows them: `auto rule add`.
    path: String,
    /// `sub` or `group sub`, as the invocation names it.
    subcommand: Option<String>,
    options: &'s [OptionSpec],
    /// How many words name the subcommand.
    consumed: usize,
}

/// Picks the subcommand the first words name. `Err` carries the usage of the level that did not
/// match, such as `auto <status|rule>`.
fn resolve<'s>(spec: &'s CommandSpec, words: &[Word]) -> Result<Target<'s>, String> {
    if spec.subcommands.is_empty() {
        return Ok(Target {
            path: spec.name.clone(),
            subcommand: None,
            options: &spec.options,
            consumed: 0,
        });
    }
    let Some(sub) = find_subcommand(&spec.subcommands, words.first()) else {
        return Err(format!("{} <{}>", spec.name, names(&spec.subcommands)));
    };
    if sub.subcommands.is_empty() {
        return Ok(Target {
            path: format!("{} {}", spec.name, sub.name),
            subcommand: Some(sub.name.clone()),
            options: &sub.options,
            consumed: 1,
        });
    }
    let Some(leaf) = find_subcommand(&sub.subcommands, words.get(1)) else {
        return Err(format!(
            "{} {} <{}>",
            spec.name,
            sub.name,
            names(&sub.subcommands)
        ));
    };
    Ok(Target {
        path: format!("{} {} {}", spec.name, sub.name, leaf.name),
        subcommand: Some(format!("{} {}", sub.name, leaf.name)),
        options: &leaf.options,
        consumed: 2,
    })
}

fn find_subcommand<'s>(
    subcommands: &'s [SubcommandSpec],
    word: Option<&Word>,
) -> Option<&'s SubcommandSpec> {
    let word = word?;
    subcommands
        .iter()
        .find(|sub| sub.name.eq_ignore_ascii_case(&word.text))
}

fn names(subcommands: &[SubcommandSpec]) -> String {
    subcommands
        .iter()
        .map(|sub| sub.name.as_str())
        .collect::<Vec<_>>()
        .join("|")
}

/// `!warn <user> [reason]`: required options in angle brackets, optional ones in square ones.
fn usage_line(prefix: &str, path: &str, options: &[OptionSpec]) -> String {
    options
        .iter()
        .fold(format!("{prefix}{path}"), |line, option| {
            if option.required {
                format!("{line} <{}>", option.name)
            } else {
                format!("{line} [{}]", option.name)
            }
        })
}

fn attachment(option: &OptionSpec, file: &AttachmentInfo) -> CommandOption {
    CommandOption {
        name: option.name.clone(),
        value: CommandOptionValue::Attachment(file.clone()),
    }
}

/// The value `raw` stands for, or `None` when it is not one the option takes.
fn convert(option: &OptionSpec, raw: &str) -> Option<CommandOption> {
    let value = match option.kind {
        OptionKind::String => CommandOptionValue::String(text(option, raw)?),
        OptionKind::Integer => CommandOptionValue::Integer(integer(option, raw)?),
        OptionKind::Number => CommandOptionValue::Number(number(option, raw)?),
        OptionKind::Boolean => CommandOptionValue::Boolean(boolean(raw)?),
        OptionKind::User => CommandOptionValue::User(user(raw)?),
        OptionKind::Channel => CommandOptionValue::Channel(channel(raw)?),
        OptionKind::Role => CommandOptionValue::Role(role(raw)?),
        OptionKind::Mentionable => CommandOptionValue::Mentionable(match user(raw) {
            Some(user) => Mentionable::User(user),
            None => Mentionable::Role(role(raw)?),
        }),
        // Attachments come with the message: see `Reader::fill`.
        OptionKind::Attachment => return None,
    };
    Some(CommandOption {
        name: option.name.clone(),
        value,
    })
}

/// The text, or the choice it names (ignoring case), in the form the choice has it.
fn text(option: &OptionSpec, raw: &str) -> Option<String> {
    let chars = raw.chars().count();
    let long_enough = option
        .min_length
        .is_none_or(|min| chars >= usize::from(min));
    let short_enough = option
        .max_length
        .is_none_or(|max| chars <= usize::from(max));
    if !(long_enough && short_enough) {
        return None;
    }
    if option.choices.is_empty() {
        return Some(raw.to_owned());
    }
    option
        .choices
        .iter()
        .find_map(|choice| match &choice.value {
            ChoiceValue::String(value) if value.eq_ignore_ascii_case(raw) => Some(value.clone()),
            _ => None,
        })
}

// The spec's bounds are `f64`; an integer beyond 2^53 is compared with a rounded bound, which
// cannot make a value that is in range look out of it or the reverse for any bound a module sets.
#[allow(clippy::cast_precision_loss)]
fn integer(option: &OptionSpec, raw: &str) -> Option<i64> {
    let number: i64 = raw.parse().ok()?;
    within(option, number as f64)?;
    let fits_choice = option.choices.is_empty()
        || option
            .choices
            .iter()
            .any(|choice| matches!(choice.value, ChoiceValue::Integer(value) if value == number));
    fits_choice.then_some(number)
}

fn number(option: &OptionSpec, raw: &str) -> Option<f64> {
    let number: f64 = raw.parse().ok()?;
    if !number.is_finite() {
        return None;
    }
    within(option, number)?;
    let fits_choice = option.choices.is_empty()
        || option.choices.iter().any(|choice| {
            matches!(choice.value, ChoiceValue::Number(value) if value.total_cmp(&number).is_eq())
        });
    fits_choice.then_some(number)
}

fn within(option: &OptionSpec, number: f64) -> Option<()> {
    let above_min = option.min_value.is_none_or(|min| number >= min);
    let below_max = option.max_value.is_none_or(|max| number <= max);
    (above_min && below_max).then_some(())
}

fn boolean(raw: &str) -> Option<bool> {
    match raw.to_ascii_lowercase().as_str() {
        "true" | "yes" | "on" | "1" => Some(true),
        "false" | "no" | "off" | "0" => Some(false),
        _ => None,
    }
}

/// The digits of a bare id or of a mention written with one of `marks` (`<@!` and so on).
fn snowflake(raw: &str, marks: &[&str]) -> Option<u64> {
    let digits = match raw
        .strip_prefix('<')
        .and_then(|inner| inner.strip_suffix('>'))
    {
        Some(inner) => marks.iter().find_map(|mark| inner.strip_prefix(mark))?,
        None => raw,
    };
    if digits.is_empty() || !digits.bytes().all(|byte| byte.is_ascii_digit()) {
        return None;
    }
    digits.parse::<u64>().ok().filter(|id| *id != 0)
}

fn user(raw: &str) -> Option<UserId> {
    Id::new_checked(snowflake(raw, &["@!", "@"])?)
}

fn channel(raw: &str) -> Option<ChannelId> {
    Id::new_checked(snowflake(raw, &["#"])?)
}

fn role(raw: &str) -> Option<Id<RoleMarker>> {
    Id::new_checked(snowflake(raw, &["@&"])?)
}

/// The permissions the invoker has in the message's channel, as far as the cache knows: the owner
/// has them all, and the roles and the channel's overwrites are applied. The roles come from the
/// message, so one without member data counts as holding only `@everyone`. A thread is judged by
/// its own overwrites, not its parent's.
pub(crate) fn invoker_permissions(cache: &InMemoryCache, message: &MessageInfo) -> Permissions {
    let guild = message.guild_id;
    // The `@everyone` role has the guild's id.
    let everyone = cache
        .role(Id::new(guild.get()))
        .map_or(Permissions::empty(), |role| role.permissions);
    let roles: Vec<(Id<RoleMarker>, Permissions)> = message
        .author_roles
        .iter()
        .filter_map(|id| cache.role(*id).map(|role| (*id, role.permissions)))
        .collect();
    let mut calculator = PermissionCalculator::new(guild, message.author.id, everyone, &roles);
    if let Some(owner) = cache.guild(guild).map(|cached| cached.owner_id()) {
        calculator = calculator.owner_id(owner);
    }
    match cache.channel(message.channel_id) {
        Some(channel) => {
            let overwrites: &[PermissionOverwrite] =
                channel.permission_overwrites.as_deref().unwrap_or_default();
            calculator.in_channel(channel.kind, overwrites)
        }
        None => calculator.root(),
    }
}

/// How long a notice about a text command stays in the channel, for the invoker to read.
pub(crate) const NOTICE_LIFETIME: Duration = Duration::from_secs(30);

/// How long an ephemeral reply stays when the guild sets no delay.
const PRIVATE_REPLY_LIFETIME: Duration = Duration::from_secs(30);

/// The audit log reason of the messages a guild's text-command delays delete.
const CLEANUP_REASON: &str = "Text command cleanup";

/// Most messages of text commands waiting to be deleted by the guilds' delays, in all guilds together.
pub(crate) const MAX_PENDING_DELETIONS: usize = 1_024;

/// The messages a text command's answer deletes later, with the guild's delays.
struct Deletions {
    channel: ChannelId,
    command: MessageId,
    command_after: Option<Duration>,
    reply_after: Option<Duration>,
}

/// Stands in for the host's API for the one answer a text command cannot give: a form. The
/// module is told so in the guild's language, and nothing is shown.
pub(crate) struct TextCommandApi {
    inner: Arc<dyn DiscordApi>,
    interactions: Arc<InteractionGuilds>,
    settings: Arc<dyn SettingsProvider>,
    translator: Arc<dyn Translator>,
    /// The places for the deletions still waiting.
    pending: Arc<Semaphore>,
}

impl TextCommandApi {
    pub(crate) fn new(
        inner: Arc<dyn DiscordApi>,
        interactions: Arc<InteractionGuilds>,
        settings: Arc<dyn SettingsProvider>,
        translator: Arc<dyn Translator>,
    ) -> Self {
        Self {
            inner,
            interactions,
            settings,
            translator,
            pending: Arc::new(Semaphore::new(MAX_PENDING_DELETIONS)),
        }
    }

    fn deletions(&self, action: &Action) -> Option<(Deletions, bool)> {
        let (token, reply, command) = match action {
            Action::Respond {
                token,
                response: InteractionResponse::Message { reply },
                ..
            } => (token, reply, true),
            Action::EditResponse { token, reply } | Action::Followup { token, reply } => {
                (token, reply, false)
            }
            _ => return None,
        };
        let (channel, command_id) = token.text_command_message()?;
        let policy = self
            .interactions
            .guild_of(token)
            .map(|guild| self.settings.policy(guild));
        let command_after = policy
            .as_ref()
            .and_then(|policy| policy.prefix_delete_command_after)
            .filter(|_| command);
        let reply_after = if command || reply.ephemeral {
            policy
                .as_ref()
                .and_then(|policy| policy.prefix_delete_reply_after)
                .or_else(|| reply.ephemeral.then_some(PRIVATE_REPLY_LIFETIME))
        } else {
            None
        };
        if command_after.is_none() && reply_after.is_none() {
            return None;
        }
        Some((
            Deletions {
                channel,
                command: command_id,
                command_after,
                reply_after,
            },
            reply.ephemeral,
        ))
    }

    fn schedule_deletions(
        &self,
        deletions: &Deletions,
        reason: &Reason,
        outcome: &Outcome,
        permit: Option<OwnedSemaphorePermit>,
    ) {
        if let Some(after) = deletions.command_after {
            self.delete_after(deletions.channel, deletions.command, after, reason.clone());
        }
        if let (Some(after), Outcome::Message(reply)) = (deletions.reply_after, outcome) {
            if let Some(permit) = permit {
                self.delete_with_permit(reply.channel_id, reply.id, after, reason.clone(), permit);
            } else {
                self.delete_after(reply.channel_id, reply.id, after, reason.clone());
            }
        }
    }

    fn locale_of(&self, token: &InteractionToken) -> Locale {
        self.interactions
            .guild_of(token)
            .map_or(Locale::DEFAULT, |guild| self.settings.locale(guild))
    }

    /// Posts `text` in the channel of a text command, as a reply to the command, and takes it back
    /// after `NOTICE_LIFETIME`. A notice is what the host says about a command: nothing in it is
    /// private.
    async fn notice(
        &self,
        token: &InteractionToken,
        text: &str,
        color: u32,
    ) -> Result<Outcome, DiscordError> {
        let Some((channel, invoker)) = token.text_command_message() else {
            return Ok(Outcome::None);
        };
        let outcome = self
            .inner
            .execute(Action::SendMessage {
                channel_id: channel,
                content: None,
                embeds: vec![notice::embed(text, color)],
                reply_to: Some(invoker),
                mentions: MentionPolicy::default(),
            })
            .await?;
        if let Outcome::Message(sent) = &outcome {
            let reason = Reason::new(CLEANUP_REASON)?;
            self.delete_after(channel, sent.id, NOTICE_LIFETIME, reason);
        }
        Ok(outcome)
    }

    fn delete_after(
        &self,
        channel: ChannelId,
        message: MessageId,
        after: Duration,
        reason: Reason,
    ) {
        let Ok(place) = Arc::clone(&self.pending).try_acquire_owned() else {
            tracing::warn!("too many messages wait to be deleted; one stays");
            return;
        };
        self.delete_with_permit(channel, message, after, reason, place);
    }

    fn delete_with_permit(
        &self,
        channel: ChannelId,
        message: MessageId,
        after: Duration,
        reason: Reason,
        place: OwnedSemaphorePermit,
    ) {
        let inner = Arc::clone(&self.inner);
        tokio::spawn(async move {
            let _place = place;
            tokio::time::sleep(after).await;
            let action = Action::DeleteMessage {
                channel_id: channel,
                message_id: message,
                reason,
            };
            if let Err(error) = inner.execute(action).await {
                tracing::warn!(%error, "could not delete a text command or its reply");
            }
        });
    }
}

#[async_trait]
impl DiscordApi for TextCommandApi {
    async fn execute(&self, action: Action) -> Result<Outcome, DiscordError> {
        match action {
            Action::Respond {
                token,
                response: InteractionResponse::Modal(_),
                ..
            } if token.text_command_message().is_some() => {
                let locale = self.locale_of(&token);
                let content = self.translator.translate(
                    locale,
                    CORE_NAMESPACE,
                    "text-command-forms",
                    &Args::new(),
                );
                self.notice(&token, &content, notice::INFO).await
            }
            mut other => {
                let deletions = self.deletions(&other);
                let permit = if deletions.as_ref().is_some_and(|(_, private)| *private) {
                    if let Ok(permit) = Arc::clone(&self.pending).try_acquire_owned() {
                        Some(permit)
                    } else {
                        tracing::warn!("too many messages wait to be deleted; text reply refused");
                        return Err(DiscordError::Invalid(
                            "text reply cleanup capacity reached".into(),
                        ));
                    }
                } else {
                    None
                };
                match &mut other {
                    Action::Respond {
                        token,
                        response: InteractionResponse::Message { reply },
                        ..
                    }
                    | Action::EditResponse { token, reply }
                    | Action::Followup { token, reply }
                        if token.text_command_message().is_some() =>
                    {
                        reply.components.clear();
                    }
                    _ => {}
                }
                let reason = Reason::new(CLEANUP_REASON)?;
                let outcome = self.inner.execute(other).await?;
                if let Some((deletions, _)) = &deletions {
                    self.schedule_deletions(deletions, &reason, &outcome, permit);
                }
                Ok(outcome)
            }
        }
    }

    async fn channel_guild(&self, channel: ChannelId) -> Result<Option<GuildId>, DiscordError> {
        self.inner.channel_guild(channel).await
    }

    async fn guild_owner(&self, guild: GuildId) -> Result<Option<UserId>, DiscordError> {
        self.inner.guild_owner(guild).await
    }

    async fn webhook_channel(&self, guild: GuildId, name: &str) -> Result<ChannelId, DiscordError> {
        self.inner.webhook_channel(guild, name).await
    }
}

#[cfg(test)]
#[path = "text_commands_tests.rs"]
mod tests;
