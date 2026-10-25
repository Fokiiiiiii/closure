//! What a command handler works with.

use std::sync::Arc;
use std::sync::atomic::{AtomicU8, Ordering};

use closure_core::action::{Action, InteractionReply};
use closure_core::client::ApiClient;
use closure_core::event::{CommandInvocation, GuildId, UserInfo};
use closure_core::locale::{Args, Locale};
use closure_core::module::EventContext;
use closure_core::permission::{Actor, Level, PermissionKey};
use serde::de::DeserializeOwned;
use twilight_model::guild::Permissions;

use crate::error::UserError;

const FRESH: u8 = 0;
const DEFERRED: u8 = 1;
const REPLIED: u8 = 2;

/// One run of a command: who ran it, in which guild and language, and the ways to answer.
///
/// Cheap to clone; the clones share what was answered, so the first answer is the response to
/// the interaction and the following ones are follow-ups, whoever sends them.
#[derive(Clone)]
pub struct CommandCx {
    ctx: EventContext,
    invocation: Arc<CommandInvocation>,
    state: Arc<AtomicU8>,
}

impl std::fmt::Debug for CommandCx {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("CommandCx")
            .field("module", &self.ctx.module)
            .field("command", &self.invocation.command)
            .finish_non_exhaustive()
    }
}

impl CommandCx {
    pub(crate) fn new(ctx: &EventContext, invocation: &CommandInvocation) -> Self {
        Self {
            ctx: ctx.clone(),
            invocation: Arc::new(invocation.clone()),
            state: Arc::new(AtomicU8::new(FRESH)),
        }
    }

    /// The context of the event, for everything this type does not cover.
    #[must_use]
    pub fn ctx(&self) -> &EventContext {
        &self.ctx
    }

    #[must_use]
    pub fn invocation(&self) -> &CommandInvocation {
        &self.invocation
    }

    /// Who ran the command.
    #[must_use]
    pub fn user(&self) -> &UserInfo {
        &self.invocation.user
    }

    #[must_use]
    pub fn guild_id(&self) -> GuildId {
        self.ctx.guild_id
    }

    /// The module's Discord client, bound to this guild.
    #[must_use]
    pub fn api(&self) -> &ApiClient {
        &self.ctx.api
    }

    /// The language to answer in: the user's own, else the guild's.
    #[must_use]
    pub fn locale(&self) -> Locale {
        self.ctx.reply_locale(&self.invocation)
    }

    /// The module's own data in this guild: entries and counters. The module needs the
    /// capability `storage` (`ModuleBuilder::capability(Capability::Storage)`).
    #[must_use]
    pub fn store(&self) -> &closure_core::storage::Store {
        self.ctx.store()
    }

    /// The module's configuration in this guild, as its own type.
    pub fn config<T: DeserializeOwned>(&self) -> Result<T, UserError> {
        Ok(self.ctx.parse_config()?)
    }

    /// A message of the module's catalog in the language of the answer.
    #[must_use]
    pub fn t(&self, key: &str, args: &Args) -> String {
        self.ctx.t_in(self.locale(), key, args)
    }

    pub(crate) fn core_message(&self, key: &str, args: &Args) -> String {
        self.ctx.t_core(self.locale(), key, args)
    }

    /// Whether anything was answered yet (a deferral counts).
    #[must_use]
    pub fn has_replied(&self) -> bool {
        self.state.load(Ordering::Acquire) != FRESH
    }

    /// Answers where everyone in the channel sees it.
    pub async fn reply(&self, content: impl Into<String>) -> Result<(), UserError> {
        self.reply_with(InteractionReply {
            content: Some(content.into()),
            ..InteractionReply::default()
        })
        .await
    }

    /// Answers so that only the user who ran the command sees it.
    pub async fn reply_private(&self, content: impl Into<String>) -> Result<(), UserError> {
        self.reply_with(InteractionReply {
            content: Some(content.into()),
            ephemeral: true,
            ..InteractionReply::default()
        })
        .await
    }

    /// [`Self::reply`] with a message of the module's catalog.
    pub async fn reply_t(&self, key: &str, args: &Args) -> Result<(), UserError> {
        self.reply(self.t(key, args)).await
    }

    /// [`Self::reply_private`] with a message of the module's catalog.
    pub async fn reply_private_t(&self, key: &str, args: &Args) -> Result<(), UserError> {
        self.reply_private(self.t(key, args)).await
    }

    /// The first answer is the response to the interaction (or, after [`Self::defer`], the
    /// edit of the "thinking" message); every later one is a follow-up.
    pub async fn reply_with(&self, reply: InteractionReply) -> Result<(), UserError> {
        let api = &self.ctx.api;
        match self.state.swap(REPLIED, Ordering::AcqRel) {
            FRESH => api.respond(&self.invocation, reply).await?,
            DEFERRED => {
                api.execute(Action::EditResponse {
                    token: self.invocation.token.clone(),
                    reply,
                })
                .await?;
            }
            _ => api.followup(&self.invocation, reply).await?,
        }
        Ok(())
    }

    /// Acknowledges the command, for work that takes more than Discord's three seconds. The
    /// next answer replaces the "thinking" message. Does nothing after an answer.
    pub async fn defer(&self, ephemeral: bool) -> Result<(), UserError> {
        if self
            .state
            .compare_exchange(FRESH, DEFERRED, Ordering::AcqRel, Ordering::Acquire)
            .is_ok()
        {
            self.ctx.api.defer(&self.invocation, ephemeral).await?;
        }
        Ok(())
    }

    /// The user as the permission system sees them.
    pub async fn actor(&self) -> Actor {
        let owner = self.ctx.api.guild_owner().await.ok().flatten() == Some(self.user().id);
        Actor {
            user: self.user().id,
            roles: self.invocation.roles.clone(),
            administrator: self
                .invocation
                .invoker_permissions
                .contains(Permissions::ADMINISTRATOR),
            owner,
        }
    }

    /// The user's level in this guild.
    pub async fn level(&self) -> Level {
        self.ctx.permissions().level_of(&self.actor().await)
    }

    /// Fails with a message to the user unless they hold the permission `key`.
    pub async fn require(&self, key: &str) -> Result<(), UserError> {
        let key = PermissionKey::new(key).map_err(|error| {
            UserError::Module(closure_core::error::ModuleError::other(error.to_string()))
        })?;
        self.require_key(&key).await
    }

    pub(crate) async fn require_key(&self, key: &PermissionKey) -> Result<(), UserError> {
        let decision = self.ctx.permissions().check(&self.actor().await, key);
        if decision.allowed {
            Ok(())
        } else {
            Err(UserError::PermissionDenied(decision))
        }
    }
}
