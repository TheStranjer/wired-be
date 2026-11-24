//! Unit tests for `pleroma.parent_visible`: the batch's one visibility
//! read over the page's replied-to statuses, so the flag reflects the
//! viewer's access to the parent and not to the rendered reply.

use serde_json::Value;
use sqlx::PgPool;

use super::{
    ViewerMode, render_statuses,
    test_support::{BASE, post, test_state, user},
};
use crate::models::{
    follow,
    status::{NewStatus, Status as StatusRow, Visibility, insert, soft_delete},
    user::User,
};

/// A public reply by `author` to `parent`, whose own author is `parent_author`.
async fn reply(
    pool: &PgPool,
    author: &User,
    parent: &StatusRow,
    parent_author: &User,
) -> StatusRow {
    insert(
        pool,
        BASE,
        &NewStatus {
            in_reply_to_id: Some(parent.id),
            in_reply_to_account_id: Some(parent_author.id),
            ..NewStatus::local(author, "an answer")
        },
    )
    .await
    .expect("a reply to a stored status inserts")
}

/// A followers-only post of `author`.
async fn private_post(pool: &PgPool, author: &User) -> StatusRow {
    insert(
        pool,
        BASE,
        &NewStatus {
            visibility: Visibility::Private,
            ..NewStatus::local(author, "followers only")
        },
    )
    .await
    .expect("a private post inserts")
}

/// The `pleroma.parent_visible` value of each of `page`, in order, as
/// `viewer` sees the page.
async fn parent_visible(
    state: &crate::state::AppState,
    viewer: ViewerMode<'_>,
    page: &[StatusRow],
) -> Vec<Value> {
    let rendered = render_statuses(state, viewer, page)
        .await
        .expect("the page renders");
    rendered
        .iter()
        .map(|entity| {
            serde_json::to_value(entity).expect("it serializes")["pleroma"]["parent_visible"]
                .clone()
        })
        .collect()
}

/// A public reply to a followers-only post: the reply itself is public, so
/// it reaches every viewer's page, but only a follower of the parent's
/// author may see the parent; an admin view bypasses the rules; a root post
/// has no parent to hide.
#[tokio::test]
async fn a_reply_to_a_private_post_is_visible_only_to_those_who_see_the_parent() {
    let db = crate::test_db::test_db().await.expect("test database slot");
    let pool = db.pool().clone();
    let state = test_state(&pool);
    let alice = user(&pool, "alice").await;
    let bob = user(&pool, "bob").await;
    let carol = user(&pool, "carol").await;
    let dave = user(&pool, "dave").await;
    let _following = follow::follow(&pool, BASE, &bob, &alice)
        .await
        .expect("bob follows alice");
    let secret = private_post(&pool, &alice).await;
    let answer = reply(&pool, &carol, &secret, &alice).await;
    let root = post(&pool, &carol, "a root post").await;
    let page = [answer, root];

    assert_eq!(
        parent_visible(&state, ViewerMode::ANONYMOUS, &page).await,
        [false, true],
        "an anonymous viewer cannot see the private parent; the root has none"
    );
    assert_eq!(
        parent_visible(&state, ViewerMode::user(&dave), &page).await,
        [false, true],
        "a stranger cannot see it either"
    );
    assert_eq!(
        parent_visible(&state, ViewerMode::user(&bob), &page).await,
        [true, true],
        "a follower of the parent's author can"
    );
    assert_eq!(
        parent_visible(&state, ViewerMode::user(&alice), &page).await,
        [true, true],
        "and so can the parent's author"
    );
    assert_eq!(
        parent_visible(&state, ViewerMode::admin(&dave), &page).await,
        [true, true],
        "the admin view runs no visibility filter"
    );
}

/// A reply to a public post and a root post both render `true`, and a
/// reply to a deleted parent renders `false` — the parent is gone.
#[tokio::test]
async fn public_and_root_parents_are_visible_and_a_gone_parent_is_not() {
    let db = crate::test_db::test_db().await.expect("test database slot");
    let pool = db.pool().clone();
    let state = test_state(&pool);
    let alice = user(&pool, "alice").await;
    let bob = user(&pool, "bob").await;
    let open = post(&pool, &alice, "open").await;
    let doomed = post(&pool, &alice, "doomed").await;
    let to_open = reply(&pool, &bob, &open, &alice).await;
    let to_doomed = reply(&pool, &bob, &doomed, &alice).await;
    let _tombstone = soft_delete(&pool, doomed.id)
        .await
        .expect("the delete runs")
        .expect("the post was live");
    let page = [open, to_open, to_doomed];

    assert_eq!(
        parent_visible(&state, ViewerMode::ANONYMOUS, &page).await,
        [true, true, false],
        "root true, public parent true, deleted parent false"
    );
}

/// Two replies on one page, one to a hidden parent and one to a visible
/// one: each answers for its own parent, whatever the page order.
#[tokio::test]
async fn the_flag_is_keyed_to_each_rows_own_parent() {
    let db = crate::test_db::test_db().await.expect("test database slot");
    let pool = db.pool().clone();
    let state = test_state(&pool);
    let alice = user(&pool, "alice").await;
    let bob = user(&pool, "bob").await;
    let secret = private_post(&pool, &alice).await;
    let open = post(&pool, &alice, "open").await;
    let hidden_answer = reply(&pool, &bob, &secret, &alice).await;
    let open_answer = reply(&pool, &bob, &open, &alice).await;

    assert_eq!(
        parent_visible(
            &state,
            ViewerMode::ANONYMOUS,
            &[hidden_answer.clone(), open_answer.clone()]
        )
        .await,
        [false, true],
        "each reply answers for its own parent"
    );
    assert_eq!(
        parent_visible(&state, ViewerMode::ANONYMOUS, &[open_answer, hidden_answer]).await,
        [true, false],
        "whatever order the page lists them in"
    );
}
