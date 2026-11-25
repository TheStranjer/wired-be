//! The fixtures the ingest module's tests share: the test state (plain and
//! config-overridden), the remote author mirror, and the plain public note
//! object they ingest. Test-only.

use serde_json::{Value, json};
use sqlx::PgPool;

use crate::{
    config::Config,
    models::{
        activitypub_actor::{self, ActivityPubActorWrite},
        user::{self, Flag, NewUser, User},
    },
    state::AppState,
};

// The test state: a slot's pool and the test config, collaborators inert.
pub(super) use crate::test_support::app_state;

/// The test state on `config` instead of the stock test one.
pub(super) fn state_with_config(pool: &PgPool, config: Config) -> AppState {
    AppState::for_tests(pool.clone(), config)
}

/// The fixture author: remote `bob@remote.example`, active by default.
pub(super) async fn author(pool: &PgPool) -> User {
    let bob = user::insert(
        pool,
        &NewUser {
            nickname: "bob@remote.example".to_owned(),
            ap_id: "https://remote.example/users/bob".to_owned(),
            local: Flag::new(false),
            domain: Some("remote.example".to_owned()),
            ..NewUser::default()
        },
    )
    .await
    .expect("the author fixture inserts");
    let _row = activitypub_actor::upsert(
        pool,
        &ActivityPubActorWrite {
            followers_url: Some("https://remote.example/users/bob/followers".to_owned()),
            ..ActivityPubActorWrite::bare(bob.id)
        },
    )
    .await
    .expect("the author's actor row writes");
    bob
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
