//! What a handler works with: who sent the interaction, in which guild and language, and the
//! ways to answer it.

use std::sync::Arc;
use std::sync::atomic::{AtomicU8, Ordering};

use closure_core::action::{Action, InteractionReply, ModalSpec};
use closure_core::client::ApiClient;
use closure_core::error::ModuleError;
use closure_core::event::{
    CommandInvocation, ComponentInvocation, GuildId, InteractionRef, ModalSubmission, UserInfo,
};
use closure_core::locale::{Args, Locale};
use closure_core::module::EventContext;
use closure_core::permission::{Actor, Level, PermissionKey};
use serde::de::DeserializeOwned;
use twilight_model::guild::Permissions;

use crate::error::UserError;
use crate::ui::CustomId;

const FRESH: u8 = 0;
const DEFERRED: u8 = 1;
const REPLIED: u8 = 2;

/// One interaction being handled: a command ([`CommandCx`]), a click ([`ComponentCx`]) or a
/// submitted modal ([`ModalCx`]).
///
/// Cheap to clone; the clones share what was answered, so the first answer is the response to
/// the interaction and the following ones are follow-ups, whoever sends them.
pub struct Cx<I> {
    ctx: EventContext,
    interaction: Arc<I>,
    state: Arc<AtomicU8>,
}

impl<I> Clone for Cx<I> {
    fn clone(&self) -> Self {
        Self {
            ctx: self.ctx.clone(),
            interaction: Arc::clone(&self.interaction),
            state: Arc::clone(&self.state),
        }
    }
}

/// A slash command being handled.
pub type CommandCx = Cx<CommandInvocation>;
/// A click on a button or a choice in a select menu.
pub type ComponentCx = Cx<ComponentInvocation>;
/// A modal the user sent.
pub type ModalCx = Cx<ModalSubmission>;

impl<I: InteractionRef> std::fmt::Debug for Cx<I> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Cx")
            .field("module", &self.ctx.module)
            .field("interaction", &self.interaction.interaction_id())
            .finish_non_exhaustive()
    }
}

impl CommandCx {
    #[must_use]
    pub fn invocation(&self) -> &CommandInvocation {
        &self.interaction
    }
}

impl ComponentCx {
    /// The click: the message it was on, the `custom_id` and what was chosen.
    #[must_use]
    pub fn click(&self) -> &ComponentInvocation {
        &self.interaction
    }

    /// What was chosen in a select menu (nothing for a button).
    #[must_use]
    pub fn values(&self) -> &[String] {
        &self.interaction.values
    }
}

impl ModalCx {
    #[must_use]
    pub fn submission(&self) -> &ModalSubmission {
        &self.interaction
    }

    /// What the user wrote in the input `name` (see [`crate::ui::text_input`]).
    #[must_use]
    pub fn field(&self, name: &str) -> Option<&str> {
        self.interaction
            .field(&format!("{}:{name}", self.ctx.module))
    }
}

impl<I: InteractionRef + 'static> Cx<I> {
    pub(crate) fn new(ctx: &EventContext, interaction: &I) -> Self
    where
        I: Clone,
    {
        Self {
            ctx: ctx.clone(),
            interaction: Arc::new(interaction.clone()),
            state: Arc::new(AtomicU8::new(FRESH)),
        }
    }

    /// The context of the event, for everything this type does not cover.
    #[must_use]
    pub fn ctx(&self) -> &EventContext {
        &self.ctx
    }

    /// The id of the module; every `custom_id` it makes starts with it.
    #[must_use]
    pub fn module(&self) -> &closure_core::module::ModuleId {
        &self.ctx.module
    }

    /// Who sent it.
    #[must_use]
    pub fn user(&self) -> &UserInfo {
        self.interaction.user()
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
        self.interaction.locale().unwrap_or(self.ctx.locale)
    }

    /// The module's configuration in this guild, as its own type.
    pub fn config<T: DeserializeOwned>(&self) -> Result<T, UserError> {
        Ok(self.ctx.parse_config()?)
    }

    /// The module's own data in this guild: entries and counters. The module needs the
    /// capability `storage` (`ModuleBuilder::capability(Capability::Storage)`).
    #[must_use]
    pub fn store(&self) -> &closure_core::storage::Store {
        self.ctx.store()
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

    /// Answers so that only the user who sent the interaction sees it.
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
            FRESH => api.respond(&*self.interaction, reply).await?,
            DEFERRED => {
                api.execute(Action::EditResponse {
                    token: self.interaction.token().clone(),
                    reply,
                })
                .await?;
            }
            _ => api.followup(&*self.interaction, reply).await?,
        }
        Ok(())
    }

    /// Acknowledges the interaction, for work that takes more than Discord's three seconds. The
    /// next answer replaces the "thinking" message. Does nothing after an answer.
    pub async fn defer(&self, ephemeral: bool) -> Result<(), UserError> {
        if self
            .state
            .compare_exchange(FRESH, DEFERRED, Ordering::AcqRel, Ordering::Acquire)
            .is_ok()
        {
            self.ctx.api.defer(&*self.interaction, ephemeral).await?;
        }
        Ok(())
    }

    /// Shows the user a form; it comes back as a [`ModalCx`] under the modal's `custom_id`.
    /// Counts as the answer to the interaction.
    pub async fn show_modal(&self, spec: ModalSpec) -> Result<(), UserError> {
        if self.state.swap(REPLIED, Ordering::AcqRel) != FRESH {
            return Err(UserError::Module(ModuleError::other(
                "a modal can only be the first answer",
            )));
        }
        self.ctx.api.modal(&*self.interaction, spec).await?;
        Ok(())
    }

    /// The user as the permission system sees them.
    pub async fn actor(&self) -> Actor {
        let owner = self.ctx.api.guild_owner().await.ok().flatten() == Some(self.user().id);
        Actor {
            user: self.user().id,
            roles: self.interaction.roles().to_vec(),
            administrator: self
                .interaction
                .invoker_permissions()
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
        let key = PermissionKey::new(key)
            .map_err(|error| UserError::Module(ModuleError::other(error.to_string())))?;
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

impl ComponentCx {
    /// What the `custom_id` says (the route, who may use it, the data), if it can be read.
    #[must_use]
    pub fn custom_id(&self) -> Option<CustomId> {
        CustomId::parse(&self.interaction.custom_id).map(|(_, id)| id)
    }

    /// The data the module put in the `custom_id`.
    #[must_use]
    pub fn data(&self) -> String {
        self.custom_id().map(|id| id.data).unwrap_or_default()
    }

    /// The page a [`Pager`](crate::ui::Pager) button asks for.
    #[must_use]
    pub fn page_requested(&self) -> Option<usize> {
        self.data().parse().ok()
    }

    /// Acknowledges the click and leaves the message as it is. Counts as the answer.
    pub async fn defer_update(&self) -> Result<(), UserError> {
        if self.state.swap(REPLIED, Ordering::AcqRel) == FRESH {
            self.ctx.api.defer_update(&*self.interaction).await?;
        }
        Ok(())
    }

    /// Answers the click by changing the message the component is on: its text, embeds and
    /// components. Counts as the answer to the interaction.
    pub async fn update(&self, reply: InteractionReply) -> Result<(), UserError> {
        if self.state.swap(REPLIED, Ordering::AcqRel) != FRESH {
            return Err(UserError::Module(ModuleError::other(
                "the message can only be updated as the first answer",
            )));
        }
        self.ctx.api.update(&*self.interaction, reply).await?;
        Ok(())
    }
}
