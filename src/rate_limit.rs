//! In-memory fixed-window rate limiting, applied per route bucket.
//!
//! The port of Pleroma's `Pleroma.Web.Plugs.RateLimiter`
//! (`lib/pleroma/web/plugs/rate_limiter.ex`) and its
//! `config :pleroma, :rate_limit` buckets. Where Pleroma keeps its windows
//! in ETS, this is one [`RateLimiter`] per process: a `Mutex<HashMap>`
//! inside an `Arc` (no external crate), keyed by bucket name plus the
//! request's subject — the authenticated user id when the request presents
//! a valid bearer token, else the client IP. Expired windows are pruned
//! whenever a new key is inserted, so the map only ever holds live
//! windows.
//!
//! [`rate_limit`] builds the axum middleware layer for one bucket through
//! [`axum::middleware::from_fn_with_state`]; area routers wrap their
//! throttled routes with it. Every response on a limited route answers the
//! `x-ratelimit-limit`, `x-ratelimit-remaining` and `x-ratelimit-reset`
//! (whole seconds until the window ends) headers; over the limit the
//! request is refused with 429 `{"error":"Throttled"}` (decision 8's
//! shape, Pleroma's wording) and a `Retry-After`.
//!
//! The client IP is the socket peer — [`serve`] wires
//! `into_make_service_with_connect_info` so [`ConnectInfo`] carries it.
//! The leftmost `X-Forwarded-For` entry is honored *only* when
//! `TRUST_PROXY_HEADERS=true` (off by default), and only then may a client
//! name itself into someone else's window. A limited route reached without
//! a socket peer (an in-process test request carrying no `ConnectInfo`)
//! shares the `unknown` subject.
//!
//! The buckets and their Pleroma defaults live in [`DEFAULT_LIMITS`];
//! `RATE_LIMITS` overrides entries of that table and adds new ones (see
//! [`parse_limits`]), and a bucket absent from the table is unlimited.
//! `RATE_LIMITS_ENABLED=false` (on by default) switches the middleware to
//! a plain pass-through: no counting, no headers.
//!
//! Three tickets have mounted buckets so far: WBE-56 the ones whose routes
//! already existed — `authentication` on `POST /oauth/token` and
//! `/oauth/authorize`, and `app_account_creation` on `POST /api/v1/apps`
//! and `POST /api/v1/accounts`; WBE-135 `password_reset` on
//! `POST /auth/password`; WBE-120 `statuses_actions` on
//! `POST /api/v1/reports`, joined by WBE-80's `POST
//! /api/v1/statuses/{id}/favourite` and its `…/unfavourite` twin (see
//! [`crate::api::statuses`]). WBE-115 adds `search` on the three account
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

use std::{
    collections::{BTreeMap, HashMap},
    fmt,
    future::Future,
    net::SocketAddr,
    pin::Pin,
    sync::{Arc, Mutex, MutexGuard, PoisonError},
    time::{Duration, Instant},
};

use axum::{
    extract::{ConnectInfo, Request, State},
    http::{HeaderMap, HeaderName, HeaderValue, header},
    middleware::{self, FromFnLayer, Next},
    response::{IntoResponse as _, Response},
};
use tracing::warn;

use crate::{
    auth::{bearer_from_headers, bearer_from_query},
    config::Config,
    error::AppError,
    state::AppState,
};

/// The limiter's buckets with their Pleroma defaults, as
/// `(name, max requests, window seconds)` triples.
///
/// The numbers of Pleroma's `config :pleroma, :rate_limit`: `authentication`
/// 20 per 5 s, `app_account_creation` 5 per 1800 s, `relations_actions`
/// 10 per 10 s, `relation_id_action` 2 per 60 s, `statuses_actions`
/// 10 per 10 s, `status_id_action` 2 per 60 s, `password_reset` 5 per
/// 1800 s, `account_confirmation_resend` 5 per 8600 s, and `search`
/// 30 per 60 s.
pub const DEFAULT_LIMITS: [(&str, u64, u64); 9] = [
    ("account_confirmation_resend", 5, 8_600),
    ("app_account_creation", 5, 1_800),
    ("authentication", 20, 5),
    ("password_reset", 5, 1_800),
    ("relation_id_action", 2, 60),
    ("relations_actions", 10, 10),
    ("search", 30, 60),
    ("status_id_action", 2, 60),
    ("statuses_actions", 10, 10),
];

/// The `x-ratelimit-limit` response header: the bucket's request budget.
const LIMIT_HEADER: HeaderName = HeaderName::from_static("x-ratelimit-limit");
/// The `x-ratelimit-remaining` response header: requests left in the
/// current window.
const REMAINING_HEADER: HeaderName = HeaderName::from_static("x-ratelimit-remaining");
/// The `x-ratelimit-reset` response header: whole seconds until the
/// current window ends.
const RESET_HEADER: HeaderName = HeaderName::from_static("x-ratelimit-reset");
/// The `X-Forwarded-For` request header, read only when
/// `TRUST_PROXY_HEADERS` is on.
const FORWARDED_FOR_HEADER: HeaderName = HeaderName::from_static("x-forwarded-for");
/// The subject of a limited request that names neither a user nor a client
/// IP: an in-process test request carrying no `ConnectInfo`.
const UNKNOWN_SUBJECT: &str = "unknown";

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

/// Where the limiter reads the time: the seam that lets tests move the
/// clock instead of sleeping.
pub trait Clock: fmt::Debug + Send + Sync {
    /// The current instant.
    fn now(&self) -> Instant;
}

/// The production [`Clock`]: the monotonic `Instant::now`.
#[derive(Clone, Copy, Debug)]
pub struct SystemClock;

impl Clock for SystemClock {
    fn now(&self) -> Instant {
        Instant::now()
    }
}

/// A [`Clock`] that stands still until a test advances it by hand.
#[derive(Debug)]
pub struct ManualClock {
    /// The instant the clock started at.
    start: Instant,
    /// Nanoseconds it has been advanced by.
    advanced: std::sync::atomic::AtomicU64,
}

impl ManualClock {
    /// A clock standing at the real [`Instant::now`], advanced by nothing.
    #[must_use]
    pub fn new() -> Self {
        Self {
            start: Instant::now(),
            advanced: std::sync::atomic::AtomicU64::new(0),
        }
    }

    /// Moves the clock `by` into its future.
    pub fn advance(&self, by: Duration) {
        let nanos = u64::try_from(by.as_nanos()).unwrap_or(u64::MAX);
        let _total = self
            .advanced
            .fetch_add(nanos, std::sync::atomic::Ordering::Relaxed);
    }
}

impl Default for ManualClock {
    fn default() -> Self {
        Self::new()
    }
}

impl Clock for ManualClock {
    fn now(&self) -> Instant {
        let nanos = self.advanced.load(std::sync::atomic::Ordering::Relaxed);
        self.start + Duration::from_nanos(nanos)
    }
}

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

/// The state [`rate_limit`] bakes into the layer through
/// [`axum::middleware::from_fn_with_state`]: the application state plus
/// the bucket its route counts into.
#[derive(Clone, Debug)]
pub struct RateLimited {
    /// Shared state: the limiter, the config, and the pool that resolves
    /// bearer tokens.
    app: AppState,
    /// The bucket name the route counts into.
    bucket: &'static str,
}

impl RateLimited {
    /// Bundles `app` with `bucket`.
    #[must_use]
    pub fn new(app: &AppState, bucket: &'static str) -> Self {
        Self {
            app: app.clone(),
            bucket,
        }
    }
}

/// The boxed future [`limiter`] hands back; the `fn`-pointer shape
/// [`middleware::from_fn_with_state`] accepts as its function argument.
type LimiterFuture = Pin<Box<dyn Future<Output = Response> + Send>>;

/// The middleware function as a nameable pointer type.
type LimiterFn = fn(State<RateLimited>, Request, Next) -> LimiterFuture;

/// The layer [`rate_limit`] answers with: [`middleware::from_fn_with_state`]
/// over the `limiter` middleware function and a [`RateLimited`] state.
pub type RateLimitLayer = FromFnLayer<LimiterFn, RateLimited, (State<RateLimited>, Request)>;

/// The rate-limit middleware for bucket `bucket`, counting against the
/// limiter, config and pool of `app`.
///
/// Apply it to a route or method router with `.layer(rate_limit(state,
/// "authentication"))`; it is [`axum::middleware::from_fn_with_state`]
/// with a [`RateLimited`] state baked in. A bucket absent from the table
/// and a disabled limiter are plain pass-throughs; every other request is
/// counted and answered with the `x-ratelimit-*` headers.
pub fn rate_limit(app: &AppState, bucket: &'static str) -> RateLimitLayer {
    let middleware_fn: LimiterFn = limiter;
    middleware::from_fn_with_state(RateLimited::new(app, bucket), middleware_fn)
}

/// The middleware function, spelled as a plain `fn` so its type is
/// nameable in [`RateLimitLayer`]; the work happens in [`throttle`].
fn limiter(ctx: State<RateLimited>, request: Request, next: Next) -> LimiterFuture {
    Box::pin(throttle(ctx.0, request, next))
}

/// Counts one request in its bucket's window: run and decorate it when it
/// fits, refuse it with 429 when it does not, pass it through untouched
/// when nothing limits it.
async fn throttle(ctx: RateLimited, request: Request, next: Next) -> Response {
    let app = ctx.app;
    if !app.config.rate_limit.enabled.get() || !app.rate_limiter.limited(ctx.bucket) {
        return next.run(request).await;
    }
    // Everything the request itself must say is read before the first
    // await: a borrow of it across one would not be `Send`.
    let token = bearer_from_headers(request.headers()).or_else(|| bearer_from_query(request.uri()));
    let client_key = format!("ip:{}", client(&app, &request));
    let identity = authenticated_subject(&app, token.as_deref(), client_key).await;
    let Some(decision) = app.rate_limiter.check(ctx.bucket, &identity) else {
        return next.run(request).await;
    };
    if !decision.allowed {
        return refused(&decision);
    }
    let mut response = next.run(request).await;
    add_limit_headers(response.headers_mut(), &decision);
    response
}

/// What a limited request is counted against: `user:<id>` when it presents
/// a valid user-bearing bearer token (Pleroma's `:oauth_token` subject,
/// resolved to the account here), `ip:<client>` for everything else. An
/// unresolvable token — app-only, expired, unknown — and a failed lookup
/// both count by client, never against the token's owner-to-be.
async fn authenticated_subject(app: &AppState, token: Option<&str>, client_key: String) -> String {
    match token {
        Some(raw) => match crate::auth::credential(app, raw).await {
            Ok(Some(crate::auth::AuthContext {
                user: Some(user), ..
            })) => format!("user:{}", user.id),
            Ok(_) => client_key,
            Err(err) => {
                warn!(%err, "rate limit token lookup failed; counting the request by client");
                client_key
            }
        },
        None => client_key,
    }
}

/// The client an anonymous request is counted against: the leftmost
/// `X-Forwarded-For` entry while `TRUST_PROXY_HEADERS` is on, else (or
/// when the header says nothing usable) the socket peer; [`UNKNOWN_SUBJECT`]
/// when even that is absent.
fn client(app: &AppState, request: &Request) -> String {
    if app.config.rate_limit.trust_proxy_headers.get()
        && let Some(forwarded) = forwarded_for(request)
    {
        return forwarded;
    }
    request
        .extensions()
        .get::<ConnectInfo<SocketAddr>>()
        .map_or_else(
            || UNKNOWN_SUBJECT.to_owned(),
            |ConnectInfo(addr)| addr.to_string(),
        )
}

/// The leftmost, trimmed, non-empty `X-Forwarded-For` entry — the first hop
/// the trusted proxy appended; `None` without one.
fn forwarded_for(request: &Request) -> Option<String> {
    let value = request
        .headers()
        .get(&FORWARDED_FOR_HEADER)?
        .to_str()
        .ok()?;
    let first = value.split(',').next()?.trim();
    (!first.is_empty()).then(|| first.to_owned())
}

/// The 429 refusal: decision 8's `{"error":"Throttled"}` body with the
/// limit headers and `Retry-After` seconds until the window ends.
fn refused(decision: &Decision) -> Response {
    let mut response = AppError::RateLimited.into_response();
    add_limit_headers(response.headers_mut(), decision);
    let _previous = response
        .headers_mut()
        .insert(header::RETRY_AFTER, header_value(decision.reset_secs));
    response
}

/// Stamps the three `x-ratelimit-*` headers of `decision` on `headers`.
fn add_limit_headers(headers: &mut HeaderMap, decision: &Decision) {
    for (name, value) in [
        (&LIMIT_HEADER, decision.limit),
        (&REMAINING_HEADER, decision.remaining),
        (&RESET_HEADER, decision.reset_secs),
    ] {
        let _previous = headers.insert(name.clone(), header_value(value));
    }
}

/// A number as a header value; decimal text is always header-safe, so the
/// parse error arm only exists to avoid a panic.
fn header_value(value: u64) -> HeaderValue {
    value
        .to_string()
        .parse()
        .unwrap_or_else(|_invalid| HeaderValue::from_static("0"))
}

/// The window's remaining time in whole seconds, rounded up: a client
/// told "600" at the window's opening is still inside it when the 600th
/// second ticks.
fn seconds_until(remaining: Duration) -> u64 {
    remaining.as_secs() + u64::from(remaining.subsec_nanos() != 0)
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
    use std::sync::Arc;

    use axum::{
        Router,
        body::Body,
        http::{Request, StatusCode, header::AUTHORIZATION},
        routing::post,
    };
    use http_body_util::BodyExt as _;
    use sqlx::postgres::PgPoolOptions;
    use tower::ServiceExt as _;

    use super::*;
    use crate::{
        config::Config,
        http_client::HttpClient,
        models::{
            oauth_app::{NewOauthApp, create as create_app},
            oauth_token::{NewToken, create_token},
            user::{NewUser, insert},
        },
        streaming::hub::Hub,
        uploads::UploaderBackend,
    };

    /// The nine buckets and their Pleroma budgets, as the ticket fixes them.
    #[test]
    fn default_limits_carry_the_pleroma_buckets() {
        let limits = default_limits();
        let want: [(&str, u64, u64); 9] = [
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
        assert_eq!(limits.len(), want.len(), "exactly the nine buckets");
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

    /// A request through the middleware gets the three `x-ratelimit-*`
    /// headers and the handler's answer; over the limit comes the 429
    /// `{"error":"Throttled"}` with `Retry-After`, and the next window
    /// takes the route again.
    #[tokio::test]
    async fn middleware_counts_windows_and_refuses_over_the_limit() {
        let clock = Arc::new(ManualClock::new());
        let app = auth_app(&clock, 2, FLAGS_ON);
        let opener = app
            .clone()
            .oneshot(limited_post("203.0.113.1:1"))
            .await
            .unwrap();
        assert_eq!(opener.status(), StatusCode::OK, "the handler answers");
        assert_eq!(header_of(&opener, "x-ratelimit-limit"), "2", "limit");
        assert_eq!(
            header_of(&opener, "x-ratelimit-remaining"),
            "1",
            "remaining decrements"
        );
        assert_eq!(header_of(&opener, "x-ratelimit-reset"), "10", "reset");
        let spent = app
            .clone()
            .oneshot(limited_post("203.0.113.1:1"))
            .await
            .unwrap();
        assert_eq!(
            header_of(&spent, "x-ratelimit-remaining"),
            "0",
            "the budget is spent"
        );
        let mut refused = app
            .clone()
            .oneshot(limited_post("203.0.113.1:1"))
            .await
            .unwrap();
        assert_eq!(
            refused.status(),
            StatusCode::TOO_MANY_REQUESTS,
            "over the limit"
        );
        assert_eq!(
            body_of(&mut refused).await,
            r#"{"error":"Throttled"}"#,
            "Pleroma's wording in decision 8's shape"
        );
        assert_eq!(header_of(&refused, "retry-after"), "10", "Retry-After");
        assert_eq!(header_of(&refused, "x-ratelimit-limit"), "2", "headers");
        clock.advance(Duration::from_secs(10));
        let later = app.oneshot(limited_post("203.0.113.1:1")).await.unwrap();
        assert_eq!(
            later.status(),
            StatusCode::OK,
            "the next window lets it through"
        );
    }

    /// Distinct socket peers keep distinct windows, and the same peer is
    /// spent even on the sibling route of the same bucket.
    #[tokio::test]
    async fn separate_keys_for_separate_clients() {
        let clock = Arc::new(ManualClock::new());
        let app = auth_app(&clock, 1, FLAGS_ON);
        let first = app
            .clone()
            .oneshot(limited_post("203.0.113.1:1"))
            .await
            .unwrap();
        assert_eq!(first.status(), StatusCode::OK, "first client passes");
        let twin = app
            .clone()
            .oneshot(limited_post("203.0.113.2:2"))
            .await
            .unwrap();
        assert_eq!(
            twin.status(),
            StatusCode::OK,
            "a second client has its own window"
        );
        let sibling = app
            .clone()
            .oneshot(
                Request::post("/also-limited")
                    .extension(ConnectInfo("203.0.113.1:1".parse::<SocketAddr>().unwrap()))
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(
            sibling.status(),
            StatusCode::TOO_MANY_REQUESTS,
            "the first client is spent even on the sibling route of the same bucket"
        );
    }

    /// `X-Forwarded-For` names the client only while
    /// `TRUST_PROXY_HEADERS` is on; while off, every request from one
    /// socket shares one window whatever the header claims.
    #[tokio::test]
    async fn forwarded_for_is_honored_only_when_trusted() {
        let trusted_clock = Arc::new(ManualClock::new());
        let trusted_app = auth_app(
            &trusted_clock,
            1,
            Flags {
                enabled: true,
                trust: true,
            },
        );
        let alice = trusted_app
            .clone()
            .oneshot(forwarded_post("10.0.0.1:1", "203.0.113.1, 70.41.3.18"))
            .await
            .unwrap();
        assert_eq!(alice.status(), StatusCode::OK, "the first hop passes");
        let bob = trusted_app
            .oneshot(forwarded_post("10.0.0.1:1", "203.0.113.2, 70.41.3.18"))
            .await
            .unwrap();
        assert_eq!(
            bob.status(),
            StatusCode::OK,
            "another forwarded client behind the same proxy keeps its own window"
        );

        let plain_clock = Arc::new(ManualClock::new());
        let plain_app = auth_app(&plain_clock, 1, FLAGS_ON);
        let one = plain_app
            .clone()
            .oneshot(forwarded_post("10.0.0.1:1", "203.0.113.1"))
            .await
            .unwrap();
        assert_eq!(one.status(), StatusCode::OK, "the first passes");
        let two = plain_app
            .oneshot(forwarded_post("10.0.0.1:1", "203.0.113.2"))
            .await
            .unwrap();
        assert_eq!(
            two.status(),
            StatusCode::TOO_MANY_REQUESTS,
            "while untrusted the header is noise: one socket, one window"
        );
    }

    /// `RATE_LIMITS_ENABLED=false` turns the middleware into a
    /// pass-through: no counting, no headers, no refusal.
    #[tokio::test]
    async fn disabled_limiter_passes_through() {
        let clock = Arc::new(ManualClock::new());
        let app = auth_app(
            &clock,
            1,
            Flags {
                enabled: false,
                trust: false,
            },
        );
        for _ in 0..5 {
            let answer = app
                .clone()
                .oneshot(limited_post("203.0.113.9:9"))
                .await
                .unwrap();
            assert_eq!(answer.status(), StatusCode::OK, "never throttled");
            assert!(
                answer.headers().get("x-ratelimit-limit").is_none(),
                "a disabled limiter answers no limit headers"
            );
        }
    }

    /// A bucket absent from the table is unlimited: no headers, no refusals.
    #[tokio::test]
    async fn unlisted_bucket_is_unlimited() {
        let clock = Arc::new(ManualClock::new());
        let app = app_with(&BTreeMap::new(), &clock, FLAGS_ON);
        for _ in 0..5 {
            let answer = app
                .clone()
                .oneshot(limited_post("203.0.113.9:9"))
                .await
                .unwrap();
            assert_eq!(answer.status(), StatusCode::OK, "nothing limits");
        }
    }

    /// A valid user bearer token keys the window on the user id, not the
    /// socket: the user is spent through another IP, while another user and
    /// an anonymous request keep their own budgets.
    #[tokio::test]
    async fn bearer_token_keys_on_the_user_id() {
        let db = crate::test_db::test_db().await.expect("test database slot");
        let pool = db.pool().clone();
        let clock = Arc::new(ManualClock::new());
        let app = app_on_pool(&auth_limits(2), &clock, FLAGS_ON, pool.clone());
        let first = login(&pool, "ratelimited-first").await;
        let second = login(&pool, "ratelimited-second").await;
        let opener = app
            .clone()
            .oneshot(limited_request(&first, Some("203.0.113.1:1")))
            .await
            .unwrap();
        assert_eq!(
            opener.status(),
            StatusCode::OK,
            "the user's first request passes"
        );
        let elsewhere = app
            .clone()
            .oneshot(limited_request(&first, Some("203.0.113.2:2")))
            .await
            .unwrap();
        assert_eq!(
            elsewhere.status(),
            StatusCode::OK,
            "one user's two sockets are one window"
        );
        let refused = app
            .clone()
            .oneshot(limited_request(&first, Some("203.0.113.1:1")))
            .await
            .unwrap();
        assert_eq!(
            refused.status(),
            StatusCode::TOO_MANY_REQUESTS,
            "the user's window is spent regardless of the socket"
        );
        let twin = app
            .clone()
            .oneshot(limited_request(&second, Some("203.0.113.1:1")))
            .await
            .unwrap();
        assert_eq!(
            twin.status(),
            StatusCode::OK,
            "another user brings their own window"
        );
        let anonymous = app.oneshot(limited_request("", None)).await.unwrap();
        assert_eq!(
            anonymous.status(),
            StatusCode::OK,
            "an anonymous request counts by IP"
        );
    }

    /// A bearer token that resolves to nothing (no such row, or a database
    /// the limiter cannot reach) never blocks: the request counts by client
    /// and the route keeps answering.
    #[tokio::test]
    async fn unresolvable_token_falls_back_to_the_client() {
        let clock = Arc::new(ManualClock::new());
        let app = auth_app(&clock, 1, FLAGS_ON);
        let pass = app
            .clone()
            .oneshot(limited_request("no-such-token", Some("203.0.113.1:1")))
            .await
            .unwrap();
        assert_eq!(
            pass.status(),
            StatusCode::OK,
            "the lazy pool's failed lookup is not fatal"
        );
        let reject = app
            .oneshot(limited_request("no-such-token", Some("203.0.113.1:1")))
            .await
            .unwrap();
        assert_eq!(
            reject.status(),
            StatusCode::TOO_MANY_REQUESTS,
            "the same client is spent by IP"
        );
    }

    /// A limiter over `clock` with one `name` bucket of `max` per
    /// `secs` seconds.
    fn limiter_with(name: &str, max: u64, secs: u64, clock: Arc<ManualClock>) -> RateLimiter {
        RateLimiter::with_clock(&single_bucket(name, max, secs), clock)
    }

    /// The one-bucket table: `max` requests per `secs` in `name`.
    fn single_bucket(name: &str, max: u64, secs: u64) -> BTreeMap<String, RateLimit> {
        let mut limits = BTreeMap::new();
        let _absent = limits.insert(
            name.to_owned(),
            RateLimit {
                max,
                window: Duration::from_secs(secs),
            },
        );
        limits
    }

    /// The `authentication` table of `max` requests per ten seconds.
    fn auth_limits(max: u64) -> BTreeMap<String, RateLimit> {
        single_bucket("authentication", max, 10)
    }

    /// The live windows currently held; a poisoned lock is treated as
    /// empty, which it cannot be here.
    fn window_count(limiter: &RateLimiter) -> usize {
        limiter.windows.lock().map_or(0, |guard| guard.len())
    }

    /// What a middleware test app does with its bucket: whether the
    /// limiter is on, and whether the proxy header may name the client.
    #[derive(Clone, Copy, Debug)]
    struct Flags {
        /// The value of `RATE_LIMITS_ENABLED`.
        enabled: bool,
        /// The value of `TRUST_PROXY_HEADERS`.
        trust: bool,
    }

    /// The flags every middleware test runs on unless it names others:
    /// limiting, unproxied.
    const FLAGS_ON: Flags = Flags {
        enabled: true,
        trust: false,
    };

    /// The middleware app over the `authentication` bucket of `max`
    /// requests per ten seconds.
    fn auth_app(clock: &Arc<ManualClock>, max: u64, flags: Flags) -> Router {
        app_with(&auth_limits(max), clock, flags)
    }

    /// [`app_with`] over the lazy pool, which token lookups fall through.
    fn app_with(
        limits: &BTreeMap<String, RateLimit>,
        clock: &Arc<ManualClock>,
        flags: Flags,
    ) -> Router {
        app_on_pool(limits, clock, flags, lazy_pool())
    }

    /// The state the middleware tests run on: `limits` over `clock`, the
    /// two `flags` in the config, `db` behind them, and one router with
    /// both routes mounted in the `authentication` bucket.
    fn app_on_pool(
        limits: &BTreeMap<String, RateLimit>,
        clock: &Arc<ManualClock>,
        flags: Flags,
        db: sqlx::PgPool,
    ) -> Router {
        let mut config = Config::for_tests();
        config.rate_limit.limits = limits.clone();
        config.rate_limit.enabled = crate::config::Toggle::new(flags.enabled);
        config.rate_limit.trust_proxy_headers = crate::config::Toggle::new(flags.trust);
        let config = Arc::new(config);
        let owned = Arc::clone(clock);
        let time: Arc<dyn Clock> = owned;
        let state = AppState {
            hub: Hub::new(&db),
            db,
            config: Arc::clone(&config),
            http_client: HttpClient::from(config.as_ref()),
            uploader: UploaderBackend::from(config.as_ref()),
            instance_stats_cache: Arc::default(),
            rate_limiter: Arc::new(RateLimiter::with_clock(limits, time)),
            mailer: Arc::new(crate::mailer::MemoryTransport::new()),
        };
        let layer = rate_limit(&state, "authentication");
        Router::new()
            .route("/limited", post(echo).layer(layer.clone()))
            .route("/also-limited", post(echo).layer(layer))
            .with_state(state)
    }

    /// A pool that never connects: token lookups fail, everything else
    /// does not need it.
    fn lazy_pool() -> sqlx::PgPool {
        PgPoolOptions::new()
            .connect_lazy("postgres://localhost/unused")
            .expect("a lazy pool needs no database")
    }

    /// The guarded route: a plain 200.
    async fn echo() -> StatusCode {
        StatusCode::OK
    }

    /// A `POST /limited` from socket peer `addr`.
    fn limited_post(addr: &str) -> Request<Body> {
        Request::post("/limited")
            .extension(ConnectInfo(
                addr.parse::<SocketAddr>().expect("the test peer parses"),
            ))
            .body(Body::empty())
            .expect("the request is valid")
    }

    /// A `POST /limited` from socket peer `addr` carrying `xff` as its
    /// `X-Forwarded-For` header.
    fn forwarded_post(addr: &str, xff: &str) -> Request<Body> {
        let (mut parts, body) = limited_post(addr).into_parts();
        let value = xff
            .parse()
            .expect("the test forwarded-for value is header-safe");
        let _previous = parts.headers.insert("x-forwarded-for", value);
        Request::from_parts(parts, body)
    }

    /// A `POST /limited` optionally presenting `token` and a socket peer.
    fn limited_request(token: &str, ip: Option<&str>) -> Request<Body> {
        let mut builder = Request::post("/limited");
        if let Some(peer) = ip {
            builder = builder.extension(ConnectInfo(
                peer.parse::<SocketAddr>().expect("the test peer parses"),
            ));
        }
        if !token.is_empty() {
            builder = builder.header(AUTHORIZATION, format!("Bearer {token}"));
        }
        builder.body(Body::empty()).expect("the request is valid")
    }

    /// A live user token for `nickname`, minted through the models.
    async fn login(pool: &sqlx::PgPool, nickname: &str) -> String {
        let user = insert(
            pool,
            &NewUser::local(nickname, &format!("https://test.example/users/{nickname}")),
        )
        .await
        .expect("user fixture inserts");
        let client = create_app(
            pool,
            &NewOauthApp {
                client_name: format!("{nickname} client"),
                redirect_uris: "https://client.example/callback".to_owned(),
                scopes: "read".to_owned(),
                website: None,
                user_id: Some(user.id),
            },
        )
        .await
        .expect("app fixture inserts");
        create_token(
            pool,
            &NewToken {
                user_id: Some(user.id),
                app_id: client.id,
                scopes: "read".to_owned(),
                ttl: None,
            },
        )
        .await
        .expect("token fixture inserts")
        .token
    }

    /// The named header of a response, as text.
    fn header_of(response: &Response, name: &str) -> String {
        response
            .headers()
            .get(name)
            .unwrap_or_else(|| panic!("{name} header is set"))
            .to_str()
            .expect("the header is visible text")
            .to_owned()
    }

    /// The buffered body of a response, as text.
    async fn body_of(response: &mut Response) -> String {
        let bytes = response
            .body_mut()
            .collect()
            .await
            .expect("the body collects")
            .to_bytes();
        String::from_utf8_lossy(&bytes).into_owned()
    }
}
