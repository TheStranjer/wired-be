//! The reblog tests: the row the boost write stores, its idempotence and
//! withdrawal, the counter steps that ride them, and the reads that name
//! the boosters.

use std::collections::HashSet;

use sqlx::PgPool;
use uuid::Uuid;

use super::*;
use crate::models::status::{
    Counter, NewStatus, insert, soft_delete_with_reblogs, test_support::*,
};

/// `author`'s reblog of the status `original` through the reblog write,
/// asserting this call created the row — the state the idempotence tests
/// start from.
async fn reblog_it(pool: &PgPool, author: &User, original: &Status) -> Status {
    let (reblog, created) = create_reblog(pool, BASE, author, original, Visibility::Public)
        .await
        .expect("a reblog of a stored status writes");
    assert!(created, "the fixture pair had not reblogged yet");
    reblog
}

/// `author` reblogs `original` through the write, for tests that only
/// need the row to exist.
async fn reblog_as(pool: &PgPool, author: &User, original: &Status) {
    let reblog = reblog_it(pool, author, original).await;
    assert_eq!(
        reblog.reblog_of_id,
        Some(original.id),
        "the fixture row names its original"
    );
}

/// The stored `reblogs_count` of status `id`, named the way the reblog
/// tests read it.
async fn reblog_count(pool: &PgPool, id: Uuid) -> i32 {
    counter(pool, id, Counter::Reblogs).await
}

#[tokio::test]
async fn create_reblog_writes_the_activity_row_and_steps_both_counters() {
    let db = crate::test_db::test_db().await.expect("test database slot");
    let pool = db.pool().clone();
    let alice = user(&pool, "alice").await;
    let bob = user(&pool, "bob").await;
    let original = post(&pool, &alice, "worth boosting").await;
    let thread_member = reply(&pool, &alice, &original, "an answer").await;

    let (reblog, created) = create_reblog(&pool, BASE, &bob, &original, Visibility::Unlisted)
        .await
        .unwrap();

    assert!(created, "the first call wrote the row");
    assert_eq!(
        reblog.uri,
        format!("{BASE}/activities/{}", reblog.id),
        "a reblog row is the Announce: its uri is the activity id, not an object id"
    );
    assert_eq!(
        reblog.url,
        Some(format!("{BASE}/notice/{}", reblog.id)),
        "and the human url derives from the same id as any local row's"
    );
    assert_eq!(reblog.content, "", "a reblog carries no text of its own");
    assert_eq!(reblog.raw_text, "", "not even as source");
    assert_eq!(
        reblog.visibility,
        Visibility::Unlisted,
        "the requested visibility is the stored one"
    );
    assert!(reblog.local, "a boost made here is local");
    assert_eq!(
        reblog.reblog_of_id,
        Some(original.id),
        "the row names what it boosts"
    );
    assert_eq!(
        reblog.conversation_id, original.conversation_id,
        "a boost joins the conversation of what it boosts"
    );
    assert_eq!(
        reblog.conversation_id, thread_member.conversation_id,
        "which is the same thread the original's answers share"
    );

    assert_eq!(
        reblog_count(&pool, original.id).await,
        1,
        "the original counts one boost"
    );
    assert_eq!(
        fresh_user(&pool, bob.id).await.statuses_count,
        1,
        "and so does the reblogger"
    );
}

#[tokio::test]
async fn reblogging_twice_is_one_row_and_one_count() {
    let db = crate::test_db::test_db().await.expect("test database slot");
    let pool = db.pool().clone();
    let alice = user(&pool, "alice").await;
    let bob = user(&pool, "bob").await;
    let original = post(&pool, &alice, "boost me once").await;
    let first = reblog_it(&pool, &bob, &original).await;

    let (again, created) = create_reblog(&pool, BASE, &bob, &original, Visibility::Public)
        .await
        .expect("repeating a reblog is not an error");

    assert!(!created, "the repeat wrote nothing");
    assert_eq!(
        again.id, first.id,
        "and answers the row that is already there"
    );
    assert_eq!(
        again.visibility, first.visibility,
        "the restated row keeps the visibility of the write that made it"
    );
    assert_eq!(
        reblog_count(&pool, original.id).await,
        1,
        "one boost, one count"
    );
    assert_eq!(
        fresh_user(&pool, bob.id).await.statuses_count,
        1,
        "and the reblogger counts it once"
    );
}

#[tokio::test]
async fn a_withdrawn_reblog_leaves_the_pair_free_again() {
    let db = crate::test_db::test_db().await.expect("test database slot");
    let pool = db.pool().clone();
    let alice = user(&pool, "alice").await;
    let bob = user(&pool, "bob").await;
    let original = post(&pool, &alice, "boost, unboost, boost").await;
    let first = reblog_it(&pool, &bob, &original).await;

    let tombstone = remove_reblog(&pool, bob.id, original.id)
        .await
        .unwrap()
        .expect("the live reblog was there to withdraw");
    assert_eq!(
        tombstone.id, first.id,
        "the withdrawal answers the row that went"
    );
    assert!(tombstone.deleted_at.is_some(), "and it is a tombstone");
    assert_eq!(
        tombstone.reblog_of_id,
        Some(original.id),
        "the tombstone still names what it boosted"
    );

    let (second, created) = create_reblog(&pool, BASE, &bob, &original, Visibility::Public)
        .await
        .expect("reblogging after a withdrawal is allowed");
    assert!(
        created,
        "the partial index is free again once the row is gone"
    );
    assert_ne!(second.id, first.id, "and the new boost is a new row");
    assert_eq!(
        reblog_count(&pool, original.id).await,
        1,
        "the original counts the survivor"
    );
    assert_eq!(
        fresh_user(&pool, bob.id).await.statuses_count,
        1,
        "and bob's own count came back up through it"
    );
}

#[tokio::test]
async fn remove_reblog_steps_both_counters_back_exactly_once() {
    let db = crate::test_db::test_db().await.expect("test database slot");
    let pool = db.pool().clone();
    let alice = user(&pool, "alice").await;
    let bob = user(&pool, "bob").await;
    let original = post(&pool, &alice, "unboost me").await;
    reblog_as(&pool, &bob, &original).await;

    assert!(
        remove_reblog(&pool, bob.id, original.id)
            .await
            .unwrap()
            .is_some(),
        "the first withdrawal finds the row"
    );
    assert_eq!(
        reblog_count(&pool, original.id).await,
        0,
        "the original stops counting it"
    );
    assert_eq!(
        fresh_user(&pool, bob.id).await.statuses_count,
        0,
        "and so does the reblogger"
    );

    assert!(
        remove_reblog(&pool, bob.id, original.id)
            .await
            .unwrap()
            .is_none(),
        "the second finds nothing to withdraw"
    );
    assert!(
        remove_reblog(&pool, alice.id, original.id)
            .await
            .unwrap()
            .is_none(),
        "and the author never reblogged their own post"
    );
    assert!(
        remove_reblog(&pool, bob.id, new_id())
            .await
            .unwrap()
            .is_none(),
        "an unknown original is nothing to withdraw"
    );
    assert_eq!(
        reblog_count(&pool, original.id).await,
        0,
        "no step moved twice"
    );
    assert_eq!(
        fresh_user(&pool, bob.id).await.statuses_count,
        0,
        "and no author-count step ran twice either"
    );
}

#[tokio::test]
async fn a_deleted_original_takes_the_reblog_flag_with_it() {
    let db = crate::test_db::test_db().await.expect("test database slot");
    let pool = db.pool().clone();
    let alice = user(&pool, "alice").await;
    let bob = user(&pool, "bob").await;
    let original = post(&pool, &alice, "gone boosted").await;
    reblog_as(&pool, &bob, &original).await;

    assert!(
        soft_delete_with_reblogs(&pool, original.id)
            .await
            .unwrap()
            .is_some(),
        "the delete finds the live original"
    );
    assert_eq!(
        reblogged_status_ids(&pool, bob.id, &[original.id])
            .await
            .unwrap(),
        HashSet::new(),
        "the tombstoned boost flags nothing any more"
    );
    assert_eq!(
        reblog_count(&pool, original.id).await,
        0,
        "and the count went down with it"
    );
}

#[tokio::test]
async fn reblogged_status_ids_answers_the_live_pairs() {
    let db = crate::test_db::test_db().await.expect("test database slot");
    let pool = db.pool().clone();
    let alice = user(&pool, "alice").await;
    let bob = user(&pool, "bob").await;
    let carol = user(&pool, "carol").await;
    let first = post(&pool, &alice, "first").await;
    let second = post(&pool, &alice, "second").await;
    let third = post(&pool, &alice, "third").await;
    reblog_as(&pool, &bob, &first).await;
    reblog_as(&pool, &bob, &third).await;
    reblog_as(&pool, &carol, &second).await;

    let asked = [first.id, second.id, third.id, new_id()];
    let bob_ids = reblogged_status_ids(&pool, bob.id, &asked).await.unwrap();
    assert!(
        bob_ids.contains(&first.id) && bob_ids.contains(&third.id),
        "bob's two boosts"
    );
    assert!(
        !bob_ids.contains(&second.id),
        "and carol's is nobody else's flag"
    );
    assert_eq!(
        reblogged_status_ids(&pool, carol.id, &asked).await.unwrap(),
        reblogged_status_ids(&pool, carol.id, &[second.id, second.id])
            .await
            .unwrap(),
        "a duplicated ask answers the one flagged original"
    );
    assert!(
        reblogged_status_ids(&pool, alice.id, &asked)
            .await
            .unwrap()
            .is_empty(),
        "the author boosted none of their own posts"
    );
    assert_eq!(
        reblogged_status_ids(&pool, bob.id, &[]).await.unwrap(),
        HashSet::new(),
        "an empty ask is an empty set"
    );

    assert!(
        remove_reblog(&pool, bob.id, first.id)
            .await
            .unwrap()
            .is_some(),
        "bob withdraws one boost"
    );
    assert_eq!(
        reblogged_status_ids(&pool, bob.id, &[first.id, third.id])
            .await
            .unwrap(),
        HashSet::from([third.id]),
        "and the flagged set shrinks with it"
    );
}

#[tokio::test]
async fn local_reblogger_ids_answers_the_live_local_boosters() {
    let db = crate::test_db::test_db().await.expect("test database slot");
    let pool = db.pool().clone();
    let alice = user(&pool, "alice").await;
    let bob = user(&pool, "bob").await;
    let carol = user(&pool, "carol").await;
    let mallory = remote_user(&pool, "mallory").await;
    let original = post(&pool, &alice, "edited soon").await;
    let untouched = post(&pool, &alice, "left alone").await;
    let bobs_boost = reblog_it(&pool, &bob, &original).await;
    reblog_as(&pool, &carol, &original).await;
    // A remote booster mirrors an `Announce` this instance delivers to,
    // never a row it can back with a notification.
    let _mirrored = insert(
        &pool,
        BASE,
        &NewStatus {
            reblog_of_id: Some(original.id),
            ..mirrored(&mallory, "https://remote.example/activities/mallory-boost")
        },
    )
    .await
    .expect("the mirrored boost writes");

    let mut boosters = local_reblogger_ids(&pool, original.id)
        .await
        .expect("the read runs");
    boosters.sort();
    let mut want = vec![bob.id, carol.id];
    want.sort();
    assert_eq!(boosters, want, "both local boosters, no remote one");
    assert!(
        local_reblogger_ids(&pool, untouched.id)
            .await
            .expect("the unboosted read")
            .is_empty(),
        "a post nobody boosts names nobody"
    );

    // A withdrawn boost leaves the audience with its row.
    let tombstone = remove_reblog(&pool, bob.id, original.id)
        .await
        .expect("the withdrawal runs")
        .expect("the boost was there");
    assert!(
        tombstone.deleted_at.is_some(),
        "the withdrawn row {} is the tombstone the read skips",
        bobs_boost.id
    );
    assert_eq!(
        local_reblogger_ids(&pool, original.id)
            .await
            .expect("the reread runs"),
        vec![carol.id],
        "and the tombstone's account is off the list"
    );
}

#[tokio::test]
async fn the_reblogger_page_lists_only_distributable_reblogs() {
    let db = crate::test_db::test_db().await.expect("test database slot");
    let pool = db.pool().clone();
    let alice = user(&pool, "alice").await;
    let original = post(&pool, &alice, "boosted at every visibility").await;
    let mut listed = Vec::new();
    for (name, visibility) in [
        ("public", Visibility::Public),
        ("unlisted", Visibility::Unlisted),
        ("private", Visibility::Private),
        ("direct", Visibility::Direct),
    ] {
        let booster = user(&pool, name).await;
        let (_, created) = create_reblog(&pool, BASE, &booster, &original, visibility)
            .await
            .expect("the reblog writes");
        assert!(created, "the fixture pair had not reblogged yet");
        if matches!(visibility, Visibility::Public | Visibility::Unlisted) {
            listed.push(booster.id);
        }
    }

    let entries = reblogger_page(&pool, original.id, page("")).await.unwrap();
    let mut got: Vec<Uuid> = entries.iter().map(|entry| entry.user.id).collect();
    got.sort();
    listed.sort();
    assert_eq!(
        got, listed,
        "the public and unlisted boosts are listed; private and direct are not"
    );
}

#[tokio::test]
async fn the_reblogger_page_walks_the_reblog_row_ids() {
    let db = crate::test_db::test_db().await.expect("test database slot");
    let pool = db.pool().clone();
    let alice = user(&pool, "alice").await;
    let original = post(&pool, &alice, "boosted by many").await;
    let other = post(&pool, &alice, "boosted once").await;
    let mut rebloggers = Vec::new();
    for n in 0..3 {
        let booster = user(&pool, &format!("booster{n}")).await;
        reblog_as(&pool, &booster, &original).await;
        rebloggers.push(booster);
    }
    let lonely = user(&pool, "lonely").await;
    reblog_as(&pool, &lonely, &other).await;
    let all: Vec<Uuid> = sqlx::query_as::<_, (Uuid,)>(
        "SELECT id FROM statuses WHERE reblog_of_id = $1 ORDER BY id DESC",
    )
    .bind(original.id)
    .fetch_all(&pool)
    .await
    .unwrap()
    .into_iter()
    .map(|(id,)| id)
    .collect();
    assert_eq!(all.len(), 3, "three live boosts of the one original");
    let entry_ids = |entries: &[ReblogPageEntry]| -> Vec<Uuid> {
        entries.iter().map(|entry| entry.reblog_id).collect()
    };

    let first_page = reblogger_page(&pool, original.id, page("limit=2"))
        .await
        .unwrap();
    assert_eq!(
        entry_ids(&first_page),
        vec![all[0], all[1]],
        "the two newest boosts, keyed on the reblog row ids"
    );
    assert_eq!(
        first_page[0].user.id, rebloggers[2].id,
        "and each entry carries the account behind its boost"
    );

    let older = reblogger_page(&pool, original.id, page(&format!("max_id={}", all[1])))
        .await
        .unwrap();
    assert_eq!(
        entry_ids(&older),
        vec![all[2]],
        "max_id continues where the page ended, and `other`'s boost is on no page"
    );

    assert!(
        reblogger_page(&pool, original.id, page(&format!("max_id={}", all[2])))
            .await
            .unwrap()
            .is_empty(),
        "past the oldest boost the page is empty"
    );

    let newer = reblogger_page(&pool, original.id, page(&format!("min_id={}", all[2])))
        .await
        .unwrap();
    assert_eq!(
        entry_ids(&newer),
        vec![all[0], all[1]],
        "min_id answers the newer page, newest-first after the flip"
    );

    assert!(
        remove_reblog(&pool, rebloggers[1].id, original.id)
            .await
            .unwrap()
            .is_some(),
        "the middle booster withdraws"
    );
    let after = reblogger_page(&pool, original.id, page("")).await.unwrap();
    assert_eq!(
        entry_ids(&after),
        vec![all[0], all[2]],
        "a withdrawn boost leaves the page"
    );
}
