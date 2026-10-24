//! Checks what actually goes over the wire, against a local stand-in for Discord.

use std::collections::HashMap;
use std::net::SocketAddr;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use closure_core::action::{Action, InteractionResponse, MentionPolicy, Outcome, Reason};
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
