//! Declaring a module: commands and what they need, and the module they make.

use std::collections::HashMap;
use std::future::Future;
use std::marker::PhantomData;
use std::sync::Arc;
use std::time::{Duration, Instant};

use async_trait::async_trait;
use closure_core::capability::Capability;
use closure_core::error::DiscordError;
use closure_core::error::{CoreError, ModuleError};
use closure_core::event::{
    AutocompleteInvocation, ComponentInvocation, ContextInvocation, ContextTarget, Event,
    EventKind, InteractionRef, ModalSubmission,
};
use closure_core::locale::{Args, Locale};
use closure_core::module::{
    ChoiceValue, CommandSpec, ContextCommandSpec, ContextKind, EventContext, Module, ModuleId,
    ModuleManifest, OptionChoice, OptionKind, OptionSpec, SubcommandSpec, TEXTS_KEY, catalog_text,
    resolve_descriptions,
};
use closure_core::permission::{PermissionKey, PermissionSpec};
use closure_core::schedule::ScheduledJob;
use closure_core::subscription::{Flow, Subscription};
use futures_util::FutureExt;
use futures_util::future::BoxFuture;
use serde_json::json;
use twilight_model::gateway::Intents;
use twilight_model::guild::Permissions;

use crate::args::CommandArgs;
use crate::cooldown::{Cooldown, Cooldowns};
use crate::cx::{AutocompleteCx, CommandCx, ComponentCx, ContextCx, Cx, JobCx, ModalCx, REPLIES};
use crate::duration::{TimeStyle, ends_at};
use crate::embed::Tone;
use crate::error::UserError;
use crate::ui::CustomId;

type Run = dyn Fn(CommandCx) -> BoxFuture<'static, Result<(), UserError>> + Send + Sync;

/// What offers the choices of an option while the user types in it.
type AutoRun =
    dyn Fn(AutocompleteCx) -> BoxFuture<'static, Result<Vec<Choice>, UserError>> + Send + Sync;

/// One choice an autocomplete offers: what the user sees, and the value the command gets.
#[derive(Debug, Clone, PartialEq)]
pub struct Choice {
    pub name: String,
    pub value: ChoiceValue,
}

impl Choice {
    #[must_use]
    pub fn new(name: impl Into<String>, value: impl Into<ChoiceValue>) -> Self {
        Self {
            name: name.into(),
            value: value.into(),
        }
    }
}

/// A command that is declared but has no handler yet; `A` is its options.
#[derive(Debug)]
pub struct Command<A> {
    name: String,
    requires: Option<String>,
    cooldown: Option<Cooldown>,
    default_member_permissions: Option<Permissions>,
    options: PhantomData<fn() -> A>,
}

impl<A: CommandArgs> Command<A> {
    /// A command (or, inside a group, a subcommand). Its description is the one of the module's
    /// catalog, under the key of the command.
    #[must_use]
    pub fn new(name: impl Into<String>) -> Self {
        Self {
            name: name.into(),
            requires: None,
            cooldown: None,
            default_member_permissions: None,
            options: PhantomData,
        }
    }

    /// The permission key the user needs; the module has to declare it.
    #[must_use]
    pub fn requires(mut self, key: impl Into<String>) -> Self {
        self.requires = Some(key.into());
        self
    }

    #[must_use]
    pub fn cooldown(mut self, cooldown: Cooldown) -> Self {
        self.cooldown = Some(cooldown);
        self
    }

    /// Who sees the command in Discord by default (a server can change that).
    #[must_use]
    pub fn default_member_permissions(mut self, permissions: Permissions) -> Self {
        self.default_member_permissions = Some(permissions);
        self
    }

    /// What the command does. The options are parsed before it runs.
    pub fn handler<F, Fut>(self, handler: F) -> BoundCommand
    where
        F: Fn(CommandCx, A) -> Fut + Send + Sync + 'static,
        Fut: Future<Output = Result<(), UserError>> + Send + 'static,
    {
        let handler = Arc::new(handler);
        let run: Arc<Run> = Arc::new(move |cx: CommandCx| {
            let handler = Arc::clone(&handler);
            async move {
                let args = A::parse(cx.invocation()).map_err(UserError::from)?;
                handler(cx, args).await
            }
            .boxed()
        });
        BoundCommand {
            name: self.name,
            options: A::options(),
            requires: self.requires,
            cooldown: self.cooldown,
            default_member_permissions: self.default_member_permissions,
            run,
        }
    }
}

/// A command with its handler.
pub struct BoundCommand {
    name: String,
    options: Vec<closure_core::module::OptionSpec>,
    requires: Option<String>,
    cooldown: Option<Cooldown>,
    default_member_permissions: Option<Permissions>,
    run: Arc<Run>,
}

impl std::fmt::Debug for BoundCommand {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("BoundCommand")
            .field("name", &self.name)
            .finish_non_exhaustive()
    }
}

/// A group of subcommands inside a command made with [`ModuleBuilder::groups`]: `/warn list all`
/// is the subcommand `all` of the group `list`. Each subcommand has its own permission and
/// cooldown.
#[derive(Debug)]
pub struct SubcommandGroup {
    name: String,
    subcommands: Vec<BoundCommand>,
}

impl SubcommandGroup {
    /// A group; it needs at least one subcommand.
    #[must_use]
    pub fn new(name: impl Into<String>) -> Self {
        Self {
            name: name.into(),
            subcommands: Vec::new(),
        }
    }

    #[must_use]
    pub fn subcommand(mut self, command: BoundCommand) -> Self {
        self.subcommands.push(command);
        self
    }
}

type ContextRun = dyn Fn(ContextCx) -> BoxFuture<'static, Result<(), UserError>> + Send + Sync;

/// A context-menu command under construction: it is used on a user or on a message. Discord
/// takes a name for it and no description.
#[derive(Debug)]
pub struct ContextCommand {
    name: String,
    kind: ContextKind,
    requires: Option<String>,
    cooldown: Option<Cooldown>,
}

impl ContextCommand {
    /// A command used on a user.
    #[must_use]
    pub fn user(name: impl Into<String>) -> Self {
        Self::of(name, ContextKind::User)
    }

    /// A command used on a message.
    #[must_use]
    pub fn message(name: impl Into<String>) -> Self {
        Self::of(name, ContextKind::Message)
    }

    fn of(name: impl Into<String>, kind: ContextKind) -> Self {
        Self {
            name: name.into(),
            kind,
            requires: None,
            cooldown: None,
        }
    }

    /// The permission key the user needs; the module has to declare it.
    #[must_use]
    pub fn requires(mut self, key: impl Into<String>) -> Self {
        self.requires = Some(key.into());
        self
    }

    #[must_use]
    pub fn cooldown(mut self, cooldown: Cooldown) -> Self {
        self.cooldown = Some(cooldown);
        self
    }

    /// What happens when the command is used. The handler reads its target with
    /// [`ContextCx::target_user`] or [`ContextCx::target_message`].
    pub fn handler<F, Fut>(self, handler: F) -> BoundContext
    where
        F: Fn(ContextCx) -> Fut + Send + Sync + 'static,
        Fut: Future<Output = Result<(), UserError>> + Send + 'static,
    {
        let run: Arc<ContextRun> = Arc::new(move |cx| handler(cx).boxed());
        BoundContext {
            name: self.name,
            kind: self.kind,
            requires: self.requires,
            cooldown: self.cooldown,
            run,
        }
    }
}

/// A context-menu command with its handler.
pub struct BoundContext {
    name: String,
    kind: ContextKind,
    requires: Option<String>,
    cooldown: Option<Cooldown>,
    run: Arc<ContextRun>,
}

impl std::fmt::Debug for BoundContext {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("BoundContext")
            .field("name", &self.name)
            .field("kind", &self.kind)
            .finish_non_exhaustive()
    }
}

/// What a context-menu command does when it is used.
struct ContextRoute {
    requires: Option<PermissionKey>,
    cooldown: Option<Cooldown>,
    run: Arc<ContextRun>,
}

/// What a command does when it runs.
struct Route {
    requires: Option<PermissionKey>,
    cooldown: Option<Cooldown>,
    run: Arc<Run>,
}

type Clock = Arc<dyn Fn() -> Instant + Send + Sync>;

type EventRun =
    dyn Fn(EventContext, Event) -> BoxFuture<'static, Result<Flow, ModuleError>> + Send + Sync;

/// What runs when a component or a modal comes back.
type UiRun<I> = dyn Fn(Cx<I>) -> BoxFuture<'static, Result<(), UserError>> + Send + Sync;

/// What runs when a job of the module is due.
type JobRun = dyn Fn(JobCx) -> BoxFuture<'static, Result<(), ModuleError>> + Send + Sync;

/// Builds a module from its commands.
pub struct ModuleBuilder {
    manifest: ModuleManifest,
    routes: HashMap<(String, Option<String>), Route>,
    handlers: HashMap<EventKind, Arc<EventRun>>,
    components: HashMap<String, Arc<UiRun<ComponentInvocation>>>,
    modals: HashMap<String, Arc<UiRun<ModalSubmission>>>,
    jobs: HashMap<String, Arc<JobRun>>,
    /// By command, subcommand (`group sub`, if any) and option.
    autocompletes: HashMap<(String, Option<String>, String), Arc<AutoRun>>,
    contexts: HashMap<(ContextKind, String), ContextRoute>,
    problem: Option<CoreError>,
    clock: Clock,
}

impl std::fmt::Debug for ModuleBuilder {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("ModuleBuilder")
            .field("id", &self.manifest.id)
            .finish_non_exhaustive()
    }
}

impl ModuleBuilder {
    /// `id` is the module's id (see [`ModuleId`]); `version` is usually
    /// `env!("CARGO_PKG_VERSION")`.
    pub fn new(
        id: &str,
        name: impl Into<String>,
        version: impl Into<String>,
    ) -> Result<Self, CoreError> {
        let manifest = ModuleManifest::new(ModuleId::new(id)?, name, version)
            .events([EventKind::Command])
            .capabilities([Capability::RespondToInteractions]);
        Ok(Self {
            manifest,
            routes: HashMap::new(),
            handlers: HashMap::new(),
            components: HashMap::new(),
            modals: HashMap::new(),
            jobs: HashMap::new(),
            autocompletes: HashMap::new(),
            contexts: HashMap::new(),
            problem: None,
            clock: Arc::new(Instant::now),
        })
    }

    /// The description of a module: the line `module-description` of its default-language catalog, so
    /// that the text is in the locale files and not in the code.
    fn module_description(manifest: &ModuleManifest) -> String {
        manifest
            .catalogs
            .iter()
            .find(|catalog| catalog.locale == Locale::DEFAULT)
            .and_then(|catalog| {
                catalog
                    .source
                    .lines()
                    .find_map(|line| line.strip_prefix("module-description = "))
            })
            .map(str::to_owned)
            .unwrap_or_default()
    }

    /// A capability the module needs besides answering commands (which it always has).
    #[must_use]
    pub fn capability(mut self, capability: Capability) -> Self {
        self.manifest.capabilities.insert(capability);
        self
    }

    /// The Gateway intents the module needs for the events it handles (`GUILD_MESSAGES` for
    /// messages, `GUILD_MEMBERS` for members, ...); the bot asks Discord for the union over all
    /// its modules. A module that subscribes to events whose intent it does not declare is
    /// refused when it is built, because Discord would never send it those events.
    #[must_use]
    pub fn intents(mut self, intents: Intents) -> Self {
        self.manifest = self.manifest.intents(intents);
        self
    }

    /// Declares a permission key; a command can require it and a server can decide who has it.
    #[must_use]
    pub fn permission(mut self, permission: PermissionSpec) -> Self {
        self.manifest = self.manifest.permission(permission);
        self
    }

    /// The JSON Schema of the module's configuration section and its default.
    #[must_use]
    pub fn config(mut self, schema: serde_json::Value, default: serde_json::Value) -> Self {
        self.manifest = self.manifest.config(schema, default);
        self
    }

    /// The module's messages in one language (a Fluent file).
    #[must_use]
    pub fn catalog(mut self, locale: Locale, source: impl Into<String>) -> Self {
        self.manifest = self.manifest.catalog(locale, source);
        self
    }

    #[must_use]
    pub fn enabled_by_default(mut self) -> Self {
        self.manifest = self.manifest.enabled_by_default();
        self
    }

    /// Replaces the clock cooldowns are measured with; for tests.
    #[must_use]
    pub fn clock(mut self, clock: impl Fn() -> Instant + Send + Sync + 'static) -> Self {
        self.clock = Arc::new(clock);
        self
    }

    /// Handles an event kind (other than commands): the handler gets only the events that pass
    /// the subscription's filter, after the modules with a higher priority, and says whether
    /// the modules below it get the event too ([`Flow::Stop`] keeps it from them). A module has
    /// one handler per kind; the capabilities the handler needs have to be declared too.
    #[must_use]
    pub fn on<F, Fut>(mut self, subscription: Subscription, handler: F) -> Self
    where
        F: Fn(EventContext, Event) -> Fut + Send + Sync + 'static,
        Fut: Future<Output = Result<Flow, ModuleError>> + Send + 'static,
    {
        let kind = subscription.kind;
        if kind == EventKind::Command {
            self.problem.get_or_insert(self.invalid(
                "",
                "commands are declared with `command`, not subscribed to".to_owned(),
            ));
        }
        if kind == EventKind::Scheduled {
            self.problem.get_or_insert(self.invalid(
                "",
                "jobs are declared with `job`, not subscribed to".to_owned(),
            ));
        }
        if matches!(kind, EventKind::Component | EventKind::Modal) {
            self.problem.get_or_insert(self.invalid(
                "",
                "buttons, menus and forms are handled with `component` and `modal`, not subscribed to"
                    .to_owned(),
            ));
        }
        let run: Arc<EventRun> = Arc::new(move |ctx, event| handler(ctx, event).boxed());
        if self.handlers.insert(kind, run).is_some() {
            self.problem.get_or_insert(self.invalid(
                "",
                format!("has two handlers for `{}` events", kind.as_str()),
            ));
        }
        self.manifest = self.manifest.subscribe(subscription);
        self
    }

    /// What happens when a button or select menu whose `custom_id` names `route` (see
    /// [`CustomId`]) is used. Answer with `cx.update(..)`, `cx.reply_private(..)` or
    /// `cx.show_modal(..)`; a handler that answers nothing just acknowledges the click.
    #[must_use]
    pub fn component<F, Fut>(mut self, route: &str, handler: F) -> Self
    where
        F: Fn(ComponentCx) -> Fut + Send + Sync + 'static,
        Fut: Future<Output = Result<(), UserError>> + Send + 'static,
    {
        self.check_route(route);
        let run: Arc<UiRun<ComponentInvocation>> = Arc::new(move |cx| handler(cx).boxed());
        if self.components.insert(route.to_owned(), run).is_some() {
            self.problem
                .get_or_insert(self.invalid(route, "has two component handlers".to_owned()));
        }
        self
    }

    /// What happens when the form whose `custom_id` names `route` is submitted.
    #[must_use]
    pub fn modal<F, Fut>(mut self, route: &str, handler: F) -> Self
    where
        F: Fn(ModalCx) -> Fut + Send + Sync + 'static,
        Fut: Future<Output = Result<(), UserError>> + Send + 'static,
    {
        self.check_route(route);
        let run: Arc<UiRun<ModalSubmission>> = Arc::new(move |cx| handler(cx).boxed());
        if self.modals.insert(route.to_owned(), run).is_some() {
            self.problem
                .get_or_insert(self.invalid(route, "has two modal handlers".to_owned()));
        }
        self
    }

    /// What happens when a job scheduled under `name` (`cx.scheduler().job(name, ..)`) is due.
    /// The module gets the capability to schedule. A handler that fails is run again after a
    /// growing pause, and when the bot stops while it runs it is run again too, so it has to be
    /// safe to run twice. A job whose name has no handler (an old one) is dropped with a warning.
    #[must_use]
    pub fn job<F, Fut>(mut self, name: &str, handler: F) -> Self
    where
        F: Fn(JobCx) -> Fut + Send + Sync + 'static,
        Fut: Future<Output = Result<(), ModuleError>> + Send + 'static,
    {
        let run: Arc<JobRun> = Arc::new(move |cx| handler(cx).boxed());
        if self.jobs.insert(name.to_owned(), run).is_some() {
            self.problem
                .get_or_insert(self.invalid(name, "has two job handlers".to_owned()));
        }
        self.manifest.capabilities.insert(Capability::Schedule);
        self
    }

    /// What offers the choices of `option` of the command at `path` (`warn`, or `warn list all`
    /// for a subcommand) while the user types in it. The option is published as autocompleted.
    /// A user who may not run the command gets no choices, and so does a handler that fails
    /// (the failure is logged).
    #[must_use]
    pub fn autocomplete<F, Fut>(mut self, path: &str, option: &str, handler: F) -> Self
    where
        F: Fn(AutocompleteCx) -> Fut + Send + Sync + 'static,
        Fut: Future<Output = Result<Vec<Choice>, UserError>> + Send + 'static,
    {
        let (command, sub) = match path.split_once(' ') {
            Some((command, sub)) => (command.to_owned(), Some(sub.to_owned())),
            None => (path.to_owned(), None),
        };
        let run: Arc<AutoRun> = Arc::new(move |cx| handler(cx).boxed());
        if self
            .autocompletes
            .insert((command, sub, option.to_owned()), run)
            .is_some()
        {
            self.problem.get_or_insert(self.invalid(
                path,
                format!("has two autocomplete handlers for `{option}`"),
            ));
        }
        self
    }

    /// A route is written into a `custom_id` between colons, so it has to survive that.
    fn check_route(&mut self, route: &str) {
        if route.is_empty() || route.contains(':') {
            self.problem.get_or_insert(self.invalid(
                route,
                "is not a usable route: it must not be empty or contain `:`".to_owned(),
            ));
        } else if self.manifest.id.as_str().chars().count() + route.chars().count() + 4 > 100 {
            // The shortest custom_id of the route is "{module}:{route}:*:".
            self.problem.get_or_insert(self.invalid(
                route,
                "makes a custom_id longer than Discord allows (100 characters)".to_owned(),
            ));
        }
    }

    /// Adds a command that has no subcommands.
    #[must_use]
    pub fn command(mut self, command: BoundCommand) -> Self {
        if self
            .manifest
            .commands
            .iter()
            .any(|spec| spec.name == command.name)
        {
            self.problem.get_or_insert(
                self.invalid(
                    &command.name,
                    "is declared twice: a command with subcommands cannot also be a plain command"
                        .to_owned(),
                ),
            );
        }
        let spec = CommandSpec {
            name: command.name.clone(),
            options: command.options.clone(),
            subcommands: Vec::new(),
            default_member_permissions: command.default_member_permissions,
        };
        self.manifest = self.manifest.command(spec);
        let name = command.name.clone();
        self.route(&name, None, command);
        self
    }

    /// Adds a command made of subcommands (`/warn add`, `/warn list`). Calls with the same name
    /// add to one command, which may mix plain subcommands and groups; the description of the first
    /// call stands.
    #[must_use]
    pub fn group(
        mut self,
        name: impl Into<String>,
        subcommands: impl IntoIterator<Item = BoundCommand>,
    ) -> Self {
        let name = name.into();
        let mut specs = Vec::new();
        for sub in subcommands {
            if sub.default_member_permissions.is_some() {
                self.problem.get_or_insert(self.invalid(
                    &sub.name,
                    "is a subcommand: Discord decides who sees a command by the command, not by its subcommands"
                        .to_owned(),
                ));
            }
            specs.push(SubcommandSpec {
                name: sub.name.clone(),
                options: sub.options.clone(),
                subcommands: Vec::new(),
            });
            let sub_name = sub.name.clone();
            self.route(&name, Some(sub_name), sub);
        }
        self.add_subcommands(&name, specs);
        self
    }

    /// Adds a command made of groups of subcommands, two levels deep (`/warn list all`). The
    /// route of a subcommand is its group and its name, separated by a space. Like [`Self::group`],
    /// it adds to a command of the same name.
    #[must_use]
    pub fn groups(
        mut self,
        name: impl Into<String>,
        groups: impl IntoIterator<Item = SubcommandGroup>,
    ) -> Self {
        let name = name.into();
        let mut specs = Vec::new();
        for SubcommandGroup {
            name: group,
            subcommands,
        } in groups
        {
            if subcommands.is_empty() {
                self.problem.get_or_insert(
                    self.invalid(&group, "is a group without subcommands".to_owned()),
                );
            }
            let mut leaves = Vec::new();
            for sub in subcommands {
                if sub.default_member_permissions.is_some() {
                    self.problem.get_or_insert(self.invalid(
                        &sub.name,
                        "is a subcommand: Discord decides who sees a command by the command, not by its subcommands"
                            .to_owned(),
                    ));
                }
                leaves.push(SubcommandSpec {
                    name: sub.name.clone(),
                    options: sub.options.clone(),
                    subcommands: Vec::new(),
                });
                let route = format!("{group} {}", sub.name);
                self.route(&name, Some(route), sub);
            }
            specs.push(SubcommandSpec {
                name: group,
                options: Vec::new(),
                subcommands: leaves,
            });
        }
        self.add_subcommands(&name, specs);
        self
    }

    /// Adds subcommands to the command `name`, which may already have some. A name is used once
    /// among a command's subcommands and groups, and a command holds at most 25 of them.
    fn add_subcommands(&mut self, name: &str, subcommands: Vec<SubcommandSpec>) {
        if self.routes.contains_key(&(name.to_owned(), None)) {
            self.problem.get_or_insert(self.invalid(
                name,
                "is a command without subcommands: it cannot take subcommands as well".to_owned(),
            ));
        }
        let commands = &mut self.manifest.commands;
        let index = if let Some(index) = commands.iter().position(|spec| spec.name == name) {
            index
        } else {
            commands.push(CommandSpec {
                name: name.to_owned(),
                options: Vec::new(),
                subcommands: Vec::new(),
                default_member_permissions: None,
            });
            commands.len() - 1
        };
        let spec = &mut commands[index];
        let mut clash = false;
        for sub in subcommands {
            clash |= spec.subcommands.iter().any(|known| known.name == sub.name);
            spec.subcommands.push(sub);
        }
        let too_many = spec.subcommands.len() > 25;
        if clash {
            self.problem.get_or_insert(CoreError::InvalidCommand {
                module: self.manifest.id.clone(),
                command: name.to_owned(),
                reason: "a subcommand or group name is used twice".to_owned(),
            });
        }
        if too_many {
            self.problem.get_or_insert(CoreError::InvalidCommand {
                module: self.manifest.id.clone(),
                command: name.to_owned(),
                reason: "at most 25 subcommands and groups".to_owned(),
            });
        }
    }

    /// Adds a context-menu command (see [`ContextCommand`]).
    #[must_use]
    pub fn context(mut self, command: BoundContext) -> Self {
        let BoundContext {
            name,
            kind,
            requires,
            cooldown,
            run,
        } = command;
        let requires = match requires.as_deref().map(PermissionKey::new).transpose() {
            Ok(key) => key,
            Err(error) => {
                self.problem.get_or_insert(error);
                None
            }
        };
        self.manifest = self.manifest.context_command(ContextCommandSpec {
            name: name.clone(),
            kind,
        });
        let route = ContextRoute {
            requires,
            cooldown,
            run,
        };
        if self.contexts.insert((kind, name.clone()), route).is_some() {
            self.problem
                .get_or_insert(self.invalid(&name, "is declared twice".to_owned()));
        }
        self
    }

    /// A context-menu command on a user, with its handler (no permission or cooldown; for those
    /// use [`ContextCommand`] with [`Self::context`]).
    #[must_use]
    pub fn user_command<F, Fut>(self, name: &str, handler: F) -> Self
    where
        F: Fn(ContextCx) -> Fut + Send + Sync + 'static,
        Fut: Future<Output = Result<(), UserError>> + Send + 'static,
    {
        self.context(ContextCommand::user(name).handler(handler))
    }

    /// A context-menu command on a message, with its handler (see [`Self::user_command`]).
    #[must_use]
    pub fn message_command<F, Fut>(self, name: &str, handler: F) -> Self
    where
        F: Fn(ContextCx) -> Fut + Send + Sync + 'static,
        Fut: Future<Output = Result<(), UserError>> + Send + 'static,
    {
        self.context(ContextCommand::message(name).handler(handler))
    }

    fn route(&mut self, command: &str, sub: Option<String>, bound: BoundCommand) {
        let requires = match bound
            .requires
            .as_deref()
            .map(PermissionKey::new)
            .transpose()
        {
            Ok(key) => key,
            Err(error) => {
                self.problem.get_or_insert(error);
                None
            }
        };
        let route = Route {
            requires,
            cooldown: bound.cooldown,
            run: bound.run,
        };
        if self
            .routes
            .insert((command.to_owned(), sub), route)
            .is_some()
        {
            self.problem
                .get_or_insert(self.invalid(command, "is declared twice".to_owned()));
        }
    }

    fn invalid(&self, command: &str, reason: String) -> CoreError {
        CoreError::InvalidCommand {
            module: self.manifest.id.clone(),
            command: command.to_owned(),
            reason,
        }
    }

    /// The checks of the context-menu commands and the autocompletes, and the marking of the
    /// options that are autocompleted in the published commands.
    fn check_interactions(&mut self) {
        for route in self.contexts.values() {
            if let Some(key) = &route.requires
                && !self
                    .manifest
                    .permissions
                    .iter()
                    .any(|spec| spec.key == *key)
            {
                self.problem.get_or_insert(self.invalid(
                    "",
                    format!("requires the permission `{key}`, which the module does not declare"),
                ));
            }
        }
        // Discord takes five context-menu commands of each kind for an application.
        for kind in [ContextKind::User, ContextKind::Message] {
            let count = self
                .manifest
                .context_commands
                .iter()
                .filter(|spec| spec.kind == kind)
                .count();
            if count > 5 {
                self.problem.get_or_insert(self.invalid(
                    "",
                    format!("has {count} {kind:?} commands; at most 5 are allowed"),
                ));
            }
        }
        // Each autocomplete has to name a command and an option that can be autocompleted, and
        // the option is published as such.
        let declared: Vec<_> = self.autocompletes.keys().cloned().collect();
        for (command, sub, option) in declared {
            let route_exists = self.routes.contains_key(&(command.clone(), sub.clone()));
            let marked = if route_exists {
                mark_autocomplete(
                    &mut self.manifest.commands,
                    &command,
                    sub.as_deref(),
                    &option,
                )
            } else {
                Err("the command or subcommand is not declared")
            };
            if let Err(reason) = marked {
                self.problem.get_or_insert(
                    self.invalid(&command, format!("autocomplete of `{option}`: {reason}")),
                );
            }
        }
    }

    /// Gives the schema's descriptions their text: the module's default language first, then the
    /// SDK's own settings, which come from the core catalog.
    fn resolve_schema_descriptions(manifest: &mut ModuleManifest) {
        let default_source = manifest
            .catalogs
            .iter()
            .find(|catalog| catalog.locale == Locale::DEFAULT)
            .map(|catalog| catalog.source.clone())
            .unwrap_or_default();
        resolve_descriptions(&mut manifest.config_schema, &|key| {
            catalog_text(&default_source, key).or_else(|| catalog_text(closure_i18n::CORE_EN, key))
        });
    }

    /// The module, or what is wrong with its declaration.
    pub fn build(mut self) -> Result<SdkModule, CoreError> {
        self.manifest.description = Self::module_description(&self.manifest);
        // Looked at here and not as each command is added, so the order of the calls does not
        // matter: a permission may be declared after the command that requires it.
        for ((command, _), route) in &self.routes {
            if let Some(key) = &route.requires
                && !self
                    .manifest
                    .permissions
                    .iter()
                    .any(|spec| spec.key == *key)
            {
                self.problem.get_or_insert(self.invalid(
                    command,
                    format!("requires the permission `{key}`, which the module does not declare"),
                ));
            }
        }
        for kind in &self.manifest.events {
            let missing = kind.required_intents() - self.manifest.intents;
            if !missing.is_empty() {
                self.problem.get_or_insert(self.invalid(
                    "",
                    format!(
                        "handles `{}` events, which need the Gateway intent {missing:?}; declare it with `intents(..)`",
                        kind.as_str()
                    ),
                ));
            }
        }
        // Every module gets the settings the SDK reserves: who sees its replies (see
        // `Cx::reply_with`) and the administrator's texts (see `TEXTS_KEY`).
        for key in [REPLIES, TEXTS_KEY] {
            let declared = self
                .manifest
                .config_schema
                .get("properties")
                .and_then(|properties| properties.get(key))
                .is_some();
            if declared {
                self.problem.get_or_insert(self.invalid(
                    "",
                    format!("declares the config key `{key}`, which the SDK reserves"),
                ));
            }
        }
        let added = self
            .manifest
            .config_schema
            .as_object_mut()
            .and_then(|schema| {
                schema
                    .entry("properties")
                    .or_insert_with(|| json!({}))
                    .as_object_mut()
            })
            .map(|properties| {
                properties.insert(
                    REPLIES.to_owned(),
                    json!({
                        "type": "string",
                        "enum": ["private", "public"],
                        "description": "config-replies"
                    }),
                );
                properties.insert(
                    TEXTS_KEY.to_owned(),
                    json!({
                        "description": "config-texts"
                    }),
                );
            })
            .is_some();
        Self::resolve_schema_descriptions(&mut self.manifest);
        if !added {
            self.problem.get_or_insert(
                self.invalid(
                    "",
                    "the config schema must be a JSON object whose `properties` is an object"
                        .to_owned(),
                ),
            );
        }
        self.check_interactions();
        if let Some(problem) = self.problem {
            return Err(problem);
        }
        Ok(SdkModule {
            manifest: self.manifest,
            routes: self.routes,
            handlers: self.handlers,
            components: self.components,
            modals: self.modals,
            jobs: self.jobs,
            autocompletes: self.autocompletes,
            contexts: self.contexts,
            cooldowns: Cooldowns::default(),
            clock: self.clock,
        })
    }
}

/// Marks the option `option` of the command `command` (of its subcommand `sub`, `group sub` for a
/// subcommand of a group) as autocompleted. Returns why that is not possible.
fn mark_autocomplete(
    commands: &mut [CommandSpec],
    command: &str,
    sub: Option<&str>,
    option: &str,
) -> Result<(), &'static str> {
    let spec = commands
        .iter_mut()
        .find(|spec| spec.name == command)
        .ok_or("the command is not declared")?;
    let options = match sub {
        None => &mut spec.options,
        Some(path) => {
            let found = match path.split_once(' ') {
                Some((group, name)) => spec
                    .subcommands
                    .iter_mut()
                    .find(|sub| sub.name == group)
                    .and_then(|group| group.subcommands.iter_mut().find(|sub| sub.name == name)),
                None => spec.subcommands.iter_mut().find(|sub| sub.name == path),
            };
            &mut found.ok_or("the subcommand is not declared")?.options
        }
    };
    let target: &mut OptionSpec = options
        .iter_mut()
        .find(|spec| spec.name == option)
        .ok_or("the option is not declared")?;
    if !matches!(
        target.kind,
        OptionKind::String | OptionKind::Integer | OptionKind::Number
    ) {
        return Err("only text and number options can be autocompleted");
    }
    target.autocomplete = true;
    Ok(())
}

/// A module made with [`ModuleBuilder`]. It answers its commands: checks the permission and
/// the cooldown, reads the options, runs the handler, and tells the user in their language
/// when they are not allowed, have to wait, or the handler gives them a message.
pub struct SdkModule {
    manifest: ModuleManifest,
    routes: HashMap<(String, Option<String>), Route>,
    handlers: HashMap<EventKind, Arc<EventRun>>,
    components: HashMap<String, Arc<UiRun<ComponentInvocation>>>,
    modals: HashMap<String, Arc<UiRun<ModalSubmission>>>,
    jobs: HashMap<String, Arc<JobRun>>,
    autocompletes: HashMap<(String, Option<String>, String), Arc<AutoRun>>,
    contexts: HashMap<(ContextKind, String), ContextRoute>,
    cooldowns: Cooldowns,
    clock: Clock,
}

impl std::fmt::Debug for SdkModule {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("SdkModule")
            .field("id", &self.manifest.id)
            .finish_non_exhaustive()
    }
}

impl SdkModule {
    async fn run(&self, cx: &CommandCx, route: &Route, name: &str) -> Result<(), UserError> {
        if let Some(key) = &route.requires {
            cx.require_key(key).await?;
        }
        if let Some(cooldown) = &route.cooldown {
            self.cooldowns
                .take(name, cx.invocation(), cooldown, (self.clock)())
                .map_err(|remaining| UserError::Cooldown { remaining })?;
        }
        (route.run)(cx.clone()).await
    }

    /// A click or a choice: finds the handler named in the `custom_id`, checks that the user is
    /// the one the component was made for, and runs it.
    async fn on_component(
        &self,
        ctx: &EventContext,
        click: &ComponentInvocation,
    ) -> Result<Flow, ModuleError> {
        let cx = ComponentCx::new(ctx, click);
        let Some((_, id)) = self.own_custom_id(&click.custom_id) else {
            return Ok(Flow::Continue);
        };
        if id.expires_at.is_some_and(|at| cx.now() >= at) {
            return Self::refuse(&cx, "sdk-expired").await;
        }
        let Some(run) = self.components.get(&id.route) else {
            return Self::refuse(&cx, "sdk-gone").await;
        };
        if id.owner.is_some_and(|owner| owner != click.user.id) {
            return Self::refuse(&cx, "sdk-not-yours").await;
        }
        match run(cx.clone()).await {
            Ok(()) => {
                if !cx.has_replied() {
                    // A click nobody answers would show as failed in Discord.
                    cx.defer_update().await.map_err(module_error)?;
                }
            }
            Err(error) => Self::answer(&cx, error).await?,
        }
        Ok(Flow::Continue)
    }

    /// A context-menu command was used: the permission and the cooldown are checked as for a
    /// command, then the handler runs and answers like a command does.
    async fn on_context(
        &self,
        ctx: &EventContext,
        invocation: &ContextInvocation,
    ) -> Result<Flow, ModuleError> {
        let kind = match invocation.target {
            ContextTarget::User(_) => ContextKind::User,
            ContextTarget::Message(_) => ContextKind::Message,
        };
        let Some(route) = self.contexts.get(&(kind, invocation.command.clone())) else {
            return Ok(Flow::Continue);
        };
        let cx = ContextCx::new(ctx, invocation);
        match self.run_context(&cx, route, &invocation.command).await {
            Ok(()) => {
                if !cx.has_replied() {
                    let done = cx.core_message("sdk-done", &Args::new());
                    cx.reply_private(done).await.map_err(module_error)?;
                }
            }
            Err(error) => Self::answer(&cx, error).await?,
        }
        Ok(Flow::Continue)
    }

    async fn run_context(
        &self,
        cx: &ContextCx,
        route: &ContextRoute,
        name: &str,
    ) -> Result<(), UserError> {
        if let Some(key) = &route.requires {
            cx.require_key(key).await?;
        }
        if let Some(cooldown) = &route.cooldown {
            // Kept apart from the commands: a context command may be named like one.
            self.cooldowns
                .take(
                    &format!("context:{name}"),
                    cx.interaction(),
                    cooldown,
                    (self.clock)(),
                )
                .map_err(|remaining| UserError::Cooldown { remaining })?;
        }
        (route.run)(cx.clone()).await
    }

    /// A user typing in an option. The choices come from the handler of that option, and none
    /// when the user may not run the command, there is no handler, or the handler fails (which
    /// is logged). Discord has to be answered in any case.
    async fn on_autocomplete(
        &self,
        ctx: &EventContext,
        query: &AutocompleteInvocation,
    ) -> Result<Flow, ModuleError> {
        let cx = AutocompleteCx::new(ctx, query);
        let route_key = (query.command.clone(), query.subcommand.clone());
        let allowed = match self.routes.get(&route_key) {
            None => false,
            Some(route) => match &route.requires {
                Some(key) => cx.require_key(key).await.is_ok(),
                None => true,
            },
        };
        let handler = self.autocompletes.get(&(
            query.command.clone(),
            query.subcommand.clone(),
            query.focused_option.clone(),
        ));
        let mut choices = Vec::new();
        if let Some(run) = handler.filter(|_| allowed) {
            match run(cx.clone()).await {
                Ok(found) => {
                    choices = found
                        .into_iter()
                        .map(|choice| OptionChoice {
                            name: choice.name,
                            value: choice.value,
                        })
                        .collect();
                }
                Err(error) => tracing::warn!(
                    module = %self.manifest.id,
                    command = %query.command,
                    option = %query.focused_option,
                    ?error,
                    "an autocomplete handler failed"
                ),
            }
        }
        match cx.answer(choices).await {
            Ok(()) => Ok(Flow::Continue),
            Err(DiscordError::Invalid(reason)) => {
                tracing::warn!(
                    module = %self.manifest.id,
                    command = %query.command,
                    %reason,
                    "autocomplete choices Discord would refuse"
                );
                cx.answer(Vec::new()).await.map_err(ModuleError::Discord)?;
                Ok(Flow::Continue)
            }
            Err(error) => Err(ModuleError::Discord(error)),
        }
    }

    /// A form that was sent.
    async fn on_modal(
        &self,
        ctx: &EventContext,
        submission: &ModalSubmission,
    ) -> Result<Flow, ModuleError> {
        let cx = ModalCx::new(ctx, submission);
        let Some((_, id)) = self.own_custom_id(&submission.custom_id) else {
            return Ok(Flow::Continue);
        };
        if id.expires_at.is_some_and(|at| cx.now() >= at) {
            return Self::refuse(&cx, "sdk-expired").await;
        }
        let Some(run) = self.modals.get(&id.route) else {
            return Self::refuse(&cx, "sdk-gone").await;
        };
        if id.owner.is_some_and(|owner| owner != submission.user.id) {
            return Self::refuse(&cx, "sdk-not-yours").await;
        }
        match run(cx.clone()).await {
            Ok(()) => {
                if !cx.has_replied() {
                    let done = cx.core_message("sdk-done", &Args::new());
                    cx.reply_private(done).await.map_err(module_error)?;
                }
            }
            Err(error) => Self::answer(&cx, error).await?,
        }
        Ok(Flow::Continue)
    }

    /// A job that is due. One without a handler is a leftover of an older version of the module:
    /// failing it would only fill the log and trip the breaker, and finishing it would keep a
    /// repeating one alive for ever, so the host is told to drop it (`Flow::Stop`).
    async fn on_job(&self, ctx: &EventContext, job: &ScheduledJob) -> Result<Flow, ModuleError> {
        let Some(run) = self.jobs.get(&job.name) else {
            tracing::warn!(
                module = %self.manifest.id,
                job = %job.name,
                "a scheduled job has no handler and is dropped"
            );
            return Ok(Flow::Stop);
        };
        run(JobCx::new(ctx, job)).await?;
        Ok(Flow::Continue)
    }

    /// The `custom_id` if it is of this module and can be read.
    fn own_custom_id(&self, custom_id: &str) -> Option<(ModuleId, CustomId)> {
        let (module, id) = CustomId::parse(custom_id)?;
        (module == self.manifest.id).then_some((module, id))
    }

    /// Tells the user, privately, that this is not for them or no longer there.
    async fn refuse<I: InteractionRef + 'static>(
        cx: &Cx<I>,
        key: &str,
    ) -> Result<Flow, ModuleError> {
        cx.say_locked(Tone::Info, cx.core_message(key, &Args::new()))
            .await
            .map_err(module_error)?;
        Ok(Flow::Continue)
    }

    /// Answers the user for an error that is theirs; anything else goes on to the dispatcher.
    async fn answer<I: InteractionRef + 'static>(
        cx: &Cx<I>,
        error: UserError,
    ) -> Result<(), ModuleError> {
        let text = match error {
            UserError::Module(error) => return Err(error),
            UserError::Message { key, args } => cx.t(&key, &args),
            UserError::Sdk { key, args } => cx.core_message(&key, &args),
            UserError::BadArgument(error) => cx.core_message(
                "sdk-bad-argument",
                &Args::new().with("option", error.option),
            ),
            UserError::PermissionDenied(decision) => match decision.reason {
                closure_core::permission::Reason::Level { actual, required } => cx.core_message(
                    "sdk-permission-level",
                    &Args::new()
                        .with("required", i64::from(required.get()))
                        .with("actual", i64::from(actual.get())),
                ),
                _ => cx.core_message("sdk-permission-denied", &Args::new()),
            },
            UserError::Cooldown { remaining } => cx.core_message(
                "sdk-cooldown",
                &Args::new().with(
                    "retry",
                    ends_at(
                        cx.now(),
                        remaining + Duration::from_secs(1),
                        TimeStyle::Relative,
                    ),
                ),
            ),
            UserError::Target(problem) => cx.core_message(problem.key(), &Args::new()),
        };
        cx.say_locked(Tone::Error, text).await.map_err(module_error)
    }
}

/// The error of an answer that could not be sent: only Discord's can happen there.
fn module_error(error: UserError) -> ModuleError {
    match error {
        UserError::Module(error) => error,
        other => ModuleError::other(format!("{other:?}")),
    }
}

#[async_trait]
impl Module for SdkModule {
    fn manifest(&self) -> &ModuleManifest {
        &self.manifest
    }

    async fn on_event(&self, ctx: &EventContext, event: &Event) -> Result<(), ModuleError> {
        self.on_event_flow(ctx, event).await.map(drop)
    }

    async fn on_event_flow(&self, ctx: &EventContext, event: &Event) -> Result<Flow, ModuleError> {
        let invocation = match event {
            Event::Command(invocation) => invocation,
            Event::Component(click) => return self.on_component(ctx, click).await,
            Event::Modal(submission) => return self.on_modal(ctx, submission).await,
            Event::Scheduled(job) => return self.on_job(ctx, job).await,
            Event::Autocomplete(query) => return self.on_autocomplete(ctx, query).await,
            Event::ContextCommand(invocation) => return self.on_context(ctx, invocation).await,
            other => {
                return match self.handlers.get(&other.kind()) {
                    Some(handler) => handler(ctx.clone(), other.clone()).await,
                    None => Ok(Flow::Continue),
                };
            }
        };
        let key = (invocation.command.clone(), invocation.subcommand.clone());
        let Some(route) = self.routes.get(&key) else {
            return Ok(Flow::Continue);
        };
        let cx = CommandCx::new(ctx, invocation);
        let name = match &key.1 {
            Some(sub) => format!("{} {sub}", key.0),
            None => key.0.clone(),
        };
        match self.run(&cx, route, &name).await {
            Ok(()) => {
                if !cx.has_replied() {
                    // A command that said nothing would show as failed in Discord.
                    let done = cx.core_message("sdk-done", &Args::new());
                    cx.reply_private(done).await.map_err(module_error)?;
                }
            }
            Err(error) => Self::answer(&cx, error).await?,
        }
        Ok(Flow::Continue)
    }
}
