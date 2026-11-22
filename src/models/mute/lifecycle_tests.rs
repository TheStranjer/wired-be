//! Mute lifecycle tests: updates, lapses, constraints and the expiry sweep.

use uuid::Uuid;

use super::test_support::{
    TIMELINE_ONLY, mute_count, mute_expiring_in, seconds_ahead, seed_mute, sqlstate_of,
    stored_mute, user, write_mute,
};
use super::*;
use crate::{error::AppError, util::id::new_id, util::time::mastodon_time};

#[tokio::test]
async fn muting_twice_updates_the_options_of_one_row() {
    let db = crate::test_db::test_db().await.expect("test database slot");
    let pool = db.pool().clone();
    let alice = user(&pool, "alice").await;
    let bob = user(&pool, "bob").await;
    let first = write_mute(&pool, &alice, &bob, MuteOptions::default()).await;

    let again = mute(&pool, &alice, &bob, TIMELINE_ONLY)
        .await
        .expect("the repeat mute is not an error");
    assert_eq!(again.id, first.id, "the pair keeps its one row");
    assert_eq!(
        again.inserted_at, first.inserted_at,
        "and its original insertion time"
    );
    assert!(
        !again.hide_notifications,
        "the options of the repeat write are stored"
    );
    assert_eq!(mute_count(&pool).await, 1, "nothing was written twice");

    let timed = MuteOptions {
        notifications: true,
        expires_in: Some(120),
    };
    let extended = mute(&pool, &alice, &bob, timed)
        .await
        .expect("re-muting with a duration stores");
    assert_eq!(extended.id, first.id, "still the same row");
    assert!(
        extended.hide_notifications,
        "and the latest options are the stored ones"
    );
    let ahead = seconds_ahead(&extended);
    assert!(
        (110..=120).contains(&ahead),
        "the duration is rewritten from the new write, got {ahead}s"
    );
}

/// Re-muting a pair whose mute has lapsed but is still stored revives it:
/// the same row, a fresh expiry, and the reads find it again.
#[tokio::test]
async fn re_muting_a_lapsed_mute_revives_the_row() {
    let db = crate::test_db::test_db().await.expect("test database slot");
    let pool = db.pool().clone();
    let alice = user(&pool, "alice").await;
    let bob = user(&pool, "bob").await;
    mute_expiring_in(&pool, &alice, &bob, -60).await;
    let lapsed = stored_mute(&pool, &alice, &bob).await;
    assert!(
        !is_muting(&pool, alice.id, bob.id).await.unwrap(),
        "the fixture mute is over"
    );

    let revived = mute(&pool, &alice, &bob, MuteOptions::default())
        .await
        .expect("muting again stores");
    assert_eq!(revived.id, lapsed.id, "the pair's row is reused");
    assert!(revived.expires_at.is_none(), "and is indefinite again");
    assert_eq!(mute_count(&pool).await, 1, "still one row for the pair");
    assert!(
        is_muting(&pool, alice.id, bob.id).await.unwrap(),
        "and the reads find the mute again"
    );
}

#[tokio::test]
async fn unmuting_reports_the_row_it_removed() {
    let db = crate::test_db::test_db().await.expect("test database slot");
    let pool = db.pool().clone();
    let alice = user(&pool, "alice").await;
    let bob = user(&pool, "bob").await;

    assert!(
        unmute(&pool, &alice, &bob).await.unwrap().is_none(),
        "a pair that was never muted has nothing to lift"
    );

    let row = write_mute(&pool, &alice, &bob, MuteOptions::default()).await;
    let removed = unmute(&pool, &alice, &bob).await.unwrap();
    assert_eq!(
        removed.as_ref(),
        Some(&row),
        "the removed row comes back as it stood"
    );
    assert!(
        !is_muting(&pool, alice.id, bob.id).await.unwrap(),
        "and the fact is gone"
    );
    assert!(
        unmute(&pool, &alice, &bob).await.unwrap().is_none(),
        "lifting it twice finds nothing the second time"
    );
    assert_eq!(mute_count(&pool).await, 0, "and writes nothing");
}

/// A lapsed mute's row is nobody's mute to the reads and yet still a row
/// for the pair key: lifting it is a delete of that row, and muting again
/// afterwards is an ordinary write.
#[tokio::test]
async fn unmute_removes_a_lapse_that_no_read_would_hide() {
    let db = crate::test_db::test_db().await.expect("test database slot");
    let pool = db.pool().clone();
    let alice = user(&pool, "alice").await;
    let bob = user(&pool, "bob").await;
    mute_expiring_in(&pool, &alice, &bob, -60).await;

    let removed = unmute(&pool, &alice, &bob)
        .await
        .expect("lifting the lapsed mute runs");
    assert!(
        removed.is_some(),
        "the row the reads ignore is the row the delete reports"
    );
    assert_eq!(mute_count(&pool).await, 0, "and it is gone");
}

#[tokio::test]
async fn muting_yourself_is_refused() {
    let db = crate::test_db::test_db().await.expect("test database slot");
    let pool = db.pool().clone();
    let alice = user(&pool, "alice").await;

    let err = mute(&pool, &alice, &alice, MuteOptions::default())
        .await
        .expect_err("nobody mutes themselves");
    assert!(
        matches!(err, AppError::Unprocessable(_)),
        "a self-mute is the 422 the endpoint renders: {err}"
    );
    assert_eq!(err.to_string(), "Can not mute yourself", "its wording");
    assert_eq!(mute_count(&pool).await, 0, "and it stored nothing");
    assert!(
        !is_muting(&pool, alice.id, alice.id).await.unwrap(),
        "the read agrees"
    );
}

/// The pair key the model's upsert builds on, checked against the table,
/// and the directionality no constraint merges away.
#[tokio::test]
async fn the_table_constraints_are_the_ones_the_model_relies_on() {
    let db = crate::test_db::test_db().await.expect("test database slot");
    let pool = db.pool().clone();
    let alice = user(&pool, "alice").await;
    let bob = user(&pool, "bob").await;
    seed_mute(&pool, &alice, &bob, MuteOptions::default()).await;

    let duplicate = sqlx::query("INSERT INTO mutes (id, muter_id, muted_id) VALUES ($1, $2, $3)")
        .bind(new_id())
        .bind(alice.id)
        .bind(bob.id)
        .execute(&pool)
        .await
        .expect_err("one row per pair");
    assert_eq!(
        sqlstate_of(duplicate),
        "23505",
        "the pair is unique, which is what mute()'s upsert builds on"
    );

    // The same pair the other way is a different mute.
    let flipped = mute(&pool, &bob, &alice, TIMELINE_ONLY)
        .await
        .expect("the reverse pair is its own row");
    assert_eq!(mute_count(&pool).await, 2, "one row per direction");
    assert!(
        !flipped.hide_notifications,
        "each with its own stored options"
    );
}

/// Deleting either account takes the mute through the foreign key's
/// cascade, from both ends of the pair.
#[tokio::test]
async fn deleting_either_side_takes_the_mute() {
    let db = crate::test_db::test_db().await.expect("test database slot");
    let pool = db.pool().clone();
    let alice = user(&pool, "alice").await;
    let bob = user(&pool, "bob").await;
    let carol = user(&pool, "carol").await;
    seed_mute(&pool, &alice, &bob, MuteOptions::default()).await;
    seed_mute(&pool, &carol, &alice, MuteOptions::default()).await;

    let deleted = sqlx::query("DELETE FROM users WHERE id = $1")
        .bind(bob.id)
        .execute(&pool)
        .await
        .expect("the muted account deletes");
    assert_eq!(deleted.rows_affected(), 1, "one account went");
    assert_eq!(
        muted_ids(&pool, alice.id).await.unwrap(),
        Vec::<Uuid>::new(),
        "alice's mute of the deleted account went with it"
    );

    let gone = sqlx::query("DELETE FROM users WHERE id = $1")
        .bind(alice.id)
        .execute(&pool)
        .await
        .expect("the muting account deletes");
    assert_eq!(gone.rows_affected(), 1, "and so did the other account");
    assert_eq!(
        muted_ids(&pool, carol.id).await.unwrap(),
        Vec::<Uuid>::new(),
        "carol's mute of the deleted account cascaded too"
    );
}

/// The `mutes.expire` job's whole statement: expired rows go, indefinite
/// ones and still-running timed ones stay.
#[tokio::test]
async fn the_sweep_deletes_only_expired_rows() {
    let db = crate::test_db::test_db().await.expect("test database slot");
    let pool = db.pool().clone();
    let alice = user(&pool, "alice").await;
    let bob = user(&pool, "bob").await;
    let carol = user(&pool, "carol").await;
    let dave = user(&pool, "dave").await;
    mute_expiring_in(&pool, &alice, &bob, -60).await;
    mute_expiring_in(&pool, &alice, &carol, -1).await;
    let indefinite = write_mute(&pool, &alice, &dave, MuteOptions::default()).await;
    let running = MuteOptions {
        notifications: true,
        expires_in: Some(600),
    };
    seed_mute(&pool, &bob, &dave, running).await;

    let removed = delete_expired(&pool).await.expect("the sweep runs");
    assert_eq!(removed, 2, "both lapsed rows went");
    assert_eq!(
        muted_ids(&pool, alice.id).await.unwrap(),
        vec![dave.id],
        "the indefinite mute is untouched"
    );
    assert_eq!(
        stored_mute(&pool, &alice, &dave).await.id,
        indefinite.id,
        "the same row, not a rewritten one"
    );
    assert!(
        is_muting(&pool, bob.id, dave.id).await.unwrap(),
        "and a mute whose expiry is still ahead stays"
    );

    let again = delete_expired(&pool).await.expect("a second sweep runs");
    assert_eq!(again, 0, "and finds nothing left to delete");
    assert_eq!(mute_count(&pool).await, 2, "leaving the live rows alone");
}

/// A sweep of an empty table is a no-op, so the periodic job costs nothing
/// when there is nothing to do.
#[tokio::test]
async fn the_sweep_of_an_empty_table_deletes_nothing() {
    let db = crate::test_db::test_db().await.expect("test database slot");
    let pool = db.pool().clone();
    let removed = delete_expired(&pool)
        .await
        .expect("the sweep over nothing runs");
    assert_eq!(removed, 0, "no rows, no deletions");
}

/// A mute of one second is a live mute the moment it is written and a
/// lapsed one once its instant has passed: the reads and the sweep agree on
/// which side of the database's `now()` it is on.
#[tokio::test]
async fn a_lapsing_mute_leaves_the_reads_with_the_sweep() {
    let db = crate::test_db::test_db().await.expect("test database slot");
    let pool = db.pool().clone();
    let alice = user(&pool, "alice").await;
    let bob = user(&pool, "bob").await;
    let options = MuteOptions {
        notifications: true,
        expires_in: Some(1),
    };
    let row = write_mute(&pool, &alice, &bob, options).await;
    let ahead = seconds_ahead(&row);
    assert!(
        (0..=1).contains(&ahead),
        "the expiry is the second the caller asked for, got {ahead}s ahead \
         at {}",
        row.expires_at
            .map_or_else(|| "nothing".to_owned(), |at| mastodon_time(&at))
    );

    let lapsed = "UPDATE mutes SET expires_at = now() - make_interval(secs => 1)
                   WHERE muter_id = $1 AND muted_id = $2";
    let moved = sqlx::query(lapsed)
        .bind(alice.id)
        .bind(bob.id)
        .execute(&pool)
        .await
        .expect("the fixture moves the expiry into the past");
    assert_eq!(moved.rows_affected(), 1, "the row was there to move");

    assert!(
        !is_muting(&pool, alice.id, bob.id).await.unwrap(),
        "the reads dropped it"
    );
    let removed = delete_expired(&pool).await.expect("the sweep runs");
    assert_eq!(removed, 1, "and so does the sweep");
    assert_eq!(mute_count(&pool).await, 0, "leaving nothing behind");
}
