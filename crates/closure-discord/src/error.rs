use std::error::Error as _;
use std::time::Duration;

use closure_core::error::DiscordError;
use twilight_http::api_error::ApiError;
use twilight_http::error::{Error, ErrorType};

/// The longest retry hint we pass on; Discord never legitimately asks for more.
const MAX_RETRY_AFTER: Duration = Duration::from_secs(3600);

/// Maps an HTTP status and Discord error body onto [`DiscordError`].
///
/// Kept free of Twilight types so every branch can be tested with plain values.
pub(crate) fn classify(
    status: u16,
    message: String,
    retry_after_secs: Option<f64>,
) -> DiscordError {
    match status {
        400 => DiscordError::Invalid(message),
        401 => DiscordError::Fatal(
            "Discord rejected the credentials: the bot token or an interaction token is invalid, expired or revoked"
                .into(),
        ),
        403 => DiscordError::Forbidden(message),
        404 => DiscordError::NotFound,
        429 => DiscordError::RateLimited {
            retry_after: retry_after_secs
                .filter(|seconds| seconds.is_finite() && *seconds >= 0.0)
                .map(|seconds| Duration::from_secs_f64(seconds).min(MAX_RETRY_AFTER)),
        },
        500..=599 => DiscordError::Transient(message),
        _ => DiscordError::Fatal(format!("unexpected status {status}: {message}")),
    }
}

pub(crate) fn map_http_error(error: &Error) -> DiscordError {
    match error.kind() {
        ErrorType::Response {
            status,
            error: api_error,
            ..
        } => {
            let retry_after = match api_error {
                ApiError::Ratelimited(limited) => Some(limited.retry_after),
                _ => None,
            };
            classify(status.get(), api_error.to_string(), retry_after)
        }
        ErrorType::RequestTimedOut | ErrorType::RequestCanceled | ErrorType::RequestError => {
            DiscordError::Transient(error.to_string())
        }
        ErrorType::Unauthorized => classify(401, String::new(), None),
        ErrorType::Validation
        | ErrorType::Json
        | ErrorType::BuildingRequest
        | ErrorType::CreatingHeader { .. } => DiscordError::Invalid(match error.source() {
            Some(source) => format!("{error}: {source}"),
            None => error.to_string(),
        }),
        _ => DiscordError::Fatal(error.to_string()),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn statuses_map_to_the_error_taxonomy() {
        assert_eq!(classify(404, "x".into(), None), DiscordError::NotFound);
        assert!(
            matches!(classify(403, "no".into(), None), DiscordError::Forbidden(m) if m == "no")
        );
        assert!(matches!(
            classify(400, "bad".into(), None),
            DiscordError::Invalid(_)
        ));
        assert!(matches!(
            classify(401, "x".into(), None),
            DiscordError::Fatal(_)
        ));
        assert!(matches!(
            classify(500, "x".into(), None),
            DiscordError::Transient(_)
        ));
        assert!(matches!(
            classify(503, "x".into(), None),
            DiscordError::Transient(_)
        ));
        assert!(matches!(
            classify(418, "x".into(), None),
            DiscordError::Fatal(_)
        ));
    }

    #[test]
    fn only_transient_and_rate_limited_errors_are_retryable() {
        assert!(classify(502, String::new(), None).is_retryable());
        assert!(classify(429, String::new(), Some(1.0)).is_retryable());
        for status in [400, 401, 403, 404] {
            assert!(
                !classify(status, String::new(), None).is_retryable(),
                "{status}"
            );
        }
    }

    #[test]
    fn retry_hints_are_sanitised() {
        let hint = |seconds| match classify(429, String::new(), seconds) {
            DiscordError::RateLimited { retry_after } => retry_after,
            other => panic!("unexpected {other:?}"),
        };
        assert_eq!(hint(Some(1.5)), Some(Duration::from_millis(1500)));
        assert_eq!(hint(Some(1.0e12)), Some(MAX_RETRY_AFTER));
        assert_eq!(hint(Some(-1.0)), None);
        assert_eq!(hint(Some(f64::NAN)), None);
        assert_eq!(hint(None), None);
    }

    #[test]
    fn unauthorized_never_echoes_response_bodies() {
        let DiscordError::Fatal(message) = classify(401, "token=abc".into(), None) else {
            panic!("expected a fatal error");
        };
        assert!(!message.contains("abc"));
    }
}
