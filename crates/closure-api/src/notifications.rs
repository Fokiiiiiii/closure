use std::future::Future;
use std::pin::Pin;
use std::sync::OnceLock;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use axum::Json;
use axum::extract::{Query, State};
use axum::http::{HeaderMap, HeaderValue, header};
use axum::response::{IntoResponse, Response};
use closure_core::event::{GuildId, UserId};
use closure_core::integration_secrets::IntegrationSecretService;
use closure_core::locale::{Args, Locale as MessageLocale, Translator};
use closure_db::notifications::{
    ClaimedNotificationEvent, EmailRequestOutcome, EmailVerificationOutcome,
    EmailVerificationRequest, NotificationOutboxChannel, StoredNotificationPreferences,
};
use closure_i18n::{CORE_NAMESPACE, FluentTranslator, TranslatorBuilder};
use lettre::{
    AsyncSmtpTransport, AsyncTransport, Message, Tokio1Executor,
    message::{Mailbox, header::ContentType},
    transport::smtp::authentication::Credentials,
};
use ring::digest::{SHA256, digest};
use ring::rand::{SecureRandom, SystemRandom};
use secrecy::{ExposeSecret, SecretString};
use serde::Deserialize;
use serde_json::{Value, json};
use tokio::task::JoinHandle;
use tokio::time::{MissedTickBehavior, interval};
use tokio_util::sync::CancellationToken;

use crate::ApiState;
use crate::access::MemberAccess;
use crate::config;
use crate::error::ApiError;
use closure_db::{Db, DbError};

const PAGE_SIZE: usize = 25;
const VERIFICATION_TTL_SECONDS: i64 = 15 * 60;
const SMTP_TIMEOUT: Duration = Duration::from_secs(10);
const OUTBOX_LEASE_SECONDS: i64 = 120;
const OUTBOX_MAX_ATTEMPTS: i32 = 10;
const OUTBOX_NOT_READY_DELAY_SECONDS: i64 = 300;
const OUTBOX_POLL: Duration = Duration::from_secs(2);
const OUTBOX_BATCH: usize = 16;
const OUTBOX_PURGE_INTERVAL: Duration = Duration::from_hours(6);
const EMAIL_SECRET_NAMESPACE: &str = "ticket_notifications";
const EMAIL_SECRET_FIELD: &str = "email";
const EMAIL_TOKEN_NAMESPACE: &str = "ticket_notifications_email";
const EMAIL_TOKEN_DOMAIN: &[u8] = b"verification:v1:";
const EMAIL_PAYLOAD_NAMESPACE: &str = "ticket_notifications_email_payload";
const EMAIL_PAYLOAD_FIELD: &str = "payload";

#[derive(Debug, Clone, Copy)]
pub enum DirectMessageError {
    Retryable,
    Permanent,
}

pub trait DirectMessageSink: Send + Sync {
    fn send(
        &self,
        guild: GuildId,
        recipient: UserId,
        content: String,
    ) -> Pin<Box<dyn Future<Output = Result<(), DirectMessageError>> + Send + '_>>;
}

struct EmailSender {
    transport: AsyncSmtpTransport<Tokio1Executor>,
    from: Mailbox,
}

impl EmailSender {
    fn from_env() -> Option<Self> {
        let host = std::env::var("TICKETS_EMAIL_SMTP_HOST").ok()?;
        let username = std::env::var("TICKETS_EMAIL_SMTP_USERNAME").ok()?;
        let password = std::env::var("TICKETS_EMAIL_SMTP_PASSWORD").ok()?;
        let from = std::env::var("TICKETS_EMAIL_FROM").ok()?;
        if host.is_empty()
            || host.len() > 253
            || host
                .bytes()
                .any(|byte| byte.is_ascii_whitespace() || byte.is_ascii_control())
            || host.contains('/')
            || host.contains('@')
            || username.is_empty()
            || username.chars().any(char::is_control)
            || password.is_empty()
            || password.chars().any(char::is_control)
        {
            return None;
        }
        let port = match std::env::var("TICKETS_EMAIL_SMTP_PORT") {
            Ok(value) => value.parse::<u16>().ok().filter(|port| *port != 0)?,
            Err(_) => 587,
        };
        let from = parse_mailbox(&from).ok()?;
        let credentials = Credentials::new(username, password);
        let builder = if port == 465 {
            AsyncSmtpTransport::<Tokio1Executor>::relay(&host).ok()?
        } else {
            AsyncSmtpTransport::<Tokio1Executor>::starttls_relay(&host)
                .ok()?
                .port(port)
        };
        let transport = builder
            .credentials(credentials)
            .timeout(Some(SMTP_TIMEOUT))
            .build();
        Some(Self { transport, from })
    }

    // A Message-ID derived from the delivery identity lets the relay and the recipient
    // recognise a resend of the same notification.
    fn message_id(&self, local_part: &str) -> String {
        delivery_message_id(&self.from, local_part)
    }

    async fn send(
        &self,
        recipient: Mailbox,
        subject: &str,
        body: &str,
        message_id: Option<String>,
    ) -> Result<(), ()> {
        let message = outbound_message(&self.from, recipient, subject, body, message_id)?;
        self.transport
            .send(message)
            .await
            .map(|_| ())
            .map_err(|_| ())
    }
}

fn delivery_message_id(from: &Mailbox, local_part: &str) -> String {
    format!("<{local_part}@{}>", from.email.domain())
}

// Hashed without the account, so the per-address limit counts mails across accounts.
fn recipient_hash(email: &str) -> Vec<u8> {
    digest(
        &SHA256,
        format!("recipient:v1:{}", email.to_ascii_lowercase()).as_bytes(),
    )
    .as_ref()
    .to_vec()
}

fn seal_payload(
    vault: &IntegrationSecretService,
    actor: UserId,
    email: &str,
    code: &str,
) -> Result<Vec<u8>, ApiError> {
    let cleartext = serde_json::to_string(&json!({"address": email, "code": code }))
        .map_err(|_| ApiError::Internal("email verification is unavailable".into()))?;
    vault
        .seal_account_secret(
            actor,
            actor,
            EMAIL_PAYLOAD_NAMESPACE,
            EMAIL_PAYLOAD_FIELD,
            SecretString::from(cleartext),
        )
        .map_err(|_| ApiError::Internal("email verification is unavailable".into()))
}

fn open_payload(
    vault: &IntegrationSecretService,
    actor: UserId,
    payload: &[u8],
) -> Result<(String, String), ApiError> {
    let unavailable = || ApiError::Internal("email verification is unavailable".into());
    let secret = vault
        .open_account_secret(
            actor,
            actor,
            EMAIL_PAYLOAD_NAMESPACE,
            EMAIL_PAYLOAD_FIELD,
            payload,
        )
        .map_err(|_| unavailable())?;
    let value: Value = serde_json::from_str(secret.expose_secret()).map_err(|_| unavailable())?;
    let address = value["address"]
        .as_str()
        .ok_or_else(unavailable)?
        .to_owned();
    let code = value["code"].as_str().ok_or_else(unavailable)?.to_owned();
    Ok((address, code))
}

fn quota_message(locale: MessageLocale) -> String {
    translated(
        locale,
        "ticket-notification-email-quota-exceeded",
        &Args::new(),
    )
    .unwrap_or_else(|| match locale {
        MessageLocale::Ja => {
            "確認メールの送信回数が上限に達しました。しばらく待ってから再度お試しください。"
                .to_owned()
        }
        _ => "Too many verification emails were requested recently. Try again later.".to_owned(),
    })
}

fn parse_mailbox(raw: &str) -> Result<Mailbox, ()> {
    let email = raw.trim();
    if email.is_empty()
        || email.len() > 254
        || email
            .bytes()
            .any(|byte| byte.is_ascii_whitespace() || byte.is_ascii_control())
        || email.matches('@').count() != 1
    {
        return Err(());
    }
    email.parse::<Mailbox>().map_err(|_| ())
}

fn outbound_message(
    from: &Mailbox,
    recipient: Mailbox,
    subject: &str,
    body: &str,
    message_id: Option<String>,
) -> Result<Message, ()> {
    Message::builder()
        .from(from.clone())
        .to(recipient)
        .subject(subject)
        .message_id(message_id)
        .header(ContentType::TEXT_PLAIN)
        .body(body.to_owned())
        .map_err(|_| ())
}

fn now_unix() -> Result<i64, ApiError> {
    let seconds = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_err(|_| ApiError::Internal("the system clock is unavailable".into()))?
        .as_secs();
    i64::try_from(seconds).map_err(|_| ApiError::Internal("the system clock is unavailable".into()))
}

fn verification_code() -> Result<String, ApiError> {
    let rng = SystemRandom::new();
    let limit = (u32::MAX / 1_000_000) * 1_000_000;
    loop {
        let mut bytes = [0_u8; 4];
        rng.fill(&mut bytes)
            .map_err(|_| ApiError::Internal("secure code generation failed".into()))?;
        let value = u32::from_be_bytes(bytes);
        if value < limit {
            return Ok(format!("{:06}", value % 1_000_000));
        }
    }
}

fn verification_message(user: u64, expires_at: i64, code: &str) -> Vec<u8> {
    let mut message = EMAIL_TOKEN_DOMAIN.to_vec();
    message.extend_from_slice(&user.to_be_bytes());
    message.extend_from_slice(&expires_at.to_be_bytes());
    message.extend_from_slice(code.as_bytes());
    message
}

fn request_fingerprint(email: &str) -> Vec<u8> {
    let mut message = b"request:v1:".to_vec();
    message.extend_from_slice(email.as_bytes());
    message
}

fn saved_locale(value: &str) -> MessageLocale {
    match value {
        "ja" => MessageLocale::Ja,
        _ => MessageLocale::En,
    }
}

fn request_locale(headers: &HeaderMap) -> Option<MessageLocale> {
    match headers
        .get("x-closure-locale")
        .and_then(|value| value.to_str().ok())
    {
        Some("ja") => Some(MessageLocale::Ja),
        Some("en") => Some(MessageLocale::En),
        _ => None,
    }
}

fn locale_tag(locale: MessageLocale) -> &'static str {
    match locale {
        MessageLocale::Ja => "ja",
        _ => "en",
    }
}

fn translated(locale: MessageLocale, key: &str, args: &Args) -> Option<String> {
    static TRANSLATOR: OnceLock<Option<FluentTranslator>> = OnceLock::new();
    TRANSLATOR
        .get_or_init(|| TranslatorBuilder::new().ok().map(TranslatorBuilder::build))
        .as_ref()
        .map(|translator| translator.translate(locale, CORE_NAMESPACE, key, args))
}

fn localized_notification(
    locale: MessageLocale,
    event_type: &str,
    submission_title: &str,
    reason: Option<&str>,
) -> Option<(String, String)> {
    let (title_key, approved) = match event_type {
        "gallery.approved" => ("ticket-notification-gallery-approved", true),
        "gallery.rejected" => ("ticket-notification-gallery-rejected", false),
        _ => return None,
    };
    let title_args = Args::new().with("title", submission_title.to_owned());
    let title =
        translated(locale, title_key, &title_args).unwrap_or_else(|| match (locale, approved) {
            (MessageLocale::Ja, true) => {
                format!("ギャラリーへの投稿「{submission_title}」が承認されました")
            }
            (MessageLocale::Ja, false) => {
                format!("ギャラリーへの投稿「{submission_title}」が却下されました")
            }
            (_, true) => format!("Your gallery submission “{submission_title}” was approved"),
            (_, false) => format!("Your gallery submission “{submission_title}” was rejected"),
        });
    let body = reason
        .filter(|reason| !reason.is_empty())
        .map(|reason| {
            let args = Args::new().with("reason", reason.to_owned());
            translated(locale, "ticket-notification-review-reason", &args).unwrap_or_else(|| {
                match locale {
                    MessageLocale::Ja => format!("審査理由: {reason}"),
                    _ => format!("Review reason: {reason}"),
                }
            })
        })
        .unwrap_or_default();
    Some((title, body))
}

fn verification_email(locale: MessageLocale, code: &str) -> (String, String) {
    let subject = translated(
        locale,
        "ticket-notification-email-verification-subject",
        &Args::new(),
    )
    .unwrap_or_else(|| match locale {
        MessageLocale::Ja => "メールアドレスを確認してください".to_owned(),
        _ => "Verify your email address".to_owned(),
    });
    let args = Args::new().with("code", code.to_owned());
    let body = translated(locale, "ticket-notification-email-verification-body", &args)
        .unwrap_or_else(|| match locale {
            MessageLocale::Ja => format!("確認コードは {code} です。有効期限は15分です。"),
            _ => format!("Your verification code is {code}. It expires in 15 minutes."),
        });
    (subject, body)
}

fn masked_email(email: &str) -> Option<String> {
    let (local, domain) = email.split_once('@')?;
    let first = local.chars().next()?;
    Some(format!("{first}***@{domain}"))
}

fn no_store(mut response: Response) -> Response {
    response
        .headers_mut()
        .insert(header::CACHE_CONTROL, HeaderValue::from_static("no-store"));
    response
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct NotificationsQuery {
    cursor: Option<String>,
}

fn cursor(value: Option<&str>) -> Result<Option<i64>, ApiError> {
    let Some(value) = value else {
        return Ok(None);
    };
    if value.is_empty() || value.len() > 19 || !value.bytes().all(|byte| byte.is_ascii_digit()) {
        return Err(ApiError::BadRequest("invalid notification cursor".into()));
    }
    value
        .parse::<i64>()
        .ok()
        .filter(|id| *id > 0)
        .map(Some)
        .ok_or_else(|| ApiError::BadRequest("invalid notification cursor".into()))
}

pub(crate) async fn list(
    State(state): State<ApiState>,
    headers: HeaderMap,
    Query(query): Query<NotificationsQuery>,
) -> Result<Response, ApiError> {
    let actor = config::actor(&headers)?
        .ok_or_else(|| ApiError::Forbidden("a verified dashboard actor is required".into()))?;
    if let Some(locale) = request_locale(&headers) {
        state
            .inner
            .db
            .set_notification_locale(actor.get(), locale_tag(locale))
            .await
            .map_err(map_db_error)?;
    }
    let page_cursor = cursor(query.cursor.as_deref())?;
    let page = state
        .inner
        .db
        .notification_page(actor.get(), page_cursor, PAGE_SIZE)
        .await
        .map_err(map_db_error)?;
    let email_available = state.inner.account_vault.is_some() && EmailSender::from_env().is_some();
    let preferences = page.preferences;
    let locale = saved_locale(&preferences.locale);
    let notifications = page
        .notifications
        .into_iter()
        .map(|item| {
            let submission_title = safe_notification_text(&item.submission_title, 100);
            let reason = item
                .reason
                .as_deref()
                .map(|reason| safe_notification_text(reason, 500));
            let (title, body) = localized_notification(
                locale,
                &item.event_type,
                &submission_title,
                reason.as_deref(),
            )
            .unwrap_or((submission_title, reason.unwrap_or_default()));
            json!({
                "id": item.id,
                "category": item.category,
                "event_type": item.event_type,
                "title": title,
                "body": body,
                "href": item.href,
                "source_guild_id": item.source_guild_id,
                "created_at": item.created_at,
                "created_at_iso": item.created_at_iso,
                "read_at": item.read_at
            })
        })
        .collect::<Vec<_>>();
    let response = Json(json!({
        "notifications": notifications,
        "unread_count": page.unread_count,
        "next_cursor": page.next_cursor.map(|id| id.to_string()),
        "preferences": {
            "in_app": preferences.in_app_enabled,
            "discord_dm": preferences.discord_dm_enabled,
            "email": preferences.email_enabled && email_available && preferences.email_verified
        },
        "email": {
            "available": email_available,
            "verified": preferences.email_verified,
            "pending": preferences.pending_email_ciphertext.is_some(),
            "address_masked": preferences.email_masked,
            "pending_address_masked": preferences.pending_email_masked
        },
        "revision": preferences.revision
    }))
    .into_response();
    Ok(no_store(config::with_etag(response, preferences.revision)))
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct NotificationAction {
    action: String,
    #[serde(default)]
    data: Value,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct ReadNotification {
    id: i64,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct SavePreferences {
    in_app: bool,
    discord_dm: bool,
    email: bool,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct RequestEmail {
    email: Option<String>,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct VerifyEmail {
    code: String,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct RemoveEmail {
    confirm: bool,
}

#[allow(clippy::too_many_lines)]
pub(crate) async fn action(
    State(state): State<ApiState>,
    headers: HeaderMap,
    Json(request): Json<NotificationAction>,
) -> Result<Response, ApiError> {
    let actor = config::actor(&headers)?
        .ok_or_else(|| ApiError::Forbidden("a verified dashboard actor is required".into()))?;
    let key = idempotency_key(&headers)?;
    let expected = config::expected_revision(&headers)?.unwrap_or(0);
    let user = actor.get();
    let now = now_unix()?;

    let (revision, result) = match request.action.as_str() {
        "mark_read" => {
            let data: ReadNotification = serde_json::from_value(request.data)
                .map_err(|_| ApiError::BadRequest("invalid notification action".into()))?;
            let Some(revision) = state
                .inner
                .db
                .mark_notification_read(user, expected, data.id)
                .await
                .map_err(map_db_error)?
            else {
                return Err(ApiError::NotFound("notification not found".into()));
            };
            (revision, json!({"marked_read": true}))
        }
        "mark_all_read" => {
            let revision = state
                .inner
                .db
                .mark_all_notifications_read(user, expected)
                .await
                .map_err(map_db_error)?;
            (revision, json!({"marked_all_read": true}))
        }
        "save_preferences" => {
            let data: SavePreferences = serde_json::from_value(request.data)
                .map_err(|_| ApiError::BadRequest("invalid notification preferences".into()))?;
            let email_available =
                state.inner.account_vault.is_some() && EmailSender::from_env().is_some();
            if data.email && !email_available {
                return Err(ApiError::BadRequest(
                    "email notifications are unavailable until SMTP and the account vault are configured".into(),
                ));
            }
            let revision = state
                .inner
                .db
                .save_notification_preferences(
                    user,
                    expected,
                    data.in_app,
                    data.discord_dm,
                    data.email,
                )
                .await
                .map_err(map_db_error)?;
            (revision, json!({"preferences_saved": true}))
        }
        "request_email_verification" => {
            let data: RequestEmail = serde_json::from_value(request.data)
                .map_err(|_| ApiError::BadRequest("invalid email address".into()))?;
            request_email_verification(&state, actor, &key, expected, now, data).await?
        }
        "verify_email" => {
            let data: VerifyEmail = serde_json::from_value(request.data)
                .map_err(|_| ApiError::BadRequest("invalid verification code".into()))?;
            if data.code.len() != 6 || !data.code.bytes().all(|byte| byte.is_ascii_digit()) {
                return Err(ApiError::BadRequest(
                    "invalid or expired verification code".into(),
                ));
            }
            let vault =
                state.inner.account_vault.as_ref().ok_or_else(|| {
                    ApiError::BadRequest("email verification is unavailable".into())
                })?;
            let preferences = state
                .inner
                .db
                .notification_preferences(user)
                .await
                .map_err(map_db_error)?;
            let expires_at = preferences.email_code_expires_at.ok_or_else(|| {
                ApiError::BadRequest("invalid or expired verification code".into())
            })?;
            let code_hash = vault
                .account_token_hmac(
                    actor,
                    actor,
                    EMAIL_TOKEN_NAMESPACE,
                    &verification_message(user, expires_at, &data.code),
                )
                .map_err(|_| ApiError::Internal("email verification is unavailable".into()))?;
            match state
                .inner
                .db
                .verify_notification_email(user, expected, &code_hash, now)
                .await
                .map_err(map_db_error)?
            {
                EmailVerificationOutcome::Verified { revision }
                | EmailVerificationOutcome::AlreadyVerified { revision } => {
                    (revision, json!({"email_verified": true}))
                }
                EmailVerificationOutcome::Invalid { .. }
                | EmailVerificationOutcome::Expired { .. } => {
                    return Err(ApiError::BadRequest(
                        "invalid or expired verification code".into(),
                    ));
                }
            }
        }
        "remove_email" => {
            let data: RemoveEmail = serde_json::from_value(request.data)
                .map_err(|_| ApiError::BadRequest("invalid email removal request".into()))?;
            if !data.confirm {
                return Err(ApiError::BadRequest("confirm email removal".into()));
            }
            let revision = state
                .inner
                .db
                .remove_notification_email(user, expected)
                .await
                .map_err(map_db_error)?;
            (revision, json!({"email_removed": true}))
        }
        _ => {
            return Err(ApiError::BadRequest(
                "unsupported notification action".into(),
            ));
        }
    };

    let mut response = Json(json!({"result": result})).into_response();
    response
        .headers_mut()
        .insert(header::CACHE_CONTROL, HeaderValue::from_static("no-store"));
    Ok(config::with_etag(response, revision))
}

// The address to verify: the one given now, or the one already pending for this account.
async fn resolve_verification_address(
    state: &ApiState,
    actor: UserId,
    data: RequestEmail,
) -> Result<(String, Vec<u8>, String), ApiError> {
    let user = actor.get();
    let vault = state
        .inner
        .account_vault
        .as_ref()
        .ok_or_else(|| ApiError::BadRequest("email verification is unavailable".into()))?;
    let (email, encrypted, masked) = if let Some(raw_email) = data.email {
        let email = normalize_email(&raw_email)?;
        let encrypted = vault
            .seal_account_secret(
                actor,
                actor,
                EMAIL_SECRET_NAMESPACE,
                EMAIL_SECRET_FIELD,
                SecretString::from(email.clone()),
            )
            .map_err(|_| ApiError::Internal("email verification is unavailable".into()))?;
        let masked = masked_email(&email)
            .ok_or_else(|| ApiError::BadRequest("enter a valid email address".into()))?;
        (email, encrypted, masked)
    } else {
        let preferences = state
            .inner
            .db
            .notification_preferences(user)
            .await
            .map_err(map_db_error)?;
        let encrypted = preferences
            .pending_email_ciphertext
            .clone()
            .ok_or_else(|| ApiError::BadRequest("there is no pending email address".into()))?;
        let masked = preferences
            .pending_email_masked
            .clone()
            .ok_or_else(|| ApiError::BadRequest("there is no pending email address".into()))?;
        let secret = vault
            .open_account_secret(
                actor,
                actor,
                EMAIL_SECRET_NAMESPACE,
                EMAIL_SECRET_FIELD,
                &encrypted,
            )
            .map_err(|_| ApiError::Internal("email verification is unavailable".into()))?;
        (normalize_email(secret.expose_secret())?, encrypted, masked)
    };
    Ok((email, encrypted, masked))
}

async fn request_email_verification(
    state: &ApiState,
    actor: UserId,
    key: &str,
    expected: i64,
    now: i64,
    data: RequestEmail,
) -> Result<(i64, Value), ApiError> {
    let user = actor.get();
    let unavailable = || ApiError::BadRequest("email verification is unavailable".into());
    let sender = EmailSender::from_env().ok_or_else(unavailable)?;
    let vault = state.inner.account_vault.as_ref().ok_or_else(unavailable)?;
    let locale_preferences = state
        .inner
        .db
        .notification_preferences(user)
        .await
        .map_err(map_db_error)?;
    let email_locale = saved_locale(&locale_preferences.locale);
    let (email, encrypted, masked) = resolve_verification_address(state, actor, data).await?;
    parse_mailbox(&email)
        .map_err(|()| ApiError::BadRequest("enter a valid email address".into()))?;
    let code = verification_code()?;
    let expires_at = now.saturating_add(VERIFICATION_TTL_SECONDS);
    let code_hash = vault
        .account_token_hmac(
            actor,
            actor,
            EMAIL_TOKEN_NAMESPACE,
            &verification_message(user, expires_at, &code),
        )
        .map_err(|_| ApiError::Internal("email verification is unavailable".into()))?;
    let fingerprint = vault
        .account_token_hmac(
            actor,
            actor,
            EMAIL_TOKEN_NAMESPACE,
            &request_fingerprint(&email),
        )
        .map_err(|_| ApiError::Internal("email verification is unavailable".into()))?;
    let payload = seal_payload(vault, actor, &email, &code)?;
    let outcome = state
        .inner
        .db
        .begin_email_verification(EmailVerificationRequest {
            user,
            expected_revision: expected,
            email_ciphertext: encrypted,
            email_masked: masked,
            code_hash: code_hash.to_vec(),
            code_expires_at: expires_at,
            request_key: key.to_owned(),
            request_fingerprint: fingerprint.to_vec(),
            payload_ciphertext: payload,
            recipient_hash: recipient_hash(&email),
            now,
        })
        .await
        .map_err(map_db_error)?;
    match outcome {
        EmailRequestOutcome::Replay { revision } => {
            Ok((revision, json!({"verification_sent": true})))
        }
        EmailRequestOutcome::Started { revision } => {
            let mailbox = parse_mailbox(&email)
                .map_err(|()| ApiError::BadRequest("enter a valid email address".into()))?;
            let (subject, body) = verification_email(email_locale, &code);
            send_verification(state, &sender, user, key, mailbox, &subject, &body).await?;
            Ok((revision, json!({"verification_sent": true})))
        }
        // The same request again after a stopped send: the stored code is sent, no new code is issued.
        EmailRequestOutcome::Resume { revision, payload } => {
            let (address, code) = open_payload(vault, actor, &payload)?;
            let mailbox = parse_mailbox(&address).map_err(|()| unavailable())?;
            let (subject, body) = verification_email(email_locale, &code);
            send_verification(state, &sender, user, key, mailbox, &subject, &body).await?;
            Ok((revision, json!({"verification_sent": true})))
        }
        EmailRequestOutcome::Cooldown {
            retry_after_seconds,
        } => Err(ApiError::Conflict(format!(
            "a verification code was recently sent; retry in {retry_after_seconds} seconds"
        ))),
        EmailRequestOutcome::IdempotencyConflict => Err(ApiError::Conflict(
            "Idempotency-Key was already used for another email address".into(),
        )),
        EmailRequestOutcome::QuotaExceeded { .. } => {
            Err(ApiError::Conflict(quota_message(email_locale)))
        }
    }
}

async fn send_verification(
    state: &ApiState,
    sender: &EmailSender,
    user: u64,
    key: &str,
    mailbox: Mailbox,
    subject: &str,
    body: &str,
) -> Result<(), ApiError> {
    // The Message-ID comes from the request key, so a resend of the same request reuses it.
    let message_id = sender.message_id(&format!("ticket-verification-{key}"));
    if sender
        .send(mailbox, subject, body, Some(message_id))
        .await
        .is_err()
    {
        // The key, cooldown and payload stay, so the same Idempotency-Key can resume the send.
        if state
            .inner
            .db
            .mark_email_verification_failed(user, key)
            .await
            .is_err()
        {
            tracing::warn!("email verification failed state was not recorded");
        }
        return Err(ApiError::Internal(
            "email verification delivery failed".into(),
        ));
    }
    if state
        .inner
        .db
        .mark_email_verification_sent(user, key)
        .await
        .is_err()
    {
        tracing::warn!("email verification sent state was not recorded");
    }
    Ok(())
}

fn idempotency_key(headers: &HeaderMap) -> Result<String, ApiError> {
    let key = headers
        .get("idempotency-key")
        .and_then(|value| value.to_str().ok())
        .filter(|key| {
            (1..=64).contains(&key.len())
                && key
                    .bytes()
                    .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'-' | b'_'))
        })
        .ok_or_else(|| ApiError::BadRequest("a valid Idempotency-Key is required".into()))?;
    Ok(key.to_owned())
}

fn normalize_email(value: &str) -> Result<String, ApiError> {
    let email = value.trim();
    if email.is_empty()
        || email.len() > 254
        || email
            .bytes()
            .any(|byte| byte.is_ascii_whitespace() || byte.is_ascii_control())
        || email.matches('@').count() != 1
    {
        return Err(ApiError::BadRequest("enter a valid email address".into()));
    }
    parse_mailbox(email)
        .map_err(|()| ApiError::BadRequest("enter a valid email address".into()))?;
    Ok(email.to_owned())
}

fn map_db_error(error: DbError) -> ApiError {
    match error {
        DbError::RevisionConflict { actual, .. } => ApiError::PreconditionFailed {
            current: actual.unwrap_or(0),
        },
        DbError::Invalid(message) if message.contains("email") => {
            ApiError::BadRequest("email notifications require a verified address".into())
        }
        DbError::Invalid(_) => ApiError::BadRequest("invalid notification request".into()),
        _ => ApiError::Internal("notification storage failed".into()),
    }
}

fn notification_content(
    event: &ClaimedNotificationEvent,
    locale: MessageLocale,
) -> Option<(String, String, String)> {
    if !matches!(
        (event.event_type.as_str(), event.status.as_str()),
        ("gallery.approved", "approved") | ("gallery.rejected", "rejected")
    ) {
        return None;
    }
    let submission_title = safe_notification_text(&event.title, 100);
    let reason = event
        .reason
        .as_deref()
        .map(|reason| safe_notification_text(reason, 500))
        .filter(|reason| !reason.is_empty());
    let (title, body) = localized_notification(
        locale,
        &event.event_type,
        &submission_title,
        reason.as_deref(),
    )?;
    let href = format!(
        "/g/{}/tickets/gallery/submissions?selected={}",
        event.guild_id, event.submission_id
    );
    Some((title, body, href))
}

fn safe_notification_text(value: &str, limit: usize) -> String {
    value
        .chars()
        .map(|character| {
            if character.is_control() {
                ' '
            } else {
                character
            }
        })
        .take(limit)
        .collect::<String>()
        .trim()
        .to_owned()
}

pub fn run_worker(
    db: Db,
    access: std::sync::Arc<dyn MemberAccess>,
    direct_messages: std::sync::Arc<dyn DirectMessageSink>,
    vault: Option<std::sync::Arc<IntegrationSecretService>>,
    shutdown: CancellationToken,
) -> JoinHandle<()> {
    tokio::spawn(async move {
        let email_sender = EmailSender::from_env();
        let mut ticks = interval(OUTBOX_POLL);
        ticks.set_missed_tick_behavior(MissedTickBehavior::Skip);
        let mut last_purge = Instant::now();
        loop {
            tokio::select! {
                () = shutdown.cancelled() => break,
                _ = ticks.tick() => {}
            }
            if last_purge.elapsed() >= OUTBOX_PURGE_INTERVAL {
                run_periodic_purges(&db).await;
                last_purge = Instant::now();
            }
            for _ in 0..OUTBOX_BATCH {
                let event = match db.claim_notification_outbox(OUTBOX_LEASE_SECONDS).await {
                    Ok(Some(event)) => event,
                    Ok(None) => break,
                    Err(_) => {
                        tracing::warn!("notification outbox claim failed");
                        break;
                    }
                };
                process_event(
                    &db,
                    access.as_ref(),
                    direct_messages.as_ref(),
                    vault.as_deref(),
                    email_sender.as_ref(),
                    event,
                )
                .await;
            }
        }
    })
}

// One pass of the periodic cleanup. Each step runs even when an earlier one fails.
async fn run_periodic_purges(db: &Db) {
    if db.purge_notification_outbox().await.is_err() {
        tracing::warn!("notification outbox cleanup failed");
    }
    if db.purge_expired_notifications().await.is_err() {
        tracing::warn!("expired notification cleanup failed");
    }
    if db.purge_email_send_history().await.is_err() {
        tracing::warn!("email send history cleanup failed");
    }
    if db.purge_expired_email_verifications().await.is_err() {
        tracing::warn!("expired email verification cleanup failed");
    }
    if db.ticket_gallery_purge_expired().await.is_err() {
        tracing::warn!("ticket gallery cleanup failed");
    }
}

struct DeliveryContent {
    title: String,
    body: String,
    href: String,
}

// A channel records its own outcome, so one failing channel does not hold back the others.
async fn process_event(
    db: &Db,
    access: &dyn MemberAccess,
    direct_messages: &dyn DirectMessageSink,
    vault: Option<&IntegrationSecretService>,
    email: Option<&EmailSender>,
    event: ClaimedNotificationEvent,
) {
    let Ok(guild) = config::guild_id(event.guild_id) else {
        let _ = db
            .abandon_notification_outbox(event.event_id, event.attempts, "permanent")
            .await;
        return;
    };
    let recipient = twilight_model::id::Id::new(event.recipient_id);
    match access.member(guild, recipient).await {
        Ok(Some(_)) => {}
        Ok(None) => {
            let _ = db
                .abandon_notification_outbox(
                    event.event_id,
                    event.attempts,
                    "recipient_unavailable",
                )
                .await;
            return;
        }
        Err(()) => {
            retry_outbox(db, &event, "transient").await;
            return;
        }
    }

    let Ok(preferences) = db.notification_preferences(event.recipient_id).await else {
        retry_outbox(db, &event, "transient").await;
        return;
    };
    let locale = saved_locale(&preferences.locale);
    let Some((title, body, href)) = notification_content(&event, locale) else {
        let _ = db
            .abandon_notification_outbox(event.event_id, event.attempts, "permanent")
            .await;
        return;
    };
    let content = DeliveryContent { title, body, href };

    let settled = deliver_in_app(db, &event, &preferences, &content).await
        && deliver_direct_message(
            db,
            direct_messages,
            &event,
            &preferences,
            guild,
            recipient,
            &content,
        )
        .await
        && deliver_email(db, email, vault, &event, &preferences, &content).await;
    if !settled {
        // The lease was lost; the worker that holds it now settles the event.
        return;
    }
    let finished = db
        .finish_notification_outbox(event.event_id, event.attempts)
        .await
        .unwrap_or(false);
    if !finished {
        let _ = db
            .release_notification_outbox(event.event_id, event.attempts)
            .await;
    }
}

// Each channel is retried on its own schedule: 5 seconds, doubling, capped at one hour.
fn channel_retry_delay(prior_attempts: i32) -> i64 {
    let shift = prior_attempts.clamp(0, 10).cast_unsigned();
    (5_i64 * (1_i64 << shift)).min(3600)
}

async fn complete_channel(
    db: &Db,
    event: &ClaimedNotificationEvent,
    channel: NotificationOutboxChannel,
    status: &'static str,
) -> bool {
    db.complete_notification_outbox_channel(event.event_id, event.attempts, channel, status)
        .await
        .unwrap_or(false)
}

async fn retry_channel(
    db: &Db,
    event: &ClaimedNotificationEvent,
    channel: NotificationOutboxChannel,
    status: &'static str,
    prior_attempts: i32,
) -> bool {
    db.retry_notification_channel(
        event.event_id,
        event.attempts,
        channel,
        status,
        channel_retry_delay(prior_attempts),
        OUTBOX_MAX_ATTEMPTS,
    )
    .await
    .unwrap_or(false)
}

async fn deliver_in_app(
    db: &Db,
    event: &ClaimedNotificationEvent,
    preferences: &StoredNotificationPreferences,
    content: &DeliveryContent,
) -> bool {
    if event.in_app_done || !event.in_app_due {
        return true;
    }
    if !preferences.in_app_enabled {
        return complete_channel(db, event, NotificationOutboxChannel::InApp, "skipped").await;
    }
    let submission_title = safe_notification_text(&event.title, 100);
    let reason = event
        .reason
        .as_deref()
        .map(|reason| safe_notification_text(reason, 500))
        .filter(|reason| !reason.is_empty());
    match db
        .insert_gallery_notification(
            event.recipient_id,
            event.guild_id,
            event.event_id,
            &event.event_type,
            &submission_title,
            reason.as_deref(),
            &content.href,
            event.occurred_at,
        )
        .await
    {
        Ok(_) => complete_channel(db, event, NotificationOutboxChannel::InApp, "sent").await,
        Err(_) => {
            retry_channel(
                db,
                event,
                NotificationOutboxChannel::InApp,
                "pending",
                event.in_app_attempts,
            )
            .await
        }
    }
}

async fn deliver_direct_message(
    db: &Db,
    direct_messages: &dyn DirectMessageSink,
    event: &ClaimedNotificationEvent,
    preferences: &StoredNotificationPreferences,
    guild: GuildId,
    recipient: UserId,
    content: &DeliveryContent,
) -> bool {
    if event.discord_dm_done || !event.discord_dm_due {
        return true;
    }
    if !preferences.discord_dm_enabled {
        return complete_channel(db, event, NotificationOutboxChannel::DiscordDm, "skipped").await;
    }
    let text = if content.body.is_empty() {
        content.title.clone()
    } else {
        format!("{}\n\n{}", content.title, content.body)
    };
    match direct_messages.send(guild, recipient, text).await {
        Ok(()) => complete_channel(db, event, NotificationOutboxChannel::DiscordDm, "sent").await,
        Err(DirectMessageError::Permanent) => {
            complete_channel(db, event, NotificationOutboxChannel::DiscordDm, "failed").await
        }
        Err(DirectMessageError::Retryable) => {
            retry_channel(
                db,
                event,
                NotificationOutboxChannel::DiscordDm,
                "pending",
                event.discord_dm_attempts,
            )
            .await
        }
    }
}

async fn deliver_email(
    db: &Db,
    email: Option<&EmailSender>,
    vault: Option<&IntegrationSecretService>,
    event: &ClaimedNotificationEvent,
    preferences: &StoredNotificationPreferences,
    content: &DeliveryContent,
) -> bool {
    if event.email_done || !event.email_due {
        return true;
    }
    if !(preferences.email_enabled && preferences.email_verified) {
        return complete_channel(db, event, NotificationOutboxChannel::Email, "skipped").await;
    }
    let (Some(vault), Some(sender), Some(encrypted)) =
        (vault, email, preferences.email_ciphertext.as_deref())
    else {
        // Not configured yet: the channel waits instead of being marked as delivered.
        return db
            .defer_notification_channel(
                event.event_id,
                event.attempts,
                NotificationOutboxChannel::Email,
                OUTBOX_NOT_READY_DELAY_SECONDS,
            )
            .await
            .unwrap_or(false);
    };
    let owner = twilight_model::id::Id::new(event.recipient_id);
    let Ok(address) = vault.open_account_secret(
        owner,
        owner,
        EMAIL_SECRET_NAMESPACE,
        EMAIL_SECRET_FIELD,
        encrypted,
    ) else {
        return complete_channel(db, event, NotificationOutboxChannel::Email, "failed").await;
    };
    let Ok(mailbox) = parse_mailbox(address.expose_secret()) else {
        return complete_channel(db, event, NotificationOutboxChannel::Email, "failed").await;
    };
    let message_body = if content.body.is_empty() {
        content.title.clone()
    } else {
        format!("{}\n\n{}", content.title, content.body)
    };
    let message_id = sender.message_id(&format!("ticket-notification-{}", event.event_id));
    match sender
        .send(mailbox, &content.title, &message_body, Some(message_id))
        .await
    {
        Ok(()) => complete_channel(db, event, NotificationOutboxChannel::Email, "sent").await,
        // After a failed SMTP call the delivery may or may not have happened. The retry reuses
        // the same Message-ID, so a relay that recognises it can drop the duplicate.
        Err(()) => {
            retry_channel(
                db,
                event,
                NotificationOutboxChannel::Email,
                "unknown",
                event.email_attempts,
            )
            .await
        }
    }
}

// Counts against the whole event, for member lookups and storage failures.
async fn retry_outbox(db: &Db, event: &ClaimedNotificationEvent, failure: &'static str) {
    if event.event_failures.saturating_add(1) >= OUTBOX_MAX_ATTEMPTS {
        let _ = db
            .abandon_notification_outbox(event.event_id, event.attempts, "delivery_exhausted")
            .await;
        return;
    }
    let _ = db
        .retry_notification_outbox(
            event.event_id,
            event.attempts,
            channel_retry_delay(event.event_failures),
            failure,
        )
        .await;
}

#[cfg(test)]
mod tests {
    use closure_core::permission::Actor;
    use sqlx::PgPool;

    use super::*;

    struct AllowAll;

    impl MemberAccess for AllowAll {
        fn member(
            &self,
            _guild: GuildId,
            user: UserId,
        ) -> Pin<Box<dyn Future<Output = Result<Option<Actor>, ()>> + Send + '_>> {
            Box::pin(async move {
                Ok(Some(Actor {
                    user,
                    roles: vec![],
                    administrator: false,
                    owner: false,
                }))
            })
        }
    }

    struct DirectMessages(DirectMessageError);

    impl DirectMessageSink for DirectMessages {
        fn send(
            &self,
            _guild: GuildId,
            _recipient: UserId,
            _content: String,
        ) -> Pin<Box<dyn Future<Output = Result<(), DirectMessageError>> + Send + '_>> {
            let error = self.0;
            Box::pin(async move { Err(error) })
        }
    }

    async fn seed_event(pool: &PgPool, recipient: i64) {
        let submission: i64 = sqlx::query_scalar(
            "INSERT INTO ticket_gallery_templates
                (kind, title, description, category, payload, author_id, source_guild_id, author_name, status)
             VALUES ('panel', 'Title', 'Description', 'support', '{}'::jsonb, 1, 10, 'author', 'approved')
             RETURNING id",
        )
        .fetch_one(pool)
        .await
        .unwrap();
        let event: i64 = sqlx::query_scalar(
            "INSERT INTO ticket_gallery_events
                (submission_id, guild_id, revision, recipient_id, reviewer_id, event_type, status, title)
             VALUES ($1, 10, 1, $2, 2, 'gallery.approved', 'approved', 'Title')
             RETURNING event_id",
        )
        .bind(submission)
        .bind(recipient)
        .fetch_one(pool)
        .await
        .unwrap();
        sqlx::query(
            "INSERT INTO ticket_notification_outbox (event_id) VALUES ($1) ON CONFLICT DO NOTHING",
        )
        .bind(event)
        .execute(pool)
        .await
        .unwrap();
    }

    async fn claim(db: &Db) -> ClaimedNotificationEvent {
        db.claim_notification_outbox(OUTBOX_LEASE_SECONDS)
            .await
            .unwrap()
            .unwrap()
    }

    #[sqlx::test(migrations = "../closure-db/migrations")]
    async fn a_retryable_direct_message_does_not_hold_back_the_email_channel(pool: PgPool) {
        sqlx::query(
            "INSERT INTO ticket_notification_preferences (user_id, discord_dm_enabled) VALUES (42, TRUE)",
        )
        .execute(&pool)
        .await
        .unwrap();
        seed_event(&pool, 42).await;
        let db = Db::from_pool(pool.clone());
        let event = claim(&db).await;

        process_event(
            &db,
            &AllowAll,
            &DirectMessages(DirectMessageError::Retryable),
            None,
            None,
            event,
        )
        .await;

        let (email_done, dm_done, finished): (bool, bool, bool) = sqlx::query_as(
            "SELECT email_done, discord_dm_done, completed_at IS NOT NULL
             FROM ticket_notification_outbox",
        )
        .fetch_one(&pool)
        .await
        .unwrap();
        assert_eq!((email_done, dm_done, finished), (true, false, false));
    }

    #[sqlx::test(migrations = "../closure-db/migrations")]
    async fn a_verified_email_waits_for_configuration_instead_of_being_dropped(pool: PgPool) {
        sqlx::query(
            "INSERT INTO ticket_notification_preferences
                (user_id, email_enabled, email_verified, email_ciphertext, email_masked)
             VALUES (42, TRUE, TRUE, $1, 'a***@example.com')",
        )
        .bind(vec![0_u8])
        .execute(&pool)
        .await
        .unwrap();
        seed_event(&pool, 42).await;
        let db = Db::from_pool(pool.clone());
        let event = claim(&db).await;

        process_event(
            &db,
            &AllowAll,
            &DirectMessages(DirectMessageError::Permanent),
            None,
            None,
            event,
        )
        .await;

        let (email_done, finished, abandoned, attempts): (bool, bool, bool, i32) = sqlx::query_as(
            "SELECT email_done, completed_at IS NOT NULL, abandoned_at IS NOT NULL, attempts
             FROM ticket_notification_outbox",
        )
        .fetch_one(&pool)
        .await
        .unwrap();
        // The claim itself is one attempt; waiting for configuration does not add a retry.
        assert_eq!(
            (email_done, finished, abandoned, attempts),
            (false, false, false, 1)
        );
    }

    #[sqlx::test(migrations = "../closure-db/migrations")]
    async fn a_long_direct_message_outage_gives_up_on_that_channel_only(pool: PgPool) {
        sqlx::query(
            "INSERT INTO ticket_notification_preferences (user_id, discord_dm_enabled) VALUES (42, TRUE)",
        )
        .execute(&pool)
        .await
        .unwrap();
        seed_event(&pool, 42).await;
        let db = Db::from_pool(pool.clone());

        for _ in 0..=OUTBOX_MAX_ATTEMPTS {
            // Make every channel due again, as if its backoff had elapsed.
            sqlx::query(
                "UPDATE ticket_notification_outbox
                 SET available_at = now(), locked_until = NULL, discord_dm_next_at = now()",
            )
            .execute(&pool)
            .await
            .unwrap();
            let Some(event) = db
                .claim_notification_outbox(OUTBOX_LEASE_SECONDS)
                .await
                .unwrap()
            else {
                break;
            };
            process_event(
                &db,
                &AllowAll,
                &DirectMessages(DirectMessageError::Retryable),
                None,
                None,
                event,
            )
            .await;
        }

        let (abandoned, finished): (bool, bool) = sqlx::query_as(
            "SELECT abandoned_at IS NOT NULL, completed_at IS NOT NULL
             FROM ticket_notification_outbox",
        )
        .fetch_one(&pool)
        .await
        .unwrap();
        assert!(
            !abandoned,
            "the event was abandoned by a direct-message outage"
        );
        assert!(
            finished,
            "the other channels should finish once direct messages give up"
        );
    }

    #[test]
    fn outbox_mail_carries_the_message_id_it_was_given() {
        let from = parse_mailbox("closure@example.com").unwrap();
        let recipient = parse_mailbox("member@example.org").unwrap();
        let message = outbound_message(
            &from,
            recipient,
            "Subject",
            "Body",
            Some("<ticket-notification-7@example.com>".to_owned()),
        )
        .unwrap();
        let raw = String::from_utf8_lossy(&message.formatted()).into_owned();
        assert!(
            raw.contains("Message-ID: <ticket-notification-7@example.com>"),
            "{raw}"
        );
    }

    #[test]
    fn a_resent_verification_reuses_the_message_id_of_its_request() {
        let from = parse_mailbox("closure@example.com").unwrap();
        let first = delivery_message_id(&from, "ticket-verification-req-1");
        assert_eq!(
            first,
            delivery_message_id(&from, "ticket-verification-req-1")
        );
        assert_ne!(
            first,
            delivery_message_id(&from, "ticket-verification-req-2")
        );
        assert_eq!(first, "<ticket-verification-req-1@example.com>");
    }

    #[test]
    fn channel_retries_double_and_stop_at_one_hour() {
        assert_eq!(channel_retry_delay(0), 5);
        assert_eq!(channel_retry_delay(1), 10);
        assert_eq!(channel_retry_delay(2), 20);
        assert_eq!(channel_retry_delay(20), 3600);
    }

    #[sqlx::test(migrations = "../closure-db/migrations")]
    async fn the_periodic_purge_removes_gallery_requests_older_than_the_retention(pool: PgPool) {
        let template: i64 = sqlx::query_scalar(
            "INSERT INTO ticket_gallery_templates
                (kind, title, description, category, payload, author_id, source_guild_id, author_name, status)
             VALUES ('panel', 'Title', 'Description', 'support', '{}'::jsonb, 1, 10, 'author', 'approved')
             RETURNING id",
        )
        .fetch_one(&pool)
        .await
        .unwrap();
        sqlx::query(
            "INSERT INTO ticket_gallery_requests (actor_id, request_key, action, fingerprint, template_id, created_at)
             VALUES (7, 'old', 'import', $1, $2, now() - make_interval(days => 400)),
                    (7, 'recent', 'import', $1, $2, now())",
        )
        .bind(vec![1_u8; 32])
        .bind(template)
        .execute(&pool)
        .await
        .unwrap();
        let db = Db::from_pool(pool.clone());

        run_periodic_purges(&db).await;

        let kept: Vec<String> =
            sqlx::query_scalar("SELECT request_key FROM ticket_gallery_requests")
                .fetch_all(&pool)
                .await
                .unwrap();
        assert_eq!(kept, vec!["recent".to_owned()]);
    }
}
