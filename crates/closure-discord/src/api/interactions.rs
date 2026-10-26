use std::collections::HashMap;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex, PoisonError};
use std::time::{Duration, Instant};

use closure_core::action::{InteractionReply, InteractionResponse, Outcome};
use closure_core::error::DiscordError;
use closure_core::event::{ChannelId, InteractionId, InteractionToken};
use closure_core::module::{ChoiceValue, OptionChoice};
use twilight_model::application::command::{CommandOptionChoice, CommandOptionChoiceValue};
use twilight_model::channel::Message;
use twilight_model::channel::message::{AllowedMentions, MessageFlags};
use twilight_model::http::interaction::{
    InteractionResponse as TwilightResponse, InteractionResponseData, InteractionResponseType,
};
use twilight_model::id::Id;
use twilight_model::id::marker::MessageMarker;

use super::{TwilightApi, read, run};

fn flags(ephemeral: bool) -> Option<MessageFlags> {
    ephemeral.then_some(MessageFlags::EPHEMERAL)
}

fn response_data(reply: &InteractionReply) -> InteractionResponseData {
    InteractionResponseData {
        allowed_mentions: Some(AllowedMentions::default()),
        content: reply.content.clone(),
        embeds: (!reply.embeds.is_empty()).then(|| reply.embeds.clone()),
        components: (!reply.components.is_empty()).then(|| reply.components.clone()),
        flags: flags(reply.ephemeral),
        ..InteractionResponseData::default()
    }
}

/// What an update says about the message it changes: its embeds and components are exactly
/// those of the reply, so none means "take the old ones off". (A new message has none to take
/// off, and leaves them out.)
/// A choice of an autocomplete answer, in the form Discord takes.
fn choice(choice: &OptionChoice) -> CommandOptionChoice {
    CommandOptionChoice {
        name: choice.name.clone(),
        name_localizations: None,
        value: match &choice.value {
            ChoiceValue::String(text) => CommandOptionChoiceValue::String(text.clone()),
            ChoiceValue::Integer(number) => CommandOptionChoiceValue::Integer(*number),
            ChoiceValue::Number(number) => CommandOptionChoiceValue::Number(*number),
        },
    }
}

fn update_data(reply: &InteractionReply) -> InteractionResponseData {
    InteractionResponseData {
        embeds: Some(reply.embeds.clone()),
        components: Some(reply.components.clone()),
        ..response_data(reply)
    }
}

fn twilight_response(response: &InteractionResponse) -> TwilightResponse {
    match response {
        InteractionResponse::Defer { ephemeral } => TwilightResponse {
            kind: InteractionResponseType::DeferredChannelMessageWithSource,
            data: flags(*ephemeral).map(|flags| InteractionResponseData {
                flags: Some(flags),
                ..InteractionResponseData::default()
            }),
        },
        InteractionResponse::Message { reply } => TwilightResponse {
            kind: InteractionResponseType::ChannelMessageWithSource,
            data: Some(response_data(reply)),
        },
        InteractionResponse::Update { reply } => TwilightResponse {
            kind: InteractionResponseType::UpdateMessage,
            data: Some(update_data(reply)),
        },
        InteractionResponse::DeferUpdate => TwilightResponse {
            kind: InteractionResponseType::DeferredUpdateMessage,
            data: None,
        },
        InteractionResponse::Autocomplete { choices } => TwilightResponse {
            kind: InteractionResponseType::ApplicationCommandAutocompleteResult,
            data: Some(InteractionResponseData {
                choices: Some(choices.iter().map(choice).collect()),
                ..InteractionResponseData::default()
            }),
        },
        InteractionResponse::Modal(spec) => TwilightResponse {
            kind: InteractionResponseType::Modal,
            data: Some(InteractionResponseData {
                custom_id: Some(spec.custom_id.clone()),
                title: Some(spec.title.clone()),
                components: Some(spec.components.clone()),
                ..InteractionResponseData::default()
            }),
        },
    }
}

impl TwilightApi {
    pub(super) async fn respond(
        &self,
        interaction_id: InteractionId,
        token: &InteractionToken,
        response: &InteractionResponse,
    ) -> Result<Outcome, DiscordError> {
        if let Some((channel, message)) = token.text_command_message() {
            return self
                .respond_to_text_command(channel, message, token, response)
                .await;
        }
        let interaction = self.http.interaction(self.application_id);
        let response = twilight_response(response);
        run(interaction.create_response(interaction_id, token.expose(), &response)).await
    }

    pub(super) async fn followup(
        &self,
        token: &InteractionToken,
        reply: &InteractionReply,
    ) -> Result<Outcome, DiscordError> {
        if let Some((channel, message)) = token.text_command_message() {
            let sent = self.send_text_reply(channel, message, reply).await?;
            if reply.ephemeral {
                self.delete_later(channel, sent.id);
            }
            return Ok(Outcome::Message(Box::new(sent)));
        }
        let interaction = self.http.interaction(self.application_id);
        let allowed = AllowedMentions::default();
        let mut request = interaction
            .create_followup(token.expose())
            .allowed_mentions(Some(&allowed));
        if let Some(content) = &reply.content {
            request = request.content(content);
        }
        if !reply.embeds.is_empty() {
            request = request.embeds(&reply.embeds);
        }
        if !reply.components.is_empty() {
            request = request.components(&reply.components);
        }
        if reply.ephemeral {
            request = request.flags(MessageFlags::EPHEMERAL);
        }
        run(request).await
    }

    /// Replaces the deferred or original response: the text if the reply has any (else it is left
    /// as it is), the embeds and components exactly as the reply has them.
    pub(super) async fn edit_response(
        &self,
        token: &InteractionToken,
        reply: &InteractionReply,
    ) -> Result<Outcome, DiscordError> {
        if let Some((channel, message)) = token.text_command_message() {
            return self
                .edit_text_command_reply(channel, message, token, reply)
                .await;
        }
        let interaction = self.http.interaction(self.application_id);
        let allowed = AllowedMentions::default();
        let mut request = interaction
            .update_response(token.expose())
            .allowed_mentions(Some(&allowed));
        if let Some(content) = &reply.content {
            request = request.content(Some(content));
        }
        request = request
            .embeds(Some(&reply.embeds))
            .components(Some(&reply.components));
        run(request).await
    }
}

/// How long an ephemeral reply to a text command stays before it is deleted.
const EPHEMERAL_LIFETIME: Duration = Duration::from_secs(30);

/// How long the bot's reply to a text command can still be edited, as long as the interaction
/// token of a slash command is good.
const EDITABLE_FOR: Duration = Duration::from_secs(15 * 60);

/// Most replies to text commands remembered for editing; the oldest are forgotten first.
const REMEMBERED_REPLIES: usize = 1_024;

/// Most ephemeral replies waiting to be deleted. Past it a reply stays: the alternative is an
/// unbounded number of sleeping tasks.
const PENDING_DELETIONS: usize = 256;

/// What the bot answered each text command with, by the command's token.
pub(super) struct TextReplies {
    replies: Mutex<HashMap<String, Remembered>>,
    pending_deletions: Arc<AtomicUsize>,
    /// How long an ephemeral reply stays; shorter in tests.
    pub(super) ephemeral_lifetime: Duration,
}

impl Default for TextReplies {
    fn default() -> Self {
        Self {
            replies: Mutex::new(HashMap::new()),
            pending_deletions: Arc::new(AtomicUsize::new(0)),
            ephemeral_lifetime: EPHEMERAL_LIFETIME,
        }
    }
}

struct Remembered {
    /// `None` after a deferral, until the first reply is sent.
    message: Option<Id<MessageMarker>>,
    ephemeral: bool,
    at: Instant,
}

impl TextReplies {
    fn remember(
        &self,
        token: &InteractionToken,
        message: Option<Id<MessageMarker>>,
        ephemeral: bool,
    ) {
        let mut replies = self.replies.lock().unwrap_or_else(PoisonError::into_inner);
        if replies.len() >= REMEMBERED_REPLIES && !replies.contains_key(token.expose()) {
            replies.retain(|_, remembered| remembered.at.elapsed() <= EDITABLE_FOR);
            if replies.len() >= REMEMBERED_REPLIES
                && let Some(oldest) = replies
                    .iter()
                    .min_by_key(|(_, remembered)| remembered.at)
                    .map(|(key, _)| key.clone())
            {
                replies.remove(&oldest);
            }
        }
        replies.insert(
            token.expose().to_owned(),
            Remembered {
                message,
                ephemeral,
                at: Instant::now(),
            },
        );
    }

    /// The message and the ephemeral flag remembered for `token`, while they can still be edited.
    fn fresh(&self, token: &InteractionToken) -> Option<(Option<Id<MessageMarker>>, bool)> {
        let replies = self.replies.lock().unwrap_or_else(PoisonError::into_inner);
        let remembered = replies.get(token.expose())?;
        (remembered.at.elapsed() <= EDITABLE_FOR)
            .then_some((remembered.message, remembered.ephemeral))
    }
}

impl TwilightApi {
    /// A text command has no interaction to answer, so the bot replies to its message in the same
    /// channel. A deferral is only remembered: the first edit sends the reply.
    async fn respond_to_text_command(
        &self,
        channel: ChannelId,
        invoker: Id<MessageMarker>,
        token: &InteractionToken,
        response: &InteractionResponse,
    ) -> Result<Outcome, DiscordError> {
        match response {
            InteractionResponse::Defer { ephemeral } => {
                self.text.remember(token, None, *ephemeral);
                Ok(Outcome::None)
            }
            InteractionResponse::Message { reply } => {
                let sent = self.send_text_reply(channel, invoker, reply).await?;
                self.text.remember(token, Some(sent.id), reply.ephemeral);
                if reply.ephemeral {
                    self.delete_later(channel, sent.id);
                }
                Ok(Outcome::Message(Box::new(sent)))
            }
            InteractionResponse::DeferUpdate => Ok(Outcome::None),
            InteractionResponse::Update { .. }
            | InteractionResponse::Autocomplete { .. }
            | InteractionResponse::Modal(_) => Err(DiscordError::Invalid(
                "a text command can only be answered with a message".into(),
            )),
        }
    }

    /// Replaces the bot's reply to the text command, or sends the first one.
    async fn edit_text_command_reply(
        &self,
        channel: ChannelId,
        invoker: Id<MessageMarker>,
        token: &InteractionToken,
        reply: &InteractionReply,
    ) -> Result<Outcome, DiscordError> {
        match self.text.fresh(token) {
            Some((Some(message), _)) => {
                let allowed = AllowedMentions::default();
                let mut request = self
                    .http
                    .update_message(channel, message)
                    .allowed_mentions(Some(&allowed));
                if let Some(content) = &reply.content {
                    request = request.content(Some(content));
                }
                request = request
                    .embeds(Some(&reply.embeds))
                    .components(Some(&reply.components));
                run(request).await
            }
            // Nothing sent yet, or only a deferral: this is the first reply.
            other => {
                let ephemeral = other.is_some_and(|(_, ephemeral)| ephemeral) || reply.ephemeral;
                let sent = self.send_text_reply(channel, invoker, reply).await?;
                self.text.remember(token, Some(sent.id), ephemeral);
                if ephemeral {
                    self.delete_later(channel, sent.id);
                }
                Ok(Outcome::Message(Box::new(sent)))
            }
        }
    }

    /// Sends `reply` as a reply to `invoker`. No one is pinged: a reply does not notify the
    /// author it answers, and neither do mentions in it.
    async fn send_text_reply(
        &self,
        channel: ChannelId,
        invoker: Id<MessageMarker>,
        reply: &InteractionReply,
    ) -> Result<Message, DiscordError> {
        let allowed = AllowedMentions::default();
        let mut request = self
            .http
            .create_message(channel)
            .allowed_mentions(Some(&allowed))
            // The invoking message may be gone by now; the reply is then sent as a plain message.
            .reply(invoker)
            .fail_if_not_exists(false);
        if let Some(content) = &reply.content {
            request = request.content(content);
        }
        if !reply.embeds.is_empty() {
            request = request.embeds(&reply.embeds);
        }
        if !reply.components.is_empty() {
            request = request.components(&reply.components);
        }
        read(request).await
    }

    /// Deletes a reply to a text command after its lifetime. Best effort: the message may be gone
    /// already, and the bot may not be allowed to delete it.
    fn delete_later(&self, channel: ChannelId, message: Id<MessageMarker>) {
        let pending = Arc::clone(&self.text.pending_deletions);
        if pending.fetch_add(1, Ordering::AcqRel) >= PENDING_DELETIONS {
            pending.fetch_sub(1, Ordering::AcqRel);
            tracing::debug!("too many replies are waiting to be deleted; this one stays");
            return;
        }
        let http = Arc::clone(&self.http);
        let lifetime = self.text.ephemeral_lifetime;
        tokio::spawn(async move {
            tokio::time::sleep(lifetime).await;
            if let Err(error) = http.delete_message(channel, message).await {
                tracing::debug!(%error, "could not delete a reply to a text command");
            }
            pending.fetch_sub(1, Ordering::AcqRel);
        });
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn autocomplete_answers_carry_their_choices_as_discord_takes_them() {
        let answer = twilight_response(&InteractionResponse::Autocomplete {
            choices: vec![
                OptionChoice {
                    name: "Spam".into(),
                    value: ChoiceValue::String("spam".into()),
                },
                OptionChoice {
                    name: "Three".into(),
                    value: ChoiceValue::Integer(3),
                },
            ],
        });
        assert_eq!(
            answer.kind,
            InteractionResponseType::ApplicationCommandAutocompleteResult
        );
        let choices = answer.data.unwrap().choices.unwrap();
        assert_eq!(choices[0].name, "Spam");
        assert_eq!(
            choices[0].value,
            CommandOptionChoiceValue::String("spam".into())
        );
        assert_eq!(choices[1].value, CommandOptionChoiceValue::Integer(3));
    }

    #[test]
    fn ephemeral_replies_set_the_flag_and_never_ping() {
        let data = response_data(&InteractionReply {
            content: Some("hi".into()),
            embeds: vec![],
            components: Vec::new(),
            ephemeral: true,
        });
        assert_eq!(data.flags, Some(MessageFlags::EPHEMERAL));
        assert_eq!(data.allowed_mentions, Some(AllowedMentions::default()));
        assert_eq!(data.embeds, None);
    }

    #[test]
    fn updates_acknowledgements_and_modals_map_to_their_discord_types() {
        let update = twilight_response(&InteractionResponse::Update {
            reply: InteractionReply {
                content: Some("changed".into()),
                ..InteractionReply::default()
            },
        });
        assert_eq!(update.kind, InteractionResponseType::UpdateMessage);
        assert_eq!(
            update.data.and_then(|d| d.content).as_deref(),
            Some("changed")
        );

        let ack = twilight_response(&InteractionResponse::DeferUpdate);
        assert_eq!(ack.kind, InteractionResponseType::DeferredUpdateMessage);
        assert!(ack.data.is_none());

        let form = twilight_response(&InteractionResponse::Modal(
            closure_core::action::ModalSpec {
                custom_id: "m:form".into(),
                title: "Report".into(),
                components: vec![],
            },
        ));
        assert_eq!(form.kind, InteractionResponseType::Modal);
        let data = form.data.unwrap();
        assert_eq!(data.custom_id.as_deref(), Some("m:form"));
        assert_eq!(data.title.as_deref(), Some("Report"));
    }

    #[test]
    fn components_are_sent_with_the_reply() {
        let data = response_data(&InteractionReply {
            content: Some("hi".into()),
            components: vec![twilight_model::channel::message::Component::ActionRow(
                twilight_model::channel::message::component::ActionRow {
                    id: None,
                    components: vec![],
                },
            )],
            ..InteractionReply::default()
        });
        assert_eq!(data.components.map(|c| c.len()), Some(1));
        assert!(
            response_data(&InteractionReply::default())
                .components
                .is_none()
        );
    }

    #[test]
    fn deferring_is_public_unless_asked_otherwise() {
        let public = twilight_response(&InteractionResponse::Defer { ephemeral: false });
        assert_eq!(
            public.kind,
            InteractionResponseType::DeferredChannelMessageWithSource
        );
        assert!(public.data.is_none());

        let private = twilight_response(&InteractionResponse::Defer { ephemeral: true });
        assert_eq!(
            private.data.and_then(|d| d.flags),
            Some(MessageFlags::EPHEMERAL)
        );
    }
}
