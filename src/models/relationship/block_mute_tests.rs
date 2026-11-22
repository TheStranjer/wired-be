//! Block and mute flags on the relationship.

use sqlx::PgPool;
use uuid::Uuid;

use super::test_support::{assert_placeholder_flags, local_user, one_relationship};
use super::*;
use crate::{
    config::TEST_BASE_URL,
    models::{block, mute, user::User},
    util::id::new_id,
};

/// The `(blocking, blocked_by)` pair [`Relationship::load_many`] answers
/// for `viewer` over `targets`, in the order requested.
async fn blocks_of(pool: &PgPool, viewer: &User, targets: &[&User]) -> Vec<(bool, bool)> {
    let ids: Vec<Uuid> = targets.iter().map(|target| target.id).collect();
    Relationship::load_many(pool, viewer.id, &ids)
        .await
        .expect("the batch reads")
        .iter()
        .map(|row| (row.blocking.0, row.blocked_by.0))
        .collect()
}

/// Records `blocker` blocking `target` through the model the table is
/// owned by — the fixture this loader only ever reads.
async fn block_pair(pool: &PgPool, blocker: &User, target: &User) {
    let stored = block::block(pool, TEST_BASE_URL, blocker, target)
        .await
        .expect("the fixture block stores");
    assert_eq!(
        stored.blocked_id, target.id,
        "the row is the pair that was asked for"
    );
}

/// Lifts `blocker`'s block on `target`, asserting one was there to lift.
async fn unblock_pair(pool: &PgPool, blocker: &User, target: &User) {
    let removed = block::unblock(pool, blocker, target)
        .await
        .expect("the fixture unblock runs");
    assert!(removed.is_some(), "the pair's block was there to lift");
}

/// The two directions of the blocks table ride the same batch: the
/// viewer's own block lights `blocking`, a target's block of the viewer
/// lights `blocked_by`, a pair blocked both ways lights both, and a
/// target with no block either way keeps both defaults — as does every
/// account once its block is lifted.
#[tokio::test]
async fn blocks_light_blocking_and_blocked_by() {
    let db = crate::test_db::test_db().await.expect("test database slot");
    let pool = db.pool().clone();
    let alice = local_user(&pool, "alice").await;
    let bob = local_user(&pool, "bob").await;
    let carol = local_user(&pool, "carol").await;
    let dave = local_user(&pool, "dave").await;
    block_pair(&pool, &alice, &bob).await;
    block_pair(&pool, &dave, &alice).await;

    assert_eq!(
        blocks_of(&pool, &alice, &[&bob, &carol, &dave]).await,
        vec![(true, false), (false, false), (false, true)],
        "alice blocks bob, blocks nothing about carol, and is blocked by dave"
    );
    assert_eq!(
        blocks_of(&pool, &bob, &[&alice]).await,
        vec![(false, true)],
        "and bob sees the same row from his own end"
    );

    block_pair(&pool, &bob, &alice).await;
    assert_eq!(
        blocks_of(&pool, &alice, &[&bob]).await,
        vec![(true, true)],
        "a pair blocked both ways lights both flags"
    );

    unblock_pair(&pool, &alice, &bob).await;
    assert_eq!(
        blocks_of(&pool, &alice, &[&bob]).await,
        vec![(false, true)],
        "lifting one direction leaves the other one flag lit"
    );

    unblock_pair(&pool, &bob, &alice).await;
    unblock_pair(&pool, &dave, &alice).await;
    let cleared = one_relationship(&pool, &alice, &bob).await;
    assert!(
        !cleared.blocking.0 && !cleared.blocked_by.0,
        "with nothing blocked the pair falls back to the defaults"
    );
    assert_eq!(
        blocks_of(&pool, &alice, &[&bob, &carol, &dave]).await,
        vec![(false, false); 3],
        "and the whole batch reads clean"
    );
    assert_placeholder_flags(&cleared);
}

/// The `(muting, muting_notifications)` pair [`Relationship::load_many`]
/// answers for `viewer` over `targets`, in the order requested.
async fn mutes_of(pool: &PgPool, viewer: &User, targets: &[&User]) -> Vec<(bool, bool)> {
    let ids: Vec<Uuid> = targets.iter().map(|target| target.id).collect();
    Relationship::load_many(pool, viewer.id, &ids)
        .await
        .expect("the batch reads")
        .iter()
        .map(|row| (row.muting.0, row.muting_notifications.0))
        .collect()
}

/// Mutes `target` for `muter` through the model that owns the table — the
/// fixture this loader only ever reads.
async fn mute_pair(pool: &PgPool, muter: &User, target: &User) {
    let stored = mute::mute(pool, muter, target, mute::MuteOptions::default())
        .await
        .expect("the fixture mute stores");
    assert_eq!(
        stored.muted_id, target.id,
        "the row is the pair that was asked for"
    );
}

/// Mutes `target` for `muter` with the notifications left visible:
/// Mastodon's `notifications: false`, the one mute that tells the two
/// flags apart.
async fn mute_timeline_only(pool: &PgPool, muter: &User, target: &User) {
    let options = mute::MuteOptions {
        notifications: false,
        expires_in: None,
    };
    let stored = mute::mute(pool, muter, target, options)
        .await
        .expect("the fixture mute stores");
    assert!(
        !stored.hide_notifications,
        "and the option is stored as the caller set it"
    );
}

/// Writes `target` into `muter`'s mute list with `expires_at` in the past:
/// the lapsed row of a mute whose duration ran out, which the sweep has not
/// deleted yet and every read must still treat as absent.
async fn mute_lapsed(pool: &PgPool, muter: &User, target: &User) {
    let sql = "INSERT INTO mutes (id, muter_id, muted_id, expires_at)
                    VALUES ($1, $2, $3, now() - make_interval(secs => 60))";
    let written = sqlx::query(sql)
        .bind(new_id())
        .bind(muter.id)
        .bind(target.id)
        .execute(pool)
        .await
        .expect("the lapsed fixture row inserts");
    assert_eq!(written.rows_affected(), 1, "one row was written");
}

/// Lifts `muter`'s mute on `target`, reporting whether one was there.
async fn unmute_pair(pool: &PgPool, muter: &User, target: &User) -> bool {
    let removed = mute::unmute(pool, muter, target)
        .await
        .expect("the fixture unmute runs");
    removed.is_some()
}

/// Both mutes flags ride the same batch: the viewer's mute of a target
/// lights `muting`, the same mute with notifications hidden lights both, a
/// lapsed mute lights neither, and a target nobody muted keeps both
/// defaults — as does every account once its mute is lifted.
#[tokio::test]
async fn mutes_light_muting_and_muting_notifications() {
    let db = crate::test_db::test_db().await.expect("test database slot");
    let pool = db.pool().clone();
    let alice = local_user(&pool, "alice").await;
    let bob = local_user(&pool, "bob").await;
    let carol = local_user(&pool, "carol").await;
    let dave = local_user(&pool, "dave").await;
    mute_timeline_only(&pool, &alice, &bob).await;
    mute_pair(&pool, &alice, &carol).await;
    mute_lapsed(&pool, &alice, &dave).await;

    assert_eq!(
        mutes_of(&pool, &alice, &[&bob, &carol, &dave]).await,
        vec![(true, false), (true, true), (false, false)],
        "alice mutes bob's timeline only, mutes carol outright, and is
             past her lapsed mute of dave"
    );
    assert_eq!(
        mutes_of(&pool, &bob, &[&alice]).await,
        vec![(false, false)],
        "being muted does not mute anybody back"
    );

    let lifted = unmute_pair(&pool, &alice, &carol).await;
    assert!(lifted, "carol's mute was there to lift");
    let cleared = one_relationship(&pool, &alice, &carol).await;
    assert!(
        !cleared.muting.0 && !cleared.muting_notifications.0,
        "with nothing muted the pair falls back to the defaults"
    );
    assert_eq!(
        mutes_of(&pool, &alice, &[&bob, &carol, &dave]).await,
        vec![(true, false), (false, false), (false, false)],
        "and the rest of the batch is untouched by the lift"
    );
    assert!(
        !cleared.following.0 && cleared.showing_reblogs.0,
        "a mute says nothing about the follow flags"
    );
}
