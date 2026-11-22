//! The limiter proper: the bucket table, the live fixed windows and the
//! decision each counted request gets.

use std::{
    collections::{BTreeMap, HashMap},
    sync::{Arc, Mutex, MutexGuard, PoisonError},
    time::Instant,
};

use super::{
    RateLimit,
    clock::{Clock, SystemClock},
    middleware::seconds_until,
};
use crate::config::Config;

/// The limiter: the bucket table, the live windows, and the clock that
/// ages them.
///
/// One instance is shared by every route through
/// [`AppState::rate_limiter`](crate::state::AppState::rate_limiter), so
/// the same bucket counts one budget across the routes that mount it.
#[derive(Debug)]
pub struct RateLimiter {
    /// Bucket name to budget; a name absent here is unlimited.
    limits: BTreeMap<String, RateLimit>,
    /// The time source [`check`](Self::check) ages windows against.
    clock: Arc<dyn Clock>,
    /// One fixed window per `bucket:subject` key.
    windows: Mutex<HashMap<String, Window>>,
}

/// One fixed window: when it ends and how many requests fell into it.
#[derive(Clone, Copy, Debug)]
struct Window {
    /// The instant the window lapses; the next request starts a fresh one.
    expires: Instant,
    /// Requests counted into the window so far, refused ones included
    /// (Pleroma's `Hammer.check_rate` counts every call too).
    count: u64,
}

impl RateLimiter {
    /// The limiter `config` describes: its parsed `RATE_LIMITS` table over
    /// the real clock.
    #[must_use]
    pub fn from_config(config: &Config) -> Self {
        Self::with_clock(&config.rate_limit.limits, Arc::new(SystemClock))
    }

    /// The same table over an injected [`Clock`], the seam tests use to
    /// roll windows without waiting.
    #[must_use]
    pub fn with_clock(limits: &BTreeMap<String, RateLimit>, clock: Arc<dyn Clock>) -> Self {
        Self {
            limits: limits.clone(),
            clock,
            windows: Mutex::new(HashMap::new()),
        }
    }

    /// Whether bucket `name` has a budget at all; `false` means unlimited
    /// and [`check`](Self::check) answers `None` for it.
    #[must_use]
    pub fn limited(&self, name: &str) -> bool {
        self.limits.contains_key(name)
    }

    /// Counts one request of `identity` against bucket `name` and reports
    /// the window's state; `None` when the bucket is unlimited.
    ///
    /// The fixed window is anchored on the request that opened it: every
    /// request until `window` later belongs to that window, and the first
    /// one at or after its end opens the next.
    #[must_use]
    pub fn check(&self, name: &str, identity: &str) -> Option<Decision> {
        let limit = self.limits.get(name).copied()?;
        let now = self.clock.now();
        let key = format!("{name}:{identity}");
        let mut windows = self.lock();
        let window = match windows.get_mut(&key) {
            Some(existing) if now < existing.expires => {
                existing.count += 1;
                *existing
            }
            Some(existing) => {
                *existing = Window {
                    expires: now + limit.window,
                    count: 1,
                };
                *existing
            }
            None => {
                // Pruning rides on the insert path only: a known key is a
                // few increments, a new one pays the map scan.
                windows.retain(|_, live| now < live.expires);
                let fresh = Window {
                    expires: now + limit.window,
                    count: 1,
                };
                let _absent = windows.insert(key, fresh);
                fresh
            }
        };
        drop(windows);
        Some(Decision {
            allowed: window.count <= limit.max,
            remaining: limit.max.saturating_sub(window.count),
            limit: limit.max,
            reset_secs: seconds_until(window.expires.saturating_duration_since(now)),
        })
    }

    /// The windows map, poison-tolerant: a handler that panicked mid-check
    /// left only a half-counted request, not corrupted bookkeeping.
    fn lock(&self) -> MutexGuard<'_, HashMap<String, Window>> {
        self.windows.lock().unwrap_or_else(PoisonError::into_inner)
    }
}

/// What one counted request got: whether it passed, and the numbers every
/// limited route answers with as headers.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Decision {
    /// Whether the request fit in the window's budget.
    pub allowed: bool,
    /// The bucket's budget, `x-ratelimit-limit`.
    pub limit: u64,
    /// Requests left in the window after this one, `x-ratelimit-remaining`.
    pub remaining: u64,
    /// Whole seconds until the window ends, `x-ratelimit-reset` and the
    /// `Retry-After` of a refusal.
    pub reset_secs: u64,
}

#[cfg(test)]
mod tests {
    use std::time::Duration;

    use super::super::{clock::ManualClock, parse_limits, test_support::single_bucket};
    use super::*;

    /// The limiter built straight from a config runs on exactly the
    /// config's table: buckets it names are limited, the rest are not.
    #[test]
    fn limiter_from_config_uses_the_table() {
        let mut config = Config::for_tests();
        config.rate_limit.limits = parse_limits("authentication=3:60").expect("valid override");
        let limiter = RateLimiter::from_config(&config);
        assert!(limiter.limited("authentication"), "the bucket is limited");
        assert!(
            !limiter.limited("statuses_actions"),
            "a bucket off the table is unlimited"
        );
        assert_eq!(limiter.check("statuses_actions", "ip:1"), None, "None");
    }

    /// Window math on an injected clock: budget, remaining, reset, refusal
    /// at the limit, rotation when the window ends.
    #[test]
    fn fixed_window_math_on_an_injected_clock() {
        let clock = Arc::new(ManualClock::new());
        let limiter = limiter_with("search", 3, 10, Arc::clone(&clock));
        let first = limiter.check("search", "ip:10.0.0.1").expect("limited");
        assert_eq!(
            first,
            Decision {
                allowed: true,
                limit: 3,
                remaining: 2,
                reset_secs: 10
            },
            "the opening request sees the full window"
        );
        clock.advance(Duration::from_secs(4));
        let second = limiter.check("search", "ip:10.0.0.1").expect("limited");
        assert_eq!(
            (second.allowed, second.remaining, second.reset_secs),
            (true, 1, 6),
            "remaining decrements and reset counts down inside the window"
        );
        let third = limiter.check("search", "ip:10.0.0.1").expect("limited");
        assert_eq!(
            (third.allowed, third.remaining, third.reset_secs),
            (true, 0, 6),
            "the last request of the budget passes with none left"
        );
        let fourth = limiter.check("search", "ip:10.0.0.1").expect("limited");
        assert!(
            !fourth.allowed && fourth.remaining == 0 && fourth.reset_secs == 6,
            "over the limit is refused inside the window: {fourth:?}"
        );
        clock.advance(Duration::from_secs(6));
        let fifth = limiter.check("search", "ip:10.0.0.1").expect("limited");
        assert_eq!(
            (fifth.allowed, fifth.remaining, fifth.reset_secs),
            (true, 2, 10),
            "the instant the window ends, a fresh one opens"
        );
    }

    /// Separate keys keep separate budgets: distinct identities in one
    /// bucket, the same identity in distinct buckets.
    #[test]
    fn budgets_are_per_bucket_and_per_identity() {
        let clock = Arc::new(ManualClock::new());
        let mut limits = single_bucket("authentication", 1, 5);
        let _added = limits.insert(
            "app_account_creation".to_owned(),
            RateLimit {
                max: 1,
                window: Duration::from_secs(5),
            },
        );
        let limiter = RateLimiter::with_clock(&limits, clock);
        assert!(
            limiter
                .check(
                    "authentication",
                    "user:11111111-1111-1111-1111-111111111111"
                )
                .is_some_and(|decision| decision.allowed),
            "the first user passes"
        );
        assert!(
            !limiter
                .check(
                    "authentication",
                    "user:11111111-1111-1111-1111-111111111111"
                )
                .is_some_and(|decision| decision.allowed),
            "the same user is spent"
        );
        assert!(
            limiter
                .check(
                    "authentication",
                    "user:22222222-2222-2222-2222-222222222222"
                )
                .is_some_and(|decision| decision.allowed),
            "another user starts their own window"
        );
        assert!(
            limiter
                .check("authentication", "ip:203.0.113.7:51234")
                .is_some_and(|decision| decision.allowed),
            "an anonymous client starts its own window"
        );
        assert!(
            limiter
                .check(
                    "app_account_creation",
                    "user:11111111-1111-1111-1111-111111111111"
                )
                .is_some_and(|decision| decision.allowed),
            "the spent user still has the other bucket"
        );
    }

    /// A window nobody has touched again is pruned when a new key inserts.
    #[test]
    fn expired_windows_are_pruned_on_insert() {
        let clock = Arc::new(ManualClock::new());
        let limiter = limiter_with("search", 5, 10, Arc::clone(&clock));
        let _first = limiter.check("search", "ip:one").expect("limited");
        assert_eq!(window_count(&limiter), 1, "one live window");
        let _second = limiter.check("search", "ip:two").expect("limited");
        assert_eq!(window_count(&limiter), 2, "the second key joins it");
        clock.advance(Duration::from_secs(10));
        let _third = limiter.check("search", "ip:three").expect("limited");
        assert_eq!(
            window_count(&limiter),
            1,
            "inserting a new key dropped the two expired windows"
        );
    }

    /// A bucket's `max` of `0` refuses every request, including the first.
    #[test]
    fn zero_max_refuses_everything() {
        let clock = Arc::new(ManualClock::new());
        let limiter = limiter_with("search", 0, 60, clock);
        let decision = limiter.check("search", "ip:1").expect("limited");
        assert!(
            !decision.allowed && decision.remaining == 0 && decision.limit == 0,
            "a closed bucket: {decision:?}"
        );
    }

    /// A limiter over `clock` with one `name` bucket of `max` per
    /// `secs` seconds.
    fn limiter_with(name: &str, max: u64, secs: u64, clock: Arc<ManualClock>) -> RateLimiter {
        RateLimiter::with_clock(&single_bucket(name, max, secs), clock)
    }

    /// The live windows currently held; a poisoned lock is treated as
    /// empty, which it cannot be here.
    fn window_count(limiter: &RateLimiter) -> usize {
        limiter.windows.lock().map_or(0, |guard| guard.len())
    }
}
