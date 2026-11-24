//! In-memory fixed-window rate limiting, applied per route bucket.
//!
//! The port of Pleroma's `Pleroma.Web.Plugs.RateLimiter`
//! (`lib/pleroma/web/plugs/rate_limiter.ex`) and its
//! `config :pleroma, :rate_limit` buckets. Where Pleroma keeps its windows
//! in ETS, this is one [`RateLimiter`] per process: a `Mutex<HashMap>`
//! inside an `Arc` (no external crate), keyed by bucket name plus the
//! request's subject — the authenticated user id when the request presents
//! a valid bearer token, else the client IP. Expired windows are pruned
//! when a new key is inserted, at most once a second, and past a soft cap
//! of held windows new subjects are refused as throttled.
//!
//! [`rate_limit`] builds the axum middleware layer for one bucket through
//! [`axum::middleware::from_fn_with_state`]; area routers wrap their
//! throttled routes with it. Every response on a limited route answers the
//! `x-ratelimit-limit`, `x-ratelimit-remaining` and `x-ratelimit-reset`
//! (whole seconds until the window ends) headers; over the limit the
//! request is refused with 429 `{"error":"Throttled"}` (decision 8's
//! shape, Pleroma's wording) and a `Retry-After`.
//!
//! The client IP is the socket peer's address — [`serve`] wires
//! `into_make_service_with_connect_info` so [`ConnectInfo`] carries it.
//! The source port is not part of the key (each connection draws a new
//! one, which would give every connection its own window), and an IPv6
//! client is keyed by its /64 prefix so one host cannot rotate through its
//! own prefix. `X-Forwarded-For` is honored *only* when
//! `TRUST_PROXY_HEADERS=true` (off by default), and then the client is the
//! entry `TRUSTED_PROXY_HOPS` (default 1) places from the *right* — the
//! address the outermost trusted proxy saw. A proxy appends the peer to
//! whatever the client sent, so entries further left are client-controlled
//! and never read; a header with fewer entries than hops, or whose chosen
//! entry is not an IP address, falls back to the socket peer. A limited route reached without
//! a socket peer (an in-process test request carrying no `ConnectInfo`)
//! shares the `unknown` subject.
//!
//! The buckets and their Pleroma defaults live in [`DEFAULT_LIMITS`];
//! `RATE_LIMITS` overrides entries of that table and adds new ones (see
//! [`parse_limits`]), and a bucket absent from the table is unlimited.
//! `RATE_LIMITS_ENABLED=false` (on by default) switches the middleware to
//! a plain pass-through: no counting, no headers.
//!
//! WBE-504 mounts `imports` on the three CSV import routes (see
//! [`crate::api::pleroma_import`]).
//!
//! Three tickets have mounted buckets so far: WBE-56 the ones whose routes
//! already existed — `authentication` on `POST /oauth/token` and
//! `/oauth/authorize`, and `app_account_creation` on `POST /api/v1/apps`
//! and `POST /api/v1/accounts`; WBE-135 `password_reset` on
//! `POST /auth/password`; WBE-120 `statuses_actions` on
//! `POST /api/v1/reports`, joined by WBE-76's `POST /api/v1/statuses` —
//! Pleroma's `@rate_limited_status_actions` keeps the status write and the
//! report filing in one window — by WBE-80's `POST
//! /api/v1/statuses/{id}/favourite`, its `…/unfavourite` twin and WBE-79's
//! `DELETE /api/v1/statuses/{id}` (see [`crate::api::statuses`]). WBE-115
//! adds `search` on the three account
//! search routes (`GET /api/v1/accounts/search`, `GET /api/v1/search`
//! and `GET /api/v2/search`, see [`crate::api::search`]). WBE-57 adds
//! `relations_actions` on `POST /api/v1/accounts/{id}/follow` and its
//! `…/unfollow` twin (see [`crate::api::accounts`]); every other
//! relationship write joins that one bucket as its ticket mounts it. The
//! remaining
//! buckets (`relation_id_action`, `status_id_action` and
//! `account_confirmation_resend`) are
//! already configured and counted by name; the later tickets that add
//! their routes attach [`rate_limit`] with the matching bucket there.
//!
//! [`serve`]: crate::serve

use std::{collections::BTreeMap, time::Duration};

#[cfg(doc)]
use axum::extract::ConnectInfo;

mod clock;
mod limiter;
mod middleware;
mod subject;
#[cfg(test)]
mod test_support;

pub use clock::{Clock, ManualClock, SystemClock};
pub use limiter::{Decision, RateLimiter};
pub use middleware::{RateLimitLayer, RateLimited, rate_limit};

/// The limiter's buckets with their Pleroma defaults, as
/// `(name, max requests, window seconds)` triples.
///
/// The numbers of Pleroma's `config :pleroma, :rate_limit`: `authentication`
/// 20 per 5 s, `app_account_creation` 5 per 1800 s, `relations_actions`
/// 10 per 10 s, `relation_id_action` 2 per 60 s, `statuses_actions`
/// 10 per 10 s, `status_id_action` 2 per 60 s, `password_reset` 5 per
/// 1800 s, `account_confirmation_resend` 5 per 8600 s, and `search`
/// 30 per 60 s. `imports` (1 per 3600 s) is this server's own: the CSV
/// follow, block and mute imports (WBE-504) each fan out to thousands of
/// outbound fetches, so a user gets one import per hour across the three.
pub const DEFAULT_LIMITS: [(&str, u64, u64); 10] = [
    ("account_confirmation_resend", 5, 8_600),
    ("app_account_creation", 5, 1_800),
    ("authentication", 20, 5),
    ("imports", 1, 3_600),
    ("password_reset", 5, 1_800),
    ("relation_id_action", 2, 60),
    ("relations_actions", 10, 10),
    ("search", 30, 60),
    ("status_id_action", 2, 60),
    ("statuses_actions", 10, 10),
];

/// The default table of [`DEFAULT_LIMITS`] as owned bucket names.
#[must_use]
pub fn default_limits() -> BTreeMap<String, RateLimit> {
    DEFAULT_LIMITS
        .iter()
        .map(|(name, max, secs)| {
            (
                (*name).to_owned(),
                RateLimit {
                    max: *max,
                    window: Duration::from_secs(*secs),
                },
            )
        })
        .collect()
}

/// One bucket's budget: at most [`max`](Self::max) requests per
/// [`window`](Self::window), Pleroma's `limit` and `period`.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct RateLimit {
    /// The number of requests one window allows; `0` refuses every
    /// request on the bucket.
    pub max: u64,
    /// The window's length; always at least one second.
    pub window: Duration,
}

/// `RATE_LIMITS` text: `bucket=max:window_secs` entries separated by
/// commas, Pleroma's `limit` and `period` flattened into one string.
///
/// Entries are trimmed; blank entries are ignored; a later entry naming an
/// earlier bucket wins. An empty text parses to an empty table (leaving
/// [`default_limits`] untouched).
///
/// # Errors
///
/// A human-readable reason, as [`crate::config::ConfigError::InvalidRateLimits`],
/// when an entry misses the `bucket=max:window_secs` shape, names no
/// bucket, holds a non-number, or sets a zero-length window.
pub fn parse_limits(text: &str) -> Result<BTreeMap<String, RateLimit>, String> {
    let mut limits = BTreeMap::new();
    for entry in text.split(',') {
        let entry = entry.trim();
        if entry.is_empty() {
            continue;
        }
        let (name, spec) = entry
            .split_once('=')
            .ok_or_else(|| format!("entry '{entry}' is not bucket=max:window_secs"))?;
        let name = name.trim();
        if name.is_empty() {
            return Err(format!("entry '{entry}' names no bucket"));
        }
        let (max, secs) = spec
            .split_once(':')
            .ok_or_else(|| format!("entry '{entry}' is not max:window_secs"))?;
        let max: u64 = max
            .trim()
            .parse()
            .map_err(|_not_a_number| format!("entry '{entry}' has a non-numeric limit"))?;
        let secs: u64 = secs
            .trim()
            .parse()
            .map_err(|_not_a_number| format!("entry '{entry}' has a non-numeric window"))?;
        if secs == 0 {
            return Err(format!("entry '{entry}' sets a zero-length window"));
        }
        let limit = RateLimit {
            max,
            window: Duration::from_secs(secs),
        };
        let _previous = limits.insert(name.to_owned(), limit);
    }
    Ok(limits)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The ten buckets and their budgets, as the tickets fix them.
    #[test]
    fn default_limits_carry_the_pleroma_buckets() {
        let limits = default_limits();
        let want: [(&str, u64, u64); 10] = [
            ("imports", 1, 3_600),
            ("authentication", 20, 5),
            ("app_account_creation", 5, 1_800),
            ("relations_actions", 10, 10),
            ("relation_id_action", 2, 60),
            ("statuses_actions", 10, 10),
            ("status_id_action", 2, 60),
            ("password_reset", 5, 1_800),
            ("account_confirmation_resend", 5, 8_600),
            ("search", 30, 60),
        ];
        assert_eq!(limits.len(), want.len(), "exactly the ten buckets");
        for (name, max, secs) in want {
            let limit = limits
                .get(name)
                .copied()
                .unwrap_or_else(|| panic!("{name} is a default bucket"));
            assert_eq!(limit.max, max, "{name} budget");
            assert_eq!(limit.window, Duration::from_secs(secs), "{name} window");
        }
    }

    /// `RATE_LIMITS` overrides replace entries of the table, unknown names
    /// join it, blanks and spaces are tolerated, and a repeat wins last.
    #[test]
    fn parse_limits_overrides_and_adds() {
        let parsed = parse_limits(
            " authentication = 40 : 10 ,, custom_bucket=1:1, search=99:1 , search=7:30",
        )
        .expect("the mixed list parses");
        assert_eq!(parsed.len(), 3, "three named buckets");
        assert_eq!(
            parsed.get("authentication").copied(),
            Some(RateLimit {
                max: 40,
                window: Duration::from_secs(10)
            }),
            "the override replaces the default number"
        );
        assert_eq!(
            parsed.get("custom_bucket").copied(),
            Some(RateLimit {
                max: 1,
                window: Duration::from_secs(1)
            }),
            "an unknown name joins the table"
        );
        assert_eq!(
            parsed.get("search").copied(),
            Some(RateLimit {
                max: 7,
                window: Duration::from_secs(30)
            }),
            "the last entry for a bucket wins"
        );
        assert!(
            parse_limits("").expect("the empty text parses").is_empty(),
            "nothing to override is an empty table"
        );
    }

    /// Every entry the parser refuses, with the reason naming it.
    #[test]
    fn parse_limits_rejects_malformed_entries() {
        for (entry, fragment) in [
            ("authentication", "not bucket=max:window_secs"),
            ("authentication=20", "not max:window_secs"),
            ("authentication=x:5", "non-numeric limit"),
            ("authentication=20:y", "non-numeric window"),
            ("authentication=20:0", "zero-length window"),
            ("=20:5", "names no bucket"),
        ] {
            let err = parse_limits(entry).expect_err("the malformed entry is refused");
            assert!(err.contains(fragment), "{entry}: {err}");
        }
    }
}
