//! The axum middleware layer that throttles one bucket's routes: the
//! `x-ratelimit-*` headers, the 429 refusal and the pass-through cases.

use std::{future::Future, pin::Pin, time::Duration};

use axum::{
    extract::{Request, State},
    http::{HeaderMap, HeaderName, HeaderValue, header},
    middleware::{self, FromFnLayer, Next},
    response::{IntoResponse as _, Response},
};

use super::{
    limiter::Decision,
    subject::{authenticated_subject, client},
};
use crate::{
    auth::{bearer_from_headers, bearer_from_query},
    error::AppError,
    state::AppState,
};

/// The `x-ratelimit-limit` response header: the bucket's request budget.
const LIMIT_HEADER: HeaderName = HeaderName::from_static("x-ratelimit-limit");
/// The `x-ratelimit-remaining` response header: requests left in the
/// current window.
const REMAINING_HEADER: HeaderName = HeaderName::from_static("x-ratelimit-remaining");
/// The `x-ratelimit-reset` response header: whole seconds until the
/// current window ends.
const RESET_HEADER: HeaderName = HeaderName::from_static("x-ratelimit-reset");
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
pub(super) fn seconds_until(remaining: Duration) -> u64 {
    remaining.as_secs() + u64::from(remaining.subsec_nanos() != 0)
}

#[cfg(test)]
mod tests {
    use std::{collections::BTreeMap, sync::Arc};

    use axum::http::StatusCode;
    use tower::ServiceExt as _;

    use super::super::{
        clock::ManualClock,
        test_support::{FLAGS_ON, Flags, app_with, auth_app, body_of, header_of, limited_post},
    };
    use super::*;

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
                hops: 1,
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
}
