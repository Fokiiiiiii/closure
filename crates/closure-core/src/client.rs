use std::fmt;
use std::sync::Arc;
use std::time::Instant;

use async_trait::async_trait;
use twilight_model::channel::message::Message;
use twilight_model::guild::Member;

use crate::action::{
    Action, Emoji, InteractionReply, InteractionResponse, MentionPolicy, Outcome, Reason,
};
use crate::capability::CapabilitySet;
use crate::error::DiscordError;
use crate::event::{ChannelId, CommandInvocation, GuildId, MessageId, RoleId, UserId};
use crate::module::ModuleId;

/// The single door to Discord. Implemented by the Twilight-backed client in production and by
/// [`MockApi`](crate::testing::MockApi) in tests.
#[async_trait]
pub trait DiscordApi: Send + Sync {
    async fn execute(&self, action: Action) -> Result<Outcome, DiscordError>;

    /// The guild `channel` belongs to, or `None` for a channel outside any guild (a direct
    /// message). Fails if Discord cannot say, e.g. the channel does not exist.
    ///
    /// This is what lets [`ApiClient`] keep actions that identify only a channel or a message
    /// inside the guild the client is bound to.
    async fn channel_guild(&self, channel: ChannelId) -> Result<Option<GuildId>, DiscordError>;
}

/// What a module holds instead of a raw HTTP client.
///
/// [`ApiClient::execute`] rejects actions the module has no capability for, validates the rest
/// and only then forwards them, so a buggy or hostile module cannot exceed its grant.
#[derive(Clone)]
pub struct ApiClient {
    api: Arc<dyn DiscordApi>,
    granted: Arc<CapabilitySet>,
    module: ModuleId,
    guild: Option<GuildId>,
}

impl fmt::Debug for ApiClient {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("ApiClient")
            .field("module", &self.module)
            .field("granted", &self.granted)
            .field("guild", &self.guild)
            .finish_non_exhaustive()
    }
}

fn unexpected(action: &'static str) -> DiscordError {
    DiscordError::Fatal(format!("unexpected outcome for `{action}`"))
}

impl ApiClient {
    #[must_use]
    pub fn new(api: Arc<dyn DiscordApi>, granted: CapabilitySet, module: ModuleId) -> Self {
        Self {
            api,
            granted: Arc::new(granted),
            module,
            guild: None,
        }
    }

    /// Restricts the client to one guild: an action that targets a different guild is refused.
    ///
    /// Capabilities are granted per guild, so without this a module enabled with `ban_members`
    /// in one guild could ban in any other. An action that names a guild is compared directly.
    /// One that identifies only a channel or a message is checked by asking
    /// [`DiscordApi::channel_guild`], and refused when the channel is in another guild, outside
    /// any guild, or its guild cannot be determined. Actions bound to an interaction token
    /// (`Respond`, `Followup`, `EditResponse`) are not checked: a client is only handed the
    /// tokens of events of its own guild.
    #[must_use]
    pub fn scoped_to(mut self, guild: GuildId) -> Self {
        // Only the first binding counts: a module holds a (cloneable) scoped client and must not
        // be able to rebind its own copy to another guild.
        self.guild.get_or_insert(guild);
        self
    }

    pub async fn execute(&self, action: Action) -> Result<Outcome, DiscordError> {
        let name = action.name();
        let capability = action.capability();
        if !self.granted.contains(capability) {
            tracing::warn!(module = %self.module, action = name, %capability, "capability denied");
            return Err(DiscordError::CapabilityDenied(capability));
        }
        if let (Some(scope), Some(target)) = (self.guild, action.guild_id())
            && scope != target
        {
            tracing::warn!(module = %self.module, action = name, %scope, %target, "action targets another guild");
            return Err(DiscordError::Forbidden(
                "the action targets a guild other than the one being served".into(),
            ));
        }
        action.validate()?;
        if let (Some(scope), None, Some(channel)) =
            (self.guild, action.guild_id(), action.channel_id())
        {
            match self.api.channel_guild(channel).await? {
                Some(guild) if guild == scope => {}
                found => {
                    tracing::warn!(module = %self.module, action = name, %scope, ?found, "action targets a channel of another guild");
                    return Err(DiscordError::Forbidden(
                        "the action targets a channel that is not in the guild being served".into(),
                    ));
                }
            }
        }

        let started = Instant::now();
        let result = self.api.execute(action).await;
        tracing::debug!(
            module = %self.module,
            action = name,
            ok = result.is_ok(),
            elapsed_ms = u64::try_from(started.elapsed().as_millis()).unwrap_or(u64::MAX),
            "discord action"
        );
        result
    }

    pub async fn send_message(
        &self,
        channel_id: ChannelId,
        content: impl Into<String> + Send,
    ) -> Result<Message, DiscordError> {
        let action = Action::SendMessage {
            channel_id,
            content: Some(content.into()),
            embeds: Vec::new(),
            reply_to: None,
            mentions: MentionPolicy::None,
        };
        match self.execute(action).await? {
            Outcome::Message(message) => Ok(*message),
            _ => Err(unexpected("send_message")),
        }
    }

    pub async fn delete_message(
        &self,
        channel_id: ChannelId,
        message_id: MessageId,
        reason: Reason,
    ) -> Result<(), DiscordError> {
        self.execute(Action::DeleteMessage {
            channel_id,
            message_id,
            reason,
        })
        .await
        .map(drop)
    }

    pub async fn bulk_delete_messages(
        &self,
        channel_id: ChannelId,
        message_ids: Vec<MessageId>,
        reason: Reason,
    ) -> Result<(), DiscordError> {
        self.execute(Action::BulkDeleteMessages {
            channel_id,
            message_ids,
            reason,
        })
        .await
        .map(drop)
    }

    pub async fn add_reaction(
        &self,
        channel_id: ChannelId,
        message_id: MessageId,
        emoji: Emoji,
    ) -> Result<(), DiscordError> {
        self.execute(Action::AddReaction {
            channel_id,
            message_id,
            emoji,
        })
        .await
        .map(drop)
    }

    pub async fn get_member(
        &self,
        guild_id: GuildId,
        user_id: UserId,
    ) -> Result<Member, DiscordError> {
        match self
            .execute(Action::GetMember { guild_id, user_id })
            .await?
        {
            Outcome::Member(member) => Ok(*member),
            _ => Err(unexpected("get_member")),
        }
    }

    pub async fn kick(
        &self,
        guild_id: GuildId,
        user_id: UserId,
        reason: Reason,
    ) -> Result<(), DiscordError> {
        self.execute(Action::Kick {
            guild_id,
            user_id,
            reason,
        })
        .await
        .map(drop)
    }

    pub async fn ban(
        &self,
        guild_id: GuildId,
        user_id: UserId,
        delete_message_seconds: u32,
        reason: Reason,
    ) -> Result<(), DiscordError> {
        self.execute(Action::Ban {
            guild_id,
            user_id,
            delete_message_seconds,
            reason,
        })
        .await
        .map(drop)
    }

    pub async fn unban(
        &self,
        guild_id: GuildId,
        user_id: UserId,
        reason: Reason,
    ) -> Result<(), DiscordError> {
        self.execute(Action::Unban {
            guild_id,
            user_id,
            reason,
        })
        .await
        .map(drop)
    }

    /// Times a member out for `duration_seconds`, or lifts the timeout when `None`.
    pub async fn timeout(
        &self,
        guild_id: GuildId,
        user_id: UserId,
        duration_seconds: Option<u32>,
        reason: Reason,
    ) -> Result<(), DiscordError> {
        self.execute(Action::Timeout {
            guild_id,
            user_id,
            duration_seconds,
            reason,
        })
        .await
        .map(drop)
    }

    pub async fn add_role(
        &self,
        guild_id: GuildId,
        user_id: UserId,
        role_id: RoleId,
        reason: Reason,
    ) -> Result<(), DiscordError> {
        self.execute(Action::AddRole {
            guild_id,
            user_id,
            role_id,
            reason,
        })
        .await
        .map(drop)
    }

    pub async fn remove_role(
        &self,
        guild_id: GuildId,
        user_id: UserId,
        role_id: RoleId,
        reason: Reason,
    ) -> Result<(), DiscordError> {
        self.execute(Action::RemoveRole {
            guild_id,
            user_id,
            role_id,
            reason,
        })
        .await
        .map(drop)
    }

    /// Answers a slash command with a message.
    pub async fn respond(
        &self,
        invocation: &CommandInvocation,
        reply: InteractionReply,
    ) -> Result<(), DiscordError> {
        self.execute(Action::Respond {
            interaction_id: invocation.interaction_id,
            token: invocation.token.clone(),
            response: InteractionResponse::Message { reply },
        })
        .await
        .map(drop)
    }

    /// Acknowledges a slash command so the answer can follow after Discord's 3 second limit.
    pub async fn defer(
        &self,
        invocation: &CommandInvocation,
        ephemeral: bool,
    ) -> Result<(), DiscordError> {
        self.execute(Action::Respond {
            interaction_id: invocation.interaction_id,
            token: invocation.token.clone(),
            response: InteractionResponse::Defer { ephemeral },
        })
        .await
        .map(drop)
    }

    pub async fn followup(
        &self,
        invocation: &CommandInvocation,
        reply: InteractionReply,
    ) -> Result<(), DiscordError> {
        self.execute(Action::Followup {
            token: invocation.token.clone(),
            reply,
        })
        .await
        .map(drop)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::capability::Capability;
    use crate::testing::MockApi;
    use twilight_model::id::Id;

    fn client(api: &Arc<MockApi>, granted: &[Capability]) -> ApiClient {
        ApiClient::new(
            Arc::clone(api) as Arc<dyn DiscordApi>,
            granted.iter().copied().collect(),
            ModuleId::new("test").unwrap(),
        )
    }

    fn reason() -> Reason {
        Reason::new("test").unwrap()
    }

    #[tokio::test]
    async fn ungranted_capability_is_denied_without_reaching_discord() {
        let api = Arc::new(MockApi::default());
        let client = client(&api, &[Capability::SendMessages]);
        let result = client.ban(Id::new(1), Id::new(2), 0, reason()).await;
        assert_eq!(
            result,
            Err(DiscordError::CapabilityDenied(Capability::BanMembers))
        );
        assert_eq!(api.actions(), []);
    }

    #[tokio::test]
    async fn a_scoped_client_refuses_other_guilds_even_with_the_capability() {
        let api = Arc::new(MockApi::default());
        let client = client(&api, &[Capability::KickMembers]).scoped_to(Id::new(1));

        let elsewhere = client.kick(Id::new(2), Id::new(9), reason()).await;
        assert!(
            matches!(elsewhere, Err(DiscordError::Forbidden(_))),
            "{elsewhere:?}"
        );
        assert_eq!(api.actions(), []);

        client.kick(Id::new(1), Id::new(9), reason()).await.unwrap();
        assert_eq!(api.actions().len(), 1);
    }

    #[tokio::test]
    async fn a_module_cannot_rebind_its_scoped_client_to_another_guild() {
        let api = Arc::new(MockApi::default());
        let given = client(&api, &[Capability::BanMembers]).scoped_to(Id::new(1));

        let rebound = given.clone().scoped_to(Id::new(2));
        let result = rebound.ban(Id::new(2), Id::new(9), 0, reason()).await;
        assert!(
            matches!(result, Err(DiscordError::Forbidden(_))),
            "{result:?}"
        );
        assert_eq!(api.actions(), []);

        rebound
            .ban(Id::new(1), Id::new(9), 0, reason())
            .await
            .unwrap();
    }

    /// Every action that identifies its target by a channel, aimed at `channel`.
    fn channel_actions(channel: u64) -> Vec<Action> {
        let channel_id = Id::new(channel);
        let message_id = Id::new(5);
        vec![
            Action::SendMessage {
                channel_id,
                content: Some("hi".into()),
                embeds: Vec::new(),
                reply_to: None,
                mentions: MentionPolicy::None,
            },
            Action::DeleteMessage {
                channel_id,
                message_id,
                reason: reason(),
            },
            Action::BulkDeleteMessages {
                channel_id,
                message_ids: vec![Id::new(5), Id::new(6)],
                reason: reason(),
            },
            Action::AddReaction {
                channel_id,
                message_id,
                emoji: Emoji::Unicode {
                    value: "👍".into()
                },
            },
            Action::GetChannel { channel_id },
            Action::DeleteChannel {
                channel_id,
                reason: reason(),
            },
        ]
    }

    #[tokio::test]
    async fn a_scoped_client_refuses_channels_of_other_guilds_and_outside_guilds() {
        let api = Arc::new(MockApi::default());
        api.add_channel(Id::new(100), Some(Id::new(1)));
        api.add_channel(Id::new(200), Some(Id::new(2)));
        api.add_channel(Id::new(300), None);
        let granted = [
            Capability::SendMessages,
            Capability::ManageMessages,
            Capability::ManageChannels,
            Capability::AddReactions,
            Capability::ReadGuild,
        ];
        let client = client(&api, &granted).scoped_to(Id::new(1));

        for (channel, what) in [(200, "another guild"), (300, "no guild"), (999, "unknown")] {
            for action in channel_actions(channel) {
                let name = action.name();
                let result = client.execute(action).await;
                if channel == 999 {
                    assert_eq!(result, Err(DiscordError::NotFound), "{name} / {what}");
                } else {
                    assert!(
                        matches!(result, Err(DiscordError::Forbidden(_))),
                        "{name} / {what}: {result:?}"
                    );
                }
            }
        }
        assert_eq!(api.actions(), [], "nothing reached Discord");

        for action in channel_actions(100) {
            let name = action.name();
            client
                .execute(action)
                .await
                .unwrap_or_else(|e| panic!("{name}: {e}"));
        }
        assert_eq!(api.actions().len(), 6);
    }

    #[tokio::test]
    async fn a_lookup_failure_refuses_the_action_instead_of_letting_it_through() {
        struct Failing;
        #[async_trait]
        impl DiscordApi for Failing {
            async fn execute(&self, _: Action) -> Result<Outcome, DiscordError> {
                panic!("the action must not be sent");
            }
            async fn channel_guild(&self, _: ChannelId) -> Result<Option<GuildId>, DiscordError> {
                Err(DiscordError::Transient("discord is down".into()))
            }
        }
        let client = ApiClient::new(
            Arc::new(Failing),
            [Capability::ManageChannels].into_iter().collect(),
            ModuleId::new("test").unwrap(),
        )
        .scoped_to(Id::new(1));

        let result = client
            .execute(Action::DeleteChannel {
                channel_id: Id::new(100),
                reason: reason(),
            })
            .await;
        assert_eq!(
            result,
            Err(DiscordError::Transient("discord is down".into()))
        );
    }

    #[tokio::test]
    async fn actions_that_name_a_guild_and_unscoped_clients_need_no_lookup() {
        let api = Arc::new(MockApi::default());
        let scoped = client(&api, &[Capability::KickMembers]).scoped_to(Id::new(1));
        scoped.kick(Id::new(1), Id::new(9), reason()).await.unwrap();
        assert_eq!(api.channel_lookups(), 0);

        let unscoped = client(&api, &[Capability::ManageChannels]);
        unscoped
            .execute(Action::DeleteChannel {
                channel_id: Id::new(100),
                reason: reason(),
            })
            .await
            .unwrap();
        assert_eq!(
            api.channel_lookups(),
            0,
            "host code is not confined to a guild"
        );
    }

    #[tokio::test]
    async fn invalid_action_is_rejected_without_reaching_discord() {
        let api = Arc::new(MockApi::default());
        let client = client(&api, &[Capability::BanMembers]);
        let result = client.ban(Id::new(1), Id::new(2), u32::MAX, reason()).await;
        assert!(matches!(result, Err(DiscordError::Invalid(_))));
        assert_eq!(api.actions(), []);
    }

    #[tokio::test]
    async fn granted_valid_action_is_forwarded_once() {
        let api = Arc::new(MockApi::default());
        let client = client(&api, &[Capability::KickMembers]);
        client.kick(Id::new(1), Id::new(2), reason()).await.unwrap();
        assert_eq!(api.actions().len(), 1);
        assert_eq!(api.actions()[0].name(), "kick");
    }

    #[tokio::test]
    async fn discord_failures_are_propagated() {
        let api = Arc::new(MockApi::default());
        api.fail_with(DiscordError::NotFound);
        let client = client(&api, &[Capability::KickMembers]);
        let result = client.kick(Id::new(1), Id::new(2), reason()).await;
        assert_eq!(result, Err(DiscordError::NotFound));
    }
}
