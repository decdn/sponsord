//! Who is asking: the requester's IP address (from the socket, or from a
//! header set by a trusted reverse proxy), and per-IP rate limits.

use std::collections::HashMap;
use std::net::{IpAddr, SocketAddr};
use std::sync::Mutex;

use axum::extract::{ConnectInfo, FromRequestParts};
use axum::http::HeaderName;
use axum::http::request::Parts;

use crate::state::AppState;

/// Where the requester's address comes from.
#[derive(Clone, Debug, Default)]
pub enum ClientIpSource {
    /// The TCP peer: right when nothing sits in front of the onramp.
    #[default]
    Peer,
    /// A header a trusted reverse proxy sets, e.g. `CF-Connecting-IP` or
    /// `X-Forwarded-For` (its first address is used). Only safe when every
    /// request comes through that proxy, since clients can set it too.
    Header(HeaderName),
}

/// The requester's IP address, when known.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct ClientIp(pub Option<IpAddr>);

impl FromRequestParts<AppState> for ClientIp {
    type Rejection = std::convert::Infallible;

    async fn from_request_parts(
        parts: &mut Parts,
        state: &AppState,
    ) -> Result<Self, Self::Rejection> {
        let ip = match &state.client_ip {
            ClientIpSource::Peer => parts
                .extensions
                .get::<ConnectInfo<SocketAddr>>()
                .map(|ConnectInfo(addr)| addr.ip()),
            ClientIpSource::Header(name) => parts
                .headers
                .get(name)
                .and_then(|v| v.to_str().ok())
                .and_then(|v| v.split(',').next())
                .and_then(|v| v.trim().parse().ok()),
        };
        Ok(Self(ip))
    }
}

/// A fixed-window limit of `per_minute` requests per IP address. A limit of
/// 0 disables it; a request with no known address is never limited.
pub struct RateLimiter {
    per_minute: u32,
    windows: Mutex<HashMap<IpAddr, (u64, u32)>>,
}

/// Distinct addresses tracked before stale windows are swept.
const SWEEP_AT: usize = 10_000;

impl RateLimiter {
    #[must_use]
    pub fn new(per_minute: u32) -> Self {
        Self {
            per_minute,
            windows: Mutex::new(HashMap::new()),
        }
    }

    /// Count a request from `ip` at `now`; `false` once `ip` is over its limit
    /// for the current minute.
    pub fn allow(&self, ip: Option<IpAddr>, now: u64) -> bool {
        let (Some(ip), true) = (ip, self.per_minute > 0) else {
            return true;
        };
        let minute = now / 60;
        let mut windows = self
            .windows
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        if windows.len() >= SWEEP_AT {
            windows.retain(|_, (m, _)| *m == minute);
        }
        let (m, count) = windows.entry(ip).or_insert((minute, 0));
        if *m != minute {
            *m = minute;
            *count = 0;
        }
        *count = count.saturating_add(1);
        *count <= self.per_minute
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used)]
mod tests {
    use super::*;

    #[test]
    fn limits_per_ip_per_minute() {
        let limiter = RateLimiter::new(2);
        let a = Some("203.0.113.1".parse().unwrap());
        let b = Some("203.0.113.2".parse().unwrap());
        assert!(limiter.allow(a, 60));
        assert!(limiter.allow(a, 61));
        assert!(!limiter.allow(a, 62));
        assert!(limiter.allow(b, 62), "another address has its own budget");
        assert!(limiter.allow(a, 120), "a new minute resets the count");
    }

    #[test]
    fn zero_or_unknown_address_is_never_limited() {
        let off = RateLimiter::new(0);
        let a = Some("203.0.113.1".parse().unwrap());
        assert!((0..100).all(|_| off.allow(a, 60)));
        let on = RateLimiter::new(1);
        assert!((0..100).all(|_| on.allow(None, 60)));
    }
}
