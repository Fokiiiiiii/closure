use closure_core::action::{InteractionReply, InteractionResponse, Outcome};
use closure_core::error::DiscordError;
use closure_core::event::{InteractionId, InteractionToken};
use twilight_model::channel::message::{AllowedMentions, MessageFlags};
use twilight_model::http::interaction::{
    InteractionResponse as TwilightResponse, InteractionResponseData, InteractionResponseType,
};

use super::{TwilightApi, run};

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
        let interaction = self.http.interaction(self.application_id);
        let response = twilight_response(response);
        run(interaction.create_response(interaction_id, token.expose(), &response)).await
    }

    pub(super) async fn followup(
        &self,
        token: &InteractionToken,
        reply: &InteractionReply,
    ) -> Result<Outcome, DiscordError> {
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

#[cfg(test)]
mod tests {
    use super::*;

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
