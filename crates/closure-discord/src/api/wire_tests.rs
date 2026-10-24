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
