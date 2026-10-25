//! What a handler works with: who sent the interaction, in which guild and language, and the
//! ways to answer it.

use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};

use closure_core::action::{Action, InteractionReply, ModalSpec};
use closure_core::client::ApiClient;
use closure_core::error::{DiscordError, ModuleError};
use closure_core::event::{
    CommandInvocation, ComponentInvocation, GuildId, InteractionRef, ModalSubmission, UserInfo,
};
use closure_core::locale::{Args, Locale};
use closure_core::module::EventContext;
use closure_core::permission::{Actor, Level, PermissionKey};
use closure_core::schedule::ScheduledJob;
use serde::de::DeserializeOwned;
use twilight_model::guild::Permissions;

use crate::error::UserError;
use crate::ui::CustomId;

/// Where the answer to one interaction stands.
#[derive(Clone, Copy, PartialEq, Eq)]
enum Phase {
    /// Nothing was answered.
    Fresh,
    /// Acknowledged with a "thinking" message that the next answer fills in; `private` says whether
    /// only the user sees it (Discord cannot change that afterwards).
    Deferred { private: bool },
    /// A click was acknowledged and the message left as it is; the next answer edits it.
    DeferredUpdate,
    /// The interaction has its answer; more are follow-ups.
    Replied,
}

/// The answers given to one interaction. They take their turns: one is sent and acknowledged by
/// Discord before the next is started, whoever of the clones of a [`Cx`] makes it.
struct Answers {
    phase: futures_util::lock::Mutex<Phase>,
    /// Whether anything was answered; readable without waiting for a send in progress.
    answered: AtomicBool,
}

impl Answers {
    fn new() -> Self {
        Self {
            phase: futures_util::lock::Mutex::new(Phase::Fresh),
            answered: AtomicBool::new(false),
        }
    }

    /// Records how a send went. A send that was refused before any request was made (the content
    /// is too long, ...) changes nothing: the interaction is as unanswered as it was.
    fn settle(
        &self,
        phase: &mut Phase,
        before: Phase,
        after: Phase,
        result: &Result<(), DiscordError>,
    ) {
        // A deferred interaction must stay deferred after any failure: the next try has to close
        // the public message again, not answer on top of it.
        let keep = match before {
            Phase::Deferred { .. } | Phase::DeferredUpdate => result.is_err(),
            _ => matches!(result, Err(DiscordError::Invalid(_))),
        };
        *phase = if keep { before } else { after };
        self.answered
            .store(*phase != Phase::Fresh, Ordering::Release);
    }
}

/// One interaction being handled: a command ([`CommandCx`]), a click ([`ComponentCx`]) or a
/// submitted modal ([`ModalCx`]).
///
/// Cheap to clone; the clones share what was answered, so the first answer is the response to
/// the interaction and the following ones are follow-ups, whoever sends them.
pub struct Cx<I> {
    ctx: EventContext,
    interaction: Arc<I>,
    answers: Arc<Answers>,
}

impl<I> Clone for Cx<I> {
    fn clone(&self) -> Self {
        Self {
            ctx: self.ctx.clone(),
            interaction: Arc::clone(&self.interaction),
            answers: Arc::clone(&self.answers),
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

    /// Shows the user a form; it comes back as a [`ModalCx`] under the modal's `custom_id`.
    /// Counts as the answer to the interaction. A submitted form cannot be answered with another
    /// form (Discord refuses it), so [`ModalCx`] has no `show_modal`.
    pub async fn show_modal(&self, spec: ModalSpec) -> Result<(), UserError> {
        self.show_modal_inner(spec).await
    }
}

impl ComponentCx {
    /// The click: the message it was on, the `custom_id` and what was chosen.
    #[must_use]
    pub fn click(&self) -> &ComponentInvocation {
        &self.interaction
    }

    /// Shows the user a form; it comes back as a [`ModalCx`] under the modal's `custom_id`.
    /// Counts as the answer to the interaction. A submitted form cannot be answered with another
    /// form (Discord refuses it), so [`ModalCx`] has no `show_modal`.
    pub async fn show_modal(&self, spec: ModalSpec) -> Result<(), UserError> {
        self.show_modal_inner(spec).await
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
            answers: Arc::new(Answers::new()),
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

    /// Work the module has done later (`ModuleBuilder::job` handles it when it is due).
    #[must_use]
    pub fn scheduler(&self) -> &closure_core::schedule::Scheduler {
        self.ctx.scheduler()
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
        self.answers.answered.load(Ordering::Acquire)
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
    /// edit of the "thinking" message); every later one is a follow-up. An answer for the user
    /// alone after a public [`Self::defer`] finishes the public message with a plain "Done." and
    /// goes to the user as a follow-up, because a public message cannot be made private.
    pub async fn reply_with(&self, reply: InteractionReply) -> Result<(), UserError> {
        let mut phase = self.answers.phase.lock().await;
        // What the interaction is back to if nothing could be sent.
        let mut back_to = *phase;
        let result = match back_to {
            Phase::Fresh => self.ctx.api.respond(&*self.interaction, reply).await,
            Phase::Deferred { private: false } if reply.ephemeral => {
                let done = InteractionReply {
                    content: Some(self.core_message("sdk-done", &Args::new())),
                    ..InteractionReply::default()
                };
                match self.edit_original(done).await {
                    Ok(()) => {
                        back_to = Phase::Replied;
                        self.ctx.api.followup(&*self.interaction, reply).await
                    }
                    Err(error) => Err(error),
                }
            }
            // The message of a click belongs to everyone, so a private answer cannot edit it.
            Phase::DeferredUpdate if reply.ephemeral => {
                self.ctx.api.followup(&*self.interaction, reply).await
            }
            Phase::Deferred { .. } | Phase::DeferredUpdate => self.edit_original(reply).await,
            Phase::Replied => self.ctx.api.followup(&*self.interaction, reply).await,
        };
        self.answers
            .settle(&mut phase, back_to, Phase::Replied, &result);
        Ok(result?)
    }

    async fn edit_original(&self, reply: InteractionReply) -> Result<(), DiscordError> {
        self.ctx
            .api
            .execute(Action::EditResponse {
                token: self.interaction.token().clone(),
                reply,
            })
            .await
            .map(drop)
    }

    /// Acknowledges the interaction, for work that takes more than Discord's three seconds. The
    /// next answer replaces the "thinking" message. Does nothing after an answer.
    pub async fn defer(&self, ephemeral: bool) -> Result<(), UserError> {
        let mut phase = self.answers.phase.lock().await;
        if *phase != Phase::Fresh {
            return Ok(());
        }
        let result = self.ctx.api.defer(&*self.interaction, ephemeral).await;
        self.answers.settle(
            &mut phase,
            Phase::Fresh,
            Phase::Deferred { private: ephemeral },
            &result,
        );
        Ok(result?)
    }

    async fn show_modal_inner(&self, spec: ModalSpec) -> Result<(), UserError> {
        let mut phase = self.answers.phase.lock().await;
        if *phase != Phase::Fresh {
            return Err(UserError::Module(ModuleError::other(
                "a modal can only be the first answer",
            )));
        }
        let result = self.ctx.api.modal(&*self.interaction, spec).await;
        self.answers
            .settle(&mut phase, Phase::Fresh, Phase::Replied, &result);
        Ok(result?)
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
        if key.module() != self.ctx.module.as_str() {
            return Err(UserError::Module(ModuleError::other(format!(
                "a module asks only about its own permission keys, not `{key}`"
            ))));
        }
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

/// A job the module scheduled, now due.
#[derive(Clone)]
pub struct JobCx {
    ctx: EventContext,
    job: ScheduledJob,
}

impl std::fmt::Debug for JobCx {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("JobCx")
            .field("module", &self.ctx.module)
            .field("job", &self.job.name)
            .finish_non_exhaustive()
    }
}

impl JobCx {
    pub(crate) fn new(ctx: &EventContext, job: &ScheduledJob) -> Self {
        Self {
            ctx: ctx.clone(),
            job: job.clone(),
        }
    }

    /// The name the job was scheduled under.
    #[must_use]
    pub fn name(&self) -> &str {
        &self.job.name
    }

    /// Which try this is: 1 the first time. A job runs again if the handler fails (or the bot
    /// stopped meanwhile), so a handler has to be safe to run twice.
    #[must_use]
    pub fn attempt(&self) -> u32 {
        self.job.attempt
    }

    /// The data the job was scheduled with, as its own type.
    pub fn payload<T: DeserializeOwned>(&self) -> Result<T, ModuleError> {
        serde_json::from_value(self.job.payload.clone()).map_err(|error| {
            ModuleError::other(format!("the payload of job `{}`: {error}", self.job.name))
        })
    }

    #[must_use]
    pub fn guild_id(&self) -> GuildId {
        self.ctx.guild_id
    }

    /// The context of the event, for everything this type does not cover.
    #[must_use]
    pub fn ctx(&self) -> &EventContext {
        &self.ctx
    }

    /// The module's Discord client, bound to this guild.
    #[must_use]
    pub fn api(&self) -> &ApiClient {
        &self.ctx.api
    }

    /// The module's own data in this guild.
    #[must_use]
    pub fn store(&self) -> &closure_core::storage::Store {
        self.ctx.store()
    }

    /// Schedules more work, in this guild.
    #[must_use]
    pub fn scheduler(&self) -> &closure_core::schedule::Scheduler {
        self.ctx.scheduler()
    }

    /// The module's configuration in this guild, as its own type.
    pub fn config<T: DeserializeOwned>(&self) -> Result<T, ModuleError> {
        self.ctx.parse_config()
    }

    /// A message of the module's catalog in the guild's language.
    #[must_use]
    pub fn t(&self, key: &str, args: &Args) -> String {
        self.ctx.t_in(self.ctx.locale, key, args)
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

    /// Acknowledges the click and leaves the message as it is. Counts as the answer; the next
    /// one ([`Self::update`] or a reply) changes the message afterwards.
    pub async fn defer_update(&self) -> Result<(), UserError> {
        let mut phase = self.answers.phase.lock().await;
        if *phase != Phase::Fresh {
            return Ok(());
        }
        let result = self.ctx.api.defer_update(&*self.interaction).await;
        self.answers
            .settle(&mut phase, Phase::Fresh, Phase::DeferredUpdate, &result);
        Ok(result?)
    }

    /// Answers the click by changing the message the component is on: its text, embeds and
    /// components (those the reply does not have are taken off). Works as the first answer, or
    /// after [`Self::defer_update`].
    /// Refuses a private reply, since the message everyone sees cannot be private ([`Self::reply_private`]).
    pub async fn update(&self, reply: InteractionReply) -> Result<(), UserError> {
        if reply.ephemeral {
            return Err(UserError::Module(ModuleError::other(
                "an update changes the message the component is on, which everyone sees; answer privately with reply_private",
            )));
        }
        let mut phase = self.answers.phase.lock().await;
        let before = *phase;
        let result = match before {
            Phase::Fresh => self.ctx.api.update(&*self.interaction, reply).await,
            Phase::DeferredUpdate => self.edit_original(reply).await,
            Phase::Deferred { .. } | Phase::Replied => {
                return Err(UserError::Module(ModuleError::other(
                    "the message can only be updated as the first answer or after defer_update",
                )));
            }
        };
        self.answers
            .settle(&mut phase, before, Phase::Replied, &result);
        Ok(result?)
    }
}
