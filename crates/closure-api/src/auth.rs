use axum::extract::{Request, State};
use axum::http::header::AUTHORIZATION;
use axum::middleware::Next;
use axum::response::Response;
use sha2::{Digest, Sha256};
use subtle::ConstantTimeEq;

use crate::ApiState;
use crate::error::ApiError;

/// SHA-256 of the admin token. Comparing digests means the comparison takes the same time
/// whatever length the presented token has, and the token itself is not kept in memory.
pub(crate) fn digest(token: &str) -> [u8; 32] {
    Sha256::digest(token.as_bytes()).into()
}

fn bearer(request: &Request) -> Option<&str> {
    let value = request.headers().get(AUTHORIZATION)?.to_str().ok()?;
    let (scheme, token) = value.split_once(' ')?;
    scheme
        .eq_ignore_ascii_case("bearer")
        .then_some(token.trim())
}

pub(crate) async fn require_token(
    State(state): State<ApiState>,
    request: Request,
    next: Next,
) -> Result<Response, ApiError> {
    // A missing header is compared against an empty token so both paths do the same work.
    let presented = digest(bearer(&request).unwrap_or(""));
    let known = bearer(&request).is_some();
    if known && bool::from(presented.ct_eq(&state.inner.token_digest)) {
        Ok(next.run(request).await)
    } else {
        Err(ApiError::Unauthorized)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn digests_are_fixed_length_and_differ_per_token() {
        assert_ne!(digest("a"), digest("b"));
        assert_eq!(digest("a"), digest("a"));
        assert_eq!(digest(&"x".repeat(10_000)).len(), 32);
    }
}
