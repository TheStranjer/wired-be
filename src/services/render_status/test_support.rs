//! The fixtures the `render_status` unit tests share: the instance origin
//! the posts live under, the state under test, a plain local account and
//! a plain local post. Test-only.

use sqlx::PgPool;

use crate::{
    config::{Config, TEST_BASE_URL},
    models::{
        status::{NewStatus, Status as StatusRow, insert},
        user::{self, NewUser, User, local_ap_id},
    },
    state::AppState,
};

/// The instance origin the fixtures post under.
pub(super) const BASE: &str = TEST_BASE_URL;

/// The state under test: `pool` and the test config, with the
/// collaborators the renderer never touches.
pub(super) fn test_state(pool: &PgPool) -> AppState {
    AppState::for_tests(pool.clone(), Config::for_tests())
}

/// Inserts a local user named `nickname`.
pub(super) async fn user(pool: &PgPool, nickname: &str) -> User {
    user::insert(
        pool,
        &NewUser::local(nickname, &local_ap_id(BASE, nickname)),
    )
    .await
    .expect("the fixture user inserts")
}

/// Inserts `text` as a local public status of `author`.
pub(super) async fn post(pool: &PgPool, author: &User, text: &str) -> StatusRow {
    insert(pool, BASE, &NewStatus::local(author, text))
        .await
        .expect("a plain local post inserts")
}
