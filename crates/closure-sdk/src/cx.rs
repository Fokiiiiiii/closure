//! What a handler works with: who sent the interaction, in which guild and language, and the
//! ways to answer it.

use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::Duration;

use closure_core::action::{Action, InteractionReply, ModalSpec, Outcome};
use closure_core::client::ApiClient;
use closure_core::error::{DiscordError, ModuleError};
use closure_core::event::{
    CommandInvocation, ComponentInvocation, GuildId, InteractionRef, ModalSubmission, RoleId,
    UserId, UserInfo,
};
use closure_core::locale::{Args, Locale};
use closure_core::module::EventContext;
use closure_core::permission::{Actor, Level, PermissionKey};
use closure_core::schedule::ScheduledJob;
use serde::de::DeserializeOwned;
use twilight_model::guild::Permissions;

use crate::dm::{DirectMessage, Sent};
use crate::embed::{EmbedBuilder, Tone};
use crate::error::{TargetProblem, UserError};
use crate::ui::CustomId;

/// The key of the module's configuration that the SDK reads for every module: who sees its replies.
pub(crate) const REPLIES: &str = "replies";

/// Whether the administrator made the replies private (`Some(true)`) or public (`Some(false)`);
/// `None` leaves each reply as the module's code chose it.
fn replies_private(ctx: &EventContext) -> Option<bool> {
    match ctx.config().get(REPLIES)?.as_str()? {
        "private" => Some(true),
        "public" => Some(false),
        _ => None,
    }
}

/// The module's configuration as its own type. The `replies` key is taken out first, so a
/// module that denies unknown fields still reads its configuration. For handlers that get an
/// event rather than an interaction.
pub fn parse_module_config<T: DeserializeOwned>(ctx: &EventContext) -> Result<T, ModuleError> {
    let mut config = ctx.config().clone();
    if let Some(object) = config.as_object_mut() {
        object.remove(REPLIES);
    }
    serde_json::from_value(config).map_err(|error| ModuleError::Config(error.to_string()))
}

/// A reply that shows `text` as the description of one embed. Empty text makes no embed, so
/// Discord's refusal of an empty reply applies as before.
fn embed_reply(
    tone: Tone,
    text: String,
    ephemeral: bool,
) -> Result<InteractionReply, DiscordError> {
    let embeds = if text.is_empty() {
        Vec::new()
    } else {
        let embed = EmbedBuilder::new()
            .description(text)
            .color(tone.color())
            .build()
            .map_err(|error| DiscordError::Invalid(error.to_string()))?;
        vec![embed]
    };
    Ok(InteractionReply {
        embeds,
        ephemeral,
        ..InteractionReply::default()
    })
}

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

    /// The time now, in Unix seconds.
    #[must_use]
    pub fn now(&self) -> i64 {
        self.ctx.now()
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
        Ok(parse_module_config(&self.ctx)?)
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

    /// The moderation cases of this guild, numbered here and shared with the other modules. The
    /// module needs the capability `cases` (`ModuleBuilder::capability(Capability::Cases)`).
    #[must_use]
    pub fn cases(&self) -> &closure_core::cases::Cases {
        self.ctx.cases()
    }

    /// Posts a case to the log channel of the guild (see [`crate::modlog`]); does nothing when
    /// the guild has none. The module needs the capability `SendMessages` as well.
    pub async fn modlog(
        &self,
        case: &closure_core::cases::Case,
    ) -> Result<Option<closure_core::event::MessageId>, UserError> {
        crate::modlog::post(self.ctx(), case).await
    }

    /// Sends `message` privately to `user`, who has to be a member of this guild (see
    /// [`crate::dm`]). The module needs the capability `SendDirectMessages`.
    pub async fn direct_message(
        &self,
        user: UserId,
        message: DirectMessage,
    ) -> Result<Sent, UserError> {
        crate::dm::send(self.ctx(), user, message).await
    }

    /// A message of the module's catalog in the language of the answer.
    #[must_use]
    pub fn t(&self, key: &str, args: &Args) -> String {
        self.ctx.t_in(self.locale(), key, args)
    }

    pub(crate) fn core_message(&self, key: &str, args: &Args) -> String {
        self.ctx.t_core(self.locale(), key, args)
    }

    /// A length of time in the user's language, in its two largest units that are not zero.
    /// Weeks are counted in days.
    #[must_use]
    pub fn format_duration(&self, duration: Duration) -> String {
        self.format_duration_in(self.locale(), duration)
    }

    /// [`Self::format_duration`] in another language, for what is not said to the user: the
    /// server's language for a message to a member or for the log.
    #[must_use]
    pub fn format_duration_in(&self, locale: Locale, duration: Duration) -> String {
        const UNITS: [(u64, &str); 4] = [
            (86_400, "sdk-unit-day"),
            (3_600, "sdk-unit-hour"),
            (60, "sdk-unit-minute"),
            (1, "sdk-unit-second"),
        ];
        let mut seconds = duration.as_secs();
        let mut parts = Vec::new();
        for (size, key) in UNITS {
            let count = seconds / size;
            seconds %= size;
            if count > 0 && parts.len() < 2 {
                let count = i64::try_from(count).unwrap_or(i64::MAX);
                parts.push(self.ctx.t_core(locale, key, &Args::new().with("n", count)));
            }
        }
        if parts.is_empty() {
            return self.ctx.t_core(locale, "sdk-duration-none", &Args::new());
        }
        parts.join(" ")
    }

    /// Whether anything was answered yet (a deferral counts).
    #[must_use]
    pub fn has_replied(&self) -> bool {
        self.answers.answered.load(Ordering::Acquire)
    }

    /// Answers with the text as an embed where everyone in the channel sees it.
    pub async fn reply(&self, content: impl Into<String>) -> Result<(), UserError> {
        self.say(Tone::Info, content).await
    }

    /// Answers so that only the user who sent the interaction sees it.
    pub async fn reply_private(&self, content: impl Into<String>) -> Result<(), UserError> {
        self.say_private(Tone::Info, content).await
    }

    /// [`Self::reply`] with a message of the module's catalog.
    pub async fn reply_t(&self, key: &str, args: &Args) -> Result<(), UserError> {
        self.say_t(Tone::Info, key, args).await
    }

    /// [`Self::reply_private`] with a message of the module's catalog.
    pub async fn reply_private_t(&self, key: &str, args: &Args) -> Result<(), UserError> {
        self.say_private_t(Tone::Info, key, args).await
    }

    /// Answers with the text as an embed of `tone`, where everyone in the channel sees it.
    pub async fn say(&self, tone: Tone, text: impl Into<String>) -> Result<(), UserError> {
        self.reply_with(embed_reply(tone, text.into(), false)?)
            .await
    }

    /// [`Self::say`] so that only the user who sent the interaction sees it.
    pub async fn say_private(&self, tone: Tone, text: impl Into<String>) -> Result<(), UserError> {
        self.reply_with(embed_reply(tone, text.into(), true)?).await
    }

    /// [`Self::say`] with a message of the module's catalog.
    pub async fn say_t(&self, tone: Tone, key: &str, args: &Args) -> Result<(), UserError> {
        self.say(tone, self.t(key, args)).await
    }

    /// [`Self::say_private`] with a message of the module's catalog.
    pub async fn say_private_t(&self, tone: Tone, key: &str, args: &Args) -> Result<(), UserError> {
        self.say_private(tone, self.t(key, args)).await
    }

    /// Answers privately whatever the `replies` setting says; for errors and refusals.
    pub(crate) async fn say_locked(&self, tone: Tone, text: String) -> Result<(), UserError> {
        self.send(embed_reply(tone, text, true)?).await
    }

    /// The first answer is the response to the interaction (or, after [`Self::defer`], the
    /// edit of the "thinking" message); every later one is a follow-up. An answer for the user
    /// alone after a public [`Self::defer`] finishes the public message with a plain "Done." and
    /// goes to the user as a follow-up, because a public message cannot be made private. The
    /// administrator's `replies` setting, when set, decides whether the reply is private.
    pub async fn reply_with(&self, reply: InteractionReply) -> Result<(), UserError> {
        let ephemeral = replies_private(&self.ctx).unwrap_or(reply.ephemeral);
        self.send(InteractionReply { ephemeral, ..reply }).await
    }

    async fn send(&self, reply: InteractionReply) -> Result<(), UserError> {
        let mut phase = self.answers.phase.lock().await;
        // What the interaction is back to if nothing could be sent.
        let mut back_to = *phase;
        let result = match back_to {
            Phase::Fresh => self.ctx.api.respond(&*self.interaction, reply).await,
            Phase::Deferred { private: false } if reply.ephemeral => {
                let done = embed_reply(
                    Tone::Info,
                    self.core_message("sdk-done", &Args::new()),
                    false,
                )?;
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
        let ephemeral = replies_private(&self.ctx).unwrap_or(ephemeral);
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

    /// Fails unless the module may act on `user` (a kick, a ban, a timeout): not the invoker
    /// themselves, not the owner, not a bot, and not someone whose highest role is not below the
    /// invoker's (the owner is exempt from that). Someone who is not a member passes. Needs the
    /// capability `ReadGuild`.
    pub async fn check_target(&self, user: UserId) -> Result<(), UserError> {
        if user == self.user().id {
            return Err(UserError::Target(TargetProblem::SelfTarget));
        }
        if self.api().guild_owner().await? == Some(user) {
            return Err(UserError::Target(TargetProblem::Owner));
        }
        let member = match self.api().get_member(self.guild_id(), user).await {
            Ok(member) => member,
            Err(DiscordError::NotFound) => return Ok(()),
            Err(error) => return Err(error.into()),
        };
        if member.user.bot {
            return Err(UserError::Target(TargetProblem::Bot));
        }
        if self.actor().await.owner {
            return Ok(());
        }
        let Outcome::Roles(roles) = self
            .api()
            .execute(Action::ListRoles {
                guild_id: self.guild_id(),
            })
            .await?
        else {
            return Err(UserError::Module(ModuleError::other(
                "the roles of the guild were not listed",
            )));
        };
        // Without a role a member ranks below every role, so two members without roles tie.
        let highest = |ids: &[RoleId]| {
            roles
                .iter()
                .filter(|role| ids.contains(&role.id))
                .map(|role| role.position)
                .max()
                .unwrap_or(-1)
        };
        if highest(&member.roles) >= highest(self.interaction.roles()) {
            return Err(UserError::Target(TargetProblem::Higher));
        }
        Ok(())
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

    /// The time now, in Unix seconds.
    #[must_use]
    pub fn now(&self) -> i64 {
        self.ctx.now()
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

    /// The moderation cases of this guild.
    #[must_use]
    pub fn cases(&self) -> &closure_core::cases::Cases {
        self.ctx.cases()
    }

    /// Posts a case to the log channel of the guild (see [`crate::modlog`]); does nothing when
    /// the guild has none. The module needs the capability `SendMessages` as well.
    pub async fn modlog(
        &self,
        case: &closure_core::cases::Case,
    ) -> Result<Option<closure_core::event::MessageId>, UserError> {
        crate::modlog::post(&self.ctx, case).await
    }

    /// Sends `message` privately to `user`, who has to be a member of this guild (see
    /// [`crate::dm`]). The module needs the capability `SendDirectMessages`.
    pub async fn direct_message(
        &self,
        user: UserId,
        message: DirectMessage,
    ) -> Result<Sent, UserError> {
        crate::dm::send(&self.ctx, user, message).await
    }

    /// The module's configuration in this guild, as its own type.
    pub fn config<T: DeserializeOwned>(&self) -> Result<T, ModuleError> {
        parse_module_config(&self.ctx)
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
