//! Fixtures the rate limiter's unit tests share: bucket tables, the
//! middleware test app and its requests.

use std::{collections::BTreeMap, net::SocketAddr, sync::Arc, time::Duration};

use axum::{
    Router,
    body::Body,
    extract::ConnectInfo,
    http::{Request, StatusCode, header::AUTHORIZATION},
    response::Response,
    routing::post,
};
use http_body_util::BodyExt as _;
use sqlx::postgres::PgPoolOptions;

use super::{
    RateLimit,
    clock::{Clock, ManualClock},
    limiter::RateLimiter,
    middleware::rate_limit,
};
use crate::{
    config::Config,
    http_client::HttpClient,
    models::{
        oauth_app::{NewOauthApp, create as create_app},
        oauth_token::{NewToken, create_token},
        user::{NewUser, insert},
    },
    state::AppState,
    streaming::hub::Hub,
    uploads::UploaderBackend,
};

/// The one-bucket table: `max` requests per `secs` in `name`.
pub(super) fn single_bucket(name: &str, max: u64, secs: u64) -> BTreeMap<String, RateLimit> {
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
pub(super) fn auth_limits(max: u64) -> BTreeMap<String, RateLimit> {
    single_bucket("authentication", max, 10)
}

/// What a middleware test app does with its bucket: whether the
/// limiter is on, and whether the proxy header may name the client.
#[derive(Clone, Copy, Debug)]
pub(super) struct Flags {
    /// The value of `RATE_LIMITS_ENABLED`.
    pub(super) enabled: bool,
    /// The value of `TRUST_PROXY_HEADERS`.
    pub(super) trust: bool,
}

/// The flags every middleware test runs on unless it names others:
/// limiting, unproxied.
pub(super) const FLAGS_ON: Flags = Flags {
    enabled: true,
    trust: false,
};

/// The middleware app over the `authentication` bucket of `max`
/// requests per ten seconds.
pub(super) fn auth_app(clock: &Arc<ManualClock>, max: u64, flags: Flags) -> Router {
    app_with(&auth_limits(max), clock, flags)
}

/// [`app_with`] over the lazy pool, which token lookups fall through.
pub(super) fn app_with(
    limits: &BTreeMap<String, RateLimit>,
    clock: &Arc<ManualClock>,
    flags: Flags,
) -> Router {
    app_on_pool(limits, clock, flags, lazy_pool())
}

/// The state the middleware tests run on: `limits` over `clock`, the
/// two `flags` in the config, `db` behind them, and one router with
/// both routes mounted in the `authentication` bucket.
pub(super) fn app_on_pool(
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
        mrf_config_cache: Arc::default(),
        translation_languages_cache: Arc::default(),
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
pub(super) fn lazy_pool() -> sqlx::PgPool {
    PgPoolOptions::new()
        .connect_lazy("postgres://localhost/unused")
        .expect("a lazy pool needs no database")
}

/// The guarded route: a plain 200.
async fn echo() -> StatusCode {
    StatusCode::OK
}

/// A `POST /limited` from socket peer `addr`.
pub(super) fn limited_post(addr: &str) -> Request<Body> {
    Request::post("/limited")
        .extension(ConnectInfo(
            addr.parse::<SocketAddr>().expect("the test peer parses"),
        ))
        .body(Body::empty())
        .expect("the request is valid")
}

/// A `POST /limited` from socket peer `addr` carrying `xff` as its
/// `X-Forwarded-For` header.
pub(super) fn forwarded_post(addr: &str, xff: &str) -> Request<Body> {
    let (mut parts, body) = limited_post(addr).into_parts();
    let value = xff
        .parse()
        .expect("the test forwarded-for value is header-safe");
    let _previous = parts.headers.insert("x-forwarded-for", value);
    Request::from_parts(parts, body)
}

/// A `POST /limited` optionally presenting `token` and a socket peer.
pub(super) fn limited_request(token: &str, ip: Option<&str>) -> Request<Body> {
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
pub(super) async fn login(pool: &sqlx::PgPool, nickname: &str) -> String {
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
pub(super) fn header_of(response: &Response, name: &str) -> String {
    response
        .headers()
        .get(name)
        .unwrap_or_else(|| panic!("{name} header is set"))
        .to_str()
        .expect("the header is visible text")
        .to_owned()
}

/// The buffered body of a response, as text.
pub(super) async fn body_of(response: &mut Response) -> String {
    let bytes = response
        .body_mut()
        .collect()
        .await
        .expect("the body collects")
        .to_bytes();
    String::from_utf8_lossy(&bytes).into_owned()
}
