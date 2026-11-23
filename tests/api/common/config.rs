//! The test database, the config variants and the router builders.

use std::path::Path;

use axum::Router;
use sqlx::PgPool;
use wired_be::{config::Config, state::AppState, test_db::TestDb, uploads::UploaderBackend};

/// A private, fully migrated, empty test database: the slot pool fixture from
/// [`wired_be::test_db`], handed over as [`TestDb`] with its [`PgPool`].
///
/// Every database test starts here and keeps the return value alive for as
/// long as it uses the pool: the fixture holds the lease on its slot
/// database, so dropping it frees the slot for the next test. Setup is
/// migrations and one `TRUNCATE`, not a database per test, which is what
/// `#[sqlx::test]` used to pay for every test in the suite.
///
/// # Panics
///
/// If `DATABASE_URL` is unset, or the template and slot databases cannot be
/// created, migrated or reset.
#[must_use]
pub async fn test_db() -> TestDb {
    wired_be::test_db::test_db()
        .await
        .expect("a migrated test database slot")
}

/// The configuration [`test_app`] runs with: `BASE_URL`
/// `https://test.example`, no environment access, and defaults everywhere
/// else.
///
/// That includes `RATE_LIMITS_ENABLED=false`, so a suite never trips a
/// limiter of its own; tests that want throttling build their own
/// [`Config`].
#[must_use]
pub fn test_config() -> Config {
    Config::for_tests()
}

/// [`test_config`] with the rate limiter on and `RATE_LIMITS` overridden
/// to the `bucket=max:window_secs` text, for the throttle tests in
/// `rate_limit.rs`.
///
/// # Panics
///
/// Never for the fixed `RATE_LIMITS` texts tests pass; a malformed one is
/// a bug in the calling test.
#[must_use]
pub fn test_config_with_rate_limits(rate_limits: &str) -> Config {
    Config::from_lookup(|key| match key {
        wired_be::config::DATABASE_URL_VAR => Some(wired_be::config::TEST_DATABASE_URL.to_owned()),
        wired_be::config::BASE_URL_VAR => Some(wired_be::config::TEST_BASE_URL.to_owned()),
        wired_be::config::RATE_LIMITS_ENABLED_VAR => Some("true".to_owned()),
        wired_be::config::RATE_LIMITS_VAR => Some(rate_limits.to_owned()),
        _ => None,
    })
    .expect("the fixed rate limit test config is valid")
}

/// [`test_config`] with its uploads moved to `uploads_dir`, for tests that
/// store files and must not write into the repository.
#[must_use]
pub fn test_config_with_uploads_dir(uploads_dir: &Path) -> Config {
    let mut config = test_config();
    config.uploads_dir = uploads_dir.to_path_buf();
    config
}

/// [`test_config`] with the emoji packs root moved to `emoji_dir`, for
/// tests that read packs from disk and must not touch the repository.
#[must_use]
pub fn test_config_with_emoji_dir(emoji_dir: &Path) -> Config {
    let mut config = test_config();
    config.emoji_dir = emoji_dir.to_path_buf();
    config
}

/// [`test_config`] with the instance-document store moved to `docs_dir`,
/// for tests that upload and read documents and must not write into the
/// repository. The document names keep their defaults.
#[must_use]
pub fn test_config_with_instance_docs_dir(docs_dir: &Path) -> Config {
    let mut config = test_config();
    config.statics.documents.dir = docs_dir.to_path_buf();
    config
}

/// The real application router, reading and writing instance documents
/// in `docs_dir`.
pub fn test_app_with_instance_docs_dir(pool: PgPool, docs_dir: &Path) -> Router {
    test_app_with_config(pool, test_config_with_instance_docs_dir(docs_dir))
}

/// The local-file [`UploaderBackend`] storing in `uploads_dir`: the same
/// storage [`test_app_with_uploads_dir`] serves from.
///
/// Use it to put or delete a file before requesting it over HTTP.
#[must_use]
pub fn test_uploader(uploads_dir: &Path) -> UploaderBackend {
    UploaderBackend::from(&test_config_with_uploads_dir(uploads_dir))
}

/// The real application router from [`wired_be::app`], serving over `pool`
/// and the [`test_config`] settings.
///
/// Uploads go to the default `UPLOADS_DIR`, which is inside the working
/// directory: tests that store a file want [`test_app_with_uploads_dir`]
/// instead.
pub fn test_app(pool: PgPool) -> Router {
    test_app_with_config(pool, test_config())
}

/// The real application router, serving uploads out of `uploads_dir`.
pub fn test_app_with_uploads_dir(pool: PgPool, uploads_dir: &Path) -> Router {
    test_app_with_config(pool, test_config_with_uploads_dir(uploads_dir))
}

/// The real application router, reading emoji packs from `emoji_dir`.
pub fn test_app_with_emoji_dir(pool: PgPool, emoji_dir: &Path) -> Router {
    test_app_with_config(pool, test_config_with_emoji_dir(emoji_dir))
}

/// The real application router, storing uploads in `uploads_dir` and
/// capping one uploaded file at `max_file_bytes`.
///
/// The upload endpoints answer 413 at `UPLOAD_LIMIT_BYTES`; tests that want
/// to see that without sending 16 MB of bytes set a small limit here. The
/// other upload setting keeps its test default.
pub fn test_app_with_upload_limits(
    pool: PgPool,
    uploads_dir: &Path,
    max_file_bytes: usize,
) -> Router {
    let mut config = test_config_with_uploads_dir(uploads_dir);
    config.upload_limits.max_file_bytes = max_file_bytes;
    test_app_with_config(pool, config)
}

/// [`test_config`] with `RESTRICT_UNAUTHENTICATED` set to `areas`.
///
/// The gated endpoints refuse unauthenticated reads of those areas with
/// 403. The area names are the entries of
/// [`wired_be::config::RESTRICT_UNAUTHENTICATED_AREAS`]; an empty slice
/// restricts nothing.
#[must_use]
pub fn test_config_with_restricted_areas(areas: &[&str]) -> Config {
    let mut config = test_config();
    config.restrict_unauthenticated = areas.iter().map(|area| (*area).to_owned()).collect();
    config
}

/// The real application router, serving over `pool` with the
/// unauthenticated restriction areas set to `areas` (see
/// [`test_config_with_restricted_areas`]).
pub fn test_app_with_restricted_areas(pool: PgPool, areas: &[&str]) -> Router {
    test_app_with_config(pool, test_config_with_restricted_areas(areas))
}

/// The shared [`AppState`] every test router is built on, and every job
/// handler is handed: `pool` plus `config`, with the hub, HTTP client,
/// uploader, stats cache and recording mailer the app needs.
///
/// A test that runs a background job rather than a request takes this
/// directly — handlers receive an [`AppState`], not a router — while
/// [`test_app_with_config`] wraps the very same state into the router.
#[must_use]
pub fn test_app_state(pool: PgPool, config: Config) -> AppState {
    AppState::for_tests(pool, config)
}

/// The real application router from [`wired_be::app`] over `pool` and
/// `config`.
///
/// Tickets whose endpoints read a config setting test the non-default
/// values through here: build a [`test_config`], set the fields, and
/// mount the router. Integration tests that need non-default settings
/// (the media proxy, for instance) build a [`Config`] and pass it here.
pub fn test_app_with_config(pool: PgPool, config: Config) -> Router {
    wired_be::app(test_app_state(pool, config))
}

/// The media proxy signing secret the tests use: 40 characters, safely
/// past the 32-character minimum, and never a real secret.
pub const MEDIA_PROXY_TEST_SECRET: &str = "0123456789abcdef0123456789abcdef01234567";

/// The translation provider key the tests configure: never a real key, and
/// sent only to a loopback mock of the provider.
pub const TRANSLATION_TEST_KEY: &str = "test-translation-key";

/// [`test_config`] with status translation pointed at the provider `api_url`
/// names: `provider` is a `TRANSLATION_PROVIDER` value and `api_url` the
/// mock's origin.
///
/// `None` for `api_url` leaves `TRANSLATION_API_URL` unset, the state no
/// provider is usable in. A `deepl` provider is always keyed with
/// [`TRANSLATION_TEST_KEY`], which that API requires and these tests never
/// send anywhere real. The mock providers live on loopback, which is why this
/// config keeps the SSRF guard off, and the rate limiter stays out of the way
/// exactly as in [`test_config`].
///
/// # Panics
///
/// Never for the fixed values tests pass: every variable is valid.
#[must_use]
pub fn test_translation_config(provider: &str, api_url: Option<&str>) -> Config {
    Config::from_lookup(|key| match key {
        wired_be::config::DATABASE_URL_VAR => Some(wired_be::config::TEST_DATABASE_URL.to_owned()),
        wired_be::config::BASE_URL_VAR => Some(wired_be::config::TEST_BASE_URL.to_owned()),
        wired_be::config::HTTP_ALLOW_PRIVATE_HOSTS_VAR => Some("true".to_owned()),
        wired_be::config::RATE_LIMITS_ENABLED_VAR => Some("false".to_owned()),
        wired_be::config::TRANSLATION_PROVIDER_VAR => Some(provider.to_owned()),
        wired_be::config::TRANSLATION_API_URL_VAR => api_url.map(str::to_owned),
        wired_be::config::TRANSLATION_API_KEY_VAR => {
            (provider == "deepl").then(|| TRANSLATION_TEST_KEY.to_owned())
        }
        _ => None,
    })
    .expect("the fixed translation test config is valid")
}

/// The application translating through `provider`, which reaches its API at
/// `api_url`, served over `pool`: what the mock-provider tests of
/// `translate.rs` are built with.
pub fn test_translation_app(pool: PgPool, provider: &str, api_url: &str) -> Router {
    test_app_with_config(pool, test_translation_config(provider, Some(api_url)))
}

/// [`test_config`] with the media proxy enabled and a proxied body capped
/// at `max_bytes` bytes.
///
/// # Panics
///
/// Never for the `max_bytes` values tests pass: every variable is fixed
/// and valid.
#[must_use]
pub fn test_media_proxy_config(max_bytes: usize) -> Config {
    Config::from_lookup(|key| match key {
        wired_be::config::DATABASE_URL_VAR => Some(wired_be::config::TEST_DATABASE_URL.to_owned()),
        wired_be::config::BASE_URL_VAR => Some(wired_be::config::TEST_BASE_URL.to_owned()),
        wired_be::config::HTTP_ALLOW_PRIVATE_HOSTS_VAR
        | wired_be::config::MEDIA_PROXY_ENABLED_VAR => Some("true".to_owned()),
        wired_be::config::MEDIA_PROXY_SECRET_VAR => Some(MEDIA_PROXY_TEST_SECRET.to_owned()),
        wired_be::config::MEDIA_PROXY_MAX_BYTES_VAR => Some(max_bytes.to_string()),
        _ => None,
    })
    .expect("the fixed media proxy test config is valid")
}

/// The media-proxy application: [`test_media_proxy_config`] with a
/// `max_bytes` cap on proxied bodies, served over `pool`.
pub fn test_media_proxy_app(pool: PgPool, max_bytes: usize) -> Router {
    test_app_with_config(pool, test_media_proxy_config(max_bytes))
}
