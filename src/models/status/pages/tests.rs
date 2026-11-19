//! The paging reads' tests: which rows each page answers, in which order,
//! and how the cursors and limits cut it.

use sqlx::PgPool;

use super::*;
use crate::models::{
    status::{NewStatus, Visibility, insert, soft_delete, test_support::*},
    user::User,
};

/// Inserts `text` as `author`'s reblog of `original`.
async fn reblog(pool: &PgPool, author: &User, original: &Status, text: &str) -> Status {
    insert(
        pool,
        BASE,
        &NewStatus {
            reblog_of_id: Some(original.id),
            ..NewStatus::local(author, text)
        },
    )
    .await
    .expect("a reblog of a stored status inserts")
}

/// Inserts `text` as `author`'s status at the given visibility.
async fn visible(pool: &PgPool, author: &User, text: &str, visibility: Visibility) -> Status {
    insert(
        pool,
        BASE,
        &NewStatus {
            visibility,
            ..NewStatus::local(author, text)
        },
    )
    .await
    .expect("a fixture status at any visibility inserts")
}

/// The ids `newest_public_posts` answers, newest first, only the live
/// `public` roots of the asked-for author: the other visibilities, the
/// replies, the reblogs and the tombstones stay out, another author's
/// posts stay out, `max_id` keys the next page on the id (decision 4),
/// and `limit` cuts the page.
#[tokio::test]
async fn newest_public_posts_reads_the_author_s_public_roots() {
    let db = crate::test_db::test_db().await.expect("test database slot");
    let pool = db.pool().clone();
    let alice = user(&pool, "alice").await;
    let bob = user(&pool, "bob").await;
    let one = post(&pool, &alice, "one").await;
    let _unlisted = visible(&pool, &alice, "unlisted", Visibility::Unlisted).await;
    let _direct = visible(&pool, &alice, "direct", Visibility::Direct).await;
    let _private = visible(&pool, &alice, "private", Visibility::Private).await;
    let _localonly = visible(&pool, &alice, "local", Visibility::Local).await;
    let _reply = reply(&pool, &alice, &one, "in answer").await;
    let two = post(&pool, &alice, "two").await;
    let _reblog = reblog(&pool, &alice, &two, "").await;
    let three = post(&pool, &alice, "three").await;
    let bob_post = post(&pool, &bob, "bob speaks").await;
    assert!(
        soft_delete(&pool, two.id).await.unwrap().is_some(),
        "one public root is deleted"
    );

    assert_eq!(
        ids(&newest_public_posts(&pool, alice.id, None, 20)
            .await
            .unwrap()),
        vec![three.id, one.id],
        "newest first; the unlisted, direct, private, local, reply, \
             reblog and deleted rows are out, and bob's posts were never in"
    );
    assert_eq!(
        ids(&newest_public_posts(&pool, alice.id, Some(three.id), 20)
            .await
            .unwrap()),
        vec![one.id],
        "the cursor page holds only the strictly older posts"
    );
    assert_eq!(
        ids(&newest_public_posts(&pool, alice.id, None, 1).await.unwrap()),
        vec![three.id],
        "the limit cuts the page"
    );
    let carol = user(&pool, "carol").await;
    assert!(
        newest_public_posts(&pool, carol.id, None, 20)
            .await
            .unwrap()
            .is_empty(),
        "an author with nothing to show answers the empty page"
    );
    assert_eq!(
        ids(&newest_public_posts(&pool, bob.id, None, 20).await.unwrap()),
        vec![bob_post.id],
        "the other author keeps their own single public root"
    );
}

/// The outbox page answers the author's live `public` and `unlisted`
/// rows newest-first — replies and reblogs among them, the other
/// visibilities, the tombstones and another author's rows never — and
/// the `PageParams` cursors walk the page on the status id.
#[tokio::test]
async fn outbox_page_reads_the_author_s_ap_visible_rows() {
    let db = crate::test_db::test_db().await.expect("test database slot");
    let pool = db.pool().clone();
    let alice = user(&pool, "alice").await;
    let bob = user(&pool, "bob").await;
    let root = post(&pool, &alice, "root").await;
    let answer = reply(&pool, &alice, &root, "answer").await;
    let hidden = visible(&pool, &alice, "unlisted", Visibility::Unlisted).await;
    let _direct = visible(&pool, &alice, "dm", Visibility::Direct).await;
    let _private = visible(&pool, &alice, "followers only", Visibility::Private).await;
    let _localonly = visible(&pool, &alice, "instance only", Visibility::Local).await;
    let gone = post(&pool, &alice, "gone").await;
    assert!(
        soft_delete(&pool, gone.id).await.unwrap().is_some(),
        "one outbox row is deleted"
    );
    let _bobs = post(&pool, &bob, "not alice's outbox").await;

    let first_page = PageParams {
        limit: 4,
        ..PageParams::from_query(None, 40).expect("no query is the default page")
    };
    assert_eq!(
        ids(&outbox_page(&pool, alice.id, &first_page).await.unwrap()),
        vec![hidden.id, answer.id, root.id],
        "newest first: the unlisted post, the answer, the root; the \
             direct, private, local and deleted rows and bob's posts stay out"
    );
    let older = PageParams {
        max_id: Some(answer.id),
        ..first_page
    };
    assert_eq!(
        ids(&outbox_page(&pool, alice.id, &older).await.unwrap()),
        vec![root.id],
        "the max_id page holds only the strictly older rows"
    );
    let newer = PageParams {
        min_id: Some(answer.id),
        ..first_page
    };
    assert_eq!(
        ids(&outbox_page(&pool, alice.id, &newer).await.unwrap()),
        vec![hidden.id],
        "the min_id page holds only the strictly newer rows, newest first"
    );
    let tiny = PageParams {
        limit: 1,
        ..first_page
    };
    assert_eq!(
        ids(&outbox_page(&pool, alice.id, &tiny).await.unwrap()),
        vec![hidden.id],
        "the limit cuts the page"
    );
}

/// The replies reads answer the live `public`/`unlisted` answers only,
/// oldest first; `min_id` walks the window forward and `max_id` back,
/// each page still oldest-first, and the count agrees with the page.
#[tokio::test]
async fn public_replies_page_walks_oldest_first() {
    /// `author`'s answer to `parent`, as one visibility.
    async fn answered(
        pool: &PgPool,
        author: &User,
        parent: &Status,
        text: &str,
        visibility: Visibility,
    ) -> Status {
        insert(
            pool,
            BASE,
            &NewStatus {
                in_reply_to_id: Some(parent.id),
                visibility,
                ..NewStatus::local(author, text)
            },
        )
        .await
        .expect("a visible reply fixture inserts")
    }

    let db = crate::test_db::test_db().await.expect("test database slot");
    let pool = db.pool().clone();
    let alice = user(&pool, "alice").await;
    let root = post(&pool, &alice, "root").await;
    let other = post(&pool, &alice, "another root").await;
    let one = reply(&pool, &alice, &root, "one").await;
    let two = answered(&pool, &alice, &root, "two", Visibility::Unlisted).await;
    let _dm = answered(&pool, &alice, &root, "dm", Visibility::Direct).await;
    let _followers_only = answered(&pool, &alice, &root, "fm", Visibility::Private).await;
    let elsewhere = reply(&pool, &alice, &other, "not a reply of root").await;
    let gone = reply(&pool, &alice, &root, "gone").await;
    assert!(
        soft_delete(&pool, gone.id).await.unwrap().is_some(),
        "one reply is deleted"
    );

    assert_eq!(
        ids(&public_replies_page(&pool, root.id, None, None, 20)
            .await
            .unwrap()),
        vec![one.id, two.id],
        "oldest first; the direct and private answers, the other thread \
             and the tombstone stay out"
    );
    assert_eq!(
        public_replies_count(&pool, root.id).await.unwrap(),
        2,
        "the count agrees with the page"
    );
    assert_eq!(
        ids(&public_replies_page(&pool, root.id, Some(one.id), None, 20)
            .await
            .unwrap()),
        vec![two.id],
        "min_id walks forward to the newer answers"
    );
    assert_eq!(
        ids(&public_replies_page(&pool, root.id, None, Some(two.id), 20)
            .await
            .unwrap()),
        vec![one.id],
        "max_id walks back to the older answers, still oldest first"
    );
    assert_eq!(
        ids(&public_replies_page(&pool, other.id, None, None, 20)
            .await
            .unwrap()),
        vec![elsewhere.id],
        "each thread pages its own answers"
    );
    assert_eq!(
        public_replies_count(&pool, other.id).await.unwrap(),
        1,
        "and counts them"
    );
}
