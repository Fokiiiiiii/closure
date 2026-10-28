//! Running the commands of a module in a test, without Discord.
//!
//! ```ignore
//! let harness = Harness::new(my_module_builder())?;
//! harness.permissions(|p| { p.role_levels.insert(Id::new(10), Level::MODERATOR); });
//! let replies = harness
//!     .invoke(Invocation::new("warn").roles([10]).option("user", CommandOptionValue::User(Id::new(7))))
//!     .await?;
//! assert_eq!(replies[0].content, "Warned.");
//! ```

use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex, PoisonError};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use closure_core::action::{Action, InteractionReply, InteractionResponse};
use closure_core::cases::{Case, CaseBackend, Cases, MemoryCases};
use closure_core::client::ApiClient;
use closure_core::error::ModuleError;
use closure_core::event::{
    AttachmentInfo, AutocompleteInvocation, CommandInvocation, CommandOption, CommandOptionValue,
    ComponentInvocation, ContextInvocation, ContextTarget, Event, GuildId, InteractionToken,
    MessageInfo, ModalSubmission, RolesChange, ThreadInfo, UserId, UserInfo,
};
use closure_core::history::{
    HistoryBackend, HistoryError, MemoryHistory, MessageHistory, SavedMessage,
};
use closure_core::interaction::InteractionGuilds;
use closure_core::latency::GatewayLatency;
use closure_core::locale::{Locale, Translator};
use closure_core::module::{EventContext, Module};
use closure_core::permission::GuildPermissions;
use closure_core::registry::ModuleRegistry;
use closure_core::schedule::{MemoryScheduler, Scheduler, SchedulerBackend, backoff};
use closure_core::settings::GuildPolicy;
use closure_core::storage::{MemoryStorage, StorageBackend, Store};
use closure_core::subscription::Flow;
use closure_core::testing::MockApi;
use closure_i18n::TranslatorBuilder;
use twilight_model::channel::message::{Component, Embed};
use twilight_model::guild::Permissions;
use twilight_model::id::Id;

use crate::module::{Choice, ModuleBuilder, SdkModule};

/// One use of a command, as a test describes it.
#[derive(Debug, Clone)]
pub struct Invocation {
    command: String,
    subcommand: Option<String>,
    options: Vec<CommandOption>,
    user: u64,
    roles: Vec<u64>,
    administrator: bool,
    owner: bool,
    locale: Option<Locale>,
    channel: u64,
}

impl Invocation {
    #[must_use]
    pub fn new(command: &str) -> Self {
        Self {
            command: command.to_owned(),
            subcommand: None,
            options: Vec::new(),
            user: 100,
            roles: Vec::new(),
            administrator: false,
            owner: false,
            locale: None,
            channel: 5,
        }
    }

    #[must_use]
    pub fn subcommand(mut self, name: &str) -> Self {
        self.subcommand = Some(name.to_owned());
        self
    }

    #[must_use]
    pub fn option(mut self, name: &str, value: CommandOptionValue) -> Self {
        self.options.push(CommandOption {
            name: name.to_owned(),
            value,
        });
        self
    }

    /// Who runs it (default 100).
    #[must_use]
    pub fn user(mut self, id: u64) -> Self {
        self.user = id;
        self
    }

    #[must_use]
    pub fn roles(mut self, roles: impl IntoIterator<Item = u64>) -> Self {
        self.roles = roles.into_iter().collect();
        self
    }

    /// The user holds Discord's Administrator permission.
    #[must_use]
    pub fn administrator(mut self) -> Self {
        self.administrator = true;
        self
    }

    /// The user owns the server.
    #[must_use]
    pub fn owner(mut self) -> Self {
        self.owner = true;
        self
    }

    /// The user's own language (the guild's is English).
    #[must_use]
    pub fn locale(mut self, locale: Locale) -> Self {
        self.locale = Some(locale);
        self
    }

    #[must_use]
    pub fn channel(mut self, id: u64) -> Self {
        self.channel = id;
        self
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ReplyKind {
    /// The response to the interaction.
    Response,
    /// The "thinking" acknowledgement.
    Deferred,
    /// The edit of the original response (after a deferral).
    Edit,
    Followup,
    /// The message a click was on, changed.
    Update,
    /// A click acknowledged, the message left as it is.
    Acknowledged,
    /// A form shown to the user; `content` is its title.
    Modal,
}

/// What the module sent back.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Reply {
    pub kind: ReplyKind,
    /// The text: the content of the message, else the description of its first embed (where
    /// the SDK's replies put their text), else empty.
    pub content: String,
    pub ephemeral: bool,
    /// Buttons and menus (for a modal: its inputs).
    pub components: Vec<Component>,
    /// The embeds of the message.
    pub embeds: Vec<Embed>,
}

impl Reply {
    /// The `custom_id` of every component of the reply, in order.
    #[must_use]
    pub fn custom_ids(&self) -> Vec<String> {
        Action::Respond {
            interaction_id: Id::new(1),
            token: InteractionToken::new("t"),
            response: InteractionResponse::Message {
                reply: InteractionReply {
                    components: self.components.clone(),
                    ..InteractionReply::default()
                },
            },
        }
        .custom_ids()
        .into_iter()
        .map(str::to_owned)
        .collect()
    }
}

/// A click on a button or a choice in a select menu, as a test describes it.
#[derive(Debug, Clone)]
pub struct Click {
    custom_id: String,
    values: Vec<String>,
    user: u64,
    roles: Vec<u64>,
    administrator: bool,
}

impl Click {
    /// A click on the component with this `custom_id` (see [`crate::CustomId`]).
    #[must_use]
    pub fn new(custom_id: impl Into<String>) -> Self {
        Self {
            custom_id: custom_id.into(),
            values: Vec::new(),
            user: 100,
            roles: Vec::new(),
            administrator: false,
        }
    }

    /// What was chosen in a select menu.
    #[must_use]
    pub fn values(mut self, values: impl IntoIterator<Item = &'static str>) -> Self {
        self.values = values.into_iter().map(str::to_owned).collect();
        self
    }

    #[must_use]
    pub fn user(mut self, id: u64) -> Self {
        self.user = id;
        self
    }

    #[must_use]
    pub fn roles(mut self, roles: impl IntoIterator<Item = u64>) -> Self {
        self.roles = roles.into_iter().collect();
        self
    }

    #[must_use]
    pub fn administrator(mut self) -> Self {
        self.administrator = true;
        self
    }
}

/// A form that was filled in and sent.
#[derive(Debug, Clone)]
pub struct Submit {
    custom_id: String,
    fields: Vec<(String, String)>,
    user: u64,
}

impl Submit {
    #[must_use]
    pub fn new(custom_id: impl Into<String>) -> Self {
        Self {
            custom_id: custom_id.into(),
            fields: Vec::new(),
            user: 100,
        }
    }

    /// The text written in the input `custom_id`.
    #[must_use]
    pub fn field(mut self, custom_id: &str, value: &str) -> Self {
        self.fields.push((custom_id.to_owned(), value.to_owned()));
        self
    }

    #[must_use]
    pub fn user(mut self, id: u64) -> Self {
        self.user = id;
        self
    }
}

/// A context-menu command used on a user or a message, as a test describes it.
#[derive(Debug, Clone)]
pub struct ContextUse {
    command: String,
    target: ContextTarget,
    user: u64,
    roles: Vec<u64>,
    administrator: bool,
}

impl ContextUse {
    /// The command `command`, used on the user `target`.
    #[must_use]
    pub fn on_user(command: &str, target: u64) -> Self {
        Self::new(command, ContextTarget::User(tester(target)))
    }

    /// The command `command`, used on message `message`, which `author` posted with `content`.
    #[must_use]
    pub fn on_message(command: &str, message: u64, author: u64, content: &str) -> Self {
        let info = MessageInfo {
            removed_attachments: Vec::new(),
            id: Id::new(message),
            channel_id: Id::new(5),
            guild_id: Id::new(GUILD),
            author: tester(author),
            author_roles: Vec::new(),
            content: content.to_owned(),
            mentioned_users: Vec::new(),
            mentioned_roles: Vec::new(),
            mentions_everyone: false,
            attachments: Vec::new(),
            embed_count: 0,
            sticker_count: 0,
            reply_to: None,
            from_webhook: false,
            created_at: 0,
            edited_at: None,
        };
        Self::new(command, ContextTarget::Message(info))
    }

    fn new(command: &str, target: ContextTarget) -> Self {
        Self {
            command: command.to_owned(),
            target,
            user: 100,
            roles: Vec::new(),
            administrator: false,
        }
    }

    /// Who uses the command (default 100).
    #[must_use]
    pub fn user(mut self, id: u64) -> Self {
        self.user = id;
        self
    }

    #[must_use]
    pub fn roles(mut self, roles: impl IntoIterator<Item = u64>) -> Self {
        self.roles = roles.into_iter().collect();
        self
    }

    /// The user holds Discord's Administrator permission.
    #[must_use]
    pub fn administrator(mut self) -> Self {
        self.administrator = true;
        self
    }
}

/// A user typing in an option of a command, as a test describes it.
#[derive(Debug, Clone)]
pub struct Autocomplete {
    command: String,
    subcommand: Option<String>,
    focused_option: String,
    focused: String,
    options: Vec<CommandOption>,
    user: u64,
    roles: Vec<u64>,
    administrator: bool,
}

impl Autocomplete {
    /// The option `focused_option` of `command` holds `typed` so far.
    #[must_use]
    pub fn new(command: &str, focused_option: &str, typed: &str) -> Self {
        Self {
            command: command.to_owned(),
            subcommand: None,
            focused_option: focused_option.to_owned(),
            focused: typed.to_owned(),
            options: Vec::new(),
            user: 100,
            roles: Vec::new(),
            administrator: false,
        }
    }

    #[must_use]
    pub fn subcommand(mut self, name: &str) -> Self {
        self.subcommand = Some(name.to_owned());
        self
    }

    /// Another option the user already filled in.
    #[must_use]
    pub fn option(mut self, name: &str, value: CommandOptionValue) -> Self {
        self.options.push(CommandOption {
            name: name.to_owned(),
            value,
        });
        self
    }

    /// Who types (default 100).
    #[must_use]
    pub fn user(mut self, id: u64) -> Self {
        self.user = id;
        self
    }

    #[must_use]
    pub fn roles(mut self, roles: impl IntoIterator<Item = u64>) -> Self {
        self.roles = roles.into_iter().collect();
        self
    }

    /// The user holds Discord's Administrator permission.
    #[must_use]
    pub fn administrator(mut self) -> Self {
        self.administrator = true;
        self
    }
}

/// A message a member posted in the guild, as a test describes it.
#[derive(Debug, Clone)]
pub struct Message {
    author: u64,
    bot: bool,
    channel: u64,
    roles: Vec<u64>,
    content: String,
    mentioned_users: Vec<u64>,
    mentioned_roles: Vec<u64>,
    mentions_everyone: bool,
    attachments: Vec<AttachmentInfo>,
}

impl Message {
    /// A message from `author` in `channel`, with no content, mentions or attachments.
    #[must_use]
    pub fn new(author: u64, channel: u64) -> Self {
        Self {
            author,
            bot: false,
            channel,
            roles: Vec::new(),
            content: String::new(),
            mentioned_users: Vec::new(),
            mentioned_roles: Vec::new(),
            mentions_everyone: false,
            attachments: Vec::new(),
        }
    }

    #[must_use]
    pub fn content(mut self, content: impl Into<String>) -> Self {
        self.content = content.into();
        self
    }

    /// The author is a bot.
    #[must_use]
    pub fn bot(mut self) -> Self {
        self.bot = true;
        self
    }

    /// The roles the author holds.
    #[must_use]
    pub fn roles(mut self, roles: impl IntoIterator<Item = u64>) -> Self {
        self.roles = roles.into_iter().collect();
        self
    }

    #[must_use]
    pub fn mention(mut self, user: u64) -> Self {
        self.mentioned_users.push(user);
        self
    }

    #[must_use]
    pub fn mention_role(mut self, role: u64) -> Self {
        self.mentioned_roles.push(role);
        self
    }

    #[must_use]
    pub fn mention_everyone(mut self) -> Self {
        self.mentions_everyone = true;
        self
    }

    /// Attaches a file of `size` bytes, with the id it has in the message (counted from 1).
    #[must_use]
    pub fn attachment(mut self, filename: &str, size: u64) -> Self {
        let id = u64::try_from(self.attachments.len() + 1).unwrap_or(u64::MAX);
        self.attachments.push(AttachmentInfo {
            id,
            filename: filename.to_owned(),
            size,
            content_type: None,
            url: format!("https://cdn.test/{id}/{filename}"),
        });
        self
    }
}

/// A module, its guild and a pretend Discord.
pub struct Harness {
    module: Arc<SdkModule>,
    api: Arc<MockApi>,
    translator: Arc<dyn Translator>,
    interactions: Arc<InteractionGuilds>,
    permissions: Mutex<GuildPermissions>,
    config: Mutex<serde_json::Value>,
    storage: Arc<MemoryStorage>,
    scheduler: Arc<MemoryScheduler>,
    cases: Arc<MemoryCases>,
    history: Arc<MemoryHistory>,
    clock: Arc<dyn Fn() -> i64 + Send + Sync>,
    elapsed: Arc<Mutex<Duration>>,
    policy: Mutex<GuildPolicy>,
    locale: Mutex<Locale>,
    next: AtomicU64,
    bot: Mutex<Option<UserId>>,
    gateway: Arc<GatewayLatency>,
}

impl std::fmt::Debug for Harness {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Harness").finish_non_exhaustive()
    }
}

/// The guild every invocation happens in.
const GUILD: u64 = 1;

/// Who owns the server when the invocation does not say its user does.
const OTHER_OWNER: u64 = 999_999_999;

/// The bot's own user id (its application id) in every run, until a test changes it.
const BOT: u64 = 9_999;

impl Harness {
    /// Builds the module on a clock the test controls ([`Self::advance`]) and makes its
    /// messages available in every language it has a catalog for.
    pub fn new(builder: ModuleBuilder) -> Result<Self, Box<dyn std::error::Error + Send + Sync>> {
        let elapsed = Arc::new(Mutex::new(Duration::ZERO));
        let start = Instant::now();
        let clock_elapsed = Arc::clone(&elapsed);
        let module = Arc::new(
            builder
                .clock(move || {
                    start + *clock_elapsed.lock().unwrap_or_else(PoisonError::into_inner)
                })
                .build()?,
        );
        // What the module keeps lives in memory, and time passes for it as for the cooldowns.
        let unix_start = SystemTime::now().duration_since(UNIX_EPOCH)?.as_secs();
        let unix_now = {
            let elapsed = Arc::clone(&elapsed);
            move || {
                let passed = elapsed
                    .lock()
                    .unwrap_or_else(PoisonError::into_inner)
                    .as_secs();
                i64::try_from(unix_start + passed).unwrap_or(i64::MAX)
            }
        };
        let clock: Arc<dyn Fn() -> i64 + Send + Sync> = Arc::new(unix_now.clone());
        let storage = Arc::new(MemoryStorage::with_clock(unix_now.clone()));
        let cases = Arc::new(MemoryCases::with_clock(unix_now.clone()));
        let history = Arc::new(MemoryHistory::default());
        let scheduler = Arc::new(MemoryScheduler::with_clock(unix_now));
        let registry = ModuleRegistry::new(vec![Arc::clone(&module) as Arc<dyn Module>])?;
        let mut catalogs = TranslatorBuilder::new()?;
        catalogs.add_registry(&registry)?;

        let manifest = module.manifest();
        let mut permissions = GuildPermissions::default();
        for spec in &manifest.permissions {
            permissions
                .defaults
                .insert(spec.key.clone(), spec.default_level);
        }
        Ok(Self {
            api: Arc::new(MockApi::default()),
            translator: Arc::new(catalogs.build()),
            interactions: Arc::new(InteractionGuilds::default()),
            permissions: Mutex::new(permissions),
            config: Mutex::new(manifest.default_config.clone()),
            storage,
            scheduler,
            cases,
            history,
            clock,
            elapsed,
            policy: Mutex::new(GuildPolicy::default()),
            locale: Mutex::new(Locale::DEFAULT),
            next: AtomicU64::new(1),
            bot: Mutex::new(Some(Id::new(BOT))),
            gateway: Arc::default(),
            module,
        })
    }

    /// Changes the guild's permissions (levels of roles and users, rules for keys).
    pub fn permissions(&self, change: impl FnOnce(&mut GuildPermissions)) {
        change(
            &mut self
                .permissions
                .lock()
                .unwrap_or_else(PoisonError::into_inner),
        );
    }

    /// Sets the module's configuration in the guild.
    pub fn set_config(&self, config: serde_json::Value) {
        *self.config.lock().unwrap_or_else(PoisonError::into_inner) = config;
    }

    /// Sets the guild's log channel. The channel exists in the guild, as Discord would have it.
    pub fn set_log_channel(&self, channel: u64) {
        self.api.add_channel(Id::new(channel), Some(Id::new(GUILD)));
        self.policy
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .log_channel = Some(Id::new(channel));
    }

    /// Replaces the guild's policy (its log channel, moderator roles, time zone, prefix and emojis).
    pub fn set_policy(&self, policy: GuildPolicy) {
        *self.policy.lock().unwrap_or_else(PoisonError::into_inner) = policy;
    }

    /// Sets the guild's language (English when not set).
    pub fn set_guild_locale(&self, locale: Locale) {
        *self.locale.lock().unwrap_or_else(PoisonError::into_inner) = locale;
    }

    /// The bot's own user id that the module's contexts carry (`9999` unless changed); `None`
    /// when the host does not know it.
    #[must_use]
    pub fn bot_id(&self) -> Option<UserId> {
        *self.bot.lock().unwrap_or_else(PoisonError::into_inner)
    }

    /// Changes, or with `None` removes, the bot's own user id.
    pub fn set_bot_id(&self, bot: Option<UserId>) {
        *self.bot.lock().unwrap_or_else(PoisonError::into_inner) = bot;
    }

    /// Records one heartbeat round trip of `shard`, as the Gateway does for a live connection.
    pub fn set_gateway_latency(&self, shard: u64, latency: Duration) {
        self.gateway.record(shard, latency);
    }

    /// Lets time pass for the cooldowns.
    pub fn advance(&self, by: Duration) {
        *self.elapsed.lock().unwrap_or_else(PoisonError::into_inner) += by;
    }

    /// What the module keeps, as the module sees it in the guild of the invocations.
    #[must_use]
    pub fn store(&self) -> Store {
        let manifest = self.module.manifest();
        Store::new(
            Arc::clone(&self.storage) as Arc<dyn StorageBackend>,
            &manifest.capabilities,
            manifest.id.clone(),
            Id::new(GUILD),
        )
    }

    /// The module's moderation cases in the guild of the invocations.
    #[must_use]
    pub fn cases(&self) -> Cases {
        let manifest = self.module.manifest();
        Cases::new(
            Arc::clone(&self.cases) as Arc<dyn CaseBackend>,
            &manifest.capabilities,
            manifest.id.clone(),
            Id::new(GUILD),
        )
    }

    /// The module's message history in the guild of the invocations.
    #[must_use]
    pub fn history(&self) -> MessageHistory {
        let manifest = self.module.manifest();
        MessageHistory::new(
            Arc::clone(&self.history) as Arc<dyn HistoryBackend>,
            &manifest.capabilities,
            manifest.id.clone(),
            Id::new(GUILD),
        )
    }

    /// Keeps `message` in the history of the guild of the invocations, as the host would have
    /// recorded it. Its `guild_id` is set to that guild.
    pub async fn seed_message(&self, mut message: SavedMessage) -> Result<(), HistoryError> {
        message.guild_id = Id::new(GUILD);
        self.history.save(&message).await
    }

    /// The module's scheduler in the guild of the invocations, to set up jobs a test starts from.
    #[must_use]
    pub fn scheduler(&self) -> Scheduler {
        let manifest = self.module.manifest();
        Scheduler::new(
            Arc::clone(&self.scheduler) as Arc<dyn SchedulerBackend>,
            &manifest.capabilities,
            manifest.id.clone(),
            Id::new(GUILD),
        )
    }

    /// How many jobs are waiting (or were given up on).
    #[must_use]
    pub fn jobs(&self) -> usize {
        self.scheduler.len()
    }

    /// Runs the jobs that are due now, the way the host does: a job that succeeds is done (a
    /// repeating one is set for its next time), one that fails comes again after its pause.
    /// Returns how each job ended, in the order they were due.
    pub async fn run_due_jobs(&self) -> Vec<Result<(), ModuleError>> {
        let due = self
            .scheduler
            .claim_due(1000, Duration::from_secs(300))
            .await
            .unwrap_or_default();
        let mut ended = Vec::new();
        for job in due {
            let (id, token, attempt) = (job.id, job.token, job.attempt);
            let ctx = self.context(job.guild_id);
            let result = self
                .module
                .on_event_flow(&ctx, &Event::Scheduled(job))
                .await;
            let recorded = match &result {
                Ok(Flow::Stop) => self.scheduler.discard(id, token).await,
                Ok(Flow::Continue) => self.scheduler.complete(id, token).await,
                Err(error) => {
                    self.scheduler
                        .retry(id, token, backoff(attempt), &error.to_string())
                        .await
                }
            };
            drop(recorded);
            ended.push(result.map(drop));
        }
        ended
    }

    /// The pretend Discord, to look at what else the module did.
    #[must_use]
    pub fn api(&self) -> &Arc<MockApi> {
        &self.api
    }

    /// Keeps a file of an attachment of a message of the harness's guild, as the host does.
    pub async fn seed_file(
        &self,
        channel: u64,
        message: u64,
        attachment: closure_core::event::AttachmentInfo,
        data: Option<Vec<u8>>,
    ) -> Result<(), closure_core::history::HistoryError> {
        closure_core::history::HistoryBackend::save_file(
            self.history.as_ref(),
            Id::new(GUILD),
            Id::new(channel),
            Id::new(message),
            &attachment,
            data.as_deref(),
            0,
        )
        .await
    }

    pub fn actions(&self) -> Vec<Action> {
        self.api.actions()
    }

    /// The embeds of every message the module sent, in order.
    #[must_use]
    pub fn sent_embeds(&self) -> Vec<Embed> {
        self.api
            .actions()
            .into_iter()
            .filter_map(|action| match action {
                Action::SendMessage { embeds, .. } => Some(embeds),
                _ => None,
            })
            .flatten()
            .collect()
    }

    /// What the module gets with an event of `guild`.
    fn context(&self, guild: GuildId) -> EventContext {
        let manifest = self.module.manifest();
        let client = ApiClient::new(
            Arc::clone(&self.api) as _,
            manifest.capabilities.clone(),
            manifest.id.clone(),
        )
        .with_interactions(Arc::clone(&self.interactions))
        .scoped_to(guild);
        let permissions = self
            .permissions
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .clone();
        let config = self
            .config
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .clone();
        let context = EventContext::new(
            manifest.id.clone(),
            guild,
            client,
            *self.locale.lock().unwrap_or_else(PoisonError::into_inner),
            Arc::new(
                self.policy
                    .lock()
                    .unwrap_or_else(PoisonError::into_inner)
                    .clone(),
            ),
            Arc::new(config),
            Arc::clone(&self.translator),
        )
        .with_permissions(Arc::new(permissions))
        .with_gateway_latency(Arc::clone(&self.gateway))
        .with_scheduler(Scheduler::new(
            Arc::clone(&self.scheduler) as Arc<dyn SchedulerBackend>,
            &manifest.capabilities,
            manifest.id.clone(),
            guild,
        ))
        .with_store(Store::new(
            Arc::clone(&self.storage) as Arc<dyn StorageBackend>,
            &manifest.capabilities,
            manifest.id.clone(),
            guild,
        ))
        .with_cases(Cases::new(
            Arc::clone(&self.cases) as Arc<dyn CaseBackend>,
            &manifest.capabilities,
            manifest.id.clone(),
            guild,
        ))
        .with_history(MessageHistory::new(
            Arc::clone(&self.history) as Arc<dyn HistoryBackend>,
            &manifest.capabilities,
            manifest.id.clone(),
            guild,
        ))
        .with_clock({
            let clock = Arc::clone(&self.clock);
            move || clock()
        });
        match self.bot_id() {
            Some(bot) => context.with_bot(bot),
            None => context,
        }
    }

    /// Delivers an event the way the dispatcher would: only if the module subscribed to its kind
    /// and the event passes the filter. `None` when it was not delivered.
    pub async fn emit(&self, event: Event) -> Result<Option<Flow>, ModuleError> {
        let manifest = self.module.manifest();
        let wanted = manifest
            .subscription(event.kind())
            .is_some_and(|subscription| subscription.filter.matches(&event));
        if !wanted {
            return Ok(None);
        }
        let ctx = self.context(event.guild_id());
        self.module.on_event_flow(&ctx, &event).await.map(Some)
    }

    /// Delivers a click on a component and returns what the module answered.
    pub async fn click(&self, click: Click) -> Result<Vec<Reply>, ModuleError> {
        let guild: GuildId = Id::new(GUILD);
        let (number, token) = self.new_interaction(guild, Id::new(click.user), false);
        let event = ComponentInvocation {
            interaction_id: Id::new(number),
            application_id: Id::new(1),
            token,
            guild_id: guild,
            channel_id: Id::new(5),
            message_id: Id::new(900),
            user: tester(click.user),
            roles: click.roles.iter().map(|id| Id::new(*id)).collect(),
            invoker_permissions: administrator_if(click.administrator),
            locale: None,
            custom_id: click.custom_id,
            values: click.values,
        };
        self.deliver(guild, Event::Component(event)).await
    }

    /// Delivers a submitted form and returns what the module answered.
    pub async fn submit(&self, submit: Submit) -> Result<Vec<Reply>, ModuleError> {
        let guild: GuildId = Id::new(GUILD);
        let (number, token) = self.new_interaction(guild, Id::new(submit.user), false);
        let event = ModalSubmission {
            interaction_id: Id::new(number),
            application_id: Id::new(1),
            token,
            guild_id: guild,
            channel_id: Id::new(5),
            user: tester(submit.user),
            roles: Vec::new(),
            invoker_permissions: Permissions::empty(),
            locale: None,
            custom_id: submit.custom_id,
            fields: submit.fields,
        };
        self.deliver(guild, Event::Modal(event)).await
    }

    /// Delivers a message posted in the guild, the way the dispatcher would: `None` when the
    /// module did not subscribe to message creation, or its filter left the message out.
    pub async fn message(&self, message: Message) -> Result<Option<Flow>, ModuleError> {
        let id = self.next.fetch_add(1, Ordering::Relaxed);
        let info = MessageInfo {
            removed_attachments: Vec::new(),
            id: Id::new(id),
            channel_id: Id::new(message.channel),
            guild_id: Id::new(GUILD),
            author: UserInfo {
                id: Id::new(message.author),
                name: "tester".into(),
                bot: message.bot,
            },
            author_roles: message.roles.iter().map(|id| Id::new(*id)).collect(),
            content: message.content,
            mentioned_users: message
                .mentioned_users
                .iter()
                .map(|id| Id::new(*id))
                .collect(),
            mentioned_roles: message
                .mentioned_roles
                .iter()
                .map(|id| Id::new(*id))
                .collect(),
            mentions_everyone: message.mentions_everyone,
            attachments: message.attachments,
            embed_count: 0,
            sticker_count: 0,
            reply_to: None,
            from_webhook: false,
            created_at: (self.clock)(),
            edited_at: None,
        };
        self.emit(Event::MessageCreated(info)).await
    }

    /// Delivers a member's role change, with the roles the gateway reports as added and removed.
    /// `None` when the module did not subscribe to it or its filter left it out.
    pub async fn roles_changed(
        &self,
        user: u64,
        added: &[u64],
        removed: &[u64],
    ) -> Result<Option<Flow>, ModuleError> {
        self.emit(Event::RolesChanged(RolesChange {
            guild_id: Id::new(GUILD),
            user: tester(user),
            added: added.iter().map(|id| Id::new(*id)).collect(),
            removed: removed.iter().map(|id| Id::new(*id)).collect(),
        }))
        .await
    }

    /// Delivers a thread of the guild that was just created under `parent`.
    pub async fn thread_created(
        &self,
        thread: u64,
        parent: u64,
    ) -> Result<Option<Flow>, ModuleError> {
        self.emit(Event::ThreadCreated(
            self.thread_info(thread, parent, false),
        ))
        .await
    }

    /// Delivers a thread of the guild that was archived.
    pub async fn thread_archived(
        &self,
        thread: u64,
        parent: u64,
    ) -> Result<Option<Flow>, ModuleError> {
        self.emit(Event::ThreadArchived(
            self.thread_info(thread, parent, true),
        ))
        .await
    }

    /// Delivers a thread of the guild that was unarchived.
    pub async fn thread_unarchived(
        &self,
        thread: u64,
        parent: u64,
    ) -> Result<Option<Flow>, ModuleError> {
        self.emit(Event::ThreadUnarchived(
            self.thread_info(thread, parent, false),
        ))
        .await
    }

    /// Delivers the deletion of a thread of the guild.
    pub async fn thread_deleted(
        &self,
        thread: u64,
        parent: u64,
    ) -> Result<Option<Flow>, ModuleError> {
        self.emit(Event::ThreadDeleted {
            guild_id: Id::new(GUILD),
            thread_id: Id::new(thread),
            parent_id: Id::new(parent),
        })
        .await
    }

    /// Delivers a change of the guild's configuration, as the host announces it.
    pub async fn config_changed(&self) -> Result<Option<Flow>, ModuleError> {
        self.emit(Event::ConfigChanged {
            guild_id: Id::new(GUILD),
        })
        .await
    }

    /// Delivers a channel created in the guild.
    pub async fn channel_created(&self, channel: u64) -> Result<Option<Flow>, ModuleError> {
        self.emit(Event::ChannelCreated(closure_core::event::ChannelInfo {
            id: Id::new(channel),
            guild_id: Id::new(GUILD),
            name: Some(format!("channel-{channel}")),
            parent_id: None,
        }))
        .await
    }

    /// Delivers a case recorded in the guild, as the dispatcher announces it to every module.
    pub async fn case_created(&self, case: Case) -> Result<Option<Flow>, ModuleError> {
        self.emit(Event::CaseCreated(case)).await
    }

    fn thread_info(&self, thread: u64, parent: u64, archived: bool) -> ThreadInfo {
        ThreadInfo {
            id: Id::new(thread),
            guild_id: Id::new(GUILD),
            parent_id: Some(Id::new(parent)),
            name: format!("thread-{thread}"),
            owner_id: Some(Id::new(OTHER_OWNER)),
            archived,
            locked: false,
            created_at: Some((self.clock)()),
        }
    }

    /// A new interaction of `guild`: its number and its recorded token. Somebody owns the server
    /// in every run; it is `user` only when `owner` says so.
    fn new_interaction(
        &self,
        guild: GuildId,
        user: closure_core::event::UserId,
        owner: bool,
    ) -> (u64, InteractionToken) {
        let number = self.next.fetch_add(1, Ordering::Relaxed);
        let token = InteractionToken::new(format!("test-token-{number}"));
        let _ = self.interactions.register(&token, guild);
        self.api
            .set_owner(guild, if owner { user } else { Id::new(OTHER_OWNER) });
        (number, token)
    }

    /// Hands the event to the module and returns what it answered meanwhile.
    async fn deliver(&self, guild: GuildId, event: Event) -> Result<Vec<Reply>, ModuleError> {
        let ctx = self.context(guild);
        let before = self.api.actions().len();
        self.module.on_event(&ctx, &event).await?;
        Ok(self.api.actions()[before..]
            .iter()
            .filter_map(reply_of)
            .collect())
    }

    /// Uses a context-menu command and returns what the module answered during this run.
    pub async fn context_command(&self, command: ContextUse) -> Result<Vec<Reply>, ModuleError> {
        let guild: GuildId = Id::new(GUILD);
        let (number, token) = self.new_interaction(guild, Id::new(command.user), false);
        let event = ContextInvocation {
            interaction_id: Id::new(number),
            application_id: Id::new(1),
            token,
            guild_id: guild,
            channel_id: Id::new(5),
            user: tester(command.user),
            roles: command.roles.iter().map(|id| Id::new(*id)).collect(),
            invoker_permissions: administrator_if(command.administrator),
            locale: None,
            command: command.command,
            target: command.target,
        };
        self.deliver(guild, Event::ContextCommand(event)).await
    }

    /// Asks the module for the choices of an option while the user types, and returns them. Empty
    /// when the module answered with none (or did not answer with choices at all).
    pub async fn autocomplete(&self, query: Autocomplete) -> Result<Vec<Choice>, ModuleError> {
        let guild: GuildId = Id::new(GUILD);
        let (number, token) = self.new_interaction(guild, Id::new(query.user), false);
        let event = AutocompleteInvocation {
            interaction_id: Id::new(number),
            application_id: Id::new(1),
            token,
            guild_id: guild,
            channel_id: Id::new(5),
            user: tester(query.user),
            roles: query.roles.iter().map(|id| Id::new(*id)).collect(),
            invoker_permissions: administrator_if(query.administrator),
            locale: None,
            command: query.command,
            subcommand: query.subcommand,
            focused_option: query.focused_option,
            focused: query.focused,
            options: query.options,
        };
        let before = self.api.actions().len();
        self.module
            .on_event(&self.context(guild), &Event::Autocomplete(event))
            .await?;
        let choices = self.api.actions()[before..]
            .iter()
            .find_map(|action| match action {
                Action::Respond {
                    response: InteractionResponse::Autocomplete { choices },
                    ..
                } => Some(choices.clone()),
                _ => None,
            })
            .unwrap_or_default();
        Ok(choices
            .into_iter()
            .map(|choice| Choice {
                name: choice.name,
                value: choice.value,
            })
            .collect())
    }

    /// Runs a command and returns what the module answered during this run.
    pub async fn invoke(&self, invocation: Invocation) -> Result<Vec<Reply>, ModuleError> {
        let guild: GuildId = Id::new(GUILD);
        let user = Id::new(invocation.user);
        let (number, token) = self.new_interaction(guild, user, invocation.owner);
        let event = CommandInvocation {
            interaction_id: Id::new(number),
            application_id: Id::new(1),
            token,
            guild_id: guild,
            channel_id: Id::new(invocation.channel),
            user: tester(invocation.user),
            roles: invocation.roles.iter().map(|id| Id::new(*id)).collect(),
            invoker_permissions: administrator_if(invocation.administrator),
            locale: invocation.locale,
            command: invocation.command,
            subcommand: invocation.subcommand,
            options: invocation.options,
        };
        self.deliver(guild, Event::Command(event)).await
    }
}

fn tester(id: u64) -> UserInfo {
    UserInfo {
        id: Id::new(id),
        name: "tester".into(),
        bot: false,
    }
}

fn administrator_if(administrator: bool) -> Permissions {
    if administrator {
        Permissions::ADMINISTRATOR
    } else {
        Permissions::empty()
    }
}

fn reply_of(action: &Action) -> Option<Reply> {
    let (kind, reply) = match action {
        Action::Respond {
            response: InteractionResponse::Message { reply },
            ..
        } => (ReplyKind::Response, reply),
        Action::Respond {
            response: InteractionResponse::Update { reply },
            ..
        } => (ReplyKind::Update, reply),
        Action::Respond {
            response: InteractionResponse::Defer { ephemeral },
            ..
        } => {
            return Some(Reply {
                kind: ReplyKind::Deferred,
                content: String::new(),
                ephemeral: *ephemeral,
                components: Vec::new(),
                embeds: Vec::new(),
            });
        }
        Action::Respond {
            response: InteractionResponse::DeferUpdate,
            ..
        } => {
            return Some(Reply {
                kind: ReplyKind::Acknowledged,
                content: String::new(),
                ephemeral: false,
                components: Vec::new(),
                embeds: Vec::new(),
            });
        }
        Action::Respond {
            response: InteractionResponse::Modal(spec),
            ..
        } => {
            return Some(Reply {
                kind: ReplyKind::Modal,
                content: spec.title.clone(),
                ephemeral: false,
                components: spec.components.clone(),
                embeds: Vec::new(),
            });
        }
        Action::EditResponse { reply, .. } => (ReplyKind::Edit, reply),
        Action::Followup { reply, .. } => (ReplyKind::Followup, reply),
        _ => return None,
    };
    Some(Reply {
        kind,
        content: reply.content.clone().unwrap_or_else(|| {
            reply
                .embeds
                .first()
                .and_then(|embed| embed.description.clone())
                .unwrap_or_default()
        }),
        ephemeral: reply.ephemeral,
        components: reply.components.clone(),
        embeds: reply.embeds.clone(),
    })
}
