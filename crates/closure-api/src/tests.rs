use std::sync::Arc;

use axum::Router;
use axum::body::Body;
use axum::http::{HeaderMap, Method, Request, StatusCode, header};
use closure_config::{ConfigCompiler, SettingsStore};
use closure_core::module::{Module, ModuleId, ModuleManifest};
use closure_core::registry::ModuleRegistry;
use closure_core::settings::SettingsProvider;
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

fn app_with(pool: PgPool, gateway: GatewayStatus) -> App {
    let registry = Arc::new(ModuleRegistry::new(vec![warn_module()]).unwrap());
    let store = Arc::new(SettingsStore::new(ConfigCompiler::new(registry).unwrap()));
    let db = Db::from_pool(pool);
    let state = ApiState::new(
        Arc::clone(&store),
        db.clone(),
        Arc::new(FixedReadiness(gateway)),
        || "closure_up 1\n".to_owned(),
        &SecretString::from(TOKEN),
    )
    .unwrap();
    App {
        router: router(state),
        db,
        store,
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

fn put(guild: u64, if_match: &str, body: &str) -> Call {
    call(Method::PUT, &format!("/v1/guilds/{guild}/config"))
        .header("if-match", if_match)
        .body(body)
}

const GUILD: u64 = 42;

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
async fn weak_admin_tokens_are_refused_at_setup(pool: PgPool) {
    let registry = Arc::new(ModuleRegistry::new(vec![]).unwrap());
    let store = Arc::new(SettingsStore::new(ConfigCompiler::new(registry).unwrap()));
    let result = ApiState::new(
        store,
        Db::from_pool(pool),
        Arc::new(FixedReadiness(GatewayStatus { ready: 0, total: 0 })),
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
        schema["properties"]["modules"]["properties"]["warn"]["properties"]["config"]["anyOf"][0]["properties"]
            ["limit"]["anyOf"][0]["maximum"],
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
