//! The fixtures every handler family's tests share: the test state, the
//! local account writer, the account re-read, the accepted follow and
//! the outcome assertions. Each family's own `test_support` re-exports
//! what it needs and keeps the fixtures only it uses. Test-only.

use sqlx::PgPool;
use uuid::Uuid;

use crate::{
    config::{Config, TEST_BASE_URL},
    federation::dispatch::{FederationError, Outcome},
    models::{
        follow,
        user::{self, NewUser, User, insert},
    },
    state::AppState,
};

/// Test state over `pool` with the fixed test config.
pub(super) fn app_state(pool: &PgPool) -> AppState {
    AppState::for_tests(pool.clone(), Config::for_tests())
}

/// A local account named `nickname` at `{TEST_BASE_URL}/users/{nickname}`.
pub(super) async fn local_user(pool: &PgPool, nickname: &str) -> User {
    insert(
        pool,
        &NewUser::local(nickname, &user::local_ap_id(TEST_BASE_URL, nickname)),
    )
    .await
    .expect("the local fixture inserts")
}

/// User `id` as it stands now.
pub(super) async fn fresh_user(pool: &PgPool, id: Uuid) -> User {
    user::get_by_id(pool, id)
        .await
        .expect("the user reads")
        .expect("the user exists")
}

/// `follower` holds an accepted follow of `target` — granted through the
/// model's own write, so the counters moved with it.
pub(super) async fn accepted_follow(pool: &PgPool, follower: &User, target: &User) {
    let requested = follow::follow(pool, TEST_BASE_URL, follower, target)
        .await
        .expect("the follow fixture is recorded");
    let _granted = follow::accept(pool, requested.id)
        .await
        .expect("the fixture grants the request");
}

/// The reason an ignored outcome carries, panicking when the handler did
/// not ignore the activity at all.
pub(super) fn ignored(outcome: Result<Outcome, FederationError>) -> &'static str {
    match outcome {
        Ok(Outcome::Ignored(reason)) => reason,
        other => panic!("expected the activity to be ignored, got {other:?}"),
    }
}

/// Asserts a handler folded its activity.
pub(super) fn assert_handled(outcome: &Result<Outcome, FederationError>) {
    assert_eq!(
        outcome,
        &Ok(Outcome::Handled),
        "the handler folded the activity"
    );
}
