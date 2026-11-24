//! Rule 1b: a deactivated author's statuses are visible to nobody.

use sqlx::PgPool;
use uuid::Uuid;

use super::{BASE, clause_ids, filter_visible, id_set, local, post, reblog, stored_statuses};
use crate::models::{follow, status::Visibility, status_visibility::can_view, user::User};

/// Sets `users.is_active` of `user` to `active`.
async fn set_active(pool: &PgPool, user: &User, active: bool) {
    let _updated: sqlx::postgres::PgQueryResult =
        sqlx::query("UPDATE users SET is_active = $2 WHERE id = $1")
            .bind(user.id)
            .bind(active)
            .execute(pool)
            .await
            .expect("the flag updates");
}

/// A public post of a deactivated author is hidden from an anonymous
/// viewer, an accepted follower and the author, in all three shapes.
#[tokio::test]
async fn a_deactivated_authors_public_status_is_visible_to_nobody() {
    let db = crate::test_db::test_db().await.expect("test database slot");
    let pool = db.pool().clone();
    let alice = local(&pool, "alice").await;
    let bob = local(&pool, "bob").await;
    let following = follow::follow(&pool, BASE, &bob, &alice)
        .await
        .expect("bob follows open alice");
    assert_eq!(following.state, follow::FollowState::Accepted);
    let status = post(&pool, &alice, Visibility::Public, "gone soon").await;
    set_active(&pool, &alice, false).await;

    let viewers: [(&str, Option<&User>); 3] = [
        ("anonymous", None),
        ("a follower", Some(&bob)),
        ("the author", Some(&alice)),
    ];
    for (who, viewer) in viewers {
        assert!(
            !can_view(&pool, &status, viewer).await.unwrap(),
            "{who}: can_view hides it"
        );
        assert!(
            clause_ids(&pool, viewer).await.is_empty(),
            "{who}: the clause hides it"
        );
        let kept = filter_visible(&pool, vec![status.clone()], viewer)
            .await
            .expect("the batch filter runs");
        assert!(kept.is_empty(), "{who}: filter_visible hides it");
    }
}

/// A reblog of a deactivated author's status is hidden, for `can_view`,
/// `filter_visible` and the SQL clause alike; the original's author's own
/// reblogger stays active throughout.
#[tokio::test]
async fn a_reblog_of_a_deactivated_authors_status_is_hidden() {
    let db = crate::test_db::test_db().await.expect("test database slot");
    let pool = db.pool().clone();
    let alice = local(&pool, "alice").await;
    let bob = local(&pool, "bob").await;
    let original = post(&pool, &alice, Visibility::Public, "boost me").await;
    let boosted = reblog(&pool, &bob, &original).await;
    set_active(&pool, &alice, false).await;

    assert!(!can_view(&pool, &boosted, None).await.unwrap());
    assert!(!can_view(&pool, &boosted, Some(&bob)).await.unwrap());
    let kept = filter_visible(&pool, vec![boosted.clone()], Some(&bob))
        .await
        .expect("the batch filter runs");
    assert!(kept.is_empty(), "the batch filter drops the reblog");
    assert!(
        clause_ids(&pool, None).await.is_empty(),
        "the clause hides the original and its reblog"
    );
}

/// Reactivating the author brings every status back, reblogs included.
#[tokio::test]
async fn reactivating_restores_visibility() {
    let db = crate::test_db::test_db().await.expect("test database slot");
    let pool = db.pool().clone();
    let alice = local(&pool, "alice").await;
    let bob = local(&pool, "bob").await;
    let original = post(&pool, &alice, Visibility::Public, "back again").await;
    let boosted = reblog(&pool, &bob, &original).await;
    let everything = stored_statuses(&pool).await;
    let all_ids: Vec<Uuid> = everything.iter().map(|status| status.id).collect();

    set_active(&pool, &alice, false).await;
    assert!(clause_ids(&pool, None).await.is_empty(), "hidden while off");

    set_active(&pool, &alice, true).await;
    assert!(can_view(&pool, &original, None).await.unwrap());
    assert!(can_view(&pool, &boosted, None).await.unwrap());
    assert_eq!(id_set(&clause_ids(&pool, None).await), id_set(&all_ids));
    let kept = filter_visible(&pool, everything, None)
        .await
        .expect("the batch filter runs");
    assert_eq!(kept.len(), 2, "both rows are back");
}
