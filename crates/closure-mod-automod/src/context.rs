//! What an event is about, as the rules see it: the member, the channel, the message, and what
//! happened. Every event, and every counter trigger that fires, becomes one [`Context`].

use closure_core::error::DiscordError;
use closure_core::event::{ThreadInfo, UserInfo};
use closure_sdk::{Case, ChannelId, Event, EventContext, MessageInfo, ModuleError, RoleId, UserId};

/// One event, or one counter trigger that fired, as the rules read it.
#[derive(Debug, Clone)]
pub struct Context {
    /// The member the event is about, when it has one.
    pub user: Option<Who>,
    pub channel: Option<ChannelId>,
    /// The message, when the event is a message.
    pub message: Option<MessageInfo>,
    pub source: Source,
    /// Whether the message is new. An edit is only read by the triggers that read text.
    pub created: bool,
}

/// A member, as far as the rules know it. Roles and the join time are looked up the first time a
/// rule needs them, when the event did not carry them.
#[derive(Debug, Clone)]
pub struct Who {
    pub id: UserId,
    pub name: String,
    pub bot: bool,
    pub roles: Option<Vec<RoleId>>,
    pub joined: Join,
}

/// When a member joined, as far as the rules know it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Join {
    /// Not looked up yet.
    NotLooked,
    /// Unix seconds.
    At(i64),
    /// Discord does not know when the member joined.
    Unknown,
}

/// What happened.
#[derive(Debug, Clone)]
pub enum Source {
    Message,
    Join,
    Leave,
    RoleChange {
        added: Vec<RoleId>,
        removed: Vec<RoleId>,
    },
    Case(Case),
    Thread(ThreadEvent),
    /// A trigger of a counter fired (or its reverse did).
    Counter {
        counter: String,
        trigger: String,
        reverse: bool,
    },
}

/// What happened to a thread.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ThreadEvent {
    Created,
    Archived,
    Unarchived,
    Deleted,
}

impl Who {
    /// A member the event names by id only.
    #[must_use]
    pub fn bare(id: UserId, name: impl Into<String>, bot: bool) -> Self {
        Self {
            id,
            name: name.into(),
            bot,
            roles: None,
            joined: Join::NotLooked,
        }
    }

    /// The roles of the member, looked up when they are not known yet.
    pub async fn roles(&mut self, ctx: &EventContext) -> Result<&[RoleId], ModuleError> {
        if self.roles.is_none() {
            self.lookup(ctx).await?;
        }
        Ok(self.roles.as_deref().unwrap_or(&[]))
    }

    /// When the member joined, looked up when it is not known yet.
    pub async fn joined(&mut self, ctx: &EventContext) -> Result<Option<i64>, ModuleError> {
        if self.joined == Join::NotLooked {
            self.lookup(ctx).await?;
        }
        Ok(match self.joined {
            Join::At(at) => Some(at),
            Join::NotLooked | Join::Unknown => None,
        })
    }

    /// One lookup fills in both; a member that left is a member with no roles and no join time.
    async fn lookup(&mut self, ctx: &EventContext) -> Result<(), ModuleError> {
        match ctx.api.get_member(ctx.guild_id, self.id).await {
            Ok(member) => {
                self.roles = Some(member.roles);
                self.joined = member
                    .joined_at
                    .map_or(Join::Unknown, |at| Join::At(at.as_secs()));
            }
            Err(DiscordError::NotFound) => {
                self.roles = Some(Vec::new());
                self.joined = Join::Unknown;
            }
            Err(error) => return Err(error.into()),
        }
        Ok(())
    }
}

impl Context {
    /// A message, new or edited.
    #[must_use]
    pub fn message(message: &MessageInfo, created: bool) -> Self {
        Self {
            user: Some(Who {
                id: message.author.id,
                name: message.author.name.clone(),
                bot: message.author.bot,
                roles: Some(message.author_roles.clone()),
                joined: Join::NotLooked,
            }),
            channel: Some(message.channel_id),
            message: Some(message.clone()),
            source: Source::Message,
            created,
        }
    }

    /// The context of a gateway event, or `None` for an event the rules do not read.
    #[must_use]
    pub fn of_event(event: &Event) -> Option<Self> {
        let plain = |source, user: Option<Who>, channel| Self {
            user,
            channel,
            message: None,
            source,
            created: true,
        };
        Some(match event {
            Event::MessageCreated(message) => Self::message(message, true),
            Event::MessageUpdated(message) => Self::message(message, false),
            Event::MemberJoined(info) => plain(
                Source::Join,
                Some(Who {
                    id: info.user.id,
                    name: info.user.name.clone(),
                    bot: info.user.bot,
                    roles: Some(info.roles.clone()),
                    joined: info.joined_at.map_or(Join::Unknown, Join::At),
                }),
                None,
            ),
            Event::MemberLeft { user, .. } => plain(Source::Leave, Some(who_of(user)), None),
            Event::RolesChanged(change) => plain(
                Source::RoleChange {
                    added: change.added.clone(),
                    removed: change.removed.clone(),
                },
                Some(who_of(&change.user)),
                None,
            ),
            Event::CaseCreated(case) => plain(
                Source::Case(case.clone()),
                Some(Who::bare(case.target, case.target.get().to_string(), false)),
                None,
            ),
            Event::ThreadCreated(thread) => thread_context(thread, ThreadEvent::Created),
            Event::ThreadArchived(thread) => thread_context(thread, ThreadEvent::Archived),
            Event::ThreadUnarchived(thread) => thread_context(thread, ThreadEvent::Unarchived),
            Event::ThreadDeleted { parent_id, .. } => {
                plain(Source::Thread(ThreadEvent::Deleted), None, Some(*parent_id))
            }
            _ => return None,
        })
    }

    /// The member the event is about, when it is the user of the context.
    #[must_use]
    pub fn user_id(&self) -> Option<UserId> {
        self.user.as_ref().map(|who| who.id)
    }
}

fn who_of(user: &UserInfo) -> Who {
    Who::bare(user.id, user.name.clone(), user.bot)
}

fn thread_context(thread: &ThreadInfo, event: ThreadEvent) -> Context {
    Context {
        user: thread
            .owner_id
            .map(|owner| Who::bare(owner, owner.get().to_string(), false)),
        channel: Some(thread.parent_id.unwrap_or(thread.id)),
        message: None,
        source: Source::Thread(event),
        created: true,
    }
}
