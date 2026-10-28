//! The module SDK of closure: declare a command, write what it does.
//!
//! ```ignore
//! #[derive(CommandArgs)]
//! struct Warn {
//!     /// Member to warn
//!     user: UserId,
//!     /// Why
//!     #[arg(max_length = 512)]
//!     reason: Option<String>,
//! }
//!
//! pub fn module() -> Result<SdkModule, CoreError> {
//!     ModuleBuilder::new("warn", "Warnings", env!("CARGO_PKG_VERSION"))?
//!         .permission(PermissionSpec::new("warn.add", Level::MODERATOR, "Warn members")?)
//!         .command(
//!             Command::<Warn>::new("warn", "Warn a member")
//!                 .requires("warn.add")
//!                 .cooldown(Cooldown::per_user(Duration::from_secs(5)))
//!                 .handler(|cx, args| async move {
//!                     cx.reply_t("warned", &Args::new().with("user", args.user.get())).await
//!                 }),
//!         )
//!         .catalog(Locale::En, EN)
//!         .catalog(Locale::Ja, JA)
//!         .build()
//! }
//! ```
//!
//! What the SDK does for a command: checks the permission and the cooldown, reads the options
//! into the struct, runs the handler, and answers the user in their language when they are not
//! allowed, have to wait, or the handler returns [`UserError::message`]. Tests use
//! [`testing::Harness`].

// The derive macro names this crate by its own name.
extern crate self as closure_sdk;

pub mod args;
mod cooldown;
pub mod counters;
mod cx;
pub mod dm;
pub mod duration;
pub mod embed;
mod error;
pub mod files;
pub mod modlog;
mod module;
pub mod punish;
pub mod recent;
mod span;
pub mod template;
pub mod testing;
pub mod text;
pub mod ui;

pub use args::{ArgError, ArgProblem, Attachment, CommandArgs, OptionType};
pub use closure_core::action::{InteractionReply, Reason};
pub use closure_core::capability::Capability;
pub use closure_core::cases::{Case, CaseError, CaseKind, Cases, NewCase, Revoked};
pub use closure_core::error::{CoreError, DiscordError, ModuleError};
pub use closure_core::event::{AttachmentInfo, Event, EventKind, InteractionRef, MessageInfo};
pub use closure_core::event::{
    ChannelId, CommandInvocation, GuildId, Mentionable, RoleId, UserId, UserInfo,
};
pub use closure_core::history::{HistoryError, MessageHistory, SavedMessage};
pub use closure_core::locale::{Args, Locale};
pub use closure_core::module::EventContext;
pub use closure_core::module::{ChoiceValue, ModuleId, OptionChoice, OptionKind, OptionSpec};
pub use closure_core::permission::{Level, PermissionKey, PermissionSpec};
pub use closure_core::schedule::{JobBuilder, ScheduleError, ScheduledJob, Scheduler};
pub use closure_core::storage::{Entry, Expect, StorageError, Store};
pub use closure_core::subscription::{EventFilter, Flow, Subscription};
pub use closure_sdk_macros::CommandArgs;
pub use cooldown::{Cooldown, CooldownScope};
pub use cx::{
    AutocompleteCx, CommandCx, ComponentCx, ContextCx, Cx, JobCx, ModalCx, parse_module_config,
};
pub use dm::{DirectMessage, Sent};
pub use duration::{DurationError, TimeStyle};
pub use embed::{Color, EmbedBuilder, EmbedError, Tone};
pub use error::{TargetProblem, UserError};
pub use module::{
    BoundCommand, BoundContext, Choice, Command, ContextCommand, ModuleBuilder, SdkModule,
    SubcommandGroup,
};
pub use span::{Span, SpanError};
pub use twilight_model::gateway::Intents;
pub use ui::{CustomId, Pager};
