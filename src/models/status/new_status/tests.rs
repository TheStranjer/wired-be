//! The insert transaction's tests: the URLs and conversation an
//! [`insert`] mints and derives, the mention, hashtag and media links it
//! writes in its own transaction, and what a refused write leaves behind.

use sqlx::PgPool;
use uuid::Uuid;

use super::*;
use crate::{
    error::AppError,
    models::{
        hashtag,
        media_attachment::{self, MediaAttachment, NewMediaAttachment},
        mention,
        status::{get_by_id, test_support::*},
    },
    util::id::new_id,
};

#[tokio::test]
async fn a_local_root_status_fills_its_urls_and_counts_its_author() {
    let db = crate::test_db::test_db().await.expect("test database slot");
    let pool = db.pool().clone();
    let alice = user(&pool, "alice").await;
    let status = post(&pool, &alice, "hello world").await;

    assert!(status.id.get_version_num() == 7, "the id is a UUIDv7");
    assert_eq!(
        status.uri,
        format!("{BASE}/objects/{}", status.id),
        "a local status owns this instance's objects id (decision 6)"
    );
    assert_eq!(
        status.url,
        Some(format!("{BASE}/notice/{}", status.id)),
        "and the human URL derives from the same id"
    );
    assert_eq!(
        status.conversation_id, status.id,
        "a root status is its own conversation"
    );
    assert!(status.local, "the post is marked local");
    assert_eq!(status.user_id, alice.id, "the author is stored");
    assert_eq!(status.content, "<p>hello world</p>", "content is HTML");
    assert_eq!(status.raw_text, "hello world", "the source is kept");
    assert_eq!(
        status.content_type, DEFAULT_CONTENT_TYPE,
        "as is its content type"
    );
    assert_eq!(
        status.visibility,
        Visibility::Public,
        "a plain post is public"
    );
    assert!(
        status.in_reply_to_id.is_none()
            && status.reblog_of_id.is_none()
            && status.quote_of_id.is_none(),
        "a root status links to nothing"
    );
    assert_eq!(
        (
            status.replies_count,
            status.reblogs_count,
            status.favourites_count,
            status.quotes_count
        ),
        (0, 0, 0, 0),
        "every counter starts at zero"
    );
    assert!(
        status.edited_at.is_none() && status.deleted_at.is_none() && status.expires_at.is_none(),
        "nothing has happened to it yet"
    );
    assert!(
        status.inserted_at <= status.updated_at,
        "the database stamps both timestamps"
    );

    let counted = fresh_user(&pool, alice.id).await;
    assert_eq!(counted.statuses_count, 1, "the author counts one post");
    assert_eq!(
        counted.last_status_at,
        Some(status.inserted_at),
        "and knows when they last posted"
    );
    assert_eq!(
        get_by_id(&pool, status.id).await.unwrap(),
        Some(status.clone()),
        "get_by_id reads the row back"
    );
}

/// The mentioned ids of `status_id` from the batch read, in order.
async fn mentioned(pool: &PgPool, status_id: Uuid) -> Vec<Uuid> {
    let map = mention::for_statuses(pool, &[status_id])
        .await
        .expect("the batch read works");
    map.get(&status_id)
        .map_or_default(|users| users.iter().map(|user| user.id).collect())
}

#[tokio::test]
async fn insert_links_the_mentioned_accounts_in_order() {
    let db = crate::test_db::test_db().await.expect("test database slot");
    let pool = db.pool().clone();
    let alice = user(&pool, "alice").await;
    let bob = user(&pool, "bob").await;
    let carol = user(&pool, "carol").await;

    let status = insert(
        &pool,
        BASE,
        &NewStatus {
            mentioned_user_ids: vec![carol.id, bob.id],
            ..NewStatus::local(&alice, "@carol and @bob")
        },
    )
    .await
    .unwrap();

    assert_eq!(
        mentioned(&pool, status.id).await,
        vec![carol.id, bob.id],
        "the links went in in mention order, in the status's transaction"
    );
    assert_eq!(
        fresh_user(&pool, alice.id).await.statuses_count,
        1,
        "and the rest of the insert happened as usual"
    );

    let plain = post(&pool, &alice, "no mentions here").await;
    assert!(
        mentioned(&pool, plain.id).await.is_empty(),
        "a status without mentioned_user_ids links nobody"
    );
}

#[tokio::test]
async fn an_unknown_mention_rolls_the_status_back() {
    let db = crate::test_db::test_db().await.expect("test database slot");
    let pool = db.pool().clone();
    let alice = user(&pool, "alice").await;
    let err = insert(
        &pool,
        BASE,
        &NewStatus {
            mentioned_user_ids: vec![new_id()],
            hashtags: vec!["doomed".to_owned()],
            ..NewStatus::local(&alice, "@ghost #doomed")
        },
    )
    .await
    .expect_err("an unknown mentioned account is the foreign key's to refuse");
    assert!(
        matches!(err, AppError::Database(_)),
        "the failure is the database's: {err}"
    );

    let left: (i64,) = sqlx::query_as("SELECT count(*) FROM statuses")
        .fetch_one(&pool)
        .await
        .unwrap();
    assert_eq!(left.0, 0, "the half-written status rolled back with it");
    let links: (i64,) = sqlx::query_as("SELECT count(*) FROM status_mentions")
        .fetch_one(&pool)
        .await
        .unwrap();
    assert_eq!(links.0, 0, "and nothing was linked");
    let tags: (i64,) = sqlx::query_as(
        "SELECT count(*) FROM hashtags h
               JOIN status_hashtags sh ON sh.hashtag_id = h.id",
    )
    .fetch_one(&pool)
    .await
    .unwrap();
    assert_eq!(tags.0, 0, "and the hashtag's link rolled back with it");
    assert_eq!(
        fresh_user(&pool, alice.id).await.statuses_count,
        0,
        "the author's count went back too"
    );
}

/// The names of the batch read's entry for `status_id`, in order.
async fn tag_names(pool: &PgPool, status_id: Uuid) -> Vec<String> {
    let map = hashtag::for_statuses(pool, &[status_id])
        .await
        .expect("the batch read works");
    map.get(&status_id)
        .map_or_default(|tags| tags.iter().map(|tag| tag.name.clone()).collect())
}

#[tokio::test]
async fn insert_links_the_hashtags_extracted_from_local_text() {
    let db = crate::test_db::test_db().await.expect("test database slot");
    let pool = db.pool().clone();
    let alice = user(&pool, "alice").await;

    let status = post(&pool, &alice, "hello #World #foo #FOO").await;
    assert_eq!(
        tag_names(&pool, status.id).await,
        vec!["foo", "world"],
        "the local post's tags were extracted on insert, in name order, \
             case folded"
    );

    let twin = post(&pool, &alice, "#foo again").await;
    let shared = post(&pool, &alice, "#bar").await;
    assert_eq!(
        tag_names(&pool, twin.id).await,
        vec!["foo"],
        "a second post with the same tag links the same tag"
    );
    let ids: (i64,) = sqlx::query_as("SELECT count(*) FROM hashtags")
        .fetch_one(&pool)
        .await
        .unwrap();
    assert_eq!(ids.0, 3, "foo, world and bar — one row per name");
    let foos: (i64,) = sqlx::query_as(
        "SELECT count(*) FROM status_hashtags sh
               JOIN hashtags h ON h.id = sh.hashtag_id
              WHERE h.name = 'foo'",
    )
    .fetch_one(&pool)
    .await
    .unwrap();
    assert_eq!(foos.0, 2, "and two statuses point at the one foo row");
    assert_eq!(
        tag_names(&pool, shared.id).await,
        vec!["bar"],
        "an untouched tag keeps its own status"
    );

    let plain = post(&pool, &alice, "no tags here").await;
    assert!(
        tag_names(&pool, plain.id).await.is_empty(),
        "a status without hashtags links none"
    );
}

#[tokio::test]
async fn insert_links_the_explicit_hashtags_of_a_remote_post() {
    let db = crate::test_db::test_db().await.expect("test database slot");
    let pool = db.pool().clone();
    let mallory = remote_user(&pool, "mallory").await;
    let status = insert(
        &pool,
        BASE,
        &NewStatus {
            hashtags: vec![
                "FromAP".to_owned(),
                "#dup".to_owned(),
                "  ".to_owned(),
                "dup".to_owned(),
            ],
            ..mirrored(&mallory, "https://remote.example/posts/tags")
        },
    )
    .await
    .unwrap();

    assert_eq!(
        tag_names(&pool, status.id).await,
        vec!["dup", "fromap"],
        "the caller's tag list is normalized, folded and linked \
             name-first, and the blank name was ignored"
    );
}

#[tokio::test]
async fn a_mirrored_status_without_a_uri_is_rejected() {
    let db = crate::test_db::test_db().await.expect("test database slot");
    let pool = db.pool().clone();
    let mallory = remote_user(&pool, "mallory").await;
    let err = insert(
        &pool,
        BASE,
        &NewStatus {
            user_id: mallory.id,
            ..NewStatus::default()
        },
    )
    .await
    .expect_err("a mirrored post must name its object id");
    assert_eq!(
        sqlstate_of(err),
        "23502",
        "the failure is the NOT NULL uri column"
    );
    assert_eq!(
        fresh_user(&pool, mallory.id).await.statuses_count,
        0,
        "and the transaction counts nothing it did not write"
    );
}

#[tokio::test]
async fn an_unknown_parent_or_author_is_rejected() {
    let db = crate::test_db::test_db().await.expect("test database slot");
    let pool = db.pool().clone();
    let alice = user(&pool, "alice").await;
    let err = insert(
        &pool,
        BASE,
        &NewStatus {
            in_reply_to_id: Some(new_id()),
            ..NewStatus::local(&alice, "nowhere")
        },
    )
    .await
    .expect_err("replying to an unsaved status is not possible");
    assert_eq!(
        sqlstate_of(err),
        "23503",
        "the failure is the in_reply_to_id foreign key"
    );

    let ghost_err = insert(
        &pool,
        BASE,
        &NewStatus {
            user_id: new_id(),
            ..NewStatus::local(&alice, "ghost")
        },
    )
    .await
    .expect_err("nobody posted this");
    assert_eq!(
        sqlstate_of(ghost_err),
        "23503",
        "the failure is the user_id foreign key"
    );
    assert_eq!(
        fresh_user(&pool, alice.id).await.statuses_count,
        0,
        "neither attempt left a count behind"
    );
}

/// A [`NewStatus`] by `author` carrying `media_ids` — the shape a local
/// post travels with when its body names attachments.
fn posting_with_media(author: &User, media_ids: Vec<Uuid>) -> NewStatus {
    NewStatus {
        media_ids,
        ..NewStatus::local(author, "pictures ride along")
    }
}

/// One unattached upload by `owner`, as the row `media_ids` names.
async fn upload(pool: &PgPool, owner: &User) -> MediaAttachment {
    media_attachment::insert(
        pool,
        &NewMediaAttachment {
            user_id: Some(owner.id),
            url: format!("{BASE}/media/fixture.png"),
            content_type: "image/png".to_owned(),
            ..NewMediaAttachment::default()
        },
    )
    .await
    .expect("a fixture upload inserts")
}

/// The media arm of [`insert`]: the named rows the author owns and no
/// status has claimed are taken on the insert's own transaction, each
/// at the position its id holds in the list; one id the author may not
/// claim refuses the whole write — no status row, no half-claimed
/// media.
#[tokio::test]
async fn insert_claims_its_media_and_a_short_count_refuses_the_write() {
    let db = crate::test_db::test_db().await.expect("test database slot");
    let pool = db.pool().clone();
    let alice = user(&pool, "alice").await;
    let bob = user(&pool, "bob").await;
    let first = upload(&pool, &alice).await;
    let second = upload(&pool, &alice).await;

    let status = insert(
        &pool,
        BASE,
        &posting_with_media(&alice, vec![second.id, first.id]),
    )
    .await
    .expect("the author's own unposted media is claimable");
    let rows = media_attachment::for_statuses(&pool, &[status.id])
        .await
        .unwrap();
    let claimed = rows.get(&status.id).expect("the media came with it");
    assert_eq!(
        claimed.iter().map(|row| row.id).collect::<Vec<_>>(),
        vec![second.id, first.id],
        "the render order is the order the list asked for"
    );
    assert_eq!(
        claimed.iter().map(|row| row.position).collect::<Vec<_>>(),
        vec![0, 1],
        "and each row holds its list position"
    );

    let foreign = upload(&pool, &bob).await;
    let live_before: (i64,) =
        sqlx::query_as("SELECT count(*) FROM statuses WHERE deleted_at IS NULL")
            .fetch_one(&pool)
            .await
            .unwrap();
    let err = insert(
        &pool,
        BASE,
        &posting_with_media(&alice, vec![first.id, foreign.id]),
    )
    .await
    .expect_err("a foreign id refuses the write");
    assert!(
        matches!(&err, AppError::Unprocessable(message)
                if message == media_attachment::MEDIA_NOT_FOUND),
        "the refusal is the 422 `Media not found`"
    );
    let live_after: (i64,) =
        sqlx::query_as("SELECT count(*) FROM statuses WHERE deleted_at IS NULL")
            .fetch_one(&pool)
            .await
            .unwrap();
    assert_eq!(
        live_after.0, live_before.0,
        "the refused write rolled its own status row back"
    );
    assert_eq!(
        media_attachment::get_by_id(&pool, first.id)
            .await
            .unwrap()
            .expect("the row stays")
            .status_id,
        Some(status.id),
        "the refused write moved the already-claimed row nowhere"
    );
    assert!(
        media_attachment::get_by_id(&pool, foreign.id)
            .await
            .unwrap()
            .expect("the row stays")
            .status_id
            .is_none(),
        "and touched nothing else either"
    );
}
