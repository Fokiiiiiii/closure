use axum::Json;
use axum::http::{HeaderValue, StatusCode, header};
use axum::response::{IntoResponse, Response};
use closure_config::ConfigError;
use closure_db::DbError;
use serde::Serialize;

#[derive(Debug)]
pub(crate) enum ApiError {
    Unauthorized,
    /// Too many failed logins from this address; it has to wait.
    TooManyAttempts {
        retry_after_secs: u64,
    },
    BadRequest(String),
    NotFound(String),
    /// A write that must say which revision it edits (`If-Match`) did not.
    PreconditionRequired,
    /// The configuration changed since the client read it.
    PreconditionFailed {
        current: i64,
    },
    Config(ConfigError),
    Internal(String),
}

impl From<ConfigError> for ApiError {
    fn from(error: ConfigError) -> Self {
        Self::Config(error)
    }
}

impl From<DbError> for ApiError {
    fn from(error: DbError) -> Self {
        match error {
            DbError::RevisionConflict { actual, .. } => Self::PreconditionFailed {
                current: actual.unwrap_or(0),
            },
            other => Self::Internal(other.to_string()),
        }
    }
}

#[derive(Serialize)]
struct Envelope {
    error: Body,
}

#[derive(Serialize)]
struct Body {
    code: &'static str,
    message: String,
    #[serde(skip_serializing_if = "Vec::is_empty")]
    issues: Vec<Issue>,
    #[serde(skip_serializing_if = "Option::is_none")]
    line: Option<u64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    column: Option<u64>,
}

#[derive(Serialize)]
struct Issue {
    path: String,
    code: &'static str,
    message: String,
}

fn body(code: &'static str, message: impl Into<String>) -> Body {
    Body {
        code,
        message: message.into(),
        issues: Vec::new(),
        line: None,
        column: None,
    }
}

fn respond(status: StatusCode, body: Body) -> Response {
    (status, Json(Envelope { error: body })).into_response()
}

impl IntoResponse for ApiError {
    fn into_response(self) -> Response {
        match self {
            Self::Unauthorized => {
                let mut response = respond(
                    StatusCode::UNAUTHORIZED,
                    body("unauthorized", "a valid bearer token is required"),
                );
                response
                    .headers_mut()
                    .insert(header::WWW_AUTHENTICATE, HeaderValue::from_static("Bearer"));
                response
            }
            Self::TooManyAttempts { retry_after_secs } => {
                let mut response = respond(
                    StatusCode::TOO_MANY_REQUESTS,
                    body(
                        "too_many_attempts",
                        "too many failed attempts to authenticate; wait before trying again",
                    ),
                );
                response
                    .headers_mut()
                    .insert(header::RETRY_AFTER, HeaderValue::from(retry_after_secs));
                response
            }
            Self::BadRequest(message) => {
                respond(StatusCode::BAD_REQUEST, body("bad_request", message))
            }
            Self::NotFound(message) => respond(StatusCode::NOT_FOUND, body("not_found", message)),
            Self::PreconditionRequired => respond(
                StatusCode::PRECONDITION_REQUIRED,
                body(
                    "precondition_required",
                    "send the revision you are editing in If-Match (\"0\" to create)",
                ),
            ),
            Self::PreconditionFailed { current } => {
                let mut response = respond(
                    StatusCode::PRECONDITION_FAILED,
                    body(
                        "revision_conflict",
                        "the configuration changed since you loaded it; reload and retry",
                    ),
                );
                if let Ok(value) = HeaderValue::from_str(&format!("\"{current}\"")) {
                    response.headers_mut().insert(header::ETAG, value);
                }
                response
            }
            Self::Config(error) => config_response(error),
            Self::Internal(detail) => {
                // Details can name tables or hosts; they go to the log, not to the client.
                tracing::error!(%detail, "internal error while serving a request");
                respond(
                    StatusCode::INTERNAL_SERVER_ERROR,
                    body("internal_error", "something went wrong on the server"),
                )
            }
        }
    }
}

fn config_response(error: ConfigError) -> Response {
    match error {
        ConfigError::TooLarge { .. } => respond(
            StatusCode::PAYLOAD_TOO_LARGE,
            body("too_large", error.to_string()),
        ),
        ConfigError::Syntax {
            message,
            line,
            column,
        } => respond(
            StatusCode::UNPROCESSABLE_ENTITY,
            Body {
                line,
                column,
                ..body("invalid_document", message)
            },
        ),
        ConfigError::UnsupportedVersion { .. } => respond(
            StatusCode::UNPROCESSABLE_ENTITY,
            body("unsupported_version", error.to_string()),
        ),
        ConfigError::Invalid(issues) => respond(
            StatusCode::UNPROCESSABLE_ENTITY,
            Body {
                issues: issues
                    .into_iter()
                    .map(|issue| Issue {
                        path: issue.path,
                        code: issue.code,
                        message: issue.message,
                    })
                    .collect(),
                ..body("invalid_config", "the configuration has problems")
            },
        ),
        ConfigError::BadModuleSchema { .. } | ConfigError::Serialize(_) => {
            ApiError::Internal(error.to_string()).into_response()
        }
    }
}
