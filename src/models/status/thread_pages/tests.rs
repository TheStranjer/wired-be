//! The thread reads' tests: which rows each page answers, whose, and in
//! which order.

use super::*;
use crate::models::status::{
    NewStatus, Visibility, create_reblog, insert, soft_delete, test_support::*,
};

/// `author`'s answer to `parent` as a status of `visibility`.
async fn answered(
    pool: &PgPool,
    author: &crate::models::user::User,
    parent: &Status,
    visibility: Visibility,
) -> Status {
    insert(
        pool,
        BASE,
        &NewStatus {
            in_reply_to_id: Some(parent.id),
            visibility,
            ..NewStatus::local(author, "answer")
        },
    )
    .await
    .expect("a reply fixture inserts")
}

/// A window with the given cursors and a generous limit.
const fn window(min_id: Option<Uuid>, max_id: Option<Uuid>, limit: i64) -> PageParams {
    PageParams {
        max_id,
        min_id,
        since_id: None,
        limit,
        offset: None,
    }
}

#[tokio::test]
async fn replies_split_by_author_and_hide_what_is_not_public() {
    let db = crate::test_db::test_db().await.expect("test database slot");
    let pool = db.pool().clone();
    let alice = user(&pool, "alice").await;
    let bob = user(&pool, "bob").await;
    let root = post(&pool, &alice, "root").await;
    let own = answered(&pool, &alice, &root, Visibility::Public).await;
    let theirs = answered(&pool, &bob, &root, Visibility::Unlisted).await;
    let _dm = answered(&pool, &bob, &root, Visibility::Direct).await;
    let _fm = answered(&pool, &alice, &root, Visibility::Private).await;
    let gone = answered(&pool, &bob, &root, Visibility::Public).await;
    assert!(soft_delete(&pool, gone.id).await.unwrap().is_some());

    let page = window(None, None, 20);
    let by = |authors| public_replies_page_by(&pool, root.id, authors, &page);
    assert_eq!(
        ids(&by(ReplyAuthors::Anyone).await.unwrap()),
        vec![own.id, theirs.id],
        "everyone's public answers, oldest first"
    );
    assert_eq!(
        ids(&by(ReplyAuthors::Only(alice.id)).await.unwrap()),
        vec![own.id],
        "only the author's own"
    );
    assert_eq!(
        ids(&by(ReplyAuthors::Others(alice.id)).await.unwrap()),
        vec![theirs.id],
        "everyone but the author"
    );
}

#[tokio::test]
async fn the_context_lists_the_threads_public_objects_oldest_first() {
    let db = crate::test_db::test_db().await.expect("test database slot");
    let pool = db.pool().clone();
    let alice = user(&pool, "alice").await;
    let bob = user(&pool, "bob").await;
    let root = post(&pool, &alice, "root").await;
    let one = answered(&pool, &bob, &root, Visibility::Public).await;
    let deep = answered(&pool, &alice, &one, Visibility::Unlisted).await;
    let _dm = answered(&pool, &bob, &deep, Visibility::Direct).await;
    let elsewhere = post(&pool, &alice, "another thread").await;
    let (boost, created) = create_reblog(&pool, BASE, &bob, &root, Visibility::Public)
        .await
        .expect("a boost inserts");
    assert!(created, "the boost is new");
    assert_eq!(
        boost.conversation_id, root.conversation_id,
        "the boost row shares the thread's conversation"
    );
    assert_ne!(elsewhere.conversation_id, root.conversation_id);

    let all = window(None, None, 20);
    assert_eq!(
        ids(&public_context_page(&pool, root.conversation_id, &all)
            .await
            .unwrap()),
        vec![root.id, one.id, deep.id],
        "public objects of the thread, no boost, no direct, no other thread"
    );
    assert_eq!(
        public_context_count(&pool, root.conversation_id)
            .await
            .unwrap(),
        3,
        "the count agrees with the page"
    );
    assert_eq!(
        ids(
            &public_context_page(&pool, root.conversation_id, &window(Some(root.id), None, 1))
                .await
                .unwrap()
        ),
        vec![one.id],
        "min_id and the limit cut the page forward"
    );
    assert_eq!(
        ids(&public_context_page(
            &pool,
            root.conversation_id,
            &window(None, Some(deep.id), 20)
        )
        .await
        .unwrap()),
        vec![root.id, one.id],
        "max_id walks back, still oldest first"
    );
}
