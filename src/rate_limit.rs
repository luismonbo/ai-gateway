//! Per-key rate limiting: a token bucket for each key ID.
//!
//! A bucket holds up to `RATE_LIMIT_RPM` tokens and refills at RPM/60 per
//! second. The arithmetic is integer: with floats, rpm = 2 and 15 s after
//! draining gives 0.49999999999999994 tokens and a 16 s wait instead of 15.

use crate::app::AppState;
use crate::auth::{Caller, KeyId};
use crate::error::ApiError;
use actix_web::body::{EitherBody, MessageBody};
use actix_web::dev::{ServiceRequest, ServiceResponse};
use actix_web::middleware::Next;
use actix_web::{web, HttpMessage, ResponseError};
use std::collections::HashMap;
use std::sync::{Mutex, PoisonError};
use std::time::{Duration, Instant};
use tracing::warn;

/// One token, in integer units. Waiting `n` nanoseconds adds `n × rpm`
/// units, so one minute (6 × 10¹⁰ ns) adds exactly `rpm` tokens.
const UNITS_PER_TOKEN: u128 = 60_000_000_000;

pub struct TokenBucket {
    units: u128,
    last: Instant,
    rpm: u32,
}

impl TokenBucket {
    /// A full bucket: `rpm` requests can go through at once.
    pub fn new(rpm: u32, now: Instant) -> Self {
        Self {
            units: capacity(rpm),
            last: now,
            rpm,
        }
    }

    /// Takes one token, or says how long until one is available.
    pub fn try_take(&mut self, now: Instant) -> Result<(), Duration> {
        let rpm = u128::from(self.rpm);
        let elapsed = now.saturating_duration_since(self.last).as_nanos();
        let units = (self.units + elapsed * rpm).min(capacity(self.rpm));
        // A caller that read the clock before the previous one got the lock
        // passes an earlier `now`; never move `last` back, or that time
        // would be refilled twice.
        self.last = self.last.max(now);

        if units >= UNITS_PER_TOKEN {
            self.units = units - UNITS_PER_TOKEN;
            Ok(())
        } else {
            self.units = units;
            let wait_nanos = (UNITS_PER_TOKEN - units).div_ceil(rpm);
            // At most one minute (rpm ≥ 1), which fits easily in a u64.
            Err(Duration::from_nanos(
                u64::try_from(wait_nanos).unwrap_or(u64::MAX),
            ))
        }
    }
}

fn capacity(rpm: u32) -> u128 {
    u128::from(rpm) * UNITS_PER_TOKEN
}

/// One bucket per key ID. Only authenticated callers get here, so the map
/// holds at most one entry per configured key.
pub struct RateLimiter {
    rpm: u32,
    buckets: Mutex<HashMap<KeyId, TokenBucket>>,
}

impl RateLimiter {
    pub fn new(rpm: u32) -> Self {
        Self {
            rpm,
            buckets: Mutex::new(HashMap::new()),
        }
    }

    /// `Err` holds the `Retry-After` seconds.
    pub fn check(&self, key: &KeyId, now: Instant) -> Result<(), u64> {
        // A panic elsewhere while holding the lock leaves the buckets valid,
        // so a poisoned lock is still usable.
        let mut buckets = self.buckets.lock().unwrap_or_else(PoisonError::into_inner);
        if let Some(bucket) = buckets.get_mut(key) {
            return bucket.try_take(now).map_err(retry_after_secs);
        }
        buckets
            .entry(key.clone())
            .or_insert_with(|| TokenBucket::new(self.rpm, now))
            .try_take(now)
            .map_err(retry_after_secs)
    }
}

/// Whole seconds, rounded up and at least 1, so a client that waits that
/// long always finds a token.
pub fn retry_after_secs(wait: Duration) -> u64 {
    let rounded_up = wait.as_secs() + u64::from(wait.subsec_nanos() > 0);
    rounded_up.max(1)
}

/// Middleware for the `/v1` scope, after authentication. A rejection is
/// returned as `Ok(response)` so the access log still records the 429.
pub async fn enforce_rate_limit<B: MessageBody>(
    state: web::Data<AppState>,
    req: ServiceRequest,
    next: Next<B>,
) -> Result<ServiceResponse<EitherBody<B>>, actix_web::Error> {
    let key_id = req
        .extensions()
        .get::<Caller>()
        .map(|caller| caller.key_id.clone());
    let rejection = match key_id {
        // Auth runs first, so this only happens if the middleware order
        // breaks: fail closed.
        None => Some(ApiError::invalid_api_key()),
        Some(key_id) => match state.limiter.check(&key_id, Instant::now()) {
            Ok(()) => None,
            Err(retry_after) => {
                warn!(key_id = %key_id, retry_after, "rate limit exceeded");
                Some(ApiError::rate_limited(retry_after))
            }
        },
    };
    match rejection {
        None => next
            .call(req)
            .await
            .map(ServiceResponse::map_into_left_body),
        Some(error) => Ok(req
            .into_response(error.error_response())
            .map_into_right_body()),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicUsize, Ordering};

    const SECOND: Duration = Duration::from_secs(1);

    fn key(name: &str) -> KeyId {
        KeyId::from_raw_key(name)
    }

    #[test]
    fn a_full_bucket_allows_rpm_requests_then_rejects() {
        let t0 = Instant::now();
        let mut bucket = TokenBucket::new(60, t0);
        for _ in 0..60 {
            assert!(bucket.try_take(t0).is_ok());
        }
        assert_eq!(bucket.try_take(t0), Err(SECOND));
    }

    #[test]
    fn the_bucket_refills_with_injected_time() {
        let t0 = Instant::now();
        let mut bucket = TokenBucket::new(2, t0);
        assert!(bucket.try_take(t0).is_ok());
        assert!(bucket.try_take(t0).is_ok());
        assert_eq!(bucket.try_take(t0), Err(30 * SECOND));
        // With floats this boundary comes out as 16 s.
        assert_eq!(bucket.try_take(t0 + 15 * SECOND), Err(15 * SECOND));
        assert_eq!(bucket.try_take(t0 + 30 * SECOND), Ok(()));
    }

    #[test]
    fn refill_is_capped_at_capacity() {
        let t0 = Instant::now();
        let mut bucket = TokenBucket::new(3, t0);
        for _ in 0..3 {
            assert!(bucket.try_take(t0).is_ok());
        }
        let later = t0 + Duration::from_secs(3600);
        for _ in 0..3 {
            assert!(bucket.try_take(later).is_ok());
        }
        assert!(bucket.try_take(later).is_err());
    }

    #[test]
    fn an_earlier_now_neither_refills_nor_rewinds() {
        let t0 = Instant::now();
        let mut bucket = TokenBucket::new(1, t0 + 10 * SECOND);
        assert!(bucket.try_take(t0 + 10 * SECOND).is_ok());
        // A request that read the clock before the last one got the lock.
        assert_eq!(bucket.try_take(t0), Err(60 * SECOND));
        assert_eq!(bucket.try_take(t0 + 40 * SECOND), Err(30 * SECOND));
    }

    #[test]
    fn retry_after_rounds_up_and_is_at_least_one() {
        assert_eq!(retry_after_secs(Duration::from_millis(1)), 1);
        assert_eq!(retry_after_secs(Duration::from_millis(1500)), 2);
        assert_eq!(retry_after_secs(30 * SECOND), 30);
        assert_eq!(retry_after_secs(Duration::ZERO), 1);
    }

    #[test]
    fn keys_have_separate_buckets() {
        let limiter = RateLimiter::new(1);
        let now = Instant::now();
        assert_eq!(limiter.check(&key("a"), now), Ok(()));
        assert_eq!(limiter.check(&key("a"), now), Err(60));
        assert_eq!(limiter.check(&key("b"), now), Ok(()));
    }

    #[test]
    fn concurrent_requests_never_exceed_capacity() {
        let limiter = RateLimiter::new(10);
        let now = Instant::now();
        let allowed = AtomicUsize::new(0);
        std::thread::scope(|scope| {
            for _ in 0..8 {
                scope.spawn(|| {
                    for _ in 0..5 {
                        if limiter.check(&key("shared"), now).is_ok() {
                            allowed.fetch_add(1, Ordering::Relaxed);
                        }
                    }
                });
            }
        });
        assert_eq!(allowed.into_inner(), 10);
    }
}
