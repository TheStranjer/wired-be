//! The fixtures the `render_status` unit tests share: the instance origin
//! the posts live under, the state under test, a plain local account and
//! a plain local post. Test-only.

use sqlx::PgPool;

use crate::{
    config::TEST_BASE_URL,
    models::{
        status::{NewStatus, Status as StatusRow, insert},
        user::User,
    },
};

// A local user named `nickname`.
pub(super) use crate::test_fixtures::local_user as user;
// The state under test: a slot's pool and the test config, with the
// collaborators the renderer never touches.
pub(super) use crate::test_support::app_state;

/// The instance origin the fixtures post under.
pub(super) const BASE: &str = TEST_BASE_URL;

/// Inserts `text` as a local public status of `author`.
pub(super) async fn post(pool: &PgPool, author: &User, text: &str) -> StatusRow {
    insert(pool, BASE, &NewStatus::local(author, text))
        .await
        .expect("a plain local post inserts")
}
