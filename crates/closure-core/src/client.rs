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
use crate::interaction::InteractionGuilds;
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
    interactions: Option<Arc<InteractionGuilds>>,
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
            interactions: None,
        }
    }

    /// Tells the client which guild each interaction token belongs to. A client that is bound
    /// to a guild and has no such record refuses every interaction response.
    ///
    /// Only the host's setup does this, before the client is bound to a guild: once it is bound,
    /// or already has a record, the call changes nothing. A module holds a bound client and
    /// could otherwise swap in a record of its own, in which any token belongs to any guild.
    #[must_use]
    pub fn with_interactions(mut self, interactions: Arc<InteractionGuilds>) -> Self {
        if self.guild.is_some() || self.interactions.is_some() {
            tracing::warn!(module = %self.module, "the interaction record of a bound client cannot be replaced");
        } else {
            self.interactions = Some(interactions);
        }
        self
    }

    /// Restricts the client to one guild: an action that targets a different guild is refused.
    ///
    /// Capabilities are granted per guild, so without this a module enabled with `ban_members`
    /// in one guild could ban in any other. An action that names a guild is compared directly.
    /// One that identifies only a channel or a message is checked by asking
    /// [`DiscordApi::channel_guild`], and refused when the channel is in another guild, outside
    /// any guild, or its guild cannot be determined; this also covers the channels an action
    /// merely refers to (a parent category, an alert or exempt channel). Responses to an
    /// interaction are answered only when the host recorded the token as coming from the
    /// bound guild (see [`with_interactions`](Self::with_interactions)).
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
        if let Some(scope) = self.guild {
            self.check_scope(scope, &action).await?;
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

    /// Refuses `action` unless everything it touches belongs to `scope`: every channel it
    /// targets or refers to, and the interaction it answers.
    async fn check_scope(&self, scope: GuildId, action: &Action) -> Result<(), DiscordError> {
        for channel in action.channels() {
            match self.api.channel_guild(channel).await? {
                Some(guild) if guild == scope => {}
                found => {
                    tracing::warn!(module = %self.module, action = action.name(), %scope, ?found, "action targets a channel of another guild");
                    return Err(DiscordError::Forbidden(
                        "the action targets a channel that is not in the guild being served".into(),
                    ));
                }
            }
        }
        if let Some(token) = action.interaction_token() {
            let known = self
                .interactions
                .as_ref()
                .and_then(|interactions| interactions.guild_of(token));
            if known != Some(scope) {
                tracing::warn!(module = %self.module, action = action.name(), %scope, "action answers an interaction of another guild, or an unknown one");
                return Err(DiscordError::Forbidden(
                    "the action answers an interaction that is not from the guild being served"
                        .into(),
                ));
            }
        }
        Ok(())
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
    use crate::action::{
        AutoModAction, AutoModTrigger, ChannelKind, ChannelPatch, InteractionReply,
        InteractionResponse, NewAutoModRule, NewChannel, OverwriteSpec, OverwriteTarget,
    };
    use crate::capability::Capability;
    use crate::event::InteractionToken;
    use crate::testing::MockApi;
    use twilight_model::guild::Permissions;
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

    /// All ten actions that identify their target by a channel, aimed at `channel`.
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
            Action::EditMessage {
                channel_id,
                message_id,
                content: Some("edited".into()),
                embeds: None,
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
            Action::EditChannel {
                channel_id,
                patch: ChannelPatch {
                    name: Some("renamed".into()),
                    ..ChannelPatch::default()
                },
                reason: reason(),
            },
            Action::DeleteChannel {
                channel_id,
                reason: reason(),
            },
            Action::SetPermissionOverwrite {
                channel_id,
                overwrite: OverwriteSpec {
                    target: OverwriteTarget::Role(Id::new(7)),
                    allow: Permissions::empty(),
                    deny: Permissions::SEND_MESSAGES,
                },
                reason: reason(),
            },
            Action::DeletePermissionOverwrite {
                channel_id,
                target: OverwriteTarget::Role(Id::new(7)),
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
        let client = client(&api, &Capability::ALL).scoped_to(Id::new(1));

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
        assert_eq!(api.actions().len(), channel_actions(100).len());
    }

    #[test]
    fn every_channel_addressed_action_reports_its_channel() {
        for action in channel_actions(100) {
            assert_eq!(action.channels(), [Id::new(100)], "{}", action.name());
            assert_eq!(action.guild_id(), None, "{}", action.name());
        }
    }

    fn new_channel(parent_id: Option<u64>) -> Action {
        Action::CreateChannel {
            guild_id: Id::new(1),
            spec: NewChannel {
                name: "new".into(),
                kind: ChannelKind::Text,
                parent_id: parent_id.map(Id::new),
                topic: None,
                nsfw: false,
                slowmode_seconds: 0,
                overwrites: Vec::new(),
            },
            reason: reason(),
        }
    }

    fn new_rule(alert: Option<u64>, exempt: Option<u64>) -> Action {
        Action::CreateAutoModRule {
            guild_id: Id::new(1),
            spec: NewAutoModRule {
                name: "rule".into(),
                trigger: AutoModTrigger::Spam,
                actions: std::iter::once(AutoModAction::BlockMessage {
                    custom_message: None,
                })
                .chain(alert.map(|channel| AutoModAction::SendAlert {
                    channel_id: Id::new(channel),
                }))
                .collect(),
                enabled: true,
                exempt_roles: Vec::new(),
                exempt_channels: exempt.map(Id::new).into_iter().collect(),
            },
            reason: reason(),
        }
    }

    #[tokio::test]
    async fn channels_an_action_merely_refers_to_must_also_be_in_the_guild() {
        let api = Arc::new(MockApi::default());
        api.add_channel(Id::new(100), Some(Id::new(1)));
        api.add_channel(Id::new(200), Some(Id::new(2)));
        let client = client(&api, &Capability::ALL).scoped_to(Id::new(1));

        for action in [
            new_channel(Some(200)),
            new_rule(Some(200), None),
            new_rule(None, Some(200)),
            new_rule(Some(100), Some(200)),
        ] {
            let name = action.name();
            let result = client.execute(action).await;
            assert!(
                matches!(result, Err(DiscordError::Forbidden(_))),
                "{name}: {result:?}"
            );
        }
        assert_eq!(api.actions(), []);

        for action in [
            new_channel(None),
            new_channel(Some(100)),
            new_rule(None, None),
            new_rule(Some(100), Some(100)),
        ] {
            let name = action.name();
            client
                .execute(action)
                .await
                .unwrap_or_else(|e| panic!("{name}: {e}"));
        }
        assert_eq!(api.actions().len(), 4);
    }

    fn respond(token: &str) -> Action {
        Action::Respond {
            interaction_id: Id::new(1),
            token: InteractionToken::new(token),
            response: InteractionResponse::Defer { ephemeral: false },
        }
    }

    fn followup(token: &str) -> Action {
        Action::Followup {
            token: InteractionToken::new(token),
            reply: InteractionReply {
                content: Some("done".into()),
                embeds: Vec::new(),
                ephemeral: false,
            },
        }
    }

    fn edit_response(token: &str) -> Action {
        Action::EditResponse {
            token: InteractionToken::new(token),
            reply: InteractionReply {
                content: Some("done".into()),
                embeds: Vec::new(),
                ephemeral: false,
            },
        }
    }

    #[tokio::test]
    async fn a_client_answers_only_interactions_of_its_own_guild() {
        let api = Arc::new(MockApi::default());
        let interactions = Arc::new(InteractionGuilds::default());
        interactions.register(&InteractionToken::new("token-1"), Id::new(1));
        interactions.register(&InteractionToken::new("token-2"), Id::new(2));
        let client = client(&api, &[Capability::RespondToInteractions])
            .with_interactions(Arc::clone(&interactions))
            .scoped_to(Id::new(1));

        for make in [respond, followup, edit_response] {
            for token in ["token-2", "forged"] {
                let action = make(token);
                let name = action.name();
                let result = client.execute(action).await;
                assert!(
                    matches!(result, Err(DiscordError::Forbidden(_))),
                    "{name} / {token}: {result:?}"
                );
            }
            client.execute(make("token-1")).await.unwrap();
        }
        assert_eq!(
            api.actions().len(),
            3,
            "only the own guild's three went through"
        );
    }

    #[tokio::test]
    async fn a_bound_client_cannot_be_given_a_record_of_its_own_making() {
        let api = Arc::new(MockApi::default());
        let real = Arc::new(InteractionGuilds::default());
        real.register(&InteractionToken::new("token-of-guild-2"), Id::new(2));
        let forged = Arc::new(InteractionGuilds::default());
        forged.register(&InteractionToken::new("token-of-guild-2"), Id::new(1));

        let with_record = client(&api, &[Capability::RespondToInteractions])
            .with_interactions(Arc::clone(&real))
            .scoped_to(Id::new(1));
        let without_record =
            client(&api, &[Capability::RespondToInteractions]).scoped_to(Id::new(1));

        for client in [with_record, without_record] {
            // What a module holding the client could try.
            let client = client.with_interactions(Arc::clone(&forged));
            for make in [respond, followup, edit_response] {
                let action = make("token-of-guild-2");
                let name = action.name();
                let result = client.execute(action).await;
                assert!(
                    matches!(result, Err(DiscordError::Forbidden(_))),
                    "{name}: {result:?}"
                );
            }
        }
        assert_eq!(api.actions(), []);
    }

    #[tokio::test]
    async fn a_module_cannot_carry_its_permission_from_one_guild_to_another() {
        // One module, shared by both guilds: it may respond in guild 1 but not in guild 2. It
        // keeps guild 1's client and uses it with a token it received in guild 2.
        let api = Arc::new(MockApi::default());
        let interactions = Arc::new(InteractionGuilds::default());
        interactions.register(&InteractionToken::new("token-of-guild-2"), Id::new(2));
        let allowed_in_1 = client(&api, &[Capability::RespondToInteractions])
            .with_interactions(Arc::clone(&interactions))
            .scoped_to(Id::new(1));

        let result = allowed_in_1.execute(respond("token-of-guild-2")).await;

        assert!(
            matches!(result, Err(DiscordError::Forbidden(_))),
            "{result:?}"
        );
        assert_eq!(api.actions(), []);
    }

    #[tokio::test]
    async fn without_a_record_of_interactions_a_scoped_client_answers_none() {
        let api = Arc::new(MockApi::default());
        let client = client(&api, &[Capability::RespondToInteractions]).scoped_to(Id::new(1));
        let result = client.execute(respond("anything")).await;
        assert!(
            matches!(result, Err(DiscordError::Forbidden(_))),
            "{result:?}"
        );

        // Host code (no scope) is not confined.
        let host = self::client(&api, &[Capability::RespondToInteractions]);
        host.execute(respond("anything")).await.unwrap();
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
