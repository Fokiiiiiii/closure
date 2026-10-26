//! Checks what actually goes over the wire, against a local stand-in for Discord.

use std::collections::HashMap;
use std::net::SocketAddr;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use closure_core::action::{
    Action, ChannelPosition, Emoji, EventLocation, GuildPatch, InteractionReply,
    InteractionResponse, MentionPolicy, MessageCursor, NewScheduledEvent, Outcome, Reason,
    ScheduledEventPatch, ScheduledEventStatus, ThreadKind, ThreadPatch, WebhookToken,
};
use closure_core::client::DiscordApi;
use closure_core::error::DiscordError;
use closure_core::event::InteractionToken;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpListener;
use twilight_http::Client;
use twilight_model::guild::{RolePosition, VerificationLevel};
use twilight_model::id::Id;

use super::{RetryPolicy, TwilightApi};
use crate::hierarchy::CacheView;

#[derive(Debug, Clone)]
struct Recorded {
    method: String,
    path: String,
    headers: HashMap<String, String>,
    body: String,
}

struct FakeDiscord {
    addr: SocketAddr,
    requests: Arc<Mutex<Vec<Recorded>>>,
}

impl FakeDiscord {
    /// Answers the n-th request with `responses[n]`, repeating the last one afterwards.
    async fn start(responses: Vec<(u16, &'static str)>) -> Self {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let requests = Arc::new(Mutex::new(Vec::new()));
        let seen = Arc::clone(&requests);

        tokio::spawn(async move {
            let mut served = 0;
            loop {
                let Ok((mut socket, _)) = listener.accept().await else {
                    return;
                };
                let recorded = read_request(&mut socket).await;
                seen.lock().unwrap().push(recorded);

                let (status, body) = responses[served.min(responses.len() - 1)];
                served += 1;
                let reply = format!(
                    "HTTP/1.1 {status} X\r\ncontent-type: application/json\r\ncontent-length: {}\r\nconnection: close\r\n\r\n{body}",
                    body.len()
                );
                let _ = socket.write_all(reply.as_bytes()).await;
                let _ = socket.shutdown().await;
            }
        });
        Self { addr, requests }
    }

    fn api(&self) -> TwilightApi {
        let http = Client::builder()
            .token("test-token".to_owned())
            .proxy(self.addr.to_string(), true)
            .ratelimiter(None)
            .timeout(Duration::from_secs(5))
            .build();
        TwilightApi::new(Arc::new(http), Id::new(1))
            .with_clock(|| 1_700_000_000)
            .with_retry(RetryPolicy {
                max_retries: 2,
                base_delay: Duration::from_millis(1),
            })
    }

    fn requests(&self) -> Vec<Recorded> {
        self.requests.lock().unwrap().clone()
    }
}

async fn read_request(socket: &mut tokio::net::TcpStream) -> Recorded {
    let mut buffer = Vec::new();
    let mut chunk = [0u8; 4096];
    let header_end = loop {
        let read = socket.read(&mut chunk).await.unwrap();
        assert!(read > 0, "connection closed before the headers ended");
        buffer.extend_from_slice(&chunk[..read]);
        if let Some(position) = buffer.windows(4).position(|w| w == b"\r\n\r\n") {
            break position + 4;
        }
    };
    let head = String::from_utf8_lossy(&buffer[..header_end]).into_owned();
    let mut lines = head.lines();
    let request_line = lines.next().unwrap();
    let mut parts = request_line.split_whitespace();
    let method = parts.next().unwrap().to_owned();
    let path = parts.next().unwrap().to_owned();
    let headers: HashMap<String, String> = lines
        .filter_map(|line| line.split_once(':'))
        .map(|(name, value)| (name.trim().to_ascii_lowercase(), value.trim().to_owned()))
        .collect();

    let length: usize = headers
        .get("content-length")
        .and_then(|value| value.parse().ok())
        .unwrap_or(0);
    while buffer.len() < header_end + length {
        let read = socket.read(&mut chunk).await.unwrap();
        assert!(read > 0, "connection closed before the body ended");
        buffer.extend_from_slice(&chunk[..read]);
    }
    let body = String::from_utf8_lossy(&buffer[header_end..header_end + length]).into_owned();
    Recorded {
        method,
        path,
        headers,
        body,
    }
}

fn reason(text: &str) -> Reason {
    Reason::new(text).unwrap()
}

fn kick() -> Action {
    Action::Kick {
        guild_id: Id::new(10),
        user_id: Id::new(20),
        reason: reason("spam"),
    }
}

const NOT_FOUND: &str = r#"{"code":10007,"message":"Unknown Member"}"#;
const FORBIDDEN: &str = r#"{"code":50013,"message":"Missing Permissions"}"#;

#[tokio::test]
async fn ban_carries_the_reason_and_the_message_window() {
    let discord = FakeDiscord::start(vec![(204, "")]).await;
    let outcome = discord
        .api()
        .execute(Action::Ban {
            guild_id: Id::new(10),
            user_id: Id::new(20),
            delete_message_seconds: 3600,
            reason: reason("spam"),
        })
        .await;

    assert_eq!(outcome, Ok(Outcome::None));
    let request = &discord.requests()[0];
    assert_eq!(request.method, "PUT");
    assert!(
        request.path.ends_with("/guilds/10/bans/20"),
        "{}",
        request.path
    );
    assert_eq!(request.headers["x-audit-log-reason"], "spam");
    assert!(
        request.body.contains("\"delete_message_seconds\":3600"),
        "{}",
        request.body
    );
    assert_eq!(request.headers["authorization"], "Bot test-token");
}

#[tokio::test]
async fn non_ascii_reasons_are_sent_percent_encoded() {
    let discord = FakeDiscord::start(vec![(204, "")]).await;
    discord
        .api()
        .execute(Action::Kick {
            guild_id: Id::new(10),
            user_id: Id::new(20),
            reason: reason("スパム"),
        })
        .await
        .unwrap();
    let header = &discord.requests()[0].headers["x-audit-log-reason"];
    assert!(header.is_ascii(), "{header}");
    assert!(header.contains("%E3%82%B9"), "{header}");
}

#[tokio::test]
async fn kick_removes_the_member() {
    let discord = FakeDiscord::start(vec![(204, "")]).await;
    discord.api().execute(kick()).await.unwrap();
    let request = &discord.requests()[0];
    assert_eq!(request.method, "DELETE");
    assert!(
        request.path.ends_with("/guilds/10/members/20"),
        "{}",
        request.path
    );
}

#[tokio::test]
async fn voice_mutes_are_patched_and_a_member_outside_voice_is_reported() {
    const NOT_IN_VOICE: &str =
        r#"{"message":"Target user is not connected to voice","code":40032}"#;
    let discord = FakeDiscord::start(vec![(200, "{}"), (400, NOT_IN_VOICE)]).await;
    let mute = |mute| Action::SetVoiceMute {
        guild_id: Id::new(10),
        user_id: Id::new(20),
        mute,
        reason: reason("noisy"),
    };
    let api = discord.api();
    assert_eq!(api.execute(mute(false)).await.map(drop), Ok(()));
    assert_eq!(
        api.execute(mute(true)).await.map(drop),
        Err(DiscordError::NotInVoice)
    );

    let requests = discord.requests();
    assert_eq!(requests[0].method, "PATCH");
    assert!(
        requests[0].body.contains("\"mute\":false"),
        "{}",
        requests[0].body
    );
    assert!(
        requests[1].body.contains("\"mute\":true"),
        "{}",
        requests[1].body
    );
}

#[tokio::test]
async fn timeouts_are_sent_as_an_absolute_deadline_and_none_lifts_them() {
    let discord = FakeDiscord::start(vec![(200, "{}")]).await;
    let timeout = |duration_seconds| Action::Timeout {
        guild_id: Id::new(10),
        user_id: Id::new(20),
        duration_seconds,
        reason: reason("calm down"),
    };
    let api = discord.api();
    let _ = api.execute(timeout(Some(3600))).await;
    let _ = api.execute(timeout(None)).await;

    let requests = discord.requests();
    assert_eq!(requests[0].method, "PATCH");
    // 1_700_000_000 is 2023-11-14T22:13:20Z; an hour later is 23:13:20.
    assert!(
        requests[0].body.contains("2023-11-14T23:13:20"),
        "{}",
        requests[0].body
    );
    assert!(
        requests[1]
            .body
            .contains("\"communication_disabled_until\":null"),
        "{}",
        requests[1].body
    );
}

#[tokio::test]
async fn outgoing_messages_never_ping_anyone_by_default() {
    let discord = FakeDiscord::start(vec![(200, "{}")]).await;
    let _ = discord
        .api()
        .execute(Action::SendMessage {
            channel_id: Id::new(30),
            content: Some("hello @everyone".into()),
            embeds: vec![],
            reply_to: None,
            mentions: MentionPolicy::None,
        })
        .await;
    let body: serde_json::Value = serde_json::from_str(&discord.requests()[0].body).unwrap();
    assert_eq!(body["content"], "hello @everyone");
    assert_eq!(body["allowed_mentions"]["parse"], serde_json::json!([]));
}

#[tokio::test]
async fn editing_leaves_unset_fields_out_of_the_request() {
    let discord = FakeDiscord::start(vec![(200, "{}")]).await;
    let _ = discord
        .api()
        .execute(Action::EditMessage {
            channel_id: Id::new(30),
            message_id: Id::new(31),
            content: None,
            embeds: None,
        })
        .await;
    let body: serde_json::Value = serde_json::from_str(&discord.requests()[0].body).unwrap();
    assert!(body.get("content").is_none(), "{body}");
    assert!(body.get("embeds").is_none(), "{body}");
}

#[tokio::test]
async fn deferring_an_interaction_privately_sets_the_ephemeral_flag() {
    let discord = FakeDiscord::start(vec![(204, "")]).await;
    discord
        .api()
        .execute(Action::Respond {
            interaction_id: Id::new(40),
            token: InteractionToken::new("tok"),
            response: InteractionResponse::Defer { ephemeral: true },
        })
        .await
        .unwrap();
    let request = &discord.requests()[0];
    assert_eq!(request.method, "POST");
    assert!(
        request.path.ends_with("/interactions/40/tok/callback"),
        "{}",
        request.path
    );
    let body: serde_json::Value = serde_json::from_str(&request.body).unwrap();
    assert_eq!(body["type"], 5);
    assert_eq!(body["data"]["flags"], 64);
}

#[tokio::test]
async fn an_update_or_an_edit_without_components_takes_the_old_ones_off() {
    let discord = FakeDiscord::start(vec![(204, ""), (200, "{}")]).await;
    let api = discord.api();
    api.execute(Action::Respond {
        interaction_id: Id::new(40),
        token: InteractionToken::new("tok"),
        response: InteractionResponse::Update {
            reply: InteractionReply {
                content: Some("done".into()),
                ..InteractionReply::default()
            },
        },
    })
    .await
    .unwrap();
    api.execute(Action::EditResponse {
        token: InteractionToken::new("tok"),
        reply: InteractionReply {
            content: Some("done".into()),
            ..InteractionReply::default()
        },
    })
    .await
    .unwrap();
    let requests = discord.requests();
    let update: serde_json::Value = serde_json::from_str(&requests[0].body).unwrap();
    assert_eq!(
        update["data"]["components"],
        serde_json::json!([]),
        "{update}"
    );
    assert_eq!(update["data"]["embeds"], serde_json::json!([]), "{update}");
    let edit: serde_json::Value = serde_json::from_str(&requests[1].body).unwrap();
    assert_eq!(edit["components"], serde_json::json!([]), "{edit}");
    assert_eq!(edit["embeds"], serde_json::json!([]), "{edit}");
}

#[tokio::test]
async fn http_failures_map_to_the_error_taxonomy() {
    let missing = FakeDiscord::start(vec![(404, NOT_FOUND)]).await;
    assert_eq!(
        missing.api().execute(kick()).await,
        Err(DiscordError::NotFound)
    );

    let forbidden = FakeDiscord::start(vec![(403, FORBIDDEN)]).await;
    let result = forbidden.api().execute(kick()).await;
    assert!(
        matches!(&result, Err(DiscordError::Forbidden(message)) if message.contains("Missing Permissions")),
        "{result:?}"
    );

    let broken = FakeDiscord::start(vec![(500, "{}")]).await;
    assert!(matches!(
        broken.api().execute(kick()).await,
        Err(DiscordError::Transient(_))
    ));
}

fn list_roles() -> Action {
    Action::ListRoles {
        guild_id: Id::new(10),
    }
}

#[tokio::test]
async fn reads_are_retried_after_transient_failures() {
    let discord = FakeDiscord::start(vec![(502, "{}"), (503, "{}"), (200, "[]")]).await;
    let outcome = discord.api().execute(list_roles()).await;
    assert_eq!(outcome, Ok(Outcome::Roles(vec![])));
    assert_eq!(discord.requests().len(), 3);
}

#[tokio::test]
async fn read_retries_are_bounded() {
    let discord = FakeDiscord::start(vec![(502, "{}")]).await;
    let outcome = discord.api().execute(list_roles()).await;
    assert!(
        matches!(outcome, Err(DiscordError::Transient(_))),
        "{outcome:?}"
    );
    assert_eq!(discord.requests().len(), 3, "one attempt plus two retries");
}

#[tokio::test]
async fn writes_are_never_retried() {
    let discord = FakeDiscord::start(vec![(502, "{}"), (204, "")]).await;
    let outcome = discord.api().execute(kick()).await;
    assert!(matches!(outcome, Err(DiscordError::Transient(_))));
    assert_eq!(discord.requests().len(), 1);
}

#[tokio::test]
async fn permanent_read_failures_are_not_retried() {
    let discord = FakeDiscord::start(vec![(404, NOT_FOUND), (200, "[]")]).await;
    assert_eq!(
        discord.api().execute(list_roles()).await,
        Err(DiscordError::NotFound)
    );
    assert_eq!(discord.requests().len(), 1);
}

#[tokio::test]
async fn listing_channels_is_a_read_of_the_guild() {
    let discord = FakeDiscord::start(vec![(502, "{}"), (200, "[]")]).await;
    let list = Action::ListChannels {
        guild_id: Id::new(10),
    };
    assert_eq!(
        discord.api().execute(list).await,
        Ok(Outcome::Channels(vec![]))
    );
    assert_eq!(discord.requests().len(), 2, "a read is retried once");
}

/// A cache that believes the bot (user 2, role 10) ranks below the target (user 3, role 11).
struct BelievesBotRanksLower;

impl CacheView for BelievesBotRanksLower {
    fn bot_user(&self) -> Option<closure_core::event::UserId> {
        Some(Id::new(2))
    }

    fn guild_owner(&self, _: closure_core::event::GuildId) -> Option<closure_core::event::UserId> {
        Some(Id::new(1))
    }

    fn member_roles(
        &self,
        _: closure_core::event::GuildId,
        user: closure_core::event::UserId,
    ) -> Option<Vec<closure_core::event::RoleId>> {
        Some(vec![Id::new(if user.get() == 2 { 10 } else { 11 })])
    }

    fn role_position(&self, role: closure_core::event::RoleId) -> Option<i64> {
        Some(if role.get() == 10 { 1 } else { 5 })
    }
}

fn kick_target() -> Action {
    Action::Kick {
        guild_id: Id::new(10),
        user_id: Id::new(3),
        reason: reason("spam"),
    }
}

#[tokio::test]
async fn a_stale_cache_cannot_block_an_action_discord_allows() {
    let discord = FakeDiscord::start(vec![(204, "")]).await;
    let api = discord.api().with_cache(Arc::new(BelievesBotRanksLower));
    assert_eq!(api.execute(kick_target()).await, Ok(Outcome::None));
    assert_eq!(
        discord.requests().len(),
        1,
        "the request was sent despite the suspicion"
    );
}

#[tokio::test]
async fn a_403_after_a_rank_suspicion_is_explained_as_a_hierarchy_problem() {
    let discord = FakeDiscord::start(vec![(403, FORBIDDEN)]).await;
    let api = discord.api().with_cache(Arc::new(BelievesBotRanksLower));
    let result = api.execute(kick_target()).await;
    assert!(
        matches!(result, Err(DiscordError::HierarchyViolation(_))),
        "{result:?}"
    );
}

#[tokio::test]
async fn a_403_without_a_suspicion_stays_a_plain_forbidden() {
    let discord = FakeDiscord::start(vec![(403, FORBIDDEN)]).await;
    let result = discord.api().execute(kick_target()).await;
    assert!(
        matches!(result, Err(DiscordError::Forbidden(_))),
        "{result:?}"
    );
}

#[tokio::test]
async fn the_server_owner_is_rejected_without_sending_anything() {
    let discord = FakeDiscord::start(vec![(204, "")]).await;
    let api = discord.api().with_cache(Arc::new(BelievesBotRanksLower));
    let owner = Action::Kick {
        guild_id: Id::new(10),
        user_id: Id::new(1),
        reason: reason("spam"),
    };
    let result = api.execute(owner).await;
    assert!(
        matches!(result, Err(DiscordError::HierarchyViolation(_))),
        "{result:?}"
    );
    assert_eq!(discord.requests().len(), 0);
}

#[tokio::test]
async fn the_guild_list_is_read_page_by_page_until_a_short_page() {
    let page = |range: std::ops::Range<u64>| -> &'static str {
        let items: Vec<String> = range
            .map(|id| {
                format!(
                    r#"{{"id":"{id}","name":"g","icon":null,"owner":false,"permissions":"0","features":[]}}"#
                )
            })
            .collect();
        Box::leak(format!("[{}]", items.join(",")).into_boxed_str())
    };
    let discord = FakeDiscord::start(vec![(200, page(1..201)), (200, page(201..204))]).await;

    let http = discord.api().http().clone();
    let guilds = super::joined_guilds(&http).await.unwrap();

    assert_eq!(guilds.len(), 203);
    assert_eq!(guilds.last().unwrap().get(), 203);
    let requests = discord.requests();
    assert_eq!(requests.len(), 2);
    assert!(
        requests[0].path.contains("limit=200"),
        "{}",
        requests[0].path
    );
    assert!(
        requests[1].path.contains("after=200"),
        "{}",
        requests[1].path
    );
}

#[tokio::test]
async fn the_guild_of_a_channel_is_read_from_discord_when_the_cache_does_not_know_it() {
    let discord = FakeDiscord::start(vec![
        (200, r#"{"id":"100","type":0,"guild_id":"10"}"#),
        (200, r#"{"id":"300","type":1}"#),
        (404, r#"{"code":10003,"message":"Unknown Channel"}"#),
    ])
    .await;
    let api = discord.api();

    assert_eq!(
        api.channel_guild(Id::new(100)).await,
        Ok(Some(Id::new(10))),
        "a guild channel"
    );
    assert_eq!(
        api.channel_guild(Id::new(300)).await,
        Ok(None),
        "a direct message channel is in no guild"
    );
    assert_eq!(
        api.channel_guild(Id::new(404)).await,
        Err(DiscordError::NotFound),
        "an unknown channel is an error, not an answer"
    );

    let paths: Vec<String> = discord.requests().into_iter().map(|r| r.path).collect();
    assert_eq!(
        paths,
        [
            "/api/v10/channels/100",
            "/api/v10/channels/300",
            "/api/v10/channels/404"
        ]
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn lookups_of_one_channel_at_the_same_time_make_one_request() {
    let discord = FakeDiscord::start(vec![(200, r#"{"id":"100","type":0,"guild_id":"10"}"#)]).await;
    let api = Arc::new(discord.api());

    let lookups: Vec<_> = (0..50)
        .map(|_| {
            let api = Arc::clone(&api);
            tokio::spawn(async move { api.channel_guild(Id::new(100)).await })
        })
        .collect();
    for lookup in lookups {
        assert_eq!(lookup.await.unwrap(), Ok(Some(Id::new(10))));
    }

    assert_eq!(discord.requests().len(), 1);
    // Nothing is left behind once the lookups are over.
    assert_eq!(api.lookups_in_flight(), 0);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_failed_lookup_is_shared_by_those_waiting_and_not_remembered() {
    let discord = FakeDiscord::start(vec![
        (404, r#"{"code":10003,"message":"Unknown Channel"}"#),
        (200, r#"{"id":"100","type":0,"guild_id":"10"}"#),
    ])
    .await;
    let api = Arc::new(discord.api());

    let lookups: Vec<_> = (0..20)
        .map(|_| {
            let api = Arc::clone(&api);
            tokio::spawn(async move { api.channel_guild(Id::new(100)).await })
        })
        .collect();
    let mut failed = 0;
    for lookup in lookups {
        if lookup.await.unwrap() == Err(DiscordError::NotFound) {
            failed += 1;
        }
    }
    assert!(failed >= 1);
    assert_eq!(api.lookups_in_flight(), 0);

    // The failure was not kept: asking again goes to Discord again.
    let before = discord.requests().len();
    assert_eq!(api.channel_guild(Id::new(100)).await, Ok(Some(Id::new(10))));
    assert_eq!(discord.requests().len(), before + 1);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_lookup_given_up_on_does_not_stop_the_others_or_leave_a_trace() {
    let discord = FakeDiscord::start(vec![(200, r#"{"id":"100","type":0,"guild_id":"10"}"#)]).await;
    let api = Arc::new(discord.api());

    let abandoned = tokio::spawn({
        let api = Arc::clone(&api);
        async move { api.channel_guild(Id::new(100)).await }
    });
    abandoned.abort();
    let _ = abandoned.await;

    assert_eq!(api.channel_guild(Id::new(100)).await, Ok(Some(Id::new(10))));
    assert_eq!(api.lookups_in_flight(), 0);
}

#[tokio::test]
async fn a_channel_the_cache_knows_is_not_looked_up_again() {
    struct KnowsChannel;
    impl CacheView for KnowsChannel {
        fn bot_user(&self) -> Option<closure_core::event::UserId> {
            None
        }
        fn guild_owner(
            &self,
            _: closure_core::event::GuildId,
        ) -> Option<closure_core::event::UserId> {
            None
        }
        fn member_roles(
            &self,
            _: closure_core::event::GuildId,
            _: closure_core::event::UserId,
        ) -> Option<Vec<closure_core::event::RoleId>> {
            None
        }
        fn role_position(&self, _: closure_core::event::RoleId) -> Option<i64> {
            None
        }
        fn channel_guild(
            &self,
            channel: closure_core::event::ChannelId,
        ) -> Option<closure_core::event::GuildId> {
            (channel.get() == 100).then(|| Id::new(10))
        }
    }

    let discord = FakeDiscord::start(vec![(500, "")]).await;
    let api = discord.api().with_cache(Arc::new(KnowsChannel));

    assert_eq!(api.channel_guild(Id::new(100)).await, Ok(Some(Id::new(10))));
    assert!(discord.requests().is_empty(), "answered from the cache");
}

mod scoped_client {
    use closure_core::capability::Capability;
    use closure_core::client::ApiClient;
    use closure_core::module::ModuleId;

    use super::*;

    const CHANNEL: &str = r#"{"id":"100","type":0,"guild_id":"10"}"#;

    fn delete_channel() -> Action {
        Action::DeleteChannel {
            channel_id: Id::new(100),
            reason: reason("cleanup"),
        }
    }

    fn client_of(discord: &FakeDiscord, guild: u64) -> ApiClient {
        ApiClient::new(
            Arc::new(discord.api()),
            Capability::ALL.into_iter().collect(),
            ModuleId::new("test").unwrap(),
        )
        .scoped_to(Id::new(guild))
    }

    /// The methods of the requests Discord received.
    fn methods(discord: &FakeDiscord) -> Vec<String> {
        discord.requests().into_iter().map(|r| r.method).collect()
    }

    #[tokio::test]
    async fn a_channel_of_the_guild_is_acted_on_after_one_lookup() {
        let discord = FakeDiscord::start(vec![(200, CHANNEL)]).await;
        let client = client_of(&discord, 10);

        client.execute(delete_channel()).await.unwrap();
        client.execute(delete_channel()).await.unwrap();

        assert_eq!(
            methods(&discord),
            ["GET", "DELETE", "DELETE"],
            "the guild of a channel is asked for once"
        );
    }

    #[tokio::test]
    async fn a_channel_of_another_guild_is_never_acted_on() {
        let discord = FakeDiscord::start(vec![(200, CHANNEL)]).await;

        let result = client_of(&discord, 99).execute(delete_channel()).await;

        assert!(
            matches!(result, Err(DiscordError::Forbidden(_))),
            "{result:?}"
        );
        assert_eq!(methods(&discord), ["GET"]);
    }

    #[tokio::test]
    async fn when_the_lookup_fails_the_action_is_not_sent() {
        let cases: [(&str, Vec<(u16, &'static str)>); 5] = [
            ("forbidden", vec![(403, FORBIDDEN)]),
            (
                "transient then forbidden",
                vec![(500, ""), (403, FORBIDDEN)],
            ),
            (
                // Twilight resends a rate limited request itself and would never stop if the
                // answer stayed 429, so the limit lifts here and the lookup is then refused.
                "rate limited, then forbidden",
                vec![
                    (
                        429,
                        r#"{"message":"slow down","retry_after":0.001,"global":false}"#,
                    ),
                    (403, FORBIDDEN),
                ],
            ),
            ("unreadable answer", vec![(200, "not json")]),
            (
                "a guild channel without a guild",
                vec![(200, r#"{"id":"100","type":0}"#)],
            ),
        ];
        for (name, responses) in cases {
            let discord = FakeDiscord::start(responses).await;

            let result = tokio::time::timeout(
                Duration::from_secs(5),
                client_of(&discord, 10).execute(delete_channel()),
            )
            .await
            .unwrap_or_else(|_| panic!("{name}: hung"));

            assert!(result.is_err(), "{name}: {result:?}");
            assert!(
                methods(&discord).iter().all(|m| m == "GET"),
                "{name}: {:?}",
                methods(&discord)
            );
        }
    }
}

mod direct_messages {
    use closure_core::event::{GuildId, RoleId, UserId};

    use super::*;

    const MEMBER: &str = r#"{"communication_disabled_until":null,"deaf":false,"flags":0,"joined_at":"2026-01-01T00:00:00.000000+00:00","mute":false,"nick":null,"roles":[],"user":{"id":"20","username":"u","discriminator":"0","avatar":null}}"#;
    const DM_CHANNEL: &str = r#"{"id":"300","type":1,"recipients":[{"id":"20","username":"u","discriminator":"0","avatar":null}]}"#;
    const DM_MESSAGE: &str = r#"{"id":"400","channel_id":"300","content":"hi","author":{"id":"1","username":"closure","discriminator":"0","avatar":null,"bot":true},"attachments":[],"embeds":[],"mention_everyone":false,"mention_roles":[],"mentions":[],"pinned":false,"timestamp":"2026-10-24T00:00:00.000000+00:00","edited_timestamp":null,"tts":false,"type":0}"#;
    const DM_CLOSED: &str = r#"{"message":"Cannot send messages to this user","code":50007}"#;

    /// A cache that knows every member of every guild.
    struct KnowsMember;

    impl CacheView for KnowsMember {
        fn bot_user(&self) -> Option<UserId> {
            None
        }

        fn guild_owner(&self, _: GuildId) -> Option<UserId> {
            None
        }

        fn member_roles(&self, _: GuildId, _: UserId) -> Option<Vec<RoleId>> {
            Some(Vec::new())
        }

        fn role_position(&self, _: RoleId) -> Option<i64> {
            None
        }
    }

    fn direct_message() -> Action {
        Action::SendDirectMessage {
            guild_id: Id::new(10),
            user_id: Id::new(20),
            content: Some("hi".into()),
            embeds: vec![],
        }
    }

    /// Method and path of each request, in the order Discord received them.
    fn calls(discord: &FakeDiscord) -> Vec<String> {
        discord
            .requests()
            .into_iter()
            .map(|request| {
                let path = request
                    .path
                    .split_once("/api/v10")
                    .map_or(request.path.as_str(), |(_, rest)| rest);
                format!("{} {path}", request.method)
            })
            .collect()
    }

    #[tokio::test]
    async fn a_member_gets_a_private_message_without_pings() {
        let discord =
            FakeDiscord::start(vec![(200, MEMBER), (200, DM_CHANNEL), (200, DM_MESSAGE)]).await;

        let outcome = discord.api().execute(direct_message()).await;

        assert!(matches!(outcome, Ok(Outcome::Message(_))), "{outcome:?}");
        assert_eq!(
            calls(&discord),
            [
                "GET /guilds/10/members/20",
                "POST /users/@me/channels",
                "POST /channels/300/messages",
            ]
        );
        let requests = discord.requests();
        let channel: serde_json::Value = serde_json::from_str(&requests[1].body).unwrap();
        assert_eq!(channel["recipient_id"], "20");
        let message: serde_json::Value = serde_json::from_str(&requests[2].body).unwrap();
        assert_eq!(message["content"], "hi");
        assert_eq!(message["allowed_mentions"]["parse"], serde_json::json!([]));
    }

    #[tokio::test]
    async fn a_user_who_is_not_known_to_be_a_member_gets_nothing() {
        for response in [(404, NOT_FOUND), (500, "{}")] {
            let discord = FakeDiscord::start(vec![response]).await;

            let result = discord.api().execute(direct_message()).await;

            assert!(result.is_err(), "{response:?}: {result:?}");
            assert_eq!(
                calls(&discord),
                ["GET /guilds/10/members/20"],
                "{response:?}"
            );
        }
    }

    #[tokio::test]
    async fn a_user_who_closed_direct_messages_is_reported_at_either_step() {
        let at_channel = FakeDiscord::start(vec![(200, MEMBER), (403, DM_CLOSED)]).await;
        assert_eq!(
            at_channel.api().execute(direct_message()).await,
            Err(DiscordError::DirectMessagesClosed)
        );

        let at_message =
            FakeDiscord::start(vec![(200, MEMBER), (200, DM_CHANNEL), (403, DM_CLOSED)]).await;
        assert_eq!(
            at_message.api().execute(direct_message()).await,
            Err(DiscordError::DirectMessagesClosed)
        );
    }

    #[tokio::test]
    async fn other_refusals_stay_forbidden() {
        let discord =
            FakeDiscord::start(vec![(200, MEMBER), (200, DM_CHANNEL), (403, FORBIDDEN)]).await;
        let result = discord.api().execute(direct_message()).await;
        assert!(
            matches!(result, Err(DiscordError::Forbidden(_))),
            "{result:?}"
        );
    }

    #[tokio::test]
    async fn a_member_known_to_the_cache_is_not_looked_up() {
        let discord = FakeDiscord::start(vec![(200, DM_CHANNEL), (200, DM_MESSAGE)]).await;
        let api = discord.api().with_cache(Arc::new(KnowsMember));

        assert!(api.execute(direct_message()).await.is_ok());
        assert_eq!(
            calls(&discord),
            ["POST /users/@me/channels", "POST /channels/300/messages"]
        );
    }
}

/// A message as Discord sends it, for the message-reading and crosspost answers.
const MESSAGE_JSON: &str = r#"{"id":"5","channel_id":"1","content":"hi","author":{"id":"1","username":"closure","discriminator":"0","avatar":null,"bot":true},"attachments":[],"embeds":[],"mention_everyone":false,"mention_roles":[],"mentions":[],"pinned":false,"timestamp":"2026-10-24T00:00:00.000000+00:00","edited_timestamp":null,"tts":false,"type":0}"#;

/// Method and path of each request, without the API version prefix.
fn method_paths(discord: &FakeDiscord) -> Vec<String> {
    discord
        .requests()
        .into_iter()
        .map(|request| {
            let path = request
                .path
                .split_once("/api/v10")
                .map_or(request.path.as_str(), |(_, rest)| rest);
            format!("{} {path}", request.method)
        })
        .collect()
}

/// The token of a text command sent in channel 1 by message 44.
fn text_token() -> InteractionToken {
    InteractionToken::for_text_command(Id::new(1), Id::new(44))
}

fn text_reply(content: &str) -> closure_core::action::InteractionReply {
    closure_core::action::InteractionReply {
        content: Some(content.into()),
        ..closure_core::action::InteractionReply::default()
    }
}

#[tokio::test]
async fn a_text_command_reply_replies_to_the_invoker_and_pings_nobody() {
    let discord = FakeDiscord::start(vec![(200, MESSAGE_JSON)]).await;
    discord
        .api()
        .execute(Action::Respond {
            interaction_id: Id::new(44),
            token: text_token(),
            response: InteractionResponse::Message {
                reply: text_reply("<@5> <@&9> @everyone"),
            },
        })
        .await
        .unwrap();
    let request = &discord.requests()[0];
    assert_eq!(request.method, "POST");
    assert!(
        request.path.ends_with("/channels/1/messages"),
        "{}",
        request.path
    );
    let body: serde_json::Value = serde_json::from_str(&request.body).unwrap();
    assert_eq!(body["content"], "<@5> <@&9> @everyone");
    assert_eq!(body["message_reference"]["message_id"], "44");
    assert_eq!(body["message_reference"]["fail_if_not_exists"], false);
    assert_eq!(body["allowed_mentions"]["parse"], serde_json::json!([]));
    // Left out when false, which is Discord's default for the replied-to author.
    assert_ne!(body["allowed_mentions"]["replied_user"], true);
}

#[tokio::test]
async fn a_deferred_text_command_is_sent_by_its_first_edit_and_edited_after_that() {
    let discord = FakeDiscord::start(vec![(200, MESSAGE_JSON)]).await;
    let api = discord.api();
    api.execute(Action::Respond {
        interaction_id: Id::new(44),
        token: text_token(),
        response: InteractionResponse::Defer { ephemeral: false },
    })
    .await
    .unwrap();
    assert!(discord.requests().is_empty(), "a deferral sends nothing");

    for content in ["first", "second"] {
        api.execute(Action::EditResponse {
            token: text_token(),
            reply: text_reply(content),
        })
        .await
        .unwrap();
    }
    let requests = discord.requests();
    let methods: Vec<&str> = requests
        .iter()
        .map(|request| request.method.as_str())
        .collect();
    assert_eq!(methods, ["POST", "PATCH"]);
    assert!(
        requests[1].path.ends_with("/channels/1/messages/5"),
        "{}",
        requests[1].path
    );
    let body: serde_json::Value = serde_json::from_str(&requests[1].body).unwrap();
    assert_eq!(body["content"], "second");
}

#[tokio::test]
async fn an_ephemeral_text_command_reply_is_deleted_after_its_lifetime() {
    let discord = FakeDiscord::start(vec![(200, MESSAGE_JSON), (204, "")]).await;
    let mut api = discord.api().with_cache(Arc::new(TextChannelInGuildTen));
    api.text.ephemeral_lifetime = Duration::from_millis(20);
    api.execute(Action::Respond {
        interaction_id: Id::new(44),
        token: text_token(),
        response: InteractionResponse::Message {
            reply: closure_core::action::InteractionReply {
                ephemeral: true,
                ..text_reply("only you")
            },
        },
    })
    .await
    .unwrap();

    // The deletion runs on its own task; wait for it, with a bound.
    for _ in 0..500 {
        if discord
            .requests()
            .iter()
            .any(|request| request.method == "DELETE")
        {
            break;
        }
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
    let requests = discord.requests();
    let deletion = requests
        .iter()
        .find(|request| request.method == "DELETE")
        .expect("the ephemeral reply is deleted");
    assert!(
        deletion.path.ends_with("/channels/1/messages/5"),
        "{}",
        deletion.path
    );
}

/// A cache that knows channel 1 is in guild 10, so a text command's guild needs no request.
struct TextChannelInGuildTen;

impl CacheView for TextChannelInGuildTen {
    fn bot_user(&self) -> Option<closure_core::event::UserId> {
        None
    }
    fn guild_owner(&self, _: closure_core::event::GuildId) -> Option<closure_core::event::UserId> {
        None
    }
    fn member_roles(
        &self,
        _: closure_core::event::GuildId,
        _: closure_core::event::UserId,
    ) -> Option<Vec<closure_core::event::RoleId>> {
        None
    }
    fn role_position(&self, _: closure_core::event::RoleId) -> Option<i64> {
        None
    }
    fn channel_guild(
        &self,
        channel: closure_core::event::ChannelId,
    ) -> Option<closure_core::event::GuildId> {
        (channel.get() == 1).then(|| Id::new(10))
    }
}

fn ephemeral_reply(content: &str) -> InteractionResponse {
    InteractionResponse::Message {
        reply: closure_core::action::InteractionReply {
            ephemeral: true,
            ..text_reply(content)
        },
    }
}

#[tokio::test]
async fn an_ephemeral_text_command_reply_is_refused_when_its_guild_has_no_place_left() {
    let discord = FakeDiscord::start(vec![(200, MESSAGE_JSON)]).await;
    let api = discord.api().with_cache(Arc::new(TextChannelInGuildTen));
    let _places: Vec<_> = (0..super::interactions::PENDING_DELETIONS_PER_GUILD)
        .map(|_| api.text.reserve(Some(Id::new(10))).unwrap())
        .collect();

    let result = api
        .execute(Action::Respond {
            interaction_id: Id::new(44),
            token: text_token(),
            response: ephemeral_reply("only you"),
        })
        .await;
    assert!(
        matches!(result, Err(DiscordError::Transient(_))),
        "a busy refusal, which the caller reports"
    );
    assert!(
        discord.requests().is_empty(),
        "nothing is sent without a place to delete it from"
    );
}

#[tokio::test]
async fn a_place_is_given_back_when_the_ephemeral_reply_cannot_be_sent() {
    let discord = FakeDiscord::start(vec![(500, "")]).await;
    let api = discord.api().with_cache(Arc::new(TextChannelInGuildTen));
    let result = api
        .execute(Action::Respond {
            interaction_id: Id::new(44),
            token: text_token(),
            response: ephemeral_reply("only you"),
        })
        .await;
    assert!(result.is_err(), "Discord refused the reply");
    assert_eq!(api.text.pending_deletions(), 0);
}

#[tokio::test]
async fn a_deleted_ephemeral_reply_gives_its_place_back() {
    let discord = FakeDiscord::start(vec![(200, MESSAGE_JSON), (204, "")]).await;
    let mut api = discord.api().with_cache(Arc::new(TextChannelInGuildTen));
    api.text.ephemeral_lifetime = Duration::from_millis(200);
    api.execute(Action::Respond {
        interaction_id: Id::new(44),
        token: text_token(),
        response: ephemeral_reply("only you"),
    })
    .await
    .unwrap();
    assert_eq!(
        api.text.pending_deletions(),
        1,
        "the place is taken while the reply waits"
    );

    // The deletion runs on its own task; wait for it, with a bound.
    for _ in 0..500 {
        if api.text.pending_deletions() == 0 {
            break;
        }
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
    assert_eq!(api.text.pending_deletions(), 0);
    assert!(
        discord
            .requests()
            .iter()
            .any(|request| request.method == "DELETE")
    );
}

#[tokio::test]
async fn messages_are_read_one_by_one_and_as_history_from_a_cursor() {
    let discord = FakeDiscord::start(vec![(200, MESSAGE_JSON), (200, "[]")]).await;
    let api = discord.api();
    let one = api
        .execute(Action::GetMessage {
            channel_id: Id::new(1),
            message_id: Id::new(5),
        })
        .await;
    assert!(matches!(one, Ok(Outcome::Message(_))), "{one:?}");
    let history = api
        .execute(Action::GetMessages {
            channel_id: Id::new(1),
            cursor: Some(MessageCursor::Before(Id::new(9))),
            limit: 50,
        })
        .await;
    assert_eq!(history, Ok(Outcome::Messages(vec![])));

    let paths = method_paths(&discord);
    assert_eq!(paths[0], "GET /channels/1/messages/5");
    assert!(
        paths[1].starts_with("GET /channels/1/messages?"),
        "{paths:?}"
    );
    assert!(paths[1].contains("before=9"), "{paths:?}");
    assert!(paths[1].contains("limit=50"), "{paths:?}");
}

#[tokio::test]
async fn the_history_cursor_is_sent_as_after_or_around_when_asked() {
    let discord = FakeDiscord::start(vec![(200, "[]")]).await;
    let api = discord.api();
    for cursor in [
        MessageCursor::After(Id::new(7)),
        MessageCursor::Around(Id::new(8)),
    ] {
        let action = Action::GetMessages {
            channel_id: Id::new(1),
            cursor: Some(cursor),
            limit: 100,
        };
        assert_eq!(api.execute(action).await, Ok(Outcome::Messages(vec![])));
    }
    let paths = method_paths(&discord);
    assert!(paths[0].contains("after=7"), "{paths:?}");
    assert!(paths[1].contains("around=8"), "{paths:?}");
}

#[tokio::test]
async fn pins_are_read_as_their_messages() {
    let pins = format!(
        r#"{{"items":[{{"pinned_at":"2026-10-24T00:00:00.000000+00:00","message":{MESSAGE_JSON}}}],"has_more":false}}"#
    );
    let discord = FakeDiscord::start(vec![(200, Box::leak(pins.into_boxed_str()))]).await;
    let outcome = discord
        .api()
        .execute(Action::GetPins {
            channel_id: Id::new(1),
        })
        .await;
    let Ok(Outcome::Messages(messages)) = outcome else {
        panic!("{outcome:?}");
    };
    assert_eq!(messages.len(), 1);
    assert_eq!(messages[0].id.get(), 5);
    assert_eq!(method_paths(&discord), ["GET /channels/1/messages/pins"]);
}

#[tokio::test]
async fn pins_and_crossposts_are_sent_with_their_reason() {
    let discord = FakeDiscord::start(vec![(204, ""), (204, ""), (200, MESSAGE_JSON)]).await;
    let api = discord.api();
    let pin = Action::PinMessage {
        channel_id: Id::new(1),
        message_id: Id::new(5),
        reason: reason("keep"),
    };
    assert_eq!(api.execute(pin).await, Ok(Outcome::None));
    let unpin = Action::UnpinMessage {
        channel_id: Id::new(1),
        message_id: Id::new(5),
        reason: reason("stale"),
    };
    assert_eq!(api.execute(unpin).await, Ok(Outcome::None));
    let crosspost = Action::CrosspostMessage {
        channel_id: Id::new(1),
        message_id: Id::new(5),
    };
    assert!(matches!(
        api.execute(crosspost).await,
        Ok(Outcome::Message(_))
    ));

    assert_eq!(
        method_paths(&discord),
        [
            "PUT /channels/1/messages/pins/5",
            "DELETE /channels/1/messages/pins/5",
            "POST /channels/1/messages/5/crosspost",
        ]
    );
    let requests = discord.requests();
    assert_eq!(requests[0].headers["x-audit-log-reason"], "keep");
    assert_eq!(requests[1].headers["x-audit-log-reason"], "stale");
}

#[tokio::test]
async fn reactions_are_removed_per_user_or_all_and_their_users_are_listed() {
    let discord = FakeDiscord::start(vec![(204, ""), (204, ""), (204, ""), (200, "[]")]).await;
    let api = discord.api();
    let thumbs = || Emoji::Unicode {
        value: "👍".into()
    };
    let (channel_id, message_id) = (Id::new(1), Id::new(5));

    let one = Action::RemoveReaction {
        channel_id,
        message_id,
        emoji: thumbs(),
        user_id: Id::new(20),
    };
    assert_eq!(api.execute(one).await, Ok(Outcome::None));
    let all = Action::RemoveAllReactions {
        channel_id,
        message_id,
        emoji: None,
    };
    assert_eq!(api.execute(all).await, Ok(Outcome::None));
    let one_emoji = Action::RemoveAllReactions {
        channel_id,
        message_id,
        emoji: Some(thumbs()),
    };
    assert_eq!(api.execute(one_emoji).await, Ok(Outcome::None));
    let users = Action::GetReactionUsers {
        channel_id,
        message_id,
        emoji: thumbs(),
        after: Some(Id::new(19)),
        limit: 10,
    };
    assert_eq!(api.execute(users).await, Ok(Outcome::Users(vec![])));

    let paths = method_paths(&discord);
    assert!(
        paths[0].starts_with("DELETE /channels/1/messages/5/reactions/"),
        "{paths:?}"
    );
    assert!(paths[0].ends_with("/20"), "{paths:?}");
    assert_eq!(paths[1], "DELETE /channels/1/messages/5/reactions");
    assert!(
        paths[2].starts_with("DELETE /channels/1/messages/5/reactions/"),
        "{paths:?}"
    );
    assert!(
        paths[3].contains("after=19") && paths[3].contains("limit=10"),
        "{paths:?}"
    );
}

#[tokio::test]
async fn voice_deafen_and_moves_are_patched_with_their_reason() {
    let discord = FakeDiscord::start(vec![(200, "{}")]).await;
    let api = discord.api();
    let deafen = Action::SetVoiceDeafen {
        guild_id: Id::new(10),
        user_id: Id::new(20),
        deaf: true,
        reason: reason("loud"),
    };
    assert_eq!(api.execute(deafen).await.map(drop), Ok(()));
    let mv = Action::MoveMember {
        guild_id: Id::new(10),
        user_id: Id::new(20),
        channel_id: Id::new(30),
        reason: reason("calm"),
    };
    assert_eq!(api.execute(mv).await.map(drop), Ok(()));
    let disconnect = Action::DisconnectMember {
        guild_id: Id::new(10),
        user_id: Id::new(20),
        reason: reason("out"),
    };
    assert_eq!(api.execute(disconnect).await.map(drop), Ok(()));

    assert_eq!(
        method_paths(&discord),
        [
            "PATCH /guilds/10/members/20",
            "PATCH /guilds/10/members/20",
            "PATCH /guilds/10/members/20",
        ]
    );
    let requests = discord.requests();
    assert!(
        requests[0].body.contains("\"deaf\":true"),
        "{}",
        requests[0].body
    );
    assert_eq!(requests[0].headers["x-audit-log-reason"], "loud");
    assert!(
        requests[1].body.contains("\"channel_id\":\"30\""),
        "{}",
        requests[1].body
    );
    assert!(
        requests[2].body.contains("\"channel_id\":null"),
        "{}",
        requests[2].body
    );
}

#[tokio::test]
async fn a_member_outside_voice_is_reported_when_moved() {
    const NOT_IN_VOICE: &str =
        r#"{"message":"Target user is not connected to voice","code":40032}"#;
    let discord = FakeDiscord::start(vec![(400, NOT_IN_VOICE)]).await;
    let result = discord
        .api()
        .execute(Action::MoveMember {
            guild_id: Id::new(10),
            user_id: Id::new(20),
            channel_id: Id::new(30),
            reason: reason("calm"),
        })
        .await;
    assert_eq!(result, Err(DiscordError::NotInVoice));
}

/// An invite as Discord sends it back, with the guild it belongs to.
const INVITE_JSON: &str = r#"{"code":"abc123","type":0,"channel":{"id":"1","name":"general","type":0},"guild":{"id":"10","name":"g","features":[],"icon":null,"banner":null,"description":null,"splash":null,"verification_level":0,"vanity_url_code":null,"nsfw_level":0},"uses":0,"max_uses":5,"max_age":3600,"temporary":false}"#;

#[tokio::test]
async fn invites_are_created_and_listed_and_deleted_after_their_guild_is_checked() {
    let discord = FakeDiscord::start(vec![
        (200, INVITE_JSON),
        (200, "[]"),
        (200, INVITE_JSON),
        (204, ""),
    ])
    .await;
    let api = discord.api();
    let create = Action::CreateInvite {
        channel_id: Id::new(1),
        max_age_seconds: 3600,
        max_uses: 5,
        temporary: false,
        unique: true,
        reason: reason("promo"),
    };
    assert!(matches!(api.execute(create).await, Ok(Outcome::Invite(_))));
    let list = Action::ListInvites {
        guild_id: Id::new(10),
    };
    assert_eq!(api.execute(list).await, Ok(Outcome::Invites(vec![])));
    let delete = Action::DeleteInvite {
        guild_id: Id::new(10),
        code: "abc123".into(),
        reason: reason("stale"),
    };
    assert_eq!(api.execute(delete).await, Ok(Outcome::None));

    assert_eq!(
        method_paths(&discord),
        [
            "POST /channels/1/invites",
            "GET /guilds/10/invites",
            "GET /invites/abc123",
            "DELETE /invites/abc123",
        ]
    );
    let requests = discord.requests();
    assert!(
        requests[0].body.contains("\"max_age\":3600"),
        "{}",
        requests[0].body
    );
    assert!(
        requests[0].body.contains("\"max_uses\":5"),
        "{}",
        requests[0].body
    );
    assert_eq!(requests[0].headers["x-audit-log-reason"], "promo");
    assert_eq!(requests[3].headers["x-audit-log-reason"], "stale");
}

#[tokio::test]
async fn an_invite_of_another_guild_is_never_deleted() {
    let discord = FakeDiscord::start(vec![(200, INVITE_JSON)]).await;
    let delete = Action::DeleteInvite {
        guild_id: Id::new(99),
        code: "abc123".into(),
        reason: reason("x"),
    };
    let result = discord.api().execute(delete).await;
    assert!(
        matches!(result, Err(DiscordError::Forbidden(_))),
        "{result:?}"
    );
    assert_eq!(method_paths(&discord), ["GET /invites/abc123"]);
}

/// A webhook as Discord sends it back. Its token is what a post authorises.
const WEBHOOK_JSON: &str = r#"{"id":"8","type":1,"guild_id":"10","channel_id":"1","name":"bridge","token":"tok-secret","avatar":null}"#;

#[tokio::test]
async fn webhooks_are_created_listed_and_deleted_after_their_guild_is_checked() {
    let discord = FakeDiscord::start(vec![
        (200, WEBHOOK_JSON),
        (200, concat!("[", r#"{"id":"8","type":1,"guild_id":"10","channel_id":"1","name":"bridge","token":"tok-secret","avatar":null}"#, "]")),
        (200, WEBHOOK_JSON),
        (204, ""),
    ])
    .await;
    let api = discord.api();
    let create = Action::CreateWebhook {
        channel_id: Id::new(1),
        name: "bridge".into(),
        reason: reason("relay"),
    };
    assert!(matches!(api.execute(create).await, Ok(Outcome::Webhook(_))));
    let list = Action::ListWebhooks {
        channel_id: Id::new(1),
    };
    assert!(matches!(api.execute(list).await, Ok(Outcome::Webhooks(hooks)) if hooks.len() == 1));
    let delete = Action::DeleteWebhook {
        guild_id: Id::new(10),
        webhook_id: Id::new(8),
        reason: reason("done"),
    };
    assert_eq!(api.execute(delete).await, Ok(Outcome::None));

    assert_eq!(
        method_paths(&discord),
        [
            "POST /channels/1/webhooks",
            "GET /channels/1/webhooks",
            "GET /webhooks/8",
            "DELETE /webhooks/8",
        ]
    );
    let requests = discord.requests();
    assert!(
        requests[0].body.contains("\"name\":\"bridge\""),
        "{}",
        requests[0].body
    );
    assert_eq!(requests[0].headers["x-audit-log-reason"], "relay");
    assert_eq!(requests[3].headers["x-audit-log-reason"], "done");
}

#[tokio::test]
async fn a_webhook_of_another_guild_is_neither_deleted_nor_posted_through() {
    let discord = FakeDiscord::start(vec![(200, WEBHOOK_JSON), (200, WEBHOOK_JSON)]).await;
    let api = discord.api();
    let delete = Action::DeleteWebhook {
        guild_id: Id::new(99),
        webhook_id: Id::new(8),
        reason: reason("x"),
    };
    assert!(matches!(
        api.execute(delete).await,
        Err(DiscordError::Forbidden(_))
    ));
    let post = Action::ExecuteWebhook {
        guild_id: Id::new(99),
        webhook_id: Id::new(8),
        token: WebhookToken::new("tok-secret"),
        content: Some("hi".into()),
        embeds: vec![],
        username: None,
        avatar_url: None,
        mentions: MentionPolicy::None,
    };
    assert!(matches!(
        api.execute(post).await,
        Err(DiscordError::Forbidden(_))
    ));
    assert_eq!(
        method_paths(&discord),
        ["GET /webhooks/8", "GET /webhooks/8/tok-secret"]
    );
}

#[tokio::test]
async fn a_webhook_post_is_checked_then_sent_and_its_reply_is_the_message() {
    let discord = FakeDiscord::start(vec![(200, WEBHOOK_JSON), (200, MESSAGE_JSON)]).await;
    let post = Action::ExecuteWebhook {
        guild_id: Id::new(10),
        webhook_id: Id::new(8),
        token: WebhookToken::new("tok-secret"),
        content: Some("hi".into()),
        embeds: vec![],
        username: Some("relay".into()),
        avatar_url: None,
        mentions: MentionPolicy::None,
    };
    assert!(matches!(
        discord.api().execute(post).await,
        Ok(Outcome::Message(_))
    ));
    let paths = method_paths(&discord);
    assert_eq!(paths[0], "GET /webhooks/8/tok-secret");
    assert!(
        paths[1].starts_with("POST /webhooks/8/tok-secret?"),
        "{paths:?}"
    );
    let requests = discord.requests();
    assert!(
        requests[1].path.contains("wait=true"),
        "{}",
        requests[1].path
    );
    assert!(
        requests[1].body.contains("\"username\":\"relay\""),
        "{}",
        requests[1].body
    );
    assert!(
        requests[1].body.contains("\"content\":\"hi\""),
        "{}",
        requests[1].body
    );
}

/// A guild as Discord sends it back; the same shape the cache tests use.
const GUILD_JSON: &str = r#"{"id":"10","name":"renamed","owner_id":"999999999","roles":[],"emojis":[],"features":[],"stickers":[],"afk_timeout":300,"default_message_notifications":0,"explicit_content_filter":0,"mfa_level":0,"nsfw_level":0,"premium_tier":0,"system_channel_flags":0,"verification_level":0,"preferred_locale":"en-US","premium_progress_bar_enabled":false,"max_members":null,"max_presences":null,"member_count":null,"splash":null,"banner":null,"icon":null,"description":null}"#;

const MEMBER_JSON: &str = r#"{"communication_disabled_until":null,"deaf":false,"flags":0,"joined_at":"2026-01-01T00:00:00.000000+00:00","mute":false,"nick":null,"roles":[],"user":{"id":"20","username":"u","discriminator":"0","avatar":null}}"#;

const DM_CHANNEL_JSON: &str = r#"{"id":"300","type":1,"recipients":[{"id":"20","username":"u","discriminator":"0","avatar":null}]}"#;

#[tokio::test]
async fn members_are_searched_and_listed_by_query_and_page() {
    let discord = FakeDiscord::start(vec![(200, "[]"), (200, "[]")]).await;
    let api = discord.api();
    let search = Action::SearchMembers {
        guild_id: Id::new(10),
        query: "ana".into(),
        limit: 5,
    };
    assert_eq!(api.execute(search).await, Ok(Outcome::Members(vec![])));
    let list = Action::ListMembers {
        guild_id: Id::new(10),
        after: Some(Id::new(3)),
        limit: 50,
    };
    assert_eq!(api.execute(list).await, Ok(Outcome::Members(vec![])));

    let paths = method_paths(&discord);
    assert!(
        paths[0].starts_with("GET /guilds/10/members/search?"),
        "{paths:?}"
    );
    assert!(
        paths[0].contains("query=ana") && paths[0].contains("limit=5"),
        "{paths:?}"
    );
    assert!(paths[1].starts_with("GET /guilds/10/members?"), "{paths:?}");
    assert!(
        paths[1].contains("after=3") && paths[1].contains("limit=50"),
        "{paths:?}"
    );
}

#[tokio::test]
async fn a_prune_is_counted_then_carried_out_with_its_reason() {
    let discord =
        FakeDiscord::start(vec![(200, r#"{"pruned":4}"#), (200, r#"{"pruned":4}"#)]).await;
    let api = discord.api();
    let count = Action::PruneCount {
        guild_id: Id::new(10),
        days: 7,
        include_roles: vec![Id::new(5)],
    };
    assert_eq!(api.execute(count).await, Ok(Outcome::Count(4)));
    let prune = Action::Prune {
        guild_id: Id::new(10),
        days: 7,
        include_roles: vec![],
        reason: reason("inactive"),
    };
    assert_eq!(api.execute(prune).await, Ok(Outcome::Count(4)));

    let paths = method_paths(&discord);
    assert!(paths[0].starts_with("GET /guilds/10/prune?"), "{paths:?}");
    assert!(
        paths[0].contains("days=7") && paths[0].contains("include_roles=5"),
        "{paths:?}"
    );
    // Discord takes the days and the count flag in the query string, not the body.
    assert!(paths[1].starts_with("POST /guilds/10/prune?"), "{paths:?}");
    assert!(
        paths[1].contains("days=7") && paths[1].contains("compute_prune_count=true"),
        "{paths:?}"
    );
    let requests = discord.requests();
    assert_eq!(requests[1].headers["x-audit-log-reason"], "inactive");
}

#[tokio::test]
async fn the_guild_roles_and_channel_order_are_changed_as_sent() {
    let discord = FakeDiscord::start(vec![(200, GUILD_JSON), (200, "[]"), (204, "")]).await;
    let api = discord.api();
    let edit = Action::EditGuild {
        guild_id: Id::new(10),
        patch: GuildPatch {
            name: Some("renamed".into()),
            verification_level: Some(VerificationLevel::High),
            invites_disabled: None,
        },
        reason: reason("raid"),
    };
    assert_eq!(api.execute(edit).await, Ok(Outcome::None));
    let roles = Action::ReorderRoles {
        guild_id: Id::new(10),
        positions: vec![RolePosition {
            id: Id::new(5),
            position: 2,
        }],
    };
    assert_eq!(api.execute(roles).await, Ok(Outcome::Roles(vec![])));
    let channels = Action::ReorderChannels {
        guild_id: Id::new(10),
        positions: vec![ChannelPosition {
            channel_id: Id::new(1),
            position: 0,
        }],
    };
    assert_eq!(api.execute(channels).await, Ok(Outcome::None));

    assert_eq!(
        method_paths(&discord),
        [
            "PATCH /guilds/10",
            "PATCH /guilds/10/roles",
            "PATCH /guilds/10/channels"
        ]
    );
    let requests = discord.requests();
    assert_eq!(requests[0].headers["x-audit-log-reason"], "raid");
    assert!(
        requests[0].body.contains("\"name\":\"renamed\""),
        "{}",
        requests[0].body
    );
    assert!(
        requests[0].body.contains("\"verification_level\":3"),
        "{}",
        requests[0].body
    );
    assert!(
        requests[1].body.contains("\"position\":2"),
        "{}",
        requests[1].body
    );
    assert!(
        requests[2].body.contains("\"id\":\"1\""),
        "{}",
        requests[2].body
    );
}

/// A guild whose features are `COMMUNITY` and, when `invites` is set, `INVITES_DISABLED`.
fn guild_with_features(invites: bool) -> &'static str {
    if invites {
        r#"{"id":"10","name":"g","owner_id":"999999999","roles":[],"emojis":[],"features":["COMMUNITY","INVITES_DISABLED"],"stickers":[],"afk_timeout":300,"default_message_notifications":0,"explicit_content_filter":0,"mfa_level":0,"nsfw_level":0,"premium_tier":0,"system_channel_flags":0,"verification_level":0,"preferred_locale":"en-US","premium_progress_bar_enabled":false,"max_members":null,"max_presences":null,"member_count":null,"splash":null,"banner":null,"icon":null,"description":null}"#
    } else {
        r#"{"id":"10","name":"g","owner_id":"999999999","roles":[],"emojis":[],"features":["COMMUNITY"],"stickers":[],"afk_timeout":300,"default_message_notifications":0,"explicit_content_filter":0,"mfa_level":0,"nsfw_level":0,"premium_tier":0,"system_channel_flags":0,"verification_level":0,"preferred_locale":"en-US","premium_progress_bar_enabled":false,"max_members":null,"max_presences":null,"member_count":null,"splash":null,"banner":null,"icon":null,"description":null}"#
    }
}

#[tokio::test]
async fn invites_are_paused_by_setting_the_feature_among_the_ones_the_guild_has() {
    let discord =
        FakeDiscord::start(vec![(200, guild_with_features(false)), (200, GUILD_JSON)]).await;
    let edit = Action::EditGuild {
        guild_id: Id::new(10),
        patch: GuildPatch {
            name: None,
            verification_level: None,
            invites_disabled: Some(true),
        },
        reason: reason("raid"),
    };
    assert_eq!(discord.api().execute(edit).await, Ok(Outcome::None));
    assert_eq!(
        method_paths(&discord),
        ["GET /guilds/10", "PATCH /guilds/10"]
    );
    assert!(
        discord.requests()[1]
            .body
            .contains(r#""features":["COMMUNITY","INVITES_DISABLED"]"#),
        "{}",
        discord.requests()[1].body
    );
}

#[tokio::test]
async fn invites_are_resumed_by_removing_only_the_invites_feature() {
    let discord =
        FakeDiscord::start(vec![(200, guild_with_features(true)), (200, GUILD_JSON)]).await;
    let edit = Action::EditGuild {
        guild_id: Id::new(10),
        patch: GuildPatch {
            name: None,
            verification_level: None,
            invites_disabled: Some(false),
        },
        reason: reason("raid over"),
    };
    assert_eq!(discord.api().execute(edit).await, Ok(Outcome::None));
    assert!(
        discord.requests()[1]
            .body
            .contains(r#""features":["COMMUNITY"]"#),
        "{}",
        discord.requests()[1].body
    );
}

#[tokio::test]
async fn a_direct_message_channel_is_opened_only_with_a_member_of_the_guild() {
    let discord = FakeDiscord::start(vec![(200, MEMBER_JSON), (200, DM_CHANNEL_JSON)]).await;
    let action = Action::CreateDirectMessageChannel {
        guild_id: Id::new(10),
        user_id: Id::new(20),
    };
    assert!(matches!(
        discord.api().execute(action).await,
        Ok(Outcome::Channel(_))
    ));
    assert_eq!(
        method_paths(&discord),
        ["GET /guilds/10/members/20", "POST /users/@me/channels"]
    );
}

/// A scheduled voice event as Discord sends it back.
const EVENT_JSON: &str = r#"{"id":"9","guild_id":"10","channel_id":"3","creator_id":null,"name":"game","description":null,"scheduled_start_time":"2027-01-01T00:00:00+00:00","scheduled_end_time":null,"privacy_level":2,"status":1,"entity_type":2,"entity_id":null,"entity_metadata":null,"user_count":null,"image":null}"#;

const EMOJI_JSON: &str = r#"{"id":"4","name":"party","roles":[],"require_colons":true,"managed":false,"animated":false,"available":true}"#;

#[tokio::test]
async fn scheduled_events_are_listed_created_edited_and_deleted_per_guild() {
    let discord = FakeDiscord::start(vec![
        (200, "[]"),
        (200, EVENT_JSON),
        (200, EVENT_JSON),
        (204, ""),
    ])
    .await;
    let api = discord.api();
    let list = Action::ListScheduledEvents {
        guild_id: Id::new(10),
        with_user_count: true,
    };
    assert_eq!(
        api.execute(list).await,
        Ok(Outcome::ScheduledEvents(vec![]))
    );
    let create = Action::CreateScheduledEvent {
        guild_id: Id::new(10),
        spec: NewScheduledEvent {
            name: "game".into(),
            description: None,
            start_unix: 1_800_000_000,
            end_unix: None,
            location: EventLocation::Voice {
                channel_id: Id::new(3),
            },
        },
    };
    assert!(matches!(
        api.execute(create).await,
        Ok(Outcome::ScheduledEvent(_))
    ));
    let edit = Action::EditScheduledEvent {
        guild_id: Id::new(10),
        event_id: Id::new(9),
        patch: ScheduledEventPatch {
            name: None,
            status: Some(ScheduledEventStatus::Canceled),
        },
    };
    assert!(matches!(
        api.execute(edit).await,
        Ok(Outcome::ScheduledEvent(_))
    ));
    let delete = Action::DeleteScheduledEvent {
        guild_id: Id::new(10),
        event_id: Id::new(9),
    };
    assert_eq!(api.execute(delete).await, Ok(Outcome::None));

    let paths = method_paths(&discord);
    assert!(
        paths[0].starts_with("GET /guilds/10/scheduled-events?"),
        "{paths:?}"
    );
    assert!(paths[0].contains("with_user_count=true"), "{paths:?}");
    assert_eq!(paths[1], "POST /guilds/10/scheduled-events");
    assert_eq!(paths[2], "PATCH /guilds/10/scheduled-events/9");
    assert_eq!(paths[3], "DELETE /guilds/10/scheduled-events/9");
    let requests = discord.requests();
    assert!(
        requests[1].body.contains("\"entity_type\":2"),
        "{}",
        requests[1].body
    );
    assert!(
        requests[1].body.contains("\"channel_id\":\"3\""),
        "{}",
        requests[1].body
    );
    assert!(
        requests[2].body.contains("\"status\":4"),
        "{}",
        requests[2].body
    );
}

#[tokio::test]
async fn emoji_are_listed_created_with_their_reason_and_deleted() {
    let discord = FakeDiscord::start(vec![(200, "[]"), (201, EMOJI_JSON), (204, "")]).await;
    let api = discord.api();
    let list = Action::ListEmojis {
        guild_id: Id::new(10),
    };
    assert_eq!(api.execute(list).await, Ok(Outcome::Emojis(vec![])));
    let create = Action::CreateEmoji {
        guild_id: Id::new(10),
        name: "party".into(),
        image: "data:image/png;base64,iVBORw0KGgo=".into(),
        reason: reason("theme"),
    };
    assert!(matches!(api.execute(create).await, Ok(Outcome::Emoji(_))));
    let delete = Action::DeleteEmoji {
        guild_id: Id::new(10),
        emoji_id: Id::new(4),
        reason: reason("unused"),
    };
    assert_eq!(api.execute(delete).await, Ok(Outcome::None));

    assert_eq!(
        method_paths(&discord),
        [
            "GET /guilds/10/emojis",
            "POST /guilds/10/emojis",
            "DELETE /guilds/10/emojis/4",
        ]
    );
    let requests = discord.requests();
    assert!(
        requests[1].body.contains("\"name\":\"party\""),
        "{}",
        requests[1].body
    );
    assert_eq!(requests[1].headers["x-audit-log-reason"], "theme");
    assert_eq!(requests[2].headers["x-audit-log-reason"], "unused");
}

/// A thread channel as Discord sends it back after it is created or edited.
const THREAD_JSON: &str = r#"{"id":"40","type":11,"guild_id":"10","name":"help","parent_id":"1","owner_id":"1","last_message_id":null,"rate_limit_per_user":0,"thread_metadata":{"archived":false,"auto_archive_duration":1440,"archive_timestamp":"2026-10-24T00:00:00.000000+00:00","locked":false}}"#;

#[tokio::test]
async fn threads_are_started_from_a_message_or_on_their_own() {
    let discord = FakeDiscord::start(vec![(201, THREAD_JSON), (201, THREAD_JSON)]).await;
    let api = discord.api();
    let from_message = Action::CreateThread {
        channel_id: Id::new(1),
        name: "help".into(),
        source: Some(Id::new(5)),
        kind: ThreadKind::Public,
        auto_archive_minutes: 1440,
    };
    assert!(matches!(
        api.execute(from_message).await,
        Ok(Outcome::Channel(_))
    ));
    let standalone = Action::CreateThread {
        channel_id: Id::new(1),
        name: "help".into(),
        source: None,
        kind: ThreadKind::Private,
        auto_archive_minutes: 10080,
    };
    assert!(matches!(
        api.execute(standalone).await,
        Ok(Outcome::Channel(_))
    ));

    assert_eq!(
        method_paths(&discord),
        [
            "POST /channels/1/messages/5/threads",
            "POST /channels/1/threads"
        ]
    );
    let requests = discord.requests();
    assert!(
        requests[0].body.contains("\"auto_archive_duration\":1440"),
        "{}",
        requests[0].body
    );
    assert!(
        requests[1].body.contains("\"type\":12"),
        "{}",
        requests[1].body
    );
    assert!(
        requests[1].body.contains("\"auto_archive_duration\":10080"),
        "{}",
        requests[1].body
    );
}

#[tokio::test]
async fn a_thread_is_edited_with_its_reason_and_its_members_are_changed() {
    let discord = FakeDiscord::start(vec![
        (200, THREAD_JSON),
        (204, ""),
        (204, ""),
        (204, ""),
        (204, ""),
        (200, "[]"),
    ])
    .await;
    let api = discord.api();
    let edit = Action::EditThread {
        thread_id: Id::new(40),
        patch: ThreadPatch {
            name: Some("done".into()),
            archived: Some(true),
            locked: Some(true),
            slowmode_seconds: Some(5),
        },
        reason: reason("solved"),
    };
    assert!(matches!(api.execute(edit).await, Ok(Outcome::Channel(_))));
    assert_eq!(
        api.execute(Action::JoinThread {
            thread_id: Id::new(40)
        })
        .await,
        Ok(Outcome::None)
    );
    assert_eq!(
        api.execute(Action::LeaveThread {
            thread_id: Id::new(40)
        })
        .await,
        Ok(Outcome::None)
    );
    let add = Action::AddThreadMember {
        thread_id: Id::new(40),
        user_id: Id::new(20),
    };
    assert_eq!(api.execute(add).await, Ok(Outcome::None));
    let remove = Action::RemoveThreadMember {
        thread_id: Id::new(40),
        user_id: Id::new(20),
    };
    assert_eq!(api.execute(remove).await, Ok(Outcome::None));
    let members = Action::ListThreadMembers {
        thread_id: Id::new(40),
        after: Some(Id::new(19)),
        limit: 25,
    };
    assert_eq!(
        api.execute(members).await,
        Ok(Outcome::ThreadMembers(vec![]))
    );

    let paths = method_paths(&discord);
    assert_eq!(
        paths[..5],
        [
            "PATCH /channels/40",
            "PUT /channels/40/thread-members/@me",
            "DELETE /channels/40/thread-members/@me",
            "PUT /channels/40/thread-members/20",
            "DELETE /channels/40/thread-members/20",
        ]
    );
    assert!(
        paths[5].starts_with("GET /channels/40/thread-members?"),
        "{paths:?}"
    );
    assert!(
        paths[5].contains("after=19") && paths[5].contains("limit=25"),
        "{paths:?}"
    );
    let requests = discord.requests();
    assert_eq!(requests[0].headers["x-audit-log-reason"], "solved");
    assert!(
        requests[0].body.contains("\"rate_limit_per_user\":5"),
        "{}",
        requests[0].body
    );
    assert!(
        requests[0].body.contains("\"locked\":true"),
        "{}",
        requests[0].body
    );
}
