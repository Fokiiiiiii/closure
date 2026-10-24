use closure_core::action::{
    AuditLogQuery, AutoModAction, AutoModTrigger, NewAutoModRule, Outcome, Reason,
};
use closure_core::error::DiscordError;
use closure_core::event::GuildId;
use twilight_http::request::AuditLogReason;
use twilight_model::guild::audit_log::AuditLogEventType;
use twilight_model::guild::auto_moderation::AutoModerationEventType;
use twilight_model::id::Id;
use twilight_model::id::marker::AutoModerationRuleMarker;

use super::{TwilightApi, read, read_list, run};

impl TwilightApi {
    pub(super) async fn list_auto_mod_rules(
        &self,
        guild: GuildId,
    ) -> Result<Outcome, DiscordError> {
        read_list(self.http.auto_moderation_rules(guild))
            .await
            .map(Outcome::AutoModRules)
    }

    pub(super) async fn create_auto_mod_rule(
        &self,
        guild: GuildId,
        spec: &NewAutoModRule,
        reason: &Reason,
    ) -> Result<Outcome, DiscordError> {
        let mut request = self
            .http
            .create_auto_moderation_rule(guild, &spec.name, AutoModerationEventType::MessageSend)
            .enabled(spec.enabled)
            .exempt_roles(&spec.exempt_roles)
            .exempt_channels(&spec.exempt_channels)
            .reason(reason.as_str());
        for action in &spec.actions {
            request = match action {
                AutoModAction::BlockMessage {
                    custom_message: None,
                } => request.action_block_message(),
                AutoModAction::BlockMessage {
                    custom_message: Some(message),
                } => request.action_block_message_with_explanation(message),
                AutoModAction::SendAlert { channel_id } => {
                    request.action_send_alert_message(*channel_id)
                }
                AutoModAction::Timeout { duration_seconds } => {
                    request.action_timeout(*duration_seconds)
                }
            };
        }

        let rule = match &spec.trigger {
            AutoModTrigger::Keyword {
                keywords,
                regex_patterns,
                allow_list,
            } => {
                let keywords: Vec<&str> = keywords.iter().map(String::as_str).collect();
                let regex: Vec<&str> = regex_patterns.iter().map(String::as_str).collect();
                let allow: Vec<&str> = allow_list.iter().map(String::as_str).collect();
                read(request.with_keyword(&keywords, &regex, &allow)).await?
            }
            AutoModTrigger::Spam => read(request.with_spam()).await?,
            AutoModTrigger::MentionSpam { total_limit } => {
                read(request.with_mention_spam(*total_limit)).await?
            }
        };
        Ok(Outcome::AutoModRule(Box::new(rule)))
    }

    pub(super) async fn delete_auto_mod_rule(
        &self,
        guild: GuildId,
        rule: Id<AutoModerationRuleMarker>,
        reason: &Reason,
    ) -> Result<Outcome, DiscordError> {
        run(self
            .http
            .delete_auto_moderation_rule(guild, rule)
            .reason(reason.as_str()))
        .await
    }

    pub(super) async fn get_audit_log(
        &self,
        guild: GuildId,
        query: &AuditLogQuery,
    ) -> Result<Outcome, DiscordError> {
        let mut request = self.http.audit_log(guild);
        if let Some(user) = query.user_id {
            request = request.user_id(user);
        }
        if let Some(action_type) = query.action_type {
            request = request.action_type(AuditLogEventType::from(action_type));
        }
        if let Some(before) = query.before {
            request = request.before(before);
        }
        if let Some(limit) = query.limit {
            request = request.limit(limit);
        }
        read(request)
            .await
            .map(|log| Outcome::AuditLog(Box::new(log)))
    }
}
