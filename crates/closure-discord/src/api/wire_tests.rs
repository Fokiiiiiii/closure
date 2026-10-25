//! Checks what actually goes over the wire, against a local stand-in for Discord.

use std::collections::HashMap;
use std::net::SocketAddr;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use closure_core::action::{
    Action, InteractionReply, InteractionResponse, MentionPolicy, Outcome, Reason,
};
use closure_core::client::DiscordApi;
use closure_core::error::DiscordError;
use closure_core::event::InteractionToken;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpListener;
use twilight_http::Client;
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
