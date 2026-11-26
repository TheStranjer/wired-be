//! Local moderation against the remote document's `suspended` flag
//! (WBE-873): the two origins of an inactive mirror never undo each other.

use serde_json::json;
use sqlx::PgPool;

use super::upsert_remote_user;
use crate::{
    federation::remote_actor::{
        parse_actor,
        test_support::{MASTODON, fixture},
    },
    models::user::{User, admin::set_active},
};

/// Upserts the Mastodon fixture actor, with `suspended` as given.
async fn fetch(pool: &PgPool, suspended: bool) -> User {
    let mut document = fixture(MASTODON);
    if suspended {
        document["suspended"] = json!(true);
    }
    upsert_remote_user(pool, parse_actor(&document).unwrap())
        .await
        .unwrap()
}

/// An admin moves the mirror `id`'s local activation, as the admin endpoints do.
async fn moderate(pool: &PgPool, id: uuid::Uuid, active: bool) -> User {
    set_active(pool, id, active)
        .await
        .unwrap()
        .expect("the mirror exists")
}

/// An `Update` without `suspended` leaves a locally deactivated mirror inactive.
#[tokio::test]
async fn an_update_does_not_reactivate_a_locally_deactivated_mirror() {
    let db = crate::test_db::test_db().await.expect("test database slot");
    let pool = db.pool().clone();
    let alice = fetch(&pool, false).await;
    let denied = moderate(&pool, alice.id, false).await;
    assert!(!denied.is_active.get(), "the admin deactivated the mirror");

    let updated = fetch(&pool, false).await;

    assert_eq!(updated.id, alice.id, "the same mirror");
    assert!(
        !updated.is_active.get(),
        "an Update does not undo the local deactivation"
    );
}

/// The remote suspending and unsuspending a locally deactivated mirror is no way back.
#[tokio::test]
async fn a_remote_unsuspension_does_not_reactivate_a_locally_deactivated_mirror() {
    let db = crate::test_db::test_db().await.expect("test database slot");
    let pool = db.pool().clone();
    let alice = fetch(&pool, false).await;
    let _denied = moderate(&pool, alice.id, false).await;

    let suspended = fetch(&pool, true).await;
    assert!(!suspended.is_active.get(), "suspended and deactivated");
    let unsuspended = fetch(&pool, false).await;
    assert!(
        !unsuspended.is_active.get(),
        "the local deactivation wins over the remote unsuspension"
    );

    let activated = moderate(&pool, alice.id, true).await;
    assert!(
        activated.is_active.get(),
        "a local activation lifts the local deactivation"
    );
}

/// A local activation does not clear a remote suspension; the remote unsuspending does.
#[tokio::test]
async fn a_local_activation_does_not_clear_a_remote_suspension() {
    let db = crate::test_db::test_db().await.expect("test database slot");
    let pool = db.pool().clone();
    let alice = fetch(&pool, true).await;
    assert!(!alice.is_active.get(), "the remote suspended the mirror");

    let activated = moderate(&pool, alice.id, true).await;
    assert!(
        !activated.is_active.get(),
        "a local activation leaves the remote suspension in force"
    );
    let still = fetch(&pool, true).await;
    assert!(!still.is_active.get(), "and so does a re-fetch");

    let restored = fetch(&pool, false).await;
    assert!(
        restored.is_active.get(),
        "the remote unsuspending shows the mirror again"
    );
}

/// With no local action the remote document alone moves the mirror (WBE-865).
#[tokio::test]
async fn the_remote_flag_alone_hides_and_shows_the_mirror() {
    let db = crate::test_db::test_db().await.expect("test database slot");
    let pool = db.pool().clone();
    let first = fetch(&pool, false).await;
    assert!(first.is_active.get(), "a fresh mirror is active");

    let hidden = fetch(&pool, true).await;
    assert!(!hidden.is_active.get(), "remote suspension hides it");
    let shown = fetch(&pool, false).await;
    assert!(shown.is_active.get(), "remote unsuspension shows it again");
}

/// A local deactivation made while the remote has the mirror suspended survives the unsuspension.
#[tokio::test]
async fn a_local_deactivation_during_a_remote_suspension_survives_it() {
    let db = crate::test_db::test_db().await.expect("test database slot");
    let pool = db.pool().clone();
    let alice = fetch(&pool, true).await;
    let _denied = moderate(&pool, alice.id, false).await;

    let unsuspended = fetch(&pool, false).await;

    assert!(
        !unsuspended.is_active.get(),
        "the mirror stays inactive: an admin deactivated it"
    );
}
