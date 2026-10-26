use closure_core::action::{
    EventLocation, NewScheduledEvent, Outcome, ScheduledEventPatch, ScheduledEventStatus,
};
use closure_core::error::DiscordError;
use closure_core::event::GuildId;
use twilight_model::guild::scheduled_event::{PrivacyLevel, Status};
use twilight_model::id::Id;
use twilight_model::id::marker::ScheduledEventMarker;
use twilight_model::util::Timestamp;

use super::{TwilightApi, read, read_list, run};

fn timestamp(unix: i64) -> Result<Timestamp, DiscordError> {
    Timestamp::from_secs(unix).map_err(|error| DiscordError::Invalid(error.to_string()))
}

impl TwilightApi {
    pub(super) async fn list_scheduled_events(
        &self,
        guild: GuildId,
        with_user_count: bool,
    ) -> Result<Outcome, DiscordError> {
        read_list(
            self.http
                .guild_scheduled_events(guild)
                .with_user_count(with_user_count),
        )
        .await
        .map(Outcome::ScheduledEvents)
    }

    /// Discord takes no audit log reason for scheduled events.
    pub(super) async fn create_scheduled_event(
        &self,
        guild: GuildId,
        spec: &NewScheduledEvent,
    ) -> Result<Outcome, DiscordError> {
        let start = timestamp(spec.start_unix)?;
        let end = spec.end_unix.map(timestamp).transpose()?;
        let request = self
            .http
            .create_guild_scheduled_event(guild, PrivacyLevel::GuildOnly);
        let event = match &spec.location {
            EventLocation::Voice { channel_id } => {
                let mut request = request.voice(*channel_id, &spec.name, &start);
                if let Some(end) = &end {
                    request = request.scheduled_end_time(end);
                }
                if let Some(description) = &spec.description {
                    request = request.description(description);
                }
                read(request).await?
            }
            EventLocation::External { place } => {
                // Validation has made sure an external event has an end.
                let Some(end) = &end else {
                    return Err(DiscordError::Invalid(
                        "an external event needs an end time".into(),
                    ));
                };
                let mut request = request.external(&spec.name, place, &start, end);
                if let Some(description) = &spec.description {
                    request = request.description(description);
                }
                read(request).await?
            }
        };
        Ok(Outcome::ScheduledEvent(Box::new(event)))
    }

    pub(super) async fn edit_scheduled_event(
        &self,
        guild: GuildId,
        event: Id<ScheduledEventMarker>,
        patch: &ScheduledEventPatch,
    ) -> Result<Outcome, DiscordError> {
        let mut request = self.http.update_guild_scheduled_event(guild, event);
        if let Some(name) = &patch.name {
            request = request.name(name);
        }
        if let Some(status) = patch.status {
            let status = match status {
                ScheduledEventStatus::Active => Status::Active,
                ScheduledEventStatus::Completed => Status::Completed,
                ScheduledEventStatus::Canceled => Status::Cancelled,
            };
            request = request.status(status);
        }
        read(request)
            .await
            .map(|event| Outcome::ScheduledEvent(Box::new(event)))
    }

    pub(super) async fn delete_scheduled_event(
        &self,
        guild: GuildId,
        event: Id<ScheduledEventMarker>,
    ) -> Result<Outcome, DiscordError> {
        run(self.http.delete_guild_scheduled_event(guild, event)).await
    }
}
