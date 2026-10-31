use std::collections::HashMap;
use std::net::{IpAddr, Ipv4Addr, Ipv6Addr, SocketAddr};
use std::sync::{Mutex, PoisonError};
use std::time::{Duration, Instant};

use axum::extract::{ConnectInfo, Request, State};
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

/// How many failed logins one address may make in [`FAILURE_WINDOW`] before it is turned away.
const MAX_FAILURES: u32 = 10;
const FAILURE_WINDOW: Duration = Duration::from_secs(60);
/// How many addresses are followed; the rest share one allowance.
const MAX_TRACKED: usize = 4096;

/// Counts failed logins per address, so a token cannot be guessed at the speed of the network.
/// Once an address has used up its allowance every request of it is refused, whatever token it
/// presents, until the window ends: letting a right guess through would make the limit useless.
///
/// Behind a reverse proxy every client has the address of the proxy, and they share an allowance.
#[derive(Debug, Default)]
pub(crate) struct FailureLimiter {
    windows: Mutex<HashMap<IpAddr, Window>>,
}

#[derive(Debug, Clone, Copy)]
struct Window {
    began: Instant,
    failures: u32,
}

impl Window {
    fn expired(&self, now: Instant) -> bool {
        now.saturating_duration_since(self.began) >= FAILURE_WINDOW
    }
}

/// The allowance of addresses that could not be told apart because too many are followed.
const OVERFLOW: IpAddr = IpAddr::V4(Ipv4Addr::UNSPECIFIED);

/// The address an allowance is kept for. An IPv6 client is counted by its /64: the host part of its
/// address is its own to choose, so counting addresses would give it a new allowance every time.
fn allowance_key(address: IpAddr) -> IpAddr {
    match address {
        IpAddr::V4(_) => address,
        IpAddr::V6(address) => {
            let mut segments = address.segments();
            segments[4..].fill(0);
            IpAddr::V6(Ipv6Addr::from(segments))
        }
    }
}

impl FailureLimiter {
    /// `Some(wait)` when `address` has used up its allowance and must wait that long.
    pub(crate) fn blocked(&self, address: IpAddr) -> Option<Duration> {
        self.blocked_at(address, Instant::now())
    }

    pub(crate) fn record_failure(&self, address: IpAddr) {
        self.record_failure_at(address, Instant::now());
    }

    fn blocked_at(&self, address: IpAddr, now: Instant) -> Option<Duration> {
        let windows = self.windows.lock().unwrap_or_else(PoisonError::into_inner);
        // An address that is not followed, because too many are, shares one allowance.
        let window = windows.get(&allowance_key(address)).or_else(|| {
            windows
                .get(&OVERFLOW)
                .filter(|_| windows.len() >= MAX_TRACKED)
        })?;
        (!window.expired(now) && window.failures >= MAX_FAILURES)
            .then(|| FAILURE_WINDOW.saturating_sub(now.saturating_duration_since(window.began)))
    }

    fn record_failure_at(&self, address: IpAddr, now: Instant) {
        let mut windows = self.windows.lock().unwrap_or_else(PoisonError::into_inner);
        let mut key = allowance_key(address);
        if !windows.contains_key(&key) && windows.len() >= MAX_TRACKED {
            windows.retain(|_, window| !window.expired(now));
            if windows.len() >= MAX_TRACKED {
                key = OVERFLOW;
            }
        }
        let window = windows.entry(key).or_insert(Window {
            began: now,
            failures: 0,
        });
        if window.expired(now) {
            *window = Window {
                began: now,
                failures: 0,
            };
        }
        window.failures += 1;
    }
}

pub(crate) async fn require_token(
    State(state): State<ApiState>,
    request: Request,
    next: Next,
) -> Result<Response, ApiError> {
    // Requests that did not come over a socket (the tests) are all one address.
    let address = request
        .extensions()
        .get::<ConnectInfo<SocketAddr>>()
        .map_or(OVERFLOW, |connection| connection.0.ip());
    if let Some(wait) = state.inner.failures.blocked(address) {
        return Err(ApiError::TooManyAttempts {
            retry_after_secs: wait.as_secs().max(1),
        });
    }
    // A missing header is compared against an empty token so both paths do the same work.
    let presented = digest(bearer(&request).unwrap_or(""));
    let known = bearer(&request).is_some();
    if known && bool::from(presented.ct_eq(&state.inner.token_digest)) {
        Ok(next.run(request).await)
    } else {
        state.inner.failures.record_failure(address);
        Err(ApiError::Unauthorized)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn ip(last: u8) -> IpAddr {
        IpAddr::V4(Ipv4Addr::new(192, 0, 2, last))
    }

    #[test]
    fn an_address_is_turned_away_after_too_many_failures_until_the_window_ends() {
        let limiter = FailureLimiter::default();
        let start = Instant::now();
        for _ in 0..MAX_FAILURES - 1 {
            limiter.record_failure_at(ip(1), start);
        }
        assert_eq!(limiter.blocked_at(ip(1), start), None);
        limiter.record_failure_at(ip(1), start);
        let wait = limiter.blocked_at(ip(1), start + Duration::from_secs(20));
        assert_eq!(wait, Some(Duration::from_secs(40)));
        // Others are not affected, and the window ends.
        assert_eq!(limiter.blocked_at(ip(2), start), None);
        assert_eq!(limiter.blocked_at(ip(1), start + FAILURE_WINDOW), None);
        // A failure after the window starts a new one.
        limiter.record_failure_at(ip(1), start + FAILURE_WINDOW);
        assert_eq!(limiter.blocked_at(ip(1), start + FAILURE_WINDOW), None);
    }

    #[test]
    fn when_too_many_addresses_are_followed_the_rest_share_one_allowance() {
        let limiter = FailureLimiter::default();
        let start = Instant::now();
        for n in 0..MAX_TRACKED {
            let address = IpAddr::V6(Ipv6Addr::from(u128::try_from(n + 1).unwrap() << 64));
            limiter.record_failure_at(address, start);
        }
        // Strangers each fail once; together they use up the shared allowance.
        for n in 0..MAX_FAILURES {
            limiter.record_failure_at(ip(u8::try_from(n).unwrap()), start);
        }
        assert!(limiter.blocked_at(ip(200), start).is_some());
        // The table does not grow past its limit.
        assert!(limiter.windows.lock().unwrap().len() <= MAX_TRACKED + 1);
        // Once the windows have ended there is room again.
        let later = start + FAILURE_WINDOW;
        limiter.record_failure_at(ip(201), later);
        assert_eq!(limiter.blocked_at(ip(200), later), None);
    }

    #[test]
    fn ipv6_clients_of_one_prefix_share_an_allowance() {
        let limiter = FailureLimiter::default();
        let start = Instant::now();
        let first: IpAddr = "2001:db8:0:1::1".parse().unwrap();
        let other_host: IpAddr = "2001:db8:0:1:ffff::2".parse().unwrap();
        let other_prefix: IpAddr = "2001:db8:0:2::1".parse().unwrap();
        for _ in 0..MAX_FAILURES {
            limiter.record_failure_at(first, start);
        }
        assert!(limiter.blocked_at(other_host, start).is_some());
        assert_eq!(limiter.blocked_at(other_prefix, start), None);
    }

    #[test]
    fn digests_are_fixed_length_and_differ_per_token() {
        assert_ne!(digest("a"), digest("b"));
        assert_eq!(digest("a"), digest("a"));
        assert_eq!(digest(&"x".repeat(10_000)).len(), 32);
    }
}
