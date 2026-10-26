use closure_core::action::{Outcome, Reason, ThreadKind, ThreadPatch};
use closure_core::error::DiscordError;
use closure_core::event::{ChannelId, MessageId, UserId};
use twilight_http::request::AuditLogReason;
use twilight_model::channel::ChannelType;
use twilight_model::channel::thread::AutoArchiveDuration;

use super::{TwilightApi, read, read_list, run};

/// Discord's four archive times. Anything else is refused, not rounded.
fn archive_duration(minutes: u16) -> Result<AutoArchiveDuration, DiscordError> {
    match minutes {
        60 => Ok(AutoArchiveDuration::Hour),
        1440 => Ok(AutoArchiveDuration::Day),
        4320 => Ok(AutoArchiveDuration::ThreeDays),
        10080 => Ok(AutoArchiveDuration::Week),
        other => Err(DiscordError::Invalid(format!(
            "auto archive of {other} minutes is not offered"
        ))),
    }
}

impl TwilightApi {
    pub(super) async fn create_thread(
        &self,
        channel: ChannelId,
        name: &str,
        source: Option<MessageId>,
        kind: ThreadKind,
        auto_archive_minutes: u16,
    ) -> Result<Outcome, DiscordError> {
        let duration = archive_duration(auto_archive_minutes)?;
        let thread = if let Some(message) = source {
            read(
                self.http
                    .create_thread_from_message(channel, message, name)
                    .auto_archive_duration(duration),
            )
            .await?
        } else {
            let kind = match kind {
                ThreadKind::Public => ChannelType::PublicThread,
                ThreadKind::Private => ChannelType::PrivateThread,
            };
            read(
                self.http
                    .create_thread(channel, name, kind)
                    .auto_archive_duration(duration),
            )
            .await?
        };
        Ok(Outcome::Channel(Box::new(thread)))
    }

    pub(super) async fn edit_thread(
        &self,
        thread: ChannelId,
        patch: &ThreadPatch,
        reason: &Reason,
    ) -> Result<Outcome, DiscordError> {
        let mut request = self.http.update_thread(thread).reason(reason.as_str());
        if let Some(name) = &patch.name {
            request = request.name(name);
        }
        if let Some(archived) = patch.archived {
            request = request.archived(archived);
        }
        if let Some(locked) = patch.locked {
            request = request.locked(locked);
        }
        if let Some(seconds) = patch.slowmode_seconds {
            request = request.rate_limit_per_user(seconds);
        }
        read(request)
            .await
            .map(|thread| Outcome::Channel(Box::new(thread)))
    }

    pub(super) async fn join_thread(&self, thread: ChannelId) -> Result<Outcome, DiscordError> {
        run(self.http.join_thread(thread)).await
    }

    pub(super) async fn leave_thread(&self, thread: ChannelId) -> Result<Outcome, DiscordError> {
        run(self.http.leave_thread(thread)).await
    }

    pub(super) async fn add_thread_member(
        &self,
        thread: ChannelId,
        user: UserId,
    ) -> Result<Outcome, DiscordError> {
        run(self.http.add_thread_member(thread, user)).await
    }

    pub(super) async fn remove_thread_member(
        &self,
        thread: ChannelId,
        user: UserId,
    ) -> Result<Outcome, DiscordError> {
        run(self.http.remove_thread_member(thread, user)).await
    }

    pub(super) async fn list_thread_members(
        &self,
        thread: ChannelId,
        after: Option<UserId>,
        limit: u16,
    ) -> Result<Outcome, DiscordError> {
        let mut request = self.http.thread_members(thread).limit(u32::from(limit));
        if let Some(after) = after {
            request = request.after(after);
        }
        read_list(request).await.map(Outcome::ThreadMembers)
    }
}
