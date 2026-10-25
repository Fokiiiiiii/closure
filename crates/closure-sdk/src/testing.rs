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

use closure_core::action::{Action, InteractionResponse};
use closure_core::client::ApiClient;
use closure_core::error::ModuleError;
use closure_core::event::{
    CommandInvocation, CommandOption, CommandOptionValue, Event, GuildId, InteractionToken,
    UserInfo,
};
use closure_core::interaction::InteractionGuilds;
use closure_core::locale::{Locale, Translator};
use closure_core::module::{EventContext, Module};
use closure_core::permission::GuildPermissions;
use closure_core::registry::ModuleRegistry;
use closure_core::settings::GuildPolicy;
use closure_core::storage::{MemoryStorage, StorageBackend, Store};
use closure_core::testing::MockApi;
use closure_i18n::TranslatorBuilder;
use twilight_model::guild::Permissions;
use twilight_model::id::Id;

use crate::module::{ModuleBuilder, SdkModule};

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
}

/// What the module sent back.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Reply {
    pub kind: ReplyKind,
    pub content: String,
    pub ephemeral: bool,
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
    elapsed: Arc<Mutex<Duration>>,
    next: AtomicU64,
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
        let storage_elapsed = Arc::clone(&elapsed);
        let storage = Arc::new(MemoryStorage::with_clock(move || {
            let passed = storage_elapsed
                .lock()
                .unwrap_or_else(PoisonError::into_inner)
                .as_secs();
            i64::try_from(unix_start + passed).unwrap_or(i64::MAX)
        }));
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
            elapsed,
            next: AtomicU64::new(1),
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

    /// The pretend Discord, to look at what else the module did.
    #[must_use]
    pub fn api(&self) -> &Arc<MockApi> {
        &self.api
    }

    /// Runs a command and returns what the module answered during this run.
    pub async fn invoke(&self, invocation: Invocation) -> Result<Vec<Reply>, ModuleError> {
        let number = self.next.fetch_add(1, Ordering::Relaxed);
        let guild: GuildId = Id::new(GUILD);
        let token = InteractionToken::new(format!("test-token-{number}"));
        let _ = self.interactions.register(&token, guild);
        let user = Id::new(invocation.user);
        // Somebody owns the server in every run; it is the user only when the test says so.
        self.api.set_owner(
            guild,
            if invocation.owner {
                user
            } else {
                Id::new(OTHER_OWNER)
            },
        );
        let event = CommandInvocation {
            interaction_id: Id::new(number),
            application_id: Id::new(1),
            token,
            guild_id: guild,
            channel_id: Id::new(invocation.channel),
            user: UserInfo {
                id: user,
                name: "tester".into(),
                bot: false,
            },
            roles: invocation.roles.iter().map(|id| Id::new(*id)).collect(),
            invoker_permissions: if invocation.administrator {
                Permissions::ADMINISTRATOR
            } else {
                Permissions::empty()
            },
            locale: invocation.locale,
            command: invocation.command,
            subcommand: invocation.subcommand,
            options: invocation.options,
        };

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
        let ctx = EventContext::new(
            manifest.id.clone(),
            guild,
            client,
            Locale::DEFAULT,
            Arc::new(GuildPolicy::default()),
            Arc::new(config),
            Arc::clone(&self.translator),
        )
        .with_permissions(Arc::new(permissions))
        .with_store(Store::new(
            Arc::clone(&self.storage) as Arc<dyn StorageBackend>,
            &manifest.capabilities,
            manifest.id.clone(),
            guild,
        ));

        let before = self.api.actions().len();
        self.module.on_event(&ctx, &Event::Command(event)).await?;
        Ok(self.api.actions()[before..]
            .iter()
            .filter_map(reply_of)
            .collect())
    }
}

fn reply_of(action: &Action) -> Option<Reply> {
    let (kind, reply) = match action {
        Action::Respond {
            response: InteractionResponse::Message { reply },
            ..
        } => (ReplyKind::Response, reply),
        Action::Respond {
            response: InteractionResponse::Defer { ephemeral },
            ..
        } => {
            return Some(Reply {
                kind: ReplyKind::Deferred,
                content: String::new(),
                ephemeral: *ephemeral,
            });
        }
        Action::EditResponse { reply, .. } => (ReplyKind::Edit, reply),
        Action::Followup { reply, .. } => (ReplyKind::Followup, reply),
        _ => return None,
    };
    Some(Reply {
        kind,
        content: reply.content.clone().unwrap_or_default(),
        ephemeral: reply.ephemeral,
    })
}
