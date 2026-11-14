use std::sync::Arc;

use axum::Router;
use axum::body::Body;
use axum::http::{HeaderMap, Method, Request, StatusCode, header};
use closure_config::{ConfigCompiler, SettingsStore};
use closure_core::capability::{Capability, CapabilitySet};
use closure_core::module::{Module, ModuleId, ModuleManifest};
use closure_core::registry::ModuleRegistry;
use closure_core::settings::SettingsProvider;
use closure_core::storage::Store;
use closure_core::testing::StubModule;
use closure_db::Db;
use http_body_util::BodyExt;
use secrecy::SecretString;
use serde_json::{Value, json};
use sqlx::PgPool;
use tower::ServiceExt;
use twilight_model::id::Id;

use super::*;

const TOKEN: &str = "test-admin-token-long-enough-to-pass";

struct FixedReadiness(GatewayStatus);

impl Readiness for FixedReadiness {
    fn gateway(&self) -> GatewayStatus {
        self.0
    }
}

struct FakeDirectory;
struct FakeAccess(bool);
impl MemberAccess for FakeAccess {
    fn member(
        &self,
        _guild: closure_core::event::GuildId,
        user: closure_core::event::UserId,
    ) -> std::pin::Pin<
        Box<
            dyn std::future::Future<Output = Result<Option<closure_core::permission::Actor>, ()>>
                + Send
                + '_,
        >,
    > {
        Box::pin(async move {
            if !self.0 {
                return Err(());
            }
            Ok(
                (user.get() != 999).then(|| closure_core::permission::Actor {
                    user,
                    roles: if user.get() == 2 {
                        vec![Id::new(5)]
                    } else {
                        vec![]
                    },
                    administrator: false,
                    owner: matches!(user.get(), 1 | 123 | 9_223_372_036_854_775_807),
                }),
            )
        })
    }
}
impl GuildDirectory for FakeDirectory {
    fn guild(&self, id: closure_core::event::GuildId) -> Option<Guild> {
        (id.get() == GUILD).then(|| Guild {
            id: id.to_string(),
            name: "Test".into(),
            icon: None,
        })
    }
    fn directory(&self, id: closure_core::event::GuildId) -> Option<Directory> {
        self.guild(id).map(|_| Directory {
            channels: vec![Channel {
                id: "5".into(),
                name: "general".into(),
                kind: "text",
                parent_id: None,
                position: 0,
            }],
            roles: vec![Role {
                id: "6".into(),
                name: "member".into(),
                color: 42,
                position: 1,
                managed: false,
            }],
        })
    }
}

struct App {
    router: Router,
    db: Db,
    store: Arc<SettingsStore>,
}

fn warn_module() -> Arc<dyn Module> {
    let mut manifest = ModuleManifest::new(ModuleId::new("warn").unwrap(), "warn", "0.1.0");
    manifest.config_schema = json!({
        "type": "object",
        "properties": { "limit": { "type": "integer", "minimum": 1, "maximum": 10 } },
        "additionalProperties": false
    });
    manifest.default_config = json!({ "limit": 3 });
    Arc::new(StubModule(manifest))
}

/// A module with one basic setting and one advanced list, for the views.
fn layered_module() -> Arc<dyn Module> {
    let mut manifest = ModuleManifest::new(ModuleId::new("layered").unwrap(), "layered", "0.1.0");
    manifest.config_schema = json!({
        "type": "object",
        "properties": {
            "dry_run": { "type": "boolean", "x-ui": { "level": "basic" } },
            "rules": { "type": "array", "items": { "type": "string" } }
        },
        "additionalProperties": false
    });
    manifest.default_config = json!({ "dry_run": false, "rules": [] });
    Arc::new(StubModule(manifest))
}

fn automod_module() -> Arc<dyn Module> {
    let mut manifest = ModuleManifest::new(ModuleId::new("automod").unwrap(), "automod", "0.1.0");
    manifest.config_schema = json!({ "type": "object" });
    manifest.default_config = json!({});
    Arc::new(StubModule(manifest))
}

fn app_with(pool: PgPool, gateway: GatewayStatus) -> App {
    app_with_access(pool, gateway, true)
}

fn app_with_access(pool: PgPool, gateway: GatewayStatus, available: bool) -> App {
    app_built(pool, gateway, available, None)
}

fn app_built(
    pool: PgPool,
    gateway: GatewayStatus,
    available: bool,
    sink: Option<Arc<dyn MessageSink>>,
) -> App {
    app_with_source(pool, gateway, Arc::new(FakeAccess(available)), sink)
}

fn app_with_source(
    pool: PgPool,
    gateway: GatewayStatus,
    access: Arc<dyn MemberAccess>,
    sink: Option<Arc<dyn MessageSink>>,
) -> App {
    let mut dashboard =
        ModuleManifest::new(ModuleId::new("dashboard").unwrap(), "dashboard", "0.1.0");
    dashboard = dashboard.permission(
        closure_core::permission::PermissionSpec::new(
            "dashboard.edit",
            closure_core::permission::Level::ADMIN,
            "edit",
        )
        .unwrap(),
    );
    let mut tickets = ModuleManifest::new(ModuleId::new("tickets").unwrap(), "tickets", "0.1.0");
    tickets.config_schema = json!({ "type": "object" });
    tickets.default_config = json!({ "staff_roles": ["5"] });
    tickets = tickets.permission(
        closure_core::permission::PermissionSpec::new(
            "tickets.manage",
            closure_core::permission::Level::MODERATOR,
            "manage",
        )
        .unwrap(),
    );
    let registry = Arc::new(
        ModuleRegistry::new(vec![
            warn_module(),
            layered_module(),
            automod_module(),
            Arc::new(StubModule(dashboard)),
            Arc::new(StubModule(tickets)),
        ])
        .unwrap(),
    );
    let store = Arc::new(SettingsStore::new(ConfigCompiler::new(registry).unwrap()));
    let db = Db::from_pool(pool);
    let state = ApiState::new(
        Arc::clone(&store),
        db.clone(),
        Arc::new(FixedReadiness(gateway)),
        Arc::new(FakeDirectory),
        access,
        || "closure_up 1\n".to_owned(),
        &SecretString::from(TOKEN),
    )
    .unwrap();
    let state = match sink {
        Some(sink) => state.with_messages(sink),
        None => state,
    };
    App {
        router: router(state),
        db,
        store,
    }
}

/// What the fake Discord answers to a message: it was sent, or it failed.
struct FakeSink(
    Result<Sent, SendFailure>,
    std::sync::Mutex<Vec<(u64, Option<String>, usize)>>,
);

impl MessageSink for FakeSink {
    fn send(
        &self,
        channel: closure_core::event::ChannelId,
        _edit: Option<closure_core::event::MessageId>,
        post: crate::Post,
    ) -> std::pin::Pin<Box<dyn std::future::Future<Output = Result<Sent, SendFailure>> + Send + '_>>
    {
        self.1
            .lock()
            .unwrap()
            .push((channel.get(), post.content, post.embeds.len()));
        let answer = self.0.clone();
        Box::pin(async move { answer })
    }
}

/// Never answers, so a request that reaches it keeps its concurrency slot for the whole test.
struct StalledAccess(std::sync::atomic::AtomicUsize);

impl MemberAccess for StalledAccess {
    fn member(
        &self,
        _guild: closure_core::event::GuildId,
        _user: closure_core::event::UserId,
    ) -> std::pin::Pin<
        Box<
            dyn std::future::Future<Output = Result<Option<closure_core::permission::Actor>, ()>>
                + Send
                + '_,
        >,
    > {
        self.0.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
        Box::pin(std::future::pending::<
            Result<Option<closure_core::permission::Actor>, ()>,
        >())
    }
}

/// An app whose test guild (the one most tests configure) has been joined by the bot.
async fn app(pool: PgPool) -> App {
    let app = app_with(pool, GatewayStatus { ready: 1, total: 1 });
    app.db.register_guild(Id::new(GUILD)).await.unwrap();
    app
}

struct Response {
    status: StatusCode,
    headers: HeaderMap,
    text: String,
}

impl Response {
    fn json(&self) -> Value {
        serde_json::from_str(&self.text).unwrap_or_else(|_| panic!("not JSON: {}", self.text))
    }

    fn etag(&self) -> &str {
        self.headers[header::ETAG].to_str().unwrap()
    }
}

struct Call {
    method: Method,
    uri: String,
    token: Option<&'static str>,
    headers: Vec<(&'static str, String)>,
    body: String,
}

fn call(method: Method, uri: &str) -> Call {
    Call {
        method,
        uri: uri.to_owned(),
        token: Some(TOKEN),
        headers: Vec::new(),
        body: String::new(),
    }
}

impl Call {
    fn token(mut self, token: Option<&'static str>) -> Self {
        self.token = token;
        self
    }

    fn header(mut self, name: &'static str, value: &str) -> Self {
        self.headers.push((name, value.to_owned()));
        self
    }

    fn body(mut self, body: &str) -> Self {
        body.clone_into(&mut self.body);
        self
    }
}

async fn send(router: &Router, call: Call) -> Response {
    let mut builder = Request::builder().method(call.method).uri(call.uri);
    if let Some(token) = call.token {
        builder = builder.header(header::AUTHORIZATION, format!("Bearer {token}"));
    }
    for (name, value) in call.headers {
        builder = builder.header(name, value);
    }
    let request = builder.body(Body::from(call.body)).unwrap();
    let response = router.clone().oneshot(request).await.unwrap();
    let (parts, body) = response.into_parts();
    let bytes = body.collect().await.unwrap().to_bytes();
    Response {
        status: parts.status,
        headers: parts.headers,
        text: String::from_utf8_lossy(&bytes).into_owned(),
    }
}

fn post_json(uri: &str) -> Call {
    call(Method::POST, uri).header("content-type", "application/json")
}

fn put(guild: u64, if_match: &str, body: &str) -> Call {
    call(Method::PUT, &format!("/v1/guilds/{guild}/config"))
        .header("if-match", if_match)
        .body(body)
}

const GUILD: u64 = 42;

#[sqlx::test(migrations = "../closure-db/migrations")]
async fn stats_validate_query_and_return_empty_guild(pool: PgPool) {
    let app = app(pool).await;
    for suffix in ["?days=0", "?days=31", "?days=abc"] {
        let response = send(
            &app.router,
            call(Method::GET, &format!("/v1/guilds/999/stats{suffix}")),
        )
        .await;
        assert_eq!(response.status, StatusCode::BAD_REQUEST);
        assert_eq!(response.json()["error"]["code"], "bad_request");
    }
    assert_eq!(
        send(&app.router, call(Method::GET, "/v1/guilds/0/stats"))
            .await
            .status,
        StatusCode::BAD_REQUEST
    );
    assert_eq!(
        send(
            &app.router,
            call(Method::GET, "/v1/guilds/999/stats").token(None)
        )
        .await
        .status,
        StatusCode::UNAUTHORIZED
    );
    let response = send(&app.router, call(Method::GET, "/v1/guilds/999/stats")).await;
    assert_eq!(response.status, StatusCode::OK);
    assert_eq!(response.headers[header::CACHE_CONTROL], "no-store");
    let body = response.json();
    assert_eq!(body["days"], 7);
    assert_eq!(body["timezone"], "UTC");
    assert!(body["members"]["total"].is_null());
    assert_eq!(body["messages"]["total"], 0);
    assert_eq!(body["cases"]["by_day"].as_array().unwrap().len(), 7);
    let days = body["cases"]["by_day"].as_array().unwrap();
    assert!(
        days.windows(2)
            .all(|pair| pair[0]["date"].as_str() < pair[1]["date"].as_str())
    );
    assert!(days.iter().all(|day| day["kinds"] == json!({})));
    assert_eq!(body.as_object().unwrap().len(), 6);
    assert_eq!(body["members"].as_object().unwrap().len(), 5);
    assert_eq!(body["messages"].as_object().unwrap().len(), 4);

    app.db
        .save_config(
            Id::new(GUILD),
            None,
            &closure_config::parse_overrides("timezone: Asia/Tokyo").unwrap(),
            None,
        )
        .await
        .unwrap();
    let response = send(&app.router, call(Method::GET, "/v1/guilds/42/stats?days=1")).await;
    assert_eq!(response.status, StatusCode::OK);
    assert_eq!(response.json()["timezone"], "Asia/Tokyo");
    assert!(response.json()["members"]["total"].is_null());
}

#[sqlx::test(migrations = "../closure-db/migrations")]
async fn unavailable_member_access_blocks_reads_and_actor_saves(pool: PgPool) {
    let app = app_with_access(pool, GatewayStatus { ready: 0, total: 1 }, false);
    app.db.register_guild(Id::new(GUILD)).await.unwrap();
    assert_eq!(
        send(&app.router, call(Method::GET, "/v1/guilds/42/access/1"))
            .await
            .status,
        StatusCode::SERVICE_UNAVAILABLE
    );
    assert_eq!(
        send(
            &app.router,
            put(42, "0", "permissions: { roles: { '6': owner } }").header("x-closure-actor", "1")
        )
        .await
        .status,
        StatusCode::SERVICE_UNAVAILABLE
    );
}

#[sqlx::test(migrations = "../closure-db/migrations")]
async fn dashboard_rejects_new_stronger_role_references(pool: PgPool) {
    let app = app(pool).await;
    let permissions = "permissions: { roles: { '5': admin, '6': owner, '7': trusted }, keys: { dashboard.edit: { allow: { roles: [8] } } } }\n";
    let document = |roles: &str| {
        format!(
            "{permissions}modules:\n  automod:\n    config:\n      rules:\n        example:\n          triggers: [{{ type: words, words: [spam] }}]\n          actions: [{{ do: add_roles, roles: [{roles}] }}]\n"
        )
    };
    assert_eq!(
        send(&app.router, put(42, "0", permissions)).await.status,
        StatusCode::OK
    );
    for role in ["6", "8"] {
        let response = send(
            &app.router,
            put(42, "1", &document(role)).header("x-closure-actor", "2"),
        )
        .await;
        assert_eq!(
            response.status,
            StatusCode::FORBIDDEN,
            "role {role}: {}",
            response.text
        );
    }
    assert_eq!(
        send(
            &app.router,
            put(42, "1", &document("7")).header("x-closure-actor", "2")
        )
        .await
        .status,
        StatusCode::OK
    );
    assert_eq!(
        send(
            &app.router,
            put(42, "2", &document("6")).header("x-closure-actor", "1")
        )
        .await
        .status,
        StatusCode::OK
    );
    assert_eq!(
        send(
            &app.router,
            put(42, "3", &document("'6'")).header("x-closure-actor", "2")
        )
        .await
        .status,
        StatusCode::OK
    );
    assert_eq!(
        send(&app.router, put(42, "4", &document("8"))).await.status,
        StatusCode::OK
    );
}

#[sqlx::test(migrations = "../closure-db/migrations")]
async fn dashboard_access_respects_rules_and_membership(pool: PgPool) {
    let app = app(pool).await;
    let document = "modules: { dashboard: { enabled: false } }\npermissions:\n  keys:\n    dashboard.edit:\n      allow: { roles: [5] }\n      deny: { users: [3] }\n";
    assert_eq!(
        send(&app.router, put(42, "0", document)).await.status,
        StatusCode::OK
    );
    for (user, level, allowed, reason) in [
        (1, 100, true, "owner"),
        (2, 1, true, "allowed_role"),
        (3, 1, false, "denied_user"),
        (4, 1, false, "level"),
        (999, 0, false, "not_member"),
    ] {
        let response = send(
            &app.router,
            call(Method::GET, &format!("/v1/guilds/42/access/{user}")),
        )
        .await;
        assert_eq!(response.status, StatusCode::OK);
        assert_eq!(response.json()["level"], level);
        assert_eq!(response.json()["allowed"], allowed);
        assert_eq!(response.json()["reason"], reason);
    }
    assert_eq!(
        send(&app.router, call(Method::GET, "/v1/guilds/43/access/1"))
            .await
            .status,
        StatusCode::NOT_FOUND
    );
    assert_eq!(
        send(
            &app.router,
            call(Method::GET, "/v1/guilds/42/access/1").token(None)
        )
        .await
        .status,
        StatusCode::UNAUTHORIZED
    );
}

#[sqlx::test(migrations = "../closure-db/migrations")]
async fn dashboard_privileged_changes_require_owner_level(pool: PgPool) {
    let app = app(pool).await;
    let document = "permissions: { roles: { '5': admin, '6': trusted } }\nmoderator_roles: [7]\n";
    assert_eq!(
        send(
            &app.router,
            put(42, "0", document).header("x-closure-actor", "2")
        )
        .await
        .status,
        StatusCode::FORBIDDEN
    );
    assert!(app.db.load_config(Id::new(GUILD)).await.unwrap().is_none());
    assert_eq!(
        send(
            &app.router,
            put(42, "0", document).header("x-closure-actor", "1")
        )
        .await
        .status,
        StatusCode::OK
    );
    let reordered = "moderator_roles: [7]\npermissions: { roles: { '6': trusted, '5': admin } }\n";
    assert_eq!(
        send(
            &app.router,
            put(42, "1", reordered).header("x-closure-actor", "2")
        )
        .await
        .status,
        StatusCode::OK
    );
    let basic = |actor: Option<&str>, revision: &str| {
        let mut request = call(Method::PUT, "/v1/guilds/42/config?view=basic")
            .header("if-match", revision)
            .body("moderator_roles: [8]\n");
        if let Some(actor) = actor {
            request = request.header("x-closure-actor", actor);
        }
        request
    };
    assert_eq!(
        send(&app.router, basic(Some("2"), "2")).await.status,
        StatusCode::FORBIDDEN
    );
    assert_eq!(
        send(&app.router, basic(Some("1"), "2")).await.status,
        StatusCode::OK
    );
    assert_eq!(
        send(&app.router, basic(Some("2"), "3")).await.status,
        StatusCode::OK
    );
    assert_eq!(
        send(&app.router, put(42, "4", "{}")).await.status,
        StatusCode::OK
    );
    assert_eq!(
        send(&app.router, basic(None, "5")).await.status,
        StatusCode::OK
    );
    assert_eq!(
        send(
            &app.router,
            put(42, "6", "permissions: { roles: { '5': owner } }")
        )
        .await
        .status,
        StatusCode::OK
    );
    assert_eq!(
        send(&app.router, basic(Some("2"), "7")).await.status,
        StatusCode::OK
    );
}

#[sqlx::test(migrations = "../closure-db/migrations")]
async fn delegated_editors_cannot_repoint_integration_credentials(pool: PgPool) {
    let app = app(pool).await;
    let permissions = "permissions: { roles: { '5': admin, '6': owner, '7': trusted }, keys: { dashboard.edit: { allow: { roles: [8] } } } }\n";
    let document = |url: &str, send_form_data: bool| {
        format!(
            "{permissions}modules:\n  tickets:\n    config:\n      integrations:\n        - id: crm\n          url: {url}\n          auth: bearer\n          headers: [{{ name: Authorization, value_template: \"Bearer %api%\" }}]\n          secrets: [api]\n          send_form_data: {send_form_data}\n"
        )
    };
    assert_eq!(
        send(
            &app.router,
            put(42, "0", &document("https://crm.example.com/a", false))
        )
        .await
        .status,
        StatusCode::OK
    );
    let moved = send(
        &app.router,
        put(42, "1", &document("https://attacker.example.com/a", false))
            .header("x-closure-actor", "2"),
    )
    .await;
    assert_eq!(moved.status, StatusCode::FORBIDDEN, "{}", moved.text);
    assert_eq!(
        send(
            &app.router,
            put(42, "1", &document("https://crm.example.com/a", true))
                .header("x-closure-actor", "2")
        )
        .await
        .status,
        StatusCode::OK
    );
    assert_eq!(
        send(
            &app.router,
            put(42, "2", &document("https://crm.example.com/b", true))
                .header("x-closure-actor", "1")
        )
        .await
        .status,
        StatusCode::OK
    );
}

#[sqlx::test(migrations = "../closure-db/migrations")]
async fn dashboard_uses_the_saved_permissions_when_the_runtime_store_is_stale(pool: PgPool) {
    let app = app(pool).await;
    let elevated = "permissions: { users: { '2': owner } }";
    assert_eq!(
        send(&app.router, put(42, "0", elevated)).await.status,
        StatusCode::OK
    );
    let empty = closure_config::parse_overrides("{}").unwrap();
    app.db
        .save_config(Id::new(42), Some(1), &empty, None)
        .await
        .unwrap();
    let access = send(&app.router, call(Method::GET, "/v1/guilds/42/access/2")).await;
    assert_eq!(access.json()["level"], 1);
    assert_eq!(
        send(
            &app.router,
            put(42, "2", elevated).header("x-closure-actor", "2")
        )
        .await
        .status,
        StatusCode::FORBIDDEN
    );
}

#[sqlx::test(migrations = "../closure-db/migrations")]

async fn dashboard_guilds_and_directory(pool: PgPool) {
    let app = app(pool).await;
    for uri in ["/v1/guilds", "/v1/guilds/42/directory"] {
        assert_eq!(
            send(&app.router, call(Method::GET, uri).token(None))
                .await
                .status,
            StatusCode::UNAUTHORIZED
        );
    }
    assert_eq!(
        send(&app.router, call(Method::GET, "/v1/guilds"))
            .await
            .json(),
        json!([{ "id": "42", "name": "Test", "icon": null }])
    );
    let directory = send(&app.router, call(Method::GET, "/v1/guilds/42/directory")).await;
    assert_eq!(directory.status, StatusCode::OK);
    assert_eq!(
        directory.json(),
        json!({ "channels": [{ "id": "5", "name": "general", "kind": "text", "parent_id": null, "position": 0 }], "roles": [{ "id": "6", "name": "member", "color": 42, "position": 1, "managed": false }] })
    );
    assert_eq!(
        send(&app.router, call(Method::GET, "/v1/guilds/43/directory"))
            .await
            .status,
        StatusCode::NOT_FOUND
    );
    app.db.register_guild(Id::new(43)).await.unwrap();
    assert_eq!(
        send(&app.router, call(Method::GET, "/v1/guilds/43/directory"))
            .await
            .status,
        StatusCode::SERVICE_UNAVAILABLE
    );
    assert_eq!(
        send(&app.router, call(Method::GET, "/v1/guilds"))
            .await
            .json(),
        json!([{ "id": "42", "name": "Test", "icon": null }])
    );
    app.db.mark_guild_left(Id::new(43)).await.unwrap();
    assert_eq!(
        send(&app.router, call(Method::GET, "/v1/guilds"))
            .await
            .json()
            .as_array()
            .unwrap()
            .len(),
        1
    );
}

#[sqlx::test(migrations = "../closure-db/migrations")]

async fn dashboard_guilds_waits_for_gateway_even_when_database_is_empty(pool: PgPool) {
    let app = app_with(pool, GatewayStatus { ready: 0, total: 1 });
    for uri in ["/v1/guilds", "/v1/guilds/42/directory"] {
        assert_eq!(
            send(&app.router, call(Method::GET, uri)).await.status,
            StatusCode::SERVICE_UNAVAILABLE,
            "{uri}"
        );
    }
}

#[sqlx::test(migrations = "../closure-db/migrations")]
async fn a_member_who_may_not_edit_the_dashboard_cannot_save_even_past_the_guard(pool: PgPool) {
    let app = app(pool).await;
    // A member of the server with no say in it: the writer is checked when the settings are saved.
    let general = send(
        &app.router,
        put(GUILD, "\"0\"", "preset: small").header("x-closure-actor", "77"),
    )
    .await;
    assert_eq!(general.status, StatusCode::FORBIDDEN, "{}", general.text);
    let stranger = send(
        &app.router,
        put(GUILD, "\"0\"", "preset: small").header("x-closure-actor", "999"),
    )
    .await;
    assert_eq!(stranger.status, StatusCode::FORBIDDEN, "{}", stranger.text);
}

#[sqlx::test(migrations = "../closure-db/migrations")]
async fn dashboard_actor_is_recorded_for_both_views(pool: PgPool) {
    let app = app(pool).await;
    for invalid in [
        "0",
        "-1",
        "+1",
        "abc",
        "9223372036854775808",
        "18446744073709551616",
        "",
    ] {
        let response = send(
            &app.router,
            put(GUILD, "\"0\"", "preset: small").header("x-closure-actor", invalid),
        )
        .await;
        assert_eq!(response.status, StatusCode::BAD_REQUEST, "{invalid:?}");
    }
    assert_eq!(
        send(
            &app.router,
            put(GUILD, "\"0\"", "preset: small")
                .header("x-closure-actor", "123")
                .header("x-closure-actor", "456"),
        )
        .await
        .status,
        StatusCode::BAD_REQUEST
    );
    let unauth = send(
        &app.router,
        put(GUILD, "\"0\"", "preset: small")
            .header("x-closure-actor", "bad")
            .token(None),
    )
    .await;
    assert_eq!(unauth.status, StatusCode::UNAUTHORIZED);
    let advanced = send(
        &app.router,
        put(GUILD, "\"0\"", "preset: small").header("x-closure-actor", "9223372036854775807"),
    )
    .await;
    assert_eq!(advanced.status, StatusCode::OK, "{}", advanced.text);
    let basic = send(
        &app.router,
        basic_put("\"1\"", "preset: large").header("x-closure-actor", "123"),
    )
    .await;
    assert_eq!(basic.status, StatusCode::OK, "{}", basic.text);
    assert_eq!(
        send(&app.router, put(GUILD, "\"2\"", "preset: medium"))
            .await
            .status,
        StatusCode::OK
    );
    let history = send(
        &app.router,
        call(Method::GET, "/v1/guilds/42/config/history"),
    )
    .await
    .json();
    assert_eq!(history[0]["actor"], Value::Null);
    assert_eq!(history[1]["actor"], json!("123"));
    assert_eq!(history[2]["actor"], json!("9223372036854775807"));
    let audit = app.db.recent_audit(Id::new(GUILD), 10).await.unwrap();
    assert_eq!(
        audit
            .iter()
            .map(|e| e.actor.map(Id::get))
            .collect::<Vec<_>>(),
        [None, Some(123), Some(9_223_372_036_854_775_807)]
    );
}

#[sqlx::test(migrations = "../closure-db/migrations")]
async fn probes_need_no_token_but_everything_else_does(pool: PgPool) {
    let app = app(pool).await;
    for uri in ["/healthz", "/readyz"] {
        let response = send(&app.router, call(Method::GET, uri).token(None)).await;
        assert_eq!(response.status, StatusCode::OK, "{uri}");
    }
    for uri in ["/metrics", "/v1/schema", "/v1/guilds/1/config"] {
        let response = send(&app.router, call(Method::GET, uri).token(None)).await;
        assert_eq!(response.status, StatusCode::UNAUTHORIZED, "{uri}");
        assert_eq!(response.headers[header::WWW_AUTHENTICATE], "Bearer");
        assert_eq!(response.json()["error"]["code"], "unauthorized");
    }
}

#[sqlx::test(migrations = "../closure-db/migrations")]
async fn wrong_or_malformed_credentials_are_refused(pool: PgPool) {
    let app = app(pool).await;
    let attempts: [(&str, Call); 4] = [
        (
            "wrong token",
            call(Method::GET, "/metrics").token(Some("not-the-right-token-but-long-enough")),
        ),
        ("empty token", call(Method::GET, "/metrics").token(Some(""))),
        (
            "basic scheme",
            call(Method::GET, "/metrics")
                .token(None)
                .header("authorization", &format!("Basic {TOKEN}")),
        ),
        (
            "no scheme",
            call(Method::GET, "/metrics")
                .token(None)
                .header("authorization", TOKEN),
        ),
    ];
    for (name, attempt) in attempts {
        let response = send(&app.router, attempt).await;
        assert_eq!(response.status, StatusCode::UNAUTHORIZED, "{name}");
    }
    let accepted = send(
        &app.router,
        call(Method::GET, "/metrics")
            .token(None)
            .header("authorization", &format!("bearer {TOKEN}")),
    )
    .await;
    assert_eq!(
        accepted.status,
        StatusCode::OK,
        "the scheme name is case-insensitive"
    );
}

#[sqlx::test(migrations = "../closure-db/migrations")]
async fn an_address_that_keeps_failing_to_log_in_is_turned_away_even_with_the_right_token(
    pool: PgPool,
) {
    use std::net::SocketAddr;

    use axum::extract::ConnectInfo;

    let app = app(pool).await;
    let from = |address: &str, token: &str| {
        let address: SocketAddr = address.parse().unwrap();
        let router = app.router.clone();
        let request = Request::builder()
            .uri("/metrics")
            .header(header::AUTHORIZATION, format!("Bearer {token}"))
            .extension(ConnectInfo(address))
            .body(Body::empty())
            .unwrap();
        async move { router.oneshot(request).await.unwrap() }
    };

    for _ in 0..10 {
        let response = from("198.51.100.7:4000", "a-wrong-token-that-is-long-enough").await;
        assert_eq!(response.status(), StatusCode::UNAUTHORIZED);
    }
    // Used up: the right token does not help until the window has passed.
    let blocked = from("198.51.100.7:4001", TOKEN).await;
    assert_eq!(blocked.status(), StatusCode::TOO_MANY_REQUESTS);
    let wait: u64 = blocked.headers()[header::RETRY_AFTER]
        .to_str()
        .unwrap()
        .parse()
        .unwrap();
    assert!((1..=60).contains(&wait), "{wait}");
    assert_eq!(
        from("198.51.100.8:4000", TOKEN).await.status(),
        StatusCode::OK
    );
    let probe = Request::builder()
        .uri("/healthz")
        .extension(ConnectInfo(
            "198.51.100.7:4002".parse::<SocketAddr>().unwrap(),
        ))
        .body(Body::empty())
        .unwrap();
    assert_eq!(
        app.router.clone().oneshot(probe).await.unwrap().status(),
        StatusCode::OK
    );
}

#[sqlx::test(migrations = "../closure-db/migrations")]
async fn liveness_probe_answers_while_every_business_slot_is_taken(pool: PgPool) {
    use std::sync::atomic::{AtomicUsize, Ordering};
    use std::time::Duration;

    let stalled = Arc::new(StalledAccess(AtomicUsize::new(0)));
    let app = app_with_source(
        pool,
        GatewayStatus { ready: 1, total: 1 },
        Arc::clone(&stalled) as Arc<dyn MemberAccess>,
        None,
    );
    app.db.register_guild(Id::new(GUILD)).await.unwrap();
    let mut requests = Vec::new();
    for _ in 0..MAX_CONCURRENT_REQUESTS {
        let router = app.router.clone();
        let uri = format!("/v1/guilds/{GUILD}/access/1");
        requests.push(tokio::spawn(async move {
            send(&router, call(Method::GET, &uri)).await
        }));
    }
    for _ in 0..500 {
        if stalled.0.load(Ordering::SeqCst) == MAX_CONCURRENT_REQUESTS {
            break;
        }
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
    assert_eq!(stalled.0.load(Ordering::SeqCst), MAX_CONCURRENT_REQUESTS);

    let probe = tokio::time::timeout(
        Duration::from_secs(2),
        send(&app.router, call(Method::GET, "/healthz").token(None)),
    )
    .await
    .expect("the liveness probe waited for a business slot");
    assert_eq!(probe.status, StatusCode::OK);
    for request in requests {
        request.abort();
    }
}

#[sqlx::test(migrations = "../closure-db/migrations")]
async fn weak_admin_tokens_are_refused_at_setup(pool: PgPool) {
    let registry = Arc::new(ModuleRegistry::new(vec![]).unwrap());
    let store = Arc::new(SettingsStore::new(ConfigCompiler::new(registry).unwrap()));
    let result = ApiState::new(
        store,
        Db::from_pool(pool),
        Arc::new(FixedReadiness(GatewayStatus { ready: 0, total: 0 })),
        Arc::new(FakeDirectory),
        Arc::new(FakeAccess(true)),
        String::new,
        &SecretString::from("short"),
    );
    assert!(matches!(result, Err(SetupError::WeakToken)));
}

#[sqlx::test(migrations = "../closure-db/migrations")]
async fn an_unconfigured_guild_reads_as_an_empty_document_at_revision_zero(pool: PgPool) {
    let app = app(pool).await;
    let response = send(&app.router, call(Method::GET, "/v1/guilds/42/config")).await;
    assert_eq!(response.status, StatusCode::OK);
    assert_eq!(response.etag(), "\"0\"");
    assert_eq!(response.json(), json!({}));
}

#[sqlx::test(migrations = "../closure-db/migrations")]
async fn saving_creates_then_updates_and_takes_effect_immediately(pool: PgPool) {
    let app = app(pool).await;
    let guild = Id::new(GUILD);
    let warn = ModuleId::new("warn").unwrap();
    assert!(!app.store.module(guild, &warn).unwrap().enabled);

    let created = send(
        &app.router,
        put(GUILD, "\"0\"", "modules:\n  warn:\n    enabled: true\n"),
    )
    .await;
    assert_eq!(created.status, StatusCode::OK, "{}", created.text);
    assert_eq!(created.etag(), "\"1\"");
    assert_eq!(created.json()["revision"], 1);
    assert!(
        app.store.module(guild, &warn).unwrap().enabled,
        "applied without a restart"
    );

    let updated = send(
        &app.router,
        put(
            GUILD,
            "\"1\"",
            "language: ja\nmodules:\n  warn:\n    enabled: true\n    config: { limit: 7 }\n",
        ),
    )
    .await;
    assert_eq!(updated.etag(), "\"2\"");
    assert_eq!(app.store.module(guild, &warn).unwrap().config["limit"], 7);

    let stored = app.db.load_config(guild).await.unwrap().unwrap();
    assert_eq!(stored.revision, 2);

    let trail = app.db.recent_audit(guild, 10).await.unwrap();
    assert_eq!(
        trail.iter().filter(|e| e.action == "config.update").count(),
        2
    );
}

#[sqlx::test(migrations = "../closure-db/migrations")]
async fn only_servers_the_bot_is_in_can_be_configured(pool: PgPool) {
    let app = app_with(pool, GatewayStatus { ready: 1, total: 1 });
    let response = send(&app.router, put(777, "\"0\"", "preset: small")).await;
    assert_eq!(response.status, StatusCode::NOT_FOUND);
    assert_eq!(response.json()["error"]["code"], "not_found");
    assert_eq!(app.db.load_config(Id::new(777)).await.unwrap(), None);

    app.db.register_guild(Id::new(777)).await.unwrap();
    let response = send(&app.router, put(777, "\"0\"", "preset: small")).await;
    assert_eq!(response.status, StatusCode::OK);

    app.db.mark_guild_left(Id::new(777)).await.unwrap();
    let response = send(&app.router, put(777, "\"1\"", "preset: large")).await;
    assert_eq!(
        response.status,
        StatusCode::NOT_FOUND,
        "a guild the bot left is not configurable"
    );
}

#[sqlx::test(migrations = "../closure-db/migrations")]
async fn writes_must_say_which_revision_they_edit(pool: PgPool) {
    let app = app(pool).await;
    let response = send(
        &app.router,
        call(Method::PUT, "/v1/guilds/42/config").body("preset: small"),
    )
    .await;
    assert_eq!(response.status, StatusCode::PRECONDITION_REQUIRED);
    assert_eq!(app.db.load_config(Id::new(GUILD)).await.unwrap(), None);
}

#[sqlx::test(migrations = "../closure-db/migrations")]
async fn a_stale_editor_gets_a_conflict_with_the_current_revision(pool: PgPool) {
    let app = app(pool).await;
    send(&app.router, put(GUILD, "\"0\"", "preset: small")).await;
    send(&app.router, put(GUILD, "\"1\"", "preset: medium")).await;

    let stale = send(&app.router, put(GUILD, "\"1\"", "preset: large")).await;
    assert_eq!(stale.status, StatusCode::PRECONDITION_FAILED);
    assert_eq!(stale.etag(), "\"2\"");
    assert_eq!(stale.json()["error"]["code"], "revision_conflict");
    let stored = app.db.load_config(Id::new(GUILD)).await.unwrap().unwrap();
    assert_eq!(stored.revision, 2);
}

#[sqlx::test(migrations = "../closure-db/migrations")]
async fn simultaneous_edits_from_one_revision_have_one_winner(pool: PgPool) {
    let app = app(pool).await;
    send(&app.router, put(GUILD, "\"0\"", "preset: small")).await;

    let (a, b) = tokio::join!(
        send(&app.router, put(GUILD, "\"1\"", "preset: medium")),
        send(&app.router, put(GUILD, "\"1\"", "preset: large")),
    );
    let mut statuses = [a.status, b.status];
    statuses.sort();
    assert_eq!(statuses, [StatusCode::OK, StatusCode::PRECONDITION_FAILED]);
}

#[sqlx::test(migrations = "../closure-db/migrations")]
async fn invalid_documents_are_rejected_before_anything_is_stored(pool: PgPool) {
    let app = app(pool).await;
    let guild = Id::new(GUILD);

    let syntax = send(
        &app.router,
        put(GUILD, "\"0\"", "preset: small\npreest: large\n"),
    )
    .await;
    assert_eq!(syntax.status, StatusCode::UNPROCESSABLE_ENTITY);
    assert_eq!(syntax.json()["error"]["code"], "invalid_document");
    assert_eq!(syntax.json()["error"]["line"], 2);

    let semantic = send(
        &app.router,
        put(
            GUILD,
            "\"0\"",
            "modules:\n  warn:\n    config: { limit: 99 }\n  wran: {}\n",
        ),
    )
    .await;
    assert_eq!(semantic.status, StatusCode::UNPROCESSABLE_ENTITY);
    let issues = semantic.json()["error"]["issues"].clone();
    let paths: Vec<&str> = issues
        .as_array()
        .unwrap()
        .iter()
        .map(|i| i["path"].as_str().unwrap())
        .collect();
    assert!(paths.contains(&"modules.warn.config.limit"), "{paths:?}");
    assert!(paths.contains(&"modules.wran"), "{paths:?}");

    assert_eq!(app.db.load_config(guild).await.unwrap(), None);
    assert_eq!(app.store.configured_guilds(), 0);
}

#[sqlx::test(migrations = "../closure-db/migrations")]
async fn json_bodies_are_accepted_and_yaml_can_be_requested(pool: PgPool) {
    let app = app(pool).await;
    let saved = send(
        &app.router,
        put(GUILD, "\"0\"", r#"{"preset":"small","language":"ja"}"#),
    )
    .await;
    assert_eq!(saved.status, StatusCode::OK, "{}", saved.text);

    let json = send(&app.router, call(Method::GET, "/v1/guilds/42/config")).await;
    assert_eq!(json.json()["preset"], "small");
    assert_eq!(json.json()["language"], "ja");

    let yaml = send(
        &app.router,
        call(Method::GET, "/v1/guilds/42/config").header("accept", "application/yaml"),
    )
    .await;
    assert_eq!(yaml.headers[header::CONTENT_TYPE], "application/yaml");
    assert!(yaml.text.contains("preset: small"), "{}", yaml.text);
}

#[sqlx::test(migrations = "../closure-db/migrations")]
async fn validating_is_a_dry_run_that_shows_the_resolved_result(pool: PgPool) {
    let app = app(pool).await;
    let checked = send(
        &app.router,
        call(Method::POST, "/v1/guilds/42/config/validate")
            .body("preset: large\nmodules:\n  warn:\n    enabled: true\n"),
    )
    .await;
    assert_eq!(checked.status, StatusCode::OK, "{}", checked.text);
    let effective = &checked.json()["effective"];
    assert_eq!(effective["preset"], "large");
    assert_eq!(effective["limits"]["concurrency"], 32);
    assert_eq!(effective["modules"]["warn"]["enabled"], true);
    assert_eq!(effective["modules"]["warn"]["config"]["limit"], 3);

    assert_eq!(
        app.db.load_config(Id::new(GUILD)).await.unwrap(),
        None,
        "nothing stored"
    );

    let bad = send(
        &app.router,
        call(Method::POST, "/v1/guilds/42/config/validate").body("preset: huge"),
    )
    .await;
    assert_eq!(bad.status, StatusCode::UNPROCESSABLE_ENTITY);
}

#[sqlx::test(migrations = "../closure-db/migrations")]
async fn the_effective_view_fills_in_every_default(pool: PgPool) {
    let app = app(pool).await;
    send(&app.router, put(GUILD, "\"0\"", "language: ja")).await;
    let effective = send(
        &app.router,
        call(Method::GET, "/v1/guilds/42/config/effective"),
    )
    .await;
    let body = effective.json();
    assert_eq!(body["language"], "ja");
    assert_eq!(body["preset"], "medium");
    assert_eq!(body["limits"]["queue_capacity"], 256);
    assert_eq!(effective.etag(), "\"1\"");
}

#[sqlx::test(migrations = "../closure-db/migrations")]
async fn history_lists_revisions_newest_first(pool: PgPool) {
    let app = app(pool).await;
    send(&app.router, put(GUILD, "\"0\"", "preset: small")).await;
    send(&app.router, put(GUILD, "\"1\"", "preset: large")).await;
    let response = send(
        &app.router,
        call(Method::GET, "/v1/guilds/42/config/history?limit=5"),
    )
    .await;
    let history = response.json();
    let revisions: Vec<i64> = history
        .as_array()
        .unwrap()
        .iter()
        .map(|h| h["revision"].as_i64().unwrap())
        .collect();
    assert_eq!(revisions, [2, 1]);
    assert_eq!(history[1]["document"]["preset"], "small");
}

#[sqlx::test(migrations = "../closure-db/migrations")]
async fn oversized_bodies_are_refused(pool: PgPool) {
    let app = app(pool).await;
    let huge = format!("# {}\n", "x".repeat(200 * 1024));
    let response = send(&app.router, put(GUILD, "\"0\"", &huge)).await;
    assert_eq!(response.status, StatusCode::PAYLOAD_TOO_LARGE);
}

#[sqlx::test(migrations = "../closure-db/migrations")]
async fn bad_guild_ids_are_rejected(pool: PgPool) {
    let app = app(pool).await;
    for uri in [
        "/v1/guilds/0/config",
        "/v1/guilds/18446744073709551615/config",
    ] {
        let response = send(&app.router, call(Method::GET, uri)).await;
        assert_eq!(response.status, StatusCode::BAD_REQUEST, "{uri}");
    }
    let response = send(&app.router, call(Method::GET, "/v1/guilds/abc/config")).await;
    assert!(response.status.is_client_error());
}

#[sqlx::test(migrations = "../closure-db/migrations")]
async fn readiness_needs_both_the_database_and_every_shard(pool: PgPool) {
    let ready = app_with(pool.clone(), GatewayStatus { ready: 2, total: 2 });
    let response = send(&ready.router, call(Method::GET, "/readyz").token(None)).await;
    assert_eq!(response.status, StatusCode::OK);
    assert_eq!(response.json()["ready"], true);

    let degraded = app_with(pool.clone(), GatewayStatus { ready: 1, total: 2 });
    let response = send(&degraded.router, call(Method::GET, "/readyz").token(None)).await;
    assert_eq!(response.status, StatusCode::SERVICE_UNAVAILABLE);
    assert_eq!(
        response.json()["gateway"],
        json!({ "ready": 1, "total": 2 })
    );

    let none = app_with(pool.clone(), GatewayStatus { ready: 0, total: 0 });
    assert_eq!(
        send(&none.router, call(Method::GET, "/readyz").token(None))
            .await
            .status,
        StatusCode::SERVICE_UNAVAILABLE
    );

    pool.close().await;
    let no_database = send(&ready.router, call(Method::GET, "/readyz").token(None)).await;
    assert_eq!(no_database.status, StatusCode::SERVICE_UNAVAILABLE);
    assert_eq!(no_database.json()["database"], false);
}

#[sqlx::test(migrations = "../closure-db/migrations")]
async fn metrics_and_the_schema_are_served_to_the_admin(pool: PgPool) {
    let app = app(pool).await;
    let metrics = send(&app.router, call(Method::GET, "/metrics")).await;
    assert_eq!(metrics.text, "closure_up 1\n");
    assert!(
        metrics.headers[header::CONTENT_TYPE]
            .to_str()
            .unwrap()
            .starts_with("text/plain")
    );

    let schema = send(&app.router, call(Method::GET, "/v1/schema"))
        .await
        .json();
    assert_eq!(
        schema["properties"]["modules"]["anyOf"][0]["properties"]["warn"]["properties"]["config"]["anyOf"]
            [0]["properties"]["limit"]["anyOf"][0]["maximum"],
        10
    );
}

#[sqlx::test(migrations = "../closure-db/migrations")]
async fn internal_failures_do_not_leak_details(pool: PgPool) {
    let app = app(pool.clone()).await;
    pool.close().await;
    let response = send(&app.router, call(Method::GET, "/v1/guilds/42/config")).await;
    assert_eq!(response.status, StatusCode::INTERNAL_SERVER_ERROR);
    assert_eq!(response.json()["error"]["code"], "internal_error");
    assert!(
        !response.text.to_lowercase().contains("pool"),
        "{}",
        response.text
    );
}

/// A full document with a basic and an advanced part, saved at revision 1.
async fn seed_both_parts(app: &App) {
    let saved = send(
        &app.router,
        put(
            GUILD,
            "\"0\"",
            "language: ja\nlimits:\n  concurrency: 12\nmodules:\n  layered:\n    enabled: true\n    config: { dry_run: true, rules: [a] }\n",
        ),
    )
    .await;
    assert_eq!(saved.status, StatusCode::OK, "{}", saved.text);
}

fn basic_put(if_match: &str, body: &str) -> Call {
    call(Method::PUT, "/v1/guilds/42/config?view=basic")
        .header("if-match", if_match)
        .body(body)
}

#[sqlx::test(migrations = "../closure-db/migrations")]
async fn the_basic_read_returns_only_the_basic_part(pool: PgPool) {
    let app = app(pool).await;
    seed_both_parts(&app).await;
    let basic = send(
        &app.router,
        call(Method::GET, "/v1/guilds/42/config?view=basic"),
    )
    .await;
    assert_eq!(basic.status, StatusCode::OK, "{}", basic.text);
    assert_eq!(basic.etag(), "\"1\"");
    assert_eq!(
        basic.json(),
        json!({
            "language": "ja",
            "modules": { "layered": { "enabled": true, "config": { "dry_run": true } } }
        })
    );

    let full = send(&app.router, call(Method::GET, "/v1/guilds/42/config")).await;
    assert_eq!(full.json()["limits"]["concurrency"], 12);
    let unspecified = send(
        &app.router,
        call(Method::GET, "/v1/guilds/42/config?view=advanced"),
    )
    .await;
    assert_eq!(unspecified.json(), full.json(), "advanced is the default");
}

#[sqlx::test(migrations = "../closure-db/migrations")]
async fn a_basic_write_sets_keeps_and_clears_only_what_it_names(pool: PgPool) {
    let app = app(pool).await;
    seed_both_parts(&app).await;
    let saved = send(
        &app.router,
        basic_put(
            "\"1\"",
            "preset: large\nlanguage: null\nmodules:\n  layered:\n    config: { dry_run: false }\n",
        ),
    )
    .await;
    assert_eq!(saved.status, StatusCode::OK, "{}", saved.text);
    assert_eq!(saved.etag(), "\"2\"");

    let full = send(&app.router, call(Method::GET, "/v1/guilds/42/config")).await;
    let body = full.json();
    assert_eq!(body["preset"], "large");
    assert!(body.get("language").is_none(), "{body}");
    assert_eq!(
        body["limits"]["concurrency"], 12,
        "advanced value untouched"
    );
    assert_eq!(
        body["modules"]["layered"]["enabled"], true,
        "absent key kept"
    );
    assert_eq!(body["modules"]["layered"]["config"]["dry_run"], false);
    assert_eq!(body["modules"]["layered"]["config"]["rules"], json!(["a"]));
}

#[sqlx::test(migrations = "../closure-db/migrations")]
async fn a_basic_write_refuses_an_advanced_key_by_name_and_stores_nothing(pool: PgPool) {
    let app = app(pool).await;
    seed_both_parts(&app).await;
    for (body, path) in [
        ("limits:\n  concurrency: 2\n", "limits"),
        (
            "modules:\n  layered:\n    config: { rules: [x] }\n",
            "modules.layered.config.rules",
        ),
        (
            "modules:\n  layered:\n    capabilities: [send_messages]\n",
            "modules.layered.capabilities",
        ),
    ] {
        let response = send(&app.router, basic_put("\"1\"", body)).await;
        assert_eq!(response.status, StatusCode::UNPROCESSABLE_ENTITY, "{body}");
        assert_eq!(response.json()["error"]["code"], "invalid_config");
        let issues = response.json()["error"]["issues"].clone();
        assert_eq!(issues[0]["path"], path, "{issues}");
        assert_eq!(issues[0]["code"], "not_basic");
    }
    let stored = app.db.load_config(Id::new(GUILD)).await.unwrap().unwrap();
    assert_eq!(stored.revision, 1);
}

#[sqlx::test(migrations = "../closure-db/migrations")]
async fn a_basic_write_obeys_the_same_revision_rules_as_a_full_one(pool: PgPool) {
    let app = app(pool).await;
    seed_both_parts(&app).await;
    let missing = send(
        &app.router,
        call(Method::PUT, "/v1/guilds/42/config?view=basic").body("preset: small"),
    )
    .await;
    assert_eq!(missing.status, StatusCode::PRECONDITION_REQUIRED);

    let stale = send(&app.router, basic_put("\"0\"", "preset: small")).await;
    assert_eq!(stale.status, StatusCode::PRECONDITION_FAILED);
    assert_eq!(stale.etag(), "\"1\"");
    assert_eq!(stale.json()["error"]["code"], "revision_conflict");
}

#[sqlx::test(migrations = "../closure-db/migrations")]
async fn a_basic_write_is_recorded_in_the_history_and_the_audit_trail(pool: PgPool) {
    let app = app(pool).await;
    seed_both_parts(&app).await;
    send(&app.router, basic_put("\"1\"", "preset: small\n")).await;

    let history = send(
        &app.router,
        call(Method::GET, "/v1/guilds/42/config/history?limit=5"),
    )
    .await
    .json();
    assert_eq!(history[0]["revision"], 2);
    assert_eq!(history[0]["document"]["preset"], "small");
    assert_eq!(history[0]["document"]["limits"]["concurrency"], 12);

    let trail = app.db.recent_audit(Id::new(GUILD), 10).await.unwrap();
    assert_eq!(
        trail.iter().filter(|e| e.action == "config.update").count(),
        2
    );
}

#[sqlx::test(migrations = "../closure-db/migrations")]
async fn validating_a_basic_document_checks_it_merged_onto_the_stored_one(pool: PgPool) {
    let app = app(pool).await;
    seed_both_parts(&app).await;
    let checked = send(
        &app.router,
        call(Method::POST, "/v1/guilds/42/config/validate?view=basic")
            .body("modules:\n  layered:\n    config: { dry_run: false }\n"),
    )
    .await;
    assert_eq!(checked.status, StatusCode::OK, "{}", checked.text);
    let config = &checked.json()["effective"]["modules"]["layered"]["config"];
    assert_eq!(config["dry_run"], false);
    assert_eq!(
        config["rules"],
        json!(["a"]),
        "the stored advanced value is used"
    );
    assert_eq!(
        app.db
            .load_config(Id::new(GUILD))
            .await
            .unwrap()
            .unwrap()
            .revision,
        1,
        "nothing stored"
    );

    let refused = send(
        &app.router,
        call(Method::POST, "/v1/guilds/42/config/validate?view=basic")
            .body("limits: {concurrency: 1}\n"),
    )
    .await;
    assert_eq!(refused.status, StatusCode::UNPROCESSABLE_ENTITY);
}

#[sqlx::test(migrations = "../closure-db/migrations")]
async fn the_schema_of_a_view_only_prunes_the_one_it_is_asked_for(pool: PgPool) {
    let app = app(pool).await;
    let basic = send(&app.router, call(Method::GET, "/v1/schema?view=basic")).await;
    assert_eq!(basic.status, StatusCode::OK, "{}", basic.text);
    let properties = &basic.json()["properties"];
    assert!(properties.get("limits").is_none());
    assert!(properties.get("language").is_some());
    let layered = &properties["modules"]["anyOf"][0]["properties"]["layered"]["properties"];
    assert!(layered.get("capabilities").is_none());
    let config = &layered["config"]["anyOf"][0]["properties"];
    assert!(config.get("dry_run").is_some(), "{config}");
    assert!(config.get("rules").is_none(), "{config}");

    let default = send(&app.router, call(Method::GET, "/v1/schema"))
        .await
        .json();
    let advanced = send(&app.router, call(Method::GET, "/v1/schema?view=advanced"))
        .await
        .json();
    assert_eq!(default, advanced);
    assert!(default["properties"].get("limits").is_some());
}

#[sqlx::test(migrations = "../closure-db/migrations")]
async fn an_unknown_view_is_a_bad_request_everywhere(pool: PgPool) {
    let app = app(pool).await;
    let uris = [
        (Method::GET, "/v1/schema?view=simple"),
        (Method::GET, "/v1/guilds/42/config?view=simple"),
        (Method::GET, "/v1/guilds/42/config/effective?view=simple"),
        (Method::POST, "/v1/guilds/42/config/validate?view=simple"),
    ];
    for (method, uri) in uris {
        let response = send(&app.router, call(method, uri).body("preset: small")).await;
        assert_eq!(response.status, StatusCode::BAD_REQUEST, "{uri}");
        assert_eq!(response.json()["error"]["code"], "bad_request", "{uri}");
    }
    let write = send(
        &app.router,
        call(Method::PUT, "/v1/guilds/42/config?view=simple")
            .header("if-match", "\"0\"")
            .body("preset: small"),
    )
    .await;
    assert_eq!(write.status, StatusCode::BAD_REQUEST);
    assert_eq!(app.db.load_config(Id::new(GUILD)).await.unwrap(), None);
}

#[sqlx::test(migrations = "../closure-db/migrations")]
async fn the_effective_view_ignores_the_view_parameter(pool: PgPool) {
    let app = app(pool).await;
    seed_both_parts(&app).await;
    let plain = send(
        &app.router,
        call(Method::GET, "/v1/guilds/42/config/effective"),
    )
    .await;
    let basic = send(
        &app.router,
        call(Method::GET, "/v1/guilds/42/config/effective?view=basic"),
    )
    .await;
    assert_eq!(basic.status, StatusCode::OK);
    assert_eq!(basic.json(), plain.json());
}

#[sqlx::test(migrations = "../closure-db/migrations")]
async fn a_message_written_in_the_dashboard_is_sent_by_the_bot(pool: PgPool) {
    let sink = Arc::new(FakeSink(Ok(Sent { id: 77 }), std::sync::Mutex::default()));
    let app = app_built(
        pool,
        GatewayStatus { ready: 1, total: 1 },
        true,
        Some(Arc::clone(&sink) as Arc<dyn MessageSink>),
    );
    app.db.register_guild(Id::new(GUILD)).await.unwrap();
    let uri = "/v1/guilds/42/messages";
    let body = json!({"channel_id": "5", "message": {
        "content": "hello",
        "embeds": [{"title": "T", "color": "#5865f2", "fields": [{"name": "a", "value": "b", "inline": true}]}]
    }})
    .to_string();
    assert_eq!(
        send(&app.router, post_json(uri).token(None).body(&body))
            .await
            .status,
        StatusCode::UNAUTHORIZED
    );
    let sent = send(&app.router, post_json(uri).body(&body)).await;
    assert_eq!(sent.status, StatusCode::OK, "{}", sent.text);
    assert_eq!(
        sent.json(),
        json!({"id": "77", "channel_id": "5", "url": "https://discord.com/channels/42/5/77"})
    );
    assert_eq!(*sink.1.lock().unwrap(), [(5, Some("hello".to_owned()), 1)]);

    for (body, status) in [
        (
            json!({"channel_id": "9", "message": {"content": "x"}}),
            StatusCode::NOT_FOUND,
        ),
        (
            json!({"channel_id": "x", "message": {"content": "x"}}),
            StatusCode::BAD_REQUEST,
        ),
        (
            json!({"channel_id": "5", "message": {}}),
            StatusCode::BAD_REQUEST,
        ),
        (
            json!({"channel_id": "5", "message": {"content": "a".repeat(2001)}}),
            StatusCode::BAD_REQUEST,
        ),
        (
            json!({"channel_id": "5", "message": {"embeds": [{"title": "t", "image": "ftp://x"}]}}),
            StatusCode::BAD_REQUEST,
        ),
    ] {
        let response = send(&app.router, post_json(uri).body(&body.to_string())).await;
        assert_eq!(response.status, status, "{body}: {}", response.text);
    }
    assert_eq!(sink.1.lock().unwrap().len(), 1);
    let unknown = json!({"channel_id": "5", "message": {"content": "x"}, "extra": 1}).to_string();
    assert!(
        send(&app.router, post_json(uri).body(&unknown))
            .await
            .status
            .is_client_error()
    );
}

#[sqlx::test(migrations = "../closure-db/migrations")]
async fn a_message_sent_with_a_key_is_sent_once(pool: PgPool) {
    let sink = Arc::new(FakeSink(Ok(Sent { id: 77 }), std::sync::Mutex::default()));
    let app = app_built(
        pool,
        GatewayStatus { ready: 1, total: 1 },
        true,
        Some(Arc::clone(&sink) as Arc<dyn MessageSink>),
    );
    app.db.register_guild(Id::new(GUILD)).await.unwrap();
    let uri = "/v1/guilds/42/messages";
    let body = json!({"channel_id": "5", "message": {"content": "hello"}}).to_string();
    let first = send(
        &app.router,
        post_json(uri)
            .header("idempotency-key", "form-1")
            .body(&body),
    )
    .await;
    assert_eq!(first.status, StatusCode::OK, "{}", first.text);
    // The same form sent again (a restart of the dashboard, a retry) gets the first answer.
    let again = send(
        &app.router,
        post_json(uri)
            .header("idempotency-key", "form-1")
            .body(&body),
    )
    .await;
    assert_eq!(again.status, StatusCode::OK, "{}", again.text);
    assert_eq!(again.json(), first.json());
    assert_eq!(sink.1.lock().unwrap().len(), 1, "it was sent once");
    // The key of another message, and a key that is not one.
    let other = json!({"channel_id": "5", "message": {"content": "other"}}).to_string();
    let reused = send(
        &app.router,
        post_json(uri)
            .header("idempotency-key", "form-1")
            .body(&other),
    )
    .await;
    assert_eq!(reused.status, StatusCode::BAD_REQUEST);
    let bad = send(
        &app.router,
        post_json(uri)
            .header("idempotency-key", "no good")
            .body(&body),
    )
    .await;
    assert_eq!(bad.status, StatusCode::BAD_REQUEST);
    assert_eq!(sink.1.lock().unwrap().len(), 1);
    // A send that surely failed gives its key up, so that it can be tried again.
    let missing = json!({"channel_id": "9", "message": {"content": "x"}}).to_string();
    for _ in 0..2 {
        let response = send(
            &app.router,
            post_json(uri)
                .header("idempotency-key", "form-2")
                .body(&missing),
        )
        .await;
        assert_eq!(response.status, StatusCode::NOT_FOUND);
    }
}

#[sqlx::test(migrations = "../closure-db/migrations")]
async fn a_post_can_be_scheduled_listed_and_taken_back_and_buttons_are_checked(pool: PgPool) {
    let sink = Arc::new(FakeSink(Ok(Sent { id: 77 }), std::sync::Mutex::default()));
    let app = app_built(
        pool,
        GatewayStatus { ready: 1, total: 1 },
        true,
        Some(Arc::clone(&sink) as Arc<dyn MessageSink>),
    );
    app.db.register_guild(Id::new(GUILD)).await.unwrap();
    let uri = "/v1/guilds/42/messages";
    let later = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_secs()
        + 3600;
    let body = json!({"channel_id": "5", "send_at": later, "message": {
        "content": "later", "buttons": [{"label": "News", "role": "9"}]
    }});
    let scheduled = send(&app.router, post_json(uri).body(&body.to_string())).await;
    assert_eq!(scheduled.status, StatusCode::OK, "{}", scheduled.text);
    assert_eq!(scheduled.json()["scheduled"], true);
    let key = scheduled.json()["key"].as_str().unwrap().to_owned();
    assert!(sink.1.lock().unwrap().is_empty(), "nothing is sent now");

    let listed = send(
        &app.router,
        call(Method::GET, "/v1/guilds/42/scheduled-posts"),
    )
    .await;
    let posts = listed.json()["posts"].as_array().unwrap().clone();
    assert_eq!(posts.len(), 1);
    assert_eq!(posts[0]["key"], key.as_str());
    assert_eq!(posts[0]["channel_id"], "5");
    assert_eq!(posts[0]["message"]["content"], "later");

    for bad in [
        json!({"channel_id": "5", "send_at": 5, "message": {"content": "x"}}),
        json!({"channel_id": "5", "send_at": later + 400 * 24 * 3600, "message": {"content": "x"}}),
        json!({"channel_id": "5", "message": {"content": "x", "buttons": [{"label": "a", "role": "r"}]}}),
    ] {
        let response = send(&app.router, post_json(uri).body(&bad.to_string())).await;
        assert_eq!(response.status, StatusCode::BAD_REQUEST, "{bad}");
    }

    let removed = send(
        &app.router,
        call(
            Method::DELETE,
            &format!("/v1/guilds/42/scheduled-posts/{key}"),
        ),
    )
    .await;
    assert_eq!(removed.status, StatusCode::OK);
    assert_eq!(removed.json()["removed"], true);
    let again = send(
        &app.router,
        call(
            Method::DELETE,
            &format!("/v1/guilds/42/scheduled-posts/{key}"),
        ),
    )
    .await;
    assert_eq!(again.status, StatusCode::NOT_FOUND);
    let listed = send(
        &app.router,
        call(Method::GET, "/v1/guilds/42/scheduled-posts"),
    )
    .await;
    assert!(listed.json()["posts"].as_array().unwrap().is_empty());

    let edit = json!({"channel_id": "5", "edit": "123", "message": {"content": "new"}});
    let sent = send(&app.router, post_json(uri).body(&edit.to_string())).await;
    assert_eq!(sent.status, StatusCode::OK, "{}", sent.text);
}

#[sqlx::test(migrations = "../closure-db/migrations")]
async fn a_refused_send_says_why_and_a_bot_without_a_sink_is_unavailable(pool: PgPool) {
    let sink = Arc::new(FakeSink(
        Err(SendFailure::Forbidden),
        std::sync::Mutex::default(),
    ));
    let app = app_built(
        pool.clone(),
        GatewayStatus { ready: 1, total: 1 },
        true,
        Some(sink as Arc<dyn MessageSink>),
    );
    app.db.register_guild(Id::new(GUILD)).await.unwrap();
    let body = json!({"channel_id": "5", "message": {"content": "x"}}).to_string();
    let uri = "/v1/guilds/42/messages";
    assert_eq!(
        send(&app.router, post_json(uri).body(&body)).await.status,
        StatusCode::FORBIDDEN
    );
    let denied = send(
        &app.router,
        post_json(uri).header("x-closure-actor", "999").body(&body),
    )
    .await;
    assert_eq!(denied.status, StatusCode::FORBIDDEN, "{}", denied.text);
    let bare = app_built(pool, GatewayStatus { ready: 1, total: 1 }, true, None);
    bare.db.register_guild(Id::new(GUILD)).await.unwrap();
    assert_eq!(
        send(&bare.router, post_json(uri).body(&body)).await.status,
        StatusCode::SERVICE_UNAVAILABLE
    );
}

async fn make_case(app: &App, kind: &str, target: u64, reason: Option<&str>) {
    closure_core::cases::CaseBackend::create(
        &app.db,
        &ModuleId::new("moderation").unwrap(),
        Id::new(GUILD),
        closure_core::cases::NewCase {
            source: closure_core::cases::CaseSource::Command,
            kind: closure_core::cases::CaseKind::new(kind).unwrap(),
            target: Id::new(target),
            moderator: Id::new(9),
            reason: reason.map(str::to_owned),
            expires_at: None,
        },
    )
    .await
    .unwrap();
}

#[sqlx::test(migrations = "../closure-db/migrations")]
async fn the_cases_of_a_server_are_listed_filtered_shown_counted_and_exported(pool: PgPool) {
    let app = app(pool).await;
    app.db.register_guild(Id::new(GUILD)).await.unwrap();
    make_case(&app, "warn", 10, Some("spam")).await;
    make_case(&app, "ban", 11, Some("=cmd|' /C calc'!A0")).await;
    make_case(&app, "warn", 10, None).await;
    let base = "/v1/guilds/42/cases";

    let page = send(&app.router, call(Method::GET, &format!("{base}?limit=2"))).await;
    assert_eq!(page.status, StatusCode::OK, "{}", page.text);
    let json = page.json();
    let numbers: Vec<i64> = json["cases"]
        .as_array()
        .unwrap()
        .iter()
        .map(|case| case["number"].as_i64().unwrap())
        .collect();
    assert_eq!(numbers, [3, 2]);
    assert_eq!(json["next_before"], 2);
    assert_eq!(json["cases"][0]["target"], "10", "ids are texts");
    let rest = send(
        &app.router,
        call(Method::GET, &format!("{base}?limit=2&before=2")),
    )
    .await;
    assert_eq!(rest.json()["cases"][0]["number"], 1);
    assert!(rest.json()["next_before"].is_null());

    for (query, expected) in [
        ("user=10", 2),
        ("kind=ban", 1),
        ("text=SPAM", 1),
        ("state=active", 3),
        ("state=revoked", 0),
        ("moderator=9", 3),
        ("source=command", 3),
        ("source=appeal", 0),
    ] {
        let found = send(&app.router, call(Method::GET, &format!("{base}?{query}"))).await;
        assert_eq!(found.status, StatusCode::OK, "{query}: {}", found.text);
        assert_eq!(
            found.json()["cases"].as_array().unwrap().len(),
            expected,
            "{query}"
        );
    }
    for bad in ["user=x", "state=gone", "source=nowhere", "kind=NOT%20OK!"] {
        let response = send(&app.router, call(Method::GET, &format!("{base}?{bad}"))).await;
        assert_eq!(response.status, StatusCode::BAD_REQUEST, "{bad}");
    }

    let one = send(&app.router, call(Method::GET, &format!("{base}/2"))).await;
    assert_eq!(one.json()["kind"], "ban");
    assert_eq!(one.json()["state"], "active");
    assert_eq!(
        send(&app.router, call(Method::GET, &format!("{base}/99")))
            .await
            .status,
        StatusCode::NOT_FOUND
    );

    let summary = send(
        &app.router,
        call(Method::GET, &format!("{base}/summary?days=7")),
    )
    .await;
    assert_eq!(summary.status, StatusCode::OK, "{}", summary.text);
    assert_eq!(summary.json()["total"], 3);
    assert_eq!(summary.json()["by_kind"]["warn"], 2);
    assert_eq!(summary.json()["targets"][0]["id"], "10");
    assert_eq!(
        send(
            &app.router,
            call(Method::GET, &format!("{base}/summary?days=0"))
        )
        .await
        .status,
        StatusCode::BAD_REQUEST
    );

    let csv = send(&app.router, call(Method::GET, &format!("{base}/export"))).await;
    assert_eq!(csv.status, StatusCode::OK);
    assert!(
        csv.headers[header::CONTENT_TYPE]
            .to_str()
            .unwrap()
            .starts_with("text/csv")
    );
    assert_eq!(csv.text.lines().count(), 4, "{}", csv.text);
    assert!(csv.text.contains("\"'=cmd|"), "{}", csv.text);
}

#[sqlx::test(migrations = "../closure-db/migrations")]
async fn a_person_needs_to_be_a_moderator_to_read_and_the_dashboard_right_to_change_cases(
    pool: PgPool,
) {
    let app = app(pool).await;
    app.db.register_guild(Id::new(GUILD)).await.unwrap();
    make_case(&app, "warn", 10, Some("spam")).await;
    let base = "/v1/guilds/42/cases";

    let denied = send(
        &app.router,
        call(Method::GET, base).header("x-closure-actor", "3"),
    )
    .await;
    assert_eq!(denied.status, StatusCode::FORBIDDEN, "{}", denied.text);
    let owner = |call: Call| call.header("x-closure-actor", "123");
    assert_eq!(
        send(&app.router, owner(call(Method::GET, base)))
            .await
            .status,
        StatusCode::OK
    );

    let note = r#"{"target":"10","reason":"asked twice"}"#;
    let anonymous = send(&app.router, post_json(base).body(note)).await;
    assert_eq!(
        anonymous.status,
        StatusCode::BAD_REQUEST,
        "{}",
        anonymous.text
    );
    let made = send(&app.router, owner(post_json(base)).body(note)).await;
    assert_eq!(made.status, StatusCode::OK, "{}", made.text);
    assert_eq!(made.json()["kind"], "note");
    assert_eq!(made.json()["source"], "dashboard");
    assert_eq!(made.json()["moderator"], "123");
    let empty = send(
        &app.router,
        owner(post_json(base)).body(r#"{"target":"10","reason":" "}"#),
    )
    .await;
    assert_eq!(empty.status, StatusCode::BAD_REQUEST);

    let patch = |body: &str| {
        owner(call(Method::PATCH, &format!("{base}/1")).header("content-type", "application/json"))
            .body(body)
    };
    let edited = send(&app.router, patch(r#"{"reason":"spam, twice"}"#)).await;
    assert_eq!(edited.json()["reason"], "spam, twice");
    let hidden = send(&app.router, patch(r#"{"hidden":true}"#)).await;
    assert_eq!(hidden.json()["hidden"], true);
    let listed = send(&app.router, owner(call(Method::GET, base))).await;
    assert_eq!(
        listed.json()["cases"].as_array().unwrap().len(),
        1,
        "the hidden case is left out"
    );
    let all = send(
        &app.router,
        owner(call(Method::GET, &format!("{base}?hidden=true"))),
    )
    .await;
    assert_eq!(all.json()["cases"].as_array().unwrap().len(), 2);
    assert_eq!(
        send(&app.router, patch("{}")).await.status,
        StatusCode::BAD_REQUEST,
        "a change says what changes"
    );
}

fn ticket_store(app: &App, guild: u64) -> Store {
    let granted: CapabilitySet = [Capability::Storage].into_iter().collect();
    let backend: Arc<dyn closure_core::storage::StorageBackend> = Arc::new(app.db.clone());
    Store::new(
        backend,
        &granted,
        ModuleId::new("tickets").unwrap(),
        Id::new(guild),
    )
}

async fn put_ticket_record(app: &App, guild: u64, key: &str, value: Value) {
    ticket_store(app, guild).set(key, &value).await.unwrap();
}

fn history_key(number: i64, channel: u64) -> String {
    format!("ticket-history:{:019}:{channel}", i64::MAX - number)
}

async fn put_history(
    app: &App,
    guild: u64,
    number: i64,
    channel: u64,
    staff_roles: Vec<u64>,
    transcript_pages: usize,
) {
    let key = history_key(number, channel);
    put_ticket_record(
        app,
        guild,
        &key,
        json!({
            "channel_id": channel,
            "number": number,
            "opener": 10,
            "claimed_by": 2,
            "created": number * 10,
            "closed": number * 10 + 5,
            "closer": 2,
            "reason": "resolved",
            "panel": { "id": "support-a", "staff_roles": staff_roles },
            "staff_roles": staff_roles,
            "status": "closed",
            "attempt": 500,
            "transcript_attempt": 700,
            "transcript_pages": transcript_pages,
            "transcript_truncated": true
        }),
    )
    .await;
    put_ticket_record(
        app,
        guild,
        &format!("ticket-history-id:{channel}"),
        json!(key),
    )
    .await;
}

#[sqlx::test(migrations = "../closure-db/migrations")]
async fn ticket_api_pages_open_tickets_and_scopes_access_to_guild_and_panel(pool: PgPool) {
    let app = app(pool).await;
    app.db.register_guild(Id::new(43)).await.unwrap();
    for (channel, roles) in [
        (101, vec![5]),
        (102, vec![5]),
        (103, vec![5]),
        (104, vec![6]),
    ] {
        put_ticket_record(
            &app,
            42,
            &format!("ticket:{channel}"),
            json!({
                "number": channel - 100,
                "opener": 10,
                "claimed_by": null,
                "created": i64::from(channel),
                "closing": if channel == 103 { Some(i64::from(channel)) } else { None },
                "staff_roles": roles,
                "panel": { "id": "support-a" }
            }),
        )
        .await;
    }
    put_ticket_record(
        &app,
        43,
        "ticket:105",
        json!({
            "number": 5,
            "opener": 10,
            "created": 105,
            "staff_roles": [5]
        }),
    )
    .await;

    let staff = |request: Call| request.header("x-closure-actor", "2");
    let base = "/v1/guilds/42/tickets?state=open&limit=2";
    let first = send(&app.router, staff(call(Method::GET, base))).await;
    assert_eq!(first.status, StatusCode::OK, "{}", first.text);
    assert_eq!(first.json()["tickets"][0]["channel_id"], "103");
    assert_eq!(first.json()["tickets"][1]["channel_id"], "102");
    assert_eq!(first.json()["tickets"][0]["panel"], "support-a");
    assert_eq!(first.json()["tickets"][0]["state"], "closing");
    assert_eq!(first.json()["next_before"], "102");

    let next = send(
        &app.router,
        staff(call(
            Method::GET,
            "/v1/guilds/42/tickets?state=open&limit=2&before=102",
        )),
    )
    .await;
    assert_eq!(next.json()["tickets"][0]["channel_id"], "101");
    assert!(next.json()["next_before"].is_null());

    put_ticket_record(
        &app,
        42,
        "ticket:109",
        json!({
            "number": 9,
            "opener": 10,
            "created": 109,
            "panel": { "id": "legacy-panel", "staff_roles": ["5"] }
        }),
    )
    .await;
    put_ticket_record(
        &app,
        42,
        "ticket:110",
        json!({
            "number": 10,
            "opener": 10,
            "created": 110,
            "panel": null
        }),
    )
    .await;
    put_ticket_record(
        &app,
        42,
        "ticket:108",
        json!({
            "number": 8,
            "opener": 10,
            "created": 108,
            "staff_roles": [],
            "panel": { "id": "legacy-panel", "staff_roles": ["5"] }
        }),
    )
    .await;

    let legacy_panel_ticket = send(
        &app.router,
        staff(call(Method::GET, "/v1/guilds/42/tickets/109")),
    )
    .await;
    assert_eq!(legacy_panel_ticket.status, StatusCode::OK);
    assert_eq!(legacy_panel_ticket.json()["panel"], "legacy-panel");
    let legacy_default_ticket = send(
        &app.router,
        staff(call(Method::GET, "/v1/guilds/42/tickets/110")),
    )
    .await;
    assert_eq!(legacy_default_ticket.status, StatusCode::OK);
    let explicit_empty_roles = send(
        &app.router,
        staff(call(Method::GET, "/v1/guilds/42/tickets/108")),
    )
    .await;
    assert_eq!(explicit_empty_roles.status, StatusCode::NOT_FOUND);

    let hidden_panel = send(
        &app.router,
        staff(call(Method::GET, "/v1/guilds/42/tickets/104")),
    )
    .await;
    assert_eq!(hidden_panel.status, StatusCode::NOT_FOUND);
    let foreign_guild = send(
        &app.router,
        staff(call(Method::GET, "/v1/guilds/42/tickets/105")),
    )
    .await;
    assert_eq!(foreign_guild.status, StatusCode::NOT_FOUND);

    let member_without_ticket_access = send(
        &app.router,
        call(Method::GET, "/v1/guilds/42/tickets?state=open").header("x-closure-actor", "3"),
    )
    .await;
    assert_eq!(member_without_ticket_access.status, StatusCode::OK);
    assert!(
        member_without_ticket_access.json()["tickets"]
            .as_array()
            .unwrap()
            .is_empty()
    );

    let forged_actor_without_admin_token = send(
        &app.router,
        call(Method::GET, "/v1/guilds/42/tickets?state=open")
            .token(None)
            .header("x-closure-actor", "123"),
    )
    .await;
    assert_eq!(
        forged_actor_without_admin_token.status,
        StatusCode::UNAUTHORIZED
    );
}

#[sqlx::test(migrations = "../closure-db/migrations")]
async fn ticket_history_is_paged_and_each_detail_page_checks_panel_access(pool: PgPool) {
    let app = app(pool).await;
    for (number, channel, roles, pages) in [
        (18, 180, vec![5], 1),
        (19, 190, vec![5], 0),
        (20, 200, vec![5], 6),
        (21, 210, vec![6], 0),
    ] {
        put_history(&app, 42, number, channel, roles, pages).await;
    }
    let store = ticket_store(&app, 42);
    for page in 0..6 {
        store
            .set(
                &format!("ticket-history-content:200:700:{page:04}"),
                &format!("transcript chunk {page}"),
            )
            .await
            .unwrap();
    }
    put_history(&app, 42, 17, 170, vec![5], 1).await;

    let staff = |request: Call| request.header("x-closure-actor", "2");
    let first = send(
        &app.router,
        staff(call(
            Method::GET,
            "/v1/guilds/42/tickets?state=closed&limit=2",
        )),
    )
    .await;
    assert_eq!(first.status, StatusCode::OK, "{}", first.text);
    assert_eq!(first.json()["tickets"][0]["number"], 20);
    assert_eq!(first.json()["tickets"][1]["number"], 19);
    assert!(first.json()["tickets"][0].get("transcript").is_none());
    assert_eq!(first.json()["next_before"], "19");

    let next = send(
        &app.router,
        staff(call(
            Method::GET,
            "/v1/guilds/42/tickets?state=closed&limit=1&before=19",
        )),
    )
    .await;
    assert_eq!(next.json()["tickets"][0]["number"], 18);
    assert_eq!(next.json()["next_before"], "18");

    let detail = send(
        &app.router,
        staff(call(Method::GET, "/v1/guilds/42/tickets/200")),
    )
    .await;
    assert_eq!(detail.status, StatusCode::OK, "{}", detail.text);
    assert_eq!(detail.json()["panel"], "support-a");
    assert_eq!(detail.json()["opener"], "10");
    assert_eq!(
        detail.json()["transcript"]["pages"]
            .as_array()
            .unwrap()
            .len(),
        4
    );
    assert_eq!(
        detail.json()["transcript"]["pages"][0]["text"],
        "transcript chunk 0"
    );
    assert_eq!(detail.json()["transcript"]["next_page"], 4);
    assert_eq!(detail.json()["transcript"]["truncated"], true);

    let rest = send(
        &app.router,
        staff(call(
            Method::GET,
            "/v1/guilds/42/tickets/200?start=4&limit=4",
        )),
    )
    .await;
    assert_eq!(
        rest.json()["transcript"]["pages"].as_array().unwrap().len(),
        2
    );
    assert!(rest.json()["transcript"]["next_page"].is_null());

    let other_panel = send(
        &app.router,
        staff(call(Method::GET, "/v1/guilds/42/tickets/210")),
    )
    .await;
    assert_eq!(other_panel.status, StatusCode::NOT_FOUND);

    let missing_chunk = send(
        &app.router,
        staff(call(Method::GET, "/v1/guilds/42/tickets/170")),
    )
    .await;
    assert_eq!(missing_chunk.status, StatusCode::NOT_FOUND);

    let denied_transcript_page = send(
        &app.router,
        call(Method::GET, "/v1/guilds/42/tickets/200?start=4&limit=4")
            .header("x-closure-actor", "3"),
    )
    .await;
    assert_eq!(denied_transcript_page.status, StatusCode::NOT_FOUND);

    let out_of_range = send(
        &app.router,
        staff(call(
            Method::GET,
            "/v1/guilds/42/tickets/200?start=7&limit=4",
        )),
    )
    .await;
    assert_eq!(out_of_range.status, StatusCode::BAD_REQUEST);
}
