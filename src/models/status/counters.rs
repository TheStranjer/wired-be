//! The denormalised counters: the author's post count and newest-post time
//! and the four per-status counters, stepped only in SQL and clamped at
//! zero, one literal statement per counter. [`insert`](super::insert) and
//! [`soft_delete`](super::soft_delete) move the linked counters with the
//! rows they write; [`bump_counter`] is left to the counters no status row
//! links to.

use sqlx::{PgConnection, PgPool};
use uuid::Uuid;

use crate::error::AppResult;

use super::{Counter, Status};

/// The author's denormalised post count and newest-post time, moved by one
/// status: `last_status_at` only ever moves forward, so importing an old
/// remote post leaves a newer one on the books.
const AUTHOR_COUNT_SQL: &str = "UPDATE users
      SET statuses_count = GREATEST(0, statuses_count + $2),
          last_status_at = GREATEST(last_status_at, $3),
          updated_at = now()
    WHERE id = $1";

/// The `UPDATE` behind [`bump_counter`] for [`Counter::Replies`]; one literal
/// per counter, so no statement is ever assembled from data.
const STEP_REPLIES_SQL: &str = "UPDATE statuses
      SET replies_count = GREATEST(0, replies_count + $2), updated_at = now()
    WHERE id = $1";

/// The same write for [`Counter::Reblogs`].
const STEP_REBLOGS_SQL: &str = "UPDATE statuses
      SET reblogs_count = GREATEST(0, reblogs_count + $2), updated_at = now()
    WHERE id = $1";

/// The same write for [`Counter::Favourites`].
const STEP_FAVOURITES_SQL: &str = "UPDATE statuses
      SET favourites_count = GREATEST(0, favourites_count + $2), updated_at = now()
    WHERE id = $1";

/// The same write for [`Counter::Quotes`].
const STEP_QUOTES_SQL: &str = "UPDATE statuses
      SET quotes_count = GREATEST(0, quotes_count + $2), updated_at = now()
    WHERE id = $1";

/// Adds `delta` to `counter` of status `id`, clamped at zero, and refreshes
/// its `updated_at`. An unknown id matches no row and changes nothing.
///
/// [`insert`](super::insert) and [`soft_delete`](super::soft_delete) keep the
/// linked counters — `replies_count`,
/// `reblogs_count` and `quotes_count` — in step with the rows they write;
/// this is how everything else that counts against a status moves its number
/// (favourites) without reading the row before writing it.
///
/// # Errors
///
/// Any database failure (connection, encoding) arrives as
/// [`AppError::Database`](crate::error::AppError).
pub async fn bump_counter(pool: &PgPool, id: Uuid, counter: Counter, delta: i32) -> AppResult<()> {
    let mut conn = pool.acquire().await?;
    step_counter(&mut conn, id, counter, delta).await
}

/// Moves the counters one fresh status feeds on `conn`: the author's
/// `statuses_count` and `last_status_at`, and whatever it counts on another
/// status.
pub(super) async fn count_new_post(conn: &mut PgConnection, status: &Status) -> AppResult<()> {
    step_author(conn, status, 1).await?;
    bump_linked(conn, status, 1).await
}

/// Steps the counters the deleted `status` had fed back down on `conn`: its
/// author's `statuses_count` and whatever it counted on another status.
pub(super) async fn uncount_deleted(conn: &mut PgConnection, status: &Status) -> AppResult<()> {
    step_author(conn, status, -1).await?;
    bump_linked(conn, status, -1).await
}

/// Adds `delta` to the post count of `status`'s author and offers its
/// creation time to `last_status_at`, which the SQL only lets move forward.
async fn step_author(conn: &mut PgConnection, status: &Status, delta: i32) -> AppResult<()> {
    sqlx::query(AUTHOR_COUNT_SQL)
        .bind(status.user_id)
        .bind(delta)
        .bind(status.inserted_at)
        .execute(&mut *conn)
        .await
        .map(drop)?;
    Ok(())
}

/// Steps the counter this status feeds on another row: `replies_count` of the
/// status it answers, `reblogs_count` of the one it reblogs and
/// `quotes_count` of the one it quotes. The three links are independent, so
/// a row that (say) answers one status and quotes another steps both, and
/// [`insert`](super::insert) and [`soft_delete`](super::soft_delete) ride this
/// on the row in hand, so no
/// counter is read before it is stepped.
async fn bump_linked(conn: &mut PgConnection, status: &Status, delta: i32) -> AppResult<()> {
    if let Some(parent_id) = status.in_reply_to_id {
        step_counter(conn, parent_id, Counter::Replies, delta).await?;
    }
    if let Some(original_id) = status.reblog_of_id {
        step_counter(conn, original_id, Counter::Reblogs, delta).await?;
    }
    if let Some(quoted_id) = status.quote_of_id {
        step_counter(conn, quoted_id, Counter::Quotes, delta).await?;
    }
    Ok(())
}

/// Adds `delta` to one counter of status `id`, in SQL, so the value the row
/// holds is what gets stepped and the result is never below zero.
pub(super) async fn step_counter(
    conn: &mut PgConnection,
    id: Uuid,
    counter: Counter,
    delta: i32,
) -> AppResult<()> {
    let sql = counter_sql(counter);
    sqlx::query(sql)
        .bind(id)
        .bind(delta)
        .execute(&mut *conn)
        .await
        .map(drop)?;
    Ok(())
}

/// The statement that steps one counter, picked by which [`Counter`] it is:
/// one literal per column instead of a string built from data, which is how
/// `sqlx` wants its SQL.
const fn counter_sql(counter: Counter) -> &'static str {
    match counter {
        Counter::Replies => STEP_REPLIES_SQL,
        Counter::Reblogs => STEP_REBLOGS_SQL,
        Counter::Favourites => STEP_FAVOURITES_SQL,
        Counter::Quotes => STEP_QUOTES_SQL,
    }
}

#[cfg(test)]
mod tests {
    use chrono::{SubsecRound as _, Utc};

    use super::*;
    use crate::models::status::{NewStatus, insert, soft_delete, test_support::*};
    use crate::util::id::new_id;

    #[tokio::test]
    async fn a_reply_inherits_the_conversation_and_counts_on_its_parent() {
        let db = crate::test_db::test_db().await.expect("test database slot");
        let pool = db.pool().clone();
        let alice = user(&pool, "alice").await;
        let bob = user(&pool, "bob").await;
        let root = post(&pool, &alice, "root").await;

        let answer = reply(&pool, &bob, &root, "an answer").await;

        assert_eq!(
            answer.conversation_id, root.conversation_id,
            "the reply joins the thread it answers"
        );
        assert_eq!(answer.in_reply_to_id, Some(root.id), "and names its parent");
        assert_eq!(
            counter(&pool, root.id, Counter::Replies).await,
            1,
            "the parent counts one answer"
        );
        assert_eq!(
            fresh_user(&pool, bob.id).await.statuses_count,
            1,
            "the reply counts for its own author too"
        );

        let deeper = reply(&pool, &alice, &answer, "deeper").await;
        assert_eq!(
            deeper.conversation_id, root.id,
            "a reply to a reply still belongs to the root's conversation"
        );
        assert_eq!(
            counter(&pool, answer.id, Counter::Replies).await,
            1,
            "each row counts its own answers"
        );
        assert_eq!(
            counter(&pool, root.id, Counter::Replies).await,
            1,
            "and not its grandparent's"
        );
    }

    /// A mirrored post cannot name a quote that is not stored here, so this
    /// covers `quote_of_id` against a real row.
    #[tokio::test]
    async fn a_quote_names_its_original_and_counts_on_it() {
        let db = crate::test_db::test_db().await.expect("test database slot");
        let pool = db.pool().clone();
        let alice = user(&pool, "alice").await;
        let bob = user(&pool, "bob").await;
        let original = post(&pool, &alice, "quote me").await;

        let quote = quote(&pool, &bob, original.id, "I second that").await;

        assert_eq!(quote.quote_of_id, Some(original.id), "the link is kept");
        assert_eq!(
            counter(&pool, original.id, Counter::Quotes).await,
            1,
            "insert counts the quote on the original, like replies and reblogs"
        );
    }

    /// A step down with no step up before it — the drift the clamp exists
    /// for — stops at zero instead of going under.
    #[tokio::test]
    async fn quote_steps_never_take_the_counter_below_zero() {
        let db = crate::test_db::test_db().await.expect("test database slot");
        let pool = db.pool().clone();
        let alice = user(&pool, "alice").await;
        let bob = user(&pool, "bob").await;
        let original = post(&pool, &alice, "quote me").await;
        // A quoting row written beside [`insert`]: the counter never saw the
        // +1 that the delete now steps back.
        let quote_id = new_id();
        let written = sqlx::query(
            "INSERT INTO statuses (id, uri, user_id, conversation_id, local, visibility, quote_of_id)
             VALUES ($1, $2, $3, $1, true, 'public', $4)",
        )
        .bind(quote_id)
        .bind(format!("{BASE}/objects/{quote_id}"))
        .bind(bob.id)
        .bind(original.id)
        .execute(&pool)
        .await
        .unwrap();
        assert_eq!(
            written.rows_affected(),
            1,
            "the uncounted quote row was written"
        );

        assert!(
            soft_delete(&pool, quote_id).await.unwrap().is_some(),
            "the uncounted quoter was live"
        );
        assert_eq!(
            counter(&pool, original.id, Counter::Quotes).await,
            0,
            "the step lands on zero, not below it"
        );
    }

    /// Several quotes and several tombstones: the counter is the live
    /// quoters, never the rows that ever quoted.
    #[tokio::test]
    async fn quotes_and_tombstones_add_up() {
        let db = crate::test_db::test_db().await.expect("test database slot");
        let pool = db.pool().clone();
        let alice = user(&pool, "alice").await;
        let original = post(&pool, &alice, "quote me a lot").await;
        let mut quotes = Vec::new();
        for n in 0..3 {
            let author = user(&pool, &format!("quoter{n}")).await;
            quotes.push(quote(&pool, &author, original.id, &format!("quote {n}")).await);
        }
        assert_eq!(
            counter(&pool, original.id, Counter::Quotes).await,
            3,
            "three live quotes count three"
        );

        assert!(
            soft_delete(&pool, quotes[0].id).await.unwrap().is_some(),
            "the first quote was live"
        );
        assert!(
            soft_delete(&pool, quotes[2].id).await.unwrap().is_some(),
            "the last quote was live"
        );
        assert_eq!(
            counter(&pool, original.id, Counter::Quotes).await,
            1,
            "two tombstones leave one"
        );

        let late = user(&pool, "again").await;
        let again = quote(&pool, &late, original.id, "a late quote").await;
        assert_eq!(
            counter(&pool, original.id, Counter::Quotes).await,
            2,
            "and a fresh quote steps back up"
        );

        for survivor in [&quotes[1], &again] {
            assert!(
                soft_delete(&pool, survivor.id).await.unwrap().is_some(),
                "the remaining quotes were live"
            );
        }
        assert_eq!(
            counter(&pool, original.id, Counter::Quotes).await,
            0,
            "tombstone them all and the original counts none"
        );
    }

    #[tokio::test]
    async fn importing_an_old_post_never_moves_last_status_back() {
        let db = crate::test_db::test_db().await.expect("test database slot");
        let pool = db.pool().clone();
        let mallory = remote_user(&pool, "mallory").await;
        let now = Utc::now().trunc_subsecs(6);
        let old = insert(
            &pool,
            BASE,
            &NewStatus {
                inserted_at: Some(now - chrono::Duration::days(10)),
                ..mirrored(&mallory, "https://remote.example/posts/old")
            },
        )
        .await
        .unwrap();
        assert_eq!(
            fresh_user(&pool, mallory.id).await.last_status_at,
            Some(old.inserted_at),
            "the first post names itself"
        );

        let newer = insert(
            &pool,
            BASE,
            &NewStatus {
                inserted_at: Some(now - chrono::Duration::days(1)),
                ..mirrored(&mallory, "https://remote.example/posts/newer")
            },
        )
        .await
        .unwrap();
        assert_eq!(
            fresh_user(&pool, mallory.id).await.last_status_at,
            Some(newer.inserted_at),
            "a newer post moves it forward"
        );

        let oldest = insert(
            &pool,
            BASE,
            &NewStatus {
                inserted_at: Some(now - chrono::Duration::days(20)),
                ..mirrored(&mallory, "https://remote.example/posts/oldest")
            },
        )
        .await
        .unwrap();
        assert_eq!(
            oldest.inserted_at,
            now - chrono::Duration::days(20),
            "the oldest import keeps the time it arrived with"
        );
        assert_eq!(
            fresh_user(&pool, mallory.id).await.last_status_at,
            Some(newer.inserted_at),
            "and an older import leaves it where it was"
        );
        assert_eq!(
            fresh_user(&pool, mallory.id).await.statuses_count,
            3,
            "while the count keeps all three posts"
        );
    }

    #[tokio::test]
    async fn counters_never_go_below_zero() {
        let db = crate::test_db::test_db().await.expect("test database slot");
        let pool = db.pool().clone();
        let alice = user(&pool, "alice").await;
        let status = post(&pool, &alice, "count me").await;

        bump_counter(&pool, status.id, Counter::Favourites, 3)
            .await
            .expect("a favourite is countable");
        assert_eq!(
            counter(&pool, status.id, Counter::Favourites).await,
            3,
            "three favourites"
        );
        bump_counter(&pool, status.id, Counter::Favourites, -5)
            .await
            .expect("undoing more than there is stays legal");
        assert_eq!(
            counter(&pool, status.id, Counter::Favourites).await,
            0,
            "the counter stops at zero instead of going under"
        );

        bump_counter(&pool, status.id, Counter::Quotes, -1)
            .await
            .expect("a negative step on a fresh counter is ignored");
        assert_eq!(
            counter(&pool, status.id, Counter::Quotes).await,
            0,
            "quotes stay at zero"
        );
        bump_counter(&pool, status.id, Counter::Replies, 2)
            .await
            .expect("replies move through the same door");
        assert_eq!(
            counter(&pool, status.id, Counter::Replies).await,
            2,
            "and do"
        );

        bump_counter(&pool, new_id(), Counter::Favourites, 1)
            .await
            .expect("an unknown id bumps nothing");
        assert!(
            stored(&pool, status.id).await.updated_at >= status.updated_at,
            "the bumped row refreshed its timestamp"
        );
    }
}
