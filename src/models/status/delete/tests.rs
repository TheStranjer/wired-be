//! The soft-delete tests: what a tombstone hides and keeps, how the
//! counters and the author's count step back with it, and the born-
//! tombstone's `uri` reservation.

use sqlx::PgPool;
use uuid::Uuid;

use super::*;
use crate::models::{
    media_attachment::{self, MediaAttachment, NewMediaAttachment},
    status::{Counter, NewStatus, get_by_id, get_by_id_including_deleted, insert, test_support::*},
    user::{self, User},
};

/// `author`'s reblog of the status `original`, as its row.
async fn boost(pool: &PgPool, author: &User, original: Uuid) -> Status {
    insert(
        pool,
        BASE,
        &NewStatus {
            reblog_of_id: Some(original),
            ..NewStatus::local(author, "")
        },
    )
    .await
    .expect("a reblog of a stored status inserts")
}

/// One media attachment of `author` posted on `status`: the row a
/// deleted status leaves behind, referenced by nothing live.
async fn attach(pool: &PgPool, author: &User, status: Uuid) -> MediaAttachment {
    media_attachment::insert(
        pool,
        &NewMediaAttachment {
            user_id: Some(author.id),
            status_id: Some(status),
            url: format!("{BASE}/media/fixture.png"),
            content_type: "image/png".to_owned(),
            ..NewMediaAttachment::default()
        },
    )
    .await
    .expect("a fixture attachment inserts")
}

/// The same quote row tombstoned steps its own +1 back down, and a
/// second delete finds nothing to step twice.
#[tokio::test]
async fn deleting_a_quote_steps_the_original_back() {
    let db = crate::test_db::test_db().await.expect("test database slot");
    let pool = db.pool().clone();
    let alice = user(&pool, "alice").await;
    let bob = user(&pool, "bob").await;
    let original = post(&pool, &alice, "quote me").await;
    let quote = quote(&pool, &bob, original.id, "I second that").await;

    let tombstone = soft_delete(&pool, quote.id)
        .await
        .unwrap()
        .expect("the quote was live");
    assert_eq!(
        tombstone.quote_of_id,
        Some(original.id),
        "the tombstone still names what it quoted"
    );
    assert_eq!(
        counter(&pool, original.id, Counter::Quotes).await,
        0,
        "and the original no longer counts it"
    );

    assert!(
        soft_delete(&pool, quote.id).await.unwrap().is_none(),
        "deleting the tombstone again finds nothing"
    );
    assert_eq!(
        counter(&pool, original.id, Counter::Quotes).await,
        0,
        "so the quote step does not run twice"
    );
}

/// `quote_of_id` is `ON DELETE SET NULL`: hard-deleting the original
/// unlinks the quoting rows with it — the counter's row is simply gone,
/// and the later deletion of a quoter has nothing left to step.
#[tokio::test]
async fn hard_deleting_the_original_leaves_the_quoter_nothing_to_count() {
    let db = crate::test_db::test_db().await.expect("test database slot");
    let pool = db.pool().clone();
    let alice = user(&pool, "alice").await;
    let bob = user(&pool, "bob").await;
    let original = post(&pool, &alice, "gone quoted").await;
    let quote = quote(&pool, &bob, original.id, "I second that").await;
    assert_eq!(
        counter(&pool, original.id, Counter::Quotes).await,
        1,
        "the quote counts while the original is there"
    );

    let dropped = sqlx::query("DELETE FROM statuses WHERE id = $1")
        .bind(original.id)
        .execute(&pool)
        .await
        .unwrap();
    assert_eq!(dropped.rows_affected(), 1, "the fixture original existed");

    let survivor = get_by_id(&pool, quote.id)
        .await
        .unwrap()
        .expect("the quoter row is not deleted with what it quoted");
    assert_eq!(
        survivor.quote_of_id, None,
        "the foreign key's SET NULL unlinks the quoter with its target"
    );
    assert!(
        get_by_id_including_deleted(&pool, original.id)
            .await
            .unwrap()
            .is_none(),
        "the counted row is gone, so there is nothing to count"
    );
    assert!(
        soft_delete(&pool, quote.id).await.unwrap().is_some(),
        "the quoter's own delete runs clean with no link to step"
    );
}

#[tokio::test]
async fn soft_delete_hides_the_status_and_undoes_its_counts() {
    let db = crate::test_db::test_db().await.expect("test database slot");
    let pool = db.pool().clone();
    let alice = user(&pool, "alice").await;
    let bob = user(&pool, "bob").await;
    let root = post(&pool, &alice, "to be deleted").await;
    let answer = reply(&pool, &bob, &root, "answer").await;

    let tombstone = soft_delete(&pool, answer.id)
        .await
        .unwrap()
        .expect("the reply was live");

    assert_eq!(tombstone.id, answer.id, "the tombstone is that row");
    assert!(tombstone.deleted_at.is_some(), "marked deleted");
    assert_eq!(tombstone.content, "", "its content is cleared");
    assert_eq!(tombstone.raw_text, "", "its source text is cleared");
    assert_eq!(tombstone.spoiler_text, "", "and its content warning");
    assert_eq!(
        tombstone.uri, answer.uri,
        "its uri survives, reserved against a re-import"
    );
    assert_eq!(
        tombstone.visibility, answer.visibility,
        "everything else of the row is kept"
    );
    assert!(
        get_by_id(&pool, answer.id).await.unwrap().is_none(),
        "a tombstone is not a status any more"
    );
    assert!(
        get_by_id_including_deleted(&pool, answer.id)
            .await
            .unwrap()
            .is_some(),
        "but the row is still there"
    );
    assert_eq!(
        counter(&pool, root.id, Counter::Replies).await,
        0,
        "the parent stops counting the answer"
    );
    assert_eq!(
        fresh_user(&pool, bob.id).await.statuses_count,
        0,
        "and so does its author"
    );
    assert_eq!(
        fresh_user(&pool, alice.id).await.statuses_count,
        1,
        "the root's author is untouched"
    );

    assert!(
        soft_delete(&pool, answer.id).await.unwrap().is_none(),
        "deleting twice finds nothing to delete"
    );
    assert_eq!(
        fresh_user(&pool, bob.id).await.statuses_count,
        0,
        "so the author's count is not stepped down twice"
    );
    assert!(
        soft_delete(&pool, new_id()).await.unwrap().is_none(),
        "an unknown id is nothing to delete"
    );
}

#[tokio::test]
async fn an_inserted_tombstone_reserves_the_uri_and_hides_itself() {
    let db = crate::test_db::test_db().await.expect("test database slot");
    let pool = db.pool().clone();
    let mallory = remote_user(&pool, "tombstoner").await;
    let uri = "https://remote.example/posts/deleted-before-arrival";

    let born_deleted = insert_tombstone(&pool, uri, mallory.id)
        .await
        .expect("the tombstone inserts")
        .expect("the uri was free");
    assert_eq!(
        born_deleted.uri, uri,
        "the row keeps the deleted object's id"
    );
    assert_eq!(born_deleted.user_id, mallory.id, "and names its author");
    assert!(born_deleted.deleted_at.is_some(), "it is born a tombstone");
    assert_eq!(born_deleted.content, "", "with nothing to read");
    assert_eq!(
        born_deleted.conversation_id, born_deleted.id,
        "and its own id as its (empty) conversation"
    );
    assert!(
        get_by_id(&pool, born_deleted.id).await.unwrap().is_none(),
        "no live read answers it"
    );

    let again = insert_tombstone(&pool, uri, mallory.id)
        .await
        .expect("the second pass runs");
    assert!(again.is_none(), "the reserved uri inserts nothing twice");
}

#[tokio::test]
async fn a_tombstone_insert_does_not_move_the_author_count() {
    let db = crate::test_db::test_db().await.expect("test database slot");
    let pool = db.pool().clone();
    let mallory = remote_user(&pool, "counter-checked").await;

    let reserved = insert_tombstone(
        &pool,
        "https://remote.example/posts/never-lived",
        mallory.id,
    )
    .await
    .expect("the tombstone inserts");
    assert!(reserved.is_some(), "the row was written");
    let after = user::get_by_id(&pool, mallory.id)
        .await
        .expect("the account read")
        .expect("the account is stored");
    assert_eq!(after.statuses_count, 0, "a born-tombstone posts nothing");
}

#[tokio::test]
async fn deleting_a_reblog_steps_the_original_back() {
    let db = crate::test_db::test_db().await.expect("test database slot");
    let pool = db.pool().clone();
    let alice = user(&pool, "alice").await;
    let bob = user(&pool, "bob").await;
    let original = post(&pool, &alice, "worth boosting").await;

    let reblog = insert(
        &pool,
        BASE,
        &NewStatus {
            reblog_of_id: Some(original.id),
            ..NewStatus::local(&bob, "")
        },
    )
    .await
    .unwrap();
    assert_eq!(reblog.content, "", "a reblog carries no text of its own");
    assert_eq!(
        counter(&pool, original.id, Counter::Reblogs).await,
        1,
        "insert counts it on the original"
    );

    let tombstone = soft_delete(&pool, reblog.id)
        .await
        .unwrap()
        .expect("the reblog was live");
    assert_eq!(
        tombstone.reblog_of_id,
        Some(original.id),
        "the tombstone still names what it boosted"
    );
    assert_eq!(
        counter(&pool, original.id, Counter::Reblogs).await,
        0,
        "and the original no longer counts it"
    );
    assert_eq!(
        fresh_user(&pool, bob.id).await.statuses_count,
        0,
        "the booster stops counting it as their own"
    );
}

#[tokio::test]
async fn deleting_a_status_takes_every_reblog_of_it() {
    let db = crate::test_db::test_db().await.expect("test database slot");
    let pool = db.pool().clone();
    let alice = user(&pool, "alice").await;
    let bob = user(&pool, "bob").await;
    let carol = user(&pool, "carol").await;
    let original = post(&pool, &alice, "delete me").await;
    let other = post(&pool, &carol, "someone else's post").await;
    let mine = boost(&pool, &bob, original.id).await;
    let theirs = boost(&pool, &carol, original.id).await;
    // A post of `other` that survives: only reblogs of the deleted
    // status are taken down, never the booster's unrelated rows.
    let untouched = boost(&pool, &bob, other.id).await;
    let attachment = attach(&pool, &alice, original.id).await;

    let (tombstone, tombstoned_reblogs) = soft_delete_with_reblogs(&pool, original.id)
        .await
        .unwrap()
        .expect("the original was live");

    assert_eq!(tombstone.id, original.id, "the answer is the status");
    assert!(tombstone.deleted_at.is_some(), "and it is a tombstone");
    assert_eq!(tombstone.content, "", "its text is cleared");
    let mut answered: Vec<_> = tombstoned_reblogs.iter().map(|row| row.id).collect();
    answered.sort();
    let mut expected = vec![mine.id, theirs.id];
    expected.sort();
    assert_eq!(
        answered, expected,
        "the answer names exactly the reblogs it tombstoned"
    );
    assert!(
        tombstoned_reblogs
            .iter()
            .all(|row| row.deleted_at.is_some() && row.reblog_of_id == Some(original.id)),
        "each of them as a tombstone still linked to the original"
    );
    for reblog in [&mine, &theirs] {
        assert!(
            get_by_id(&pool, reblog.id).await.unwrap().is_none(),
            "the reblog of the deleted status is gone from the API"
        );
        let stored = get_by_id_including_deleted(&pool, reblog.id)
            .await
            .unwrap()
            .expect("but its row is still there");
        assert!(stored.deleted_at.is_some(), "as a tombstone too");
        assert_eq!(stored.content, "", "with its text cleared");
    }
    assert_eq!(
        counter(&pool, original.id, Counter::Reblogs).await,
        0,
        "the deleted status counts no boosts any more"
    );
    assert_eq!(
        fresh_user(&pool, bob.id).await.statuses_count,
        1,
        "bob keeps his other boost and loses the one that went down"
    );
    assert_eq!(
        fresh_user(&pool, carol.id).await.statuses_count,
        1,
        "and carol keeps her own post and loses hers"
    );
    assert_eq!(
        get_by_id(&pool, untouched.id).await.unwrap(),
        Some(untouched.clone()),
        "a reblog of another status is nobody's business here"
    );
    assert!(
        media_attachment::get_by_id(&pool, attachment.id)
            .await
            .unwrap()
            .is_some(),
        "the media row stays in the table for the cleanup job to find"
    );
}

#[tokio::test]
async fn deleting_twice_with_reblogs_moves_nothing_twice() {
    let db = crate::test_db::test_db().await.expect("test database slot");
    let pool = db.pool().clone();
    let alice = user(&pool, "alice").await;
    let bob = user(&pool, "bob").await;
    let original = post(&pool, &alice, "already gone").await;
    let mine = boost(&pool, &bob, original.id).await;

    assert!(
        soft_delete_with_reblogs(&pool, original.id)
            .await
            .unwrap()
            .is_some(),
        "the first delete finds a live row"
    );
    assert!(
        soft_delete_with_reblogs(&pool, original.id)
            .await
            .unwrap()
            .is_none(),
        "the second one finds only the tombstone"
    );
    assert_eq!(
        fresh_user(&pool, alice.id).await.statuses_count,
        0,
        "so the author is not stepped down twice"
    );
    assert_eq!(
        fresh_user(&pool, bob.id).await.statuses_count,
        0,
        "and neither is the booster"
    );
    assert!(
        soft_delete_with_reblogs(&pool, new_id())
            .await
            .unwrap()
            .is_none(),
        "an unknown id is nothing to delete"
    );
    assert!(
        get_by_id_including_deleted(&pool, mine.id)
            .await
            .unwrap()
            .is_some(),
        "and the reblog row is still there, tombstoned once"
    );
}

#[tokio::test]
async fn deleting_an_author_takes_their_statuses_with_it() {
    let db = crate::test_db::test_db().await.expect("test database slot");
    let pool = db.pool().clone();
    let alice = user(&pool, "alice").await;
    let root = post(&pool, &alice, "root").await;
    let answer = reply(&pool, &alice, &root, "answer").await;
    assert_eq!(
        answer.in_reply_to_id,
        Some(root.id),
        "the thread holds a root and an answer"
    );

    let dropped = sqlx::query("DELETE FROM users WHERE id = $1")
        .bind(alice.id)
        .execute(&pool)
        .await
        .unwrap();
    assert_eq!(dropped.rows_affected(), 1, "the fixture author existed");
    assert!(
        get_by_id_including_deleted(&pool, root.id)
            .await
            .unwrap()
            .is_none(),
        "cascade on user_id removed the author's statuses"
    );
}

/// The number of pins stored for status `id`, tombstoned or not.
async fn pin_rows(pool: &PgPool, id: Uuid) -> i64 {
    sqlx::query_scalar("SELECT count(*) FROM pinned_statuses WHERE status_id = $1")
        .bind(id)
        .fetch_one(pool)
        .await
        .expect("the pin count reads")
}

#[tokio::test]
async fn sweeping_an_account_steps_the_counters_of_other_users_posts() {
    let db = crate::test_db::test_db().await.expect("test database slot");
    let pool = db.pool().clone();
    let alice = user(&pool, "alice").await;
    let bob = user(&pool, "bob").await;
    let target = post(&pool, &bob, "bob's post").await;
    let answer = reply(&pool, &alice, &target, "an answer").await;
    let boosted = boost(&pool, &alice, target.id).await;
    let quoted = quote(&pool, &alice, target.id, "a quote").await;
    assert_eq!(counter(&pool, target.id, Counter::Replies).await, 1);
    assert_eq!(counter(&pool, target.id, Counter::Reblogs).await, 1);
    assert_eq!(counter(&pool, target.id, Counter::Quotes).await, 1);

    let swept = soft_delete_all_for_user(&pool, alice.id).await.unwrap();

    assert_eq!(swept, 3, "the answer, the boost and the quote");
    for id in [answer.id, boosted.id, quoted.id] {
        assert!(
            stored(&pool, id).await.deleted_at.is_some(),
            "alice's row is a tombstone"
        );
    }
    assert_eq!(
        counter(&pool, target.id, Counter::Replies).await,
        0,
        "bob's post stops counting the answer"
    );
    assert_eq!(
        counter(&pool, target.id, Counter::Reblogs).await,
        0,
        "and the boost"
    );
    assert_eq!(
        counter(&pool, target.id, Counter::Quotes).await,
        0,
        "and the quote"
    );
    assert!(
        get_by_id(&pool, target.id).await.unwrap().is_some(),
        "bob's post is still live"
    );
    assert_eq!(
        fresh_user(&pool, bob.id).await.statuses_count,
        1,
        "bob's own count is not touched"
    );
}

#[tokio::test]
async fn sweeping_an_account_clears_the_pins_of_its_posts() {
    let db = crate::test_db::test_db().await.expect("test database slot");
    let pool = db.pool().clone();
    let alice = user(&pool, "alice").await;
    let bob = user(&pool, "bob").await;
    let pinned = post(&pool, &alice, "pinned").await;
    let other = post(&pool, &bob, "bob's pinned").await;
    let alice_pin = crate::models::pinned_status::pin(&pool, alice.id, pinned.id)
        .await
        .unwrap();
    assert!(alice_pin.is_some(), "alice's pin is stored");
    let bob_pin = crate::models::pinned_status::pin(&pool, bob.id, other.id)
        .await
        .unwrap();
    assert!(bob_pin.is_some(), "bob's pin is stored");

    let swept = soft_delete_all_for_user(&pool, alice.id).await.unwrap();

    assert_eq!(swept, 1, "alice's one post is tombstoned");
    assert_eq!(pin_rows(&pool, pinned.id).await, 0, "alice's pin is gone");
    assert_eq!(pin_rows(&pool, other.id).await, 1, "bob's pin stays");
}

#[tokio::test]
async fn a_sweep_of_more_than_one_batch_brings_the_counter_to_exactly_zero() {
    let db = crate::test_db::test_db().await.expect("test database slot");
    let pool = db.pool().clone();
    let alice = user(&pool, "alice").await;
    let bob = user(&pool, "bob").await;
    let target = post(&pool, &bob, "bob's post").await;
    let survivor = reply(&pool, &bob, &target, "bob answers himself").await;
    let rows = SOFT_DELETE_BATCH + 1;
    for _ in 0..rows {
        let answer = reply(&pool, &alice, &target, "an answer").await;
        assert_eq!(answer.in_reply_to_id, Some(target.id), "it answers P");
    }
    assert_eq!(
        i64::from(counter(&pool, target.id, Counter::Replies).await),
        rows + 1,
        "every answer counts"
    );

    let swept = soft_delete_all_for_user(&pool, alice.id).await.unwrap();

    assert_eq!(i64::try_from(swept).unwrap(), rows, "every row is swept");
    assert_eq!(
        counter(&pool, target.id, Counter::Replies).await,
        1,
        "only bob's own answer is left on the books"
    );
    assert!(
        get_by_id(&pool, survivor.id).await.unwrap().is_some(),
        "and it is live"
    );
}

#[tokio::test]
async fn sweeping_an_account_twice_moves_nothing_the_second_time() {
    let db = crate::test_db::test_db().await.expect("test database slot");
    let pool = db.pool().clone();
    let alice = user(&pool, "alice").await;
    let bob = user(&pool, "bob").await;
    let target = post(&pool, &bob, "bob's post").await;
    let alices = reply(&pool, &alice, &target, "an answer").await;
    let bobs = reply(&pool, &bob, &target, "bob's answer").await;
    assert_ne!(alices.id, bobs.id, "two separate answers");

    assert_eq!(soft_delete_all_for_user(&pool, alice.id).await.unwrap(), 1);
    assert_eq!(counter(&pool, target.id, Counter::Replies).await, 1);

    assert_eq!(
        soft_delete_all_for_user(&pool, alice.id).await.unwrap(),
        0,
        "nothing is left to tombstone"
    );
    assert_eq!(
        counter(&pool, target.id, Counter::Replies).await,
        1,
        "so nothing is stepped twice"
    );
}

#[tokio::test]
async fn sweeping_an_account_with_an_internal_thread_tombstones_it_whole() {
    let db = crate::test_db::test_db().await.expect("test database slot");
    let pool = db.pool().clone();
    let alice = user(&pool, "alice").await;
    let root = post(&pool, &alice, "root").await;
    let answer = reply(&pool, &alice, &root, "answer").await;

    let swept = soft_delete_all_for_user(&pool, alice.id).await.unwrap();

    assert_eq!(swept, 2, "both rows are tombstoned");
    assert!(stored(&pool, root.id).await.deleted_at.is_some());
    assert!(stored(&pool, answer.id).await.deleted_at.is_some());
}
