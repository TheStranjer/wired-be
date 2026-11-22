//! Fixtures the timeline children's unit tests share.

use crate::models::user::User;

/// A database-backed [`User`] for [`DirectParams::into_timeline`],
/// which owns its viewer and so cannot be built from a literal.
///
/// [`DirectParams::into_timeline`]: super::direct_list
pub(super) async fn fixture_user() -> (crate::test_db::TestDb, User) {
    let db = crate::test_db::test_db().await.expect("test database slot");
    let alice = crate::models::user::insert(
        db.pool(),
        &crate::models::user::NewUser::local(
            "alice",
            &crate::models::user::local_ap_id(crate::config::TEST_BASE_URL, "alice"),
        ),
    )
    .await
    .expect("the fixture user inserts");
    (db, alice)
}
