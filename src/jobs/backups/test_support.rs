//! The fixtures the account backups' unit tests share: the test state
//! (plain, config-overridden, and uploads-backed), the seeded local account
//! with its posts and marks, the two handler calls the tests await, and the
//! attempt info they pass. Each child's `tests` adds the fixtures only its
//! own tests read. Test-only.

use serde_json::json;
use sqlx::PgPool;
use uuid::Uuid;

use super::{handle_purge, handle_run};
use crate::{
    config::{Config, TEST_BASE_URL},
    jobs::{AttemptInfo, JobError},
    models::{
        bookmark, favourite,
        status::{NewStatus, Status, Visibility, insert},
        user::{NewUser, User, insert as insert_user, local_ap_id},
    },
    state::AppState,
};

/// The instance origin the fixtures post under.
const BASE: &str = TEST_BASE_URL;

/// The attempt info a test hands a handler: the first pass of a job
/// with an ordinary budget.
pub(super) const FIRST_ATTEMPT: AttemptInfo = AttemptInfo {
    attempts: 1,
    max_attempts: 5,
};

/// Test state over `pool` with `config`.
pub(super) fn state_with_config(pool: &PgPool, config: Config) -> AppState {
    AppState::for_tests(pool.clone(), config)
}

/// Test state over `pool` with the fixed test config.
pub(super) fn app_state(pool: &PgPool) -> AppState {
    state_with_config(pool, Config::for_tests())
}

/// Test state over `pool` whose uploads live in a fresh temporary
/// directory, together with that directory's guard: the handler stores
/// into and deletes from that same directory, so the guard must
/// outlive the run (the pattern of `src/jobs/maintenance.rs`'s
/// `uploads_state`).
pub(super) fn uploads_state(pool: &PgPool) -> (tempfile::TempDir, AppState) {
    let dir = tempfile::TempDir::new().expect("a temp uploads dir");
    let mut config = Config::for_tests();
    config.uploads_dir = dir.path().to_path_buf();
    let state = state_with_config(pool, config);
    (dir, state)
}

/// A local user named `nickname`.
pub(super) async fn user(pool: &PgPool, nickname: &str) -> User {
    insert_user(
        pool,
        &NewUser::local(nickname, &local_ap_id(BASE, nickname)),
    )
    .await
    .expect("the fixture user inserts")
}

/// A post of `text` by `author`, visible to `visibility`.
pub(super) async fn post(
    pool: &PgPool,
    author: &User,
    visibility: Visibility,
    text: &str,
) -> Status {
    insert(
        pool,
        BASE,
        &NewStatus {
            visibility,
            ..NewStatus::local(author, text)
        },
    )
    .await
    .expect("the fixture post inserts")
}

/// Favourites `status_id` as `who`, asserting the row is new.
pub(super) async fn like(pool: &PgPool, who: &User, status_id: Uuid) {
    assert!(
        favourite::favourite(pool, BASE, who.id, status_id)
            .await
            .expect("the fixture favourite stores")
            .is_some(),
        "the fixture pair had not favourited yet"
    );
}

/// Bookmarks `status_id` as `who`.
pub(super) async fn mark(pool: &PgPool, who: &User, status_id: Uuid) {
    let _row: bookmark::Bookmark = bookmark::add(pool, who.id, status_id, None)
        .await
        .expect("the fixture bookmark stores");
}

/// Runs [`handle_run`] to completion, over the argument
/// document `POST /api/v1/pleroma/backups` writes.
pub(super) async fn run(state: &AppState, backup_id: Uuid) -> Result<(), JobError> {
    handle_run(
        state.clone(),
        FIRST_ATTEMPT,
        json!({ "backup_id": backup_id.to_string() }),
    )
    .await
}

/// Runs [`handle_purge`] to completion; periodic jobs arrive with
/// empty args.
pub(super) async fn purge(state: &AppState) -> Result<(), JobError> {
    handle_purge(state.clone(), FIRST_ATTEMPT, json!({})).await
}
