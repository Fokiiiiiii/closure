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
        flags: flags(reply.ephemeral),
        ..InteractionResponseData::default()
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
        if reply.ephemeral {
            request = request.flags(MessageFlags::EPHEMERAL);
        }
        run(request).await
    }

    /// Replaces the deferred or original response; unset fields are left unchanged.
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
        if !reply.embeds.is_empty() {
            request = request.embeds(Some(&reply.embeds));
        }
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
            ephemeral: true,
        });
        assert_eq!(data.flags, Some(MessageFlags::EPHEMERAL));
        assert_eq!(data.allowed_mentions, Some(AllowedMentions::default()));
        assert_eq!(data.embeds, None);
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
