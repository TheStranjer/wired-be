//! The context a local status is stamped with on [`insert`] (WBE-886), and
//! the backfill of `20261127083250_stamp_local_status_contexts.sql` that
//! stamps the local statuses written before.

use sqlx::PgPool;
use uuid::Uuid;

use super::*;
use crate::models::status::{create_reblog, test_support::*};

/// The migration's own SQL: a data move only.
const MIGRATION_SQL: &str =
    include_str!("../../../../migrations/20261127083250_stamp_local_status_contexts.sql");

/// The context Pleroma names for a reply in a Misskey thread: the root id.
const PLEROMA_CONTEXT: &str = "https://misskey.test/notes/root1";

/// A context Mastodon minted for its thread.
const MASTODON_CONTEXT: &str = "https://mastodon.test/contexts/117-118";

/// The minted context of conversation `conversation`.
fn minted(conversation: Uuid) -> String {
    format!("{BASE}/contexts/{conversation}")
}

/// A mirrored status `uri` by `author`, answering `parent` when given and
/// naming `context`.
async fn mirror(
    pool: &PgPool,
    author: &User,
    uri: &str,
    parent: Option<&Status>,
    context: Option<&str>,
) -> Status {
    insert(
        pool,
        BASE,
        &NewStatus {
            in_reply_to_id: parent.map(|row| row.id),
            ap_context: context.map(str::to_owned),
            ..mirrored(author, uri)
        },
    )
    .await
    .expect("the mirrored fixture inserts")
}

/// The `ap_context` stored on status `id` now.
async fn context_of(pool: &PgPool, id: Uuid) -> Option<String> {
    sqlx::query_scalar::<_, Option<String>>("SELECT ap_context FROM statuses WHERE id = $1")
        .bind(id)
        .fetch_one(pool)
        .await
        .expect("the context reads")
}

#[tokio::test]
async fn a_local_root_is_stamped_with_its_minted_context() {
    let db = crate::test_db::test_db().await.expect("test database slot");
    let pool = db.pool().clone();
    let alice = user(&pool, "alice").await;
    let root = post(&pool, &alice, "hello").await;
    let answer = reply(&pool, &alice, &root, "again").await;

    assert_eq!(root.ap_context, Some(minted(root.id)), "the root's own");
    assert_eq!(
        answer.ap_context,
        Some(minted(root.id)),
        "a reply shares the thread's minted context"
    );
}

#[tokio::test]
async fn a_local_reply_in_a_remote_thread_with_a_context_is_stamped_with_it() {
    let db = crate::test_db::test_db().await.expect("test database slot");
    let pool = db.pool().clone();
    let alice = user(&pool, "alice").await;
    let bob = remote_user(&pool, "bob").await;
    let root = mirror(
        &pool,
        &bob,
        "https://mastodon.test/users/bob/statuses/1",
        None,
        Some(MASTODON_CONTEXT),
    )
    .await;
    let answer = reply(&pool, &alice, &root, "hi bob").await;

    assert_eq!(
        answer.ap_context.as_deref(),
        Some(MASTODON_CONTEXT),
        "the remote thread's context"
    );
    assert_eq!(
        root.ap_context.as_deref(),
        Some(MASTODON_CONTEXT),
        "a mirrored row keeps the context it arrived with"
    );
}

#[tokio::test]
async fn a_minted_stamp_survives_a_later_remote_context() {
    let db = crate::test_db::test_db().await.expect("test database slot");
    let pool = db.pool().clone();
    let alice = user(&pool, "alice").await;
    let carol = remote_user(&pool, "carol").await;
    let root = mirror(
        &pool,
        &carol,
        "https://misskey.test/notes/root1",
        None,
        None,
    )
    .await;
    let first = reply(&pool, &alice, &root, "first").await;
    let theirs = mirror(
        &pool,
        &carol,
        "https://pleroma.test/objects/p1",
        Some(&first),
        Some(PLEROMA_CONTEXT),
    )
    .await;
    let later = reply(&pool, &alice, &theirs, "later").await;

    assert_eq!(root.ap_context, None, "Misskey sent no context");
    assert_eq!(
        context_of(&pool, first.id).await,
        Some(minted(root.id)),
        "the published reply keeps its minted context"
    );
    assert_eq!(
        later.ap_context,
        Some(minted(root.id)),
        "and the thread keeps it once minted here"
    );
}

#[tokio::test]
async fn remote_statuses_and_reblogs_are_not_stamped() {
    let db = crate::test_db::test_db().await.expect("test database slot");
    let pool = db.pool().clone();
    let alice = user(&pool, "alice").await;
    let carol = remote_user(&pool, "carol").await;
    let theirs = mirror(&pool, &carol, "https://misskey.test/notes/n2", None, None).await;
    let (boost, created) = create_reblog(&pool, BASE, &alice, &theirs, Visibility::Public)
        .await
        .expect("the reblog writes");

    assert!(created, "a fresh reblog");
    assert_eq!(theirs.ap_context, None, "a remote row stores what it sent");
    assert_eq!(boost.ap_context, None, "a reblog row renders no note");
}

/// Clears the stamp of every local status, as rows written before the
/// stamp stand.
async fn unstamp(pool: &PgPool) {
    let cleared = sqlx::query("UPDATE statuses SET ap_context = NULL WHERE local")
        .execute(pool)
        .await
        .expect("the stamps clear")
        .rows_affected();
    assert!(cleared > 0, "the fixture has local statuses");
}

/// Replays the migration.
async fn migrate(pool: &PgPool) {
    let _migrated = sqlx::raw_sql(MIGRATION_SQL)
        .execute(pool)
        .await
        .expect("the backfill runs");
}

#[tokio::test]
async fn the_backfill_stamps_what_each_local_status_was_published_with() {
    let db = crate::test_db::test_db().await.expect("test database slot");
    let pool = db.pool().clone();
    let alice = user(&pool, "alice").await;
    let carol = remote_user(&pool, "carol").await;
    let own = post(&pool, &alice, "a thread of mine").await;
    let misskey = mirror(
        &pool,
        &carol,
        "https://misskey.test/notes/root1",
        None,
        None,
    )
    .await;
    let early = reply(&pool, &alice, &misskey, "before any context").await;
    let pleroma = mirror(
        &pool,
        &carol,
        "https://pleroma.test/objects/p1",
        Some(&early),
        Some(PLEROMA_CONTEXT),
    )
    .await;
    let after = reply(&pool, &alice, &pleroma, "after the context").await;
    let mastodon = mirror(
        &pool,
        &carol,
        "https://mastodon.test/users/carol/statuses/9",
        None,
        Some(MASTODON_CONTEXT),
    )
    .await;
    let inside = reply(&pool, &alice, &mastodon, "in a contexted thread").await;
    let (boost, _created) = create_reblog(&pool, BASE, &alice, &misskey, Visibility::Public)
        .await
        .expect("the reblog writes");
    unstamp(&pool).await;

    migrate(&pool).await;

    assert_eq!(
        context_of(&pool, own.id).await,
        Some(minted(own.id)),
        "a local root: minted"
    );
    assert_eq!(
        context_of(&pool, early.id).await,
        Some(minted(misskey.id)),
        "the remote context arrived after it: minted"
    );
    assert_eq!(
        context_of(&pool, after.id).await.as_deref(),
        Some(PLEROMA_CONTEXT),
        "the remote context was stored before it: that one"
    );
    assert_eq!(
        context_of(&pool, inside.id).await.as_deref(),
        Some(MASTODON_CONTEXT),
        "the remote root's own context"
    );
    assert_eq!(context_of(&pool, boost.id).await, None, "a reblog stays");
    assert_eq!(
        context_of(&pool, pleroma.id).await.as_deref(),
        Some(PLEROMA_CONTEXT),
        "a remote row is untouched"
    );
    assert_eq!(
        context_of(&pool, misskey.id).await,
        None,
        "and so is one that sent none"
    );
}

#[tokio::test]
async fn the_backfill_leaves_stamped_rows_alone_and_runs_twice() {
    let db = crate::test_db::test_db().await.expect("test database slot");
    let pool = db.pool().clone();
    let alice = user(&pool, "alice").await;
    let root = post(&pool, &alice, "stamped already").await;
    let foreign = "https://elsewhere.test/contexts/kept";
    let moved = sqlx::query("UPDATE statuses SET ap_context = $2 WHERE id = $1")
        .bind(root.id)
        .bind(foreign)
        .execute(&pool)
        .await
        .expect("the stamp rewrites")
        .rows_affected();
    assert_eq!(moved, 1, "the fixture row exists");

    migrate(&pool).await;
    migrate(&pool).await;

    assert_eq!(
        context_of(&pool, root.id).await.as_deref(),
        Some(foreign),
        "a stamped row keeps its stamp"
    );
}
