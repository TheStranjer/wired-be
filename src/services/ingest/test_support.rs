//! The fixtures the ingest module's tests share: the test state (plain and
//! config-overridden), the remote author mirror, and the plain public note
//! object they ingest. Test-only.

use std::sync::Arc;

use serde_json::{Value, json};
use sqlx::PgPool;

use crate::{
    config::Config,
    http_client::HttpClient,
    models::user::{self, Flag, NewUser, User},
    state::AppState,
    uploads::UploaderBackend,
};

/// The test state: `pool` and the test config, collaborators inert.
pub(super) fn app_state(pool: &PgPool) -> AppState {
    state_with_config(pool, Config::for_tests())
}

/// The test state on `config` instead of the stock test one.
pub(super) fn state_with_config(pool: &PgPool, config: Config) -> AppState {
    let config = Arc::new(config);
    AppState {
        db: pool.clone(),
        hub: crate::streaming::hub::Hub::new(pool),
        config: Arc::clone(&config),
        http_client: HttpClient::from(config.as_ref()),
        uploader: UploaderBackend::from(config.as_ref()),
        instance_stats_cache: Arc::default(),
        mrf_config_cache: Arc::default(),
        translation_languages_cache: Arc::default(),
        rate_limiter: Arc::new(crate::rate_limit::RateLimiter::from_config(config.as_ref())),
        mailer: Arc::new(crate::mailer::MemoryTransport::new()),
    }
}

/// The fixture author: remote `bob@remote.example`, active by default.
pub(super) async fn author(pool: &PgPool) -> User {
    user::insert(
        pool,
        &NewUser {
            nickname: "bob@remote.example".to_owned(),
            ap_id: "https://remote.example/users/bob".to_owned(),
            local: Flag::new(false),
            domain: Some("remote.example".to_owned()),
            followers_url: Some("https://remote.example/users/bob/followers".to_owned()),
            ..NewUser::default()
        },
    )
    .await
    .expect("the author fixture inserts")
}

/// `bob`'s public note object named `n`, replying to nothing.
pub(super) fn note(n: &str) -> Value {
    json!({
        "id": format!("https://remote.example/objects/{n}"),
        "type": "Note",
        "attributedTo": "https://remote.example/users/bob",
        "to": ["https://www.w3.org/ns/activitystreams#Public"],
        "cc": ["https://remote.example/users/bob/followers"],
        "content": format!("<p>ingest note {n}</p>"),
        "published": "2024-02-01T10:00:00.000Z",
    })
}
