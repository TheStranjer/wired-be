//! The read side of mutes: the existence check, the id sets and the page.

use sqlx::{PgPool, Postgres, QueryBuilder};
use uuid::Uuid;

use super::{Mute, MuteUserRow, into_entry};
use crate::{error::AppResult, models::user::User, pagination::PageParams};

/// [`is_muting`]: the one directional fact, asked as an existence check so a
/// muter never costs more than a `true`. The expiry clause is what makes a
/// lapsed mute answer `false` before the sweep has deleted its row.
const IS_MUTING_SQL: &str = "SELECT EXISTS (
    SELECT 1 FROM mutes
     WHERE muter_id = $1 AND muted_id = $2
       AND (expires_at IS NULL OR expires_at > now())
)";

/// [`muted_ids`]: every account one muter still mutes, oldest mute first (the
/// `UUIDv7` text sorts in creation order, decision 4). The order is a
/// stability promise, not semantics: callers filter by membership and page
/// through [`muting_page`].
const MUTED_IDS_SQL: &str = "SELECT muted_id FROM mutes
      WHERE muter_id = $1 AND (expires_at IS NULL OR expires_at > now())
   ORDER BY id";

/// [`notification_muted_ids`]: the still-muted accounts whose notifications
/// the muter also hides — [`MUTED_IDS_SQL`]'s set narrowed to the rows whose
/// `hide_notifications` column is true. An indefinite mute counts; an expired
/// one does not.
const NOTIFICATION_MUTED_IDS_SQL: &str = "SELECT muted_id FROM mutes
      WHERE muter_id = $1 AND hide_notifications
        AND (expires_at IS NULL OR expires_at > now())
   ORDER BY id";

/// The leading text of [`muting_page`]: the still-active muted accounts of one
/// muter, each entry led by the columns of the `mutes` row that put them on
/// the page (aliased where [`User`] has a column of the same name: `id` and
/// `inserted_at`), joined to the account on the other end. The expiry filter
/// comes before the muter's bind because [`PageParams::apply_keyset`] appends
/// its own conditions after it, and the statement stops where that placeholder
/// belongs: the builder is what numbers it `$1`, and the keyset runs on the
/// mute row's own `id`.
const PAGE_SQL: &str = "SELECT m.id AS mute_id, m.muter_id, m.muted_id,
                               m.hide_notifications, m.expires_at,
                               m.inserted_at AS mute_inserted_at, u.*
                          FROM mutes m
                          JOIN users u ON u.id = m.muted_id
                         WHERE (m.expires_at IS NULL OR m.expires_at > now())
                           AND m.muter_id = ";

/// Whether `muter` currently mutes `target`.
///
/// The one directional fact the timeline and notification filters ask for. A
/// mute at or past its [`Mute::expires_at`] is not there to find, and neither
/// is a pair that was never muted — including one account with itself, which
/// has no row to find because [`mute`](super::mute) refuses to write it.
///
/// # Errors
///
/// Any database failure (connection, encoding) arrives as
/// [`AppError::Database`](crate::error::AppError).
pub async fn is_muting(pool: &PgPool, muter: Uuid, target: Uuid) -> AppResult<bool> {
    let row = sqlx::query_scalar::<_, bool>(IS_MUTING_SQL)
        .bind(muter)
        .bind(target)
        .fetch_one(pool)
        .await?;
    Ok(row)
}

/// The ids of the accounts `muter` still mutes, oldest mute first.
///
/// The whole set in one statement: [`crate::models::relationship`] answers
/// `muting` from the same table, and the timeline filters that hide a muted
/// account's posts take this flat form. Expired mutes are absent from it, and a
/// muter with nothing muted answers the empty `Vec`.
///
/// # Errors
///
/// Any database failure (connection, encoding) arrives as
/// [`AppError::Database`](crate::error::AppError).
pub async fn muted_ids(pool: &PgPool, muter: Uuid) -> AppResult<Vec<Uuid>> {
    let rows = sqlx::query_as::<_, (Uuid,)>(MUTED_IDS_SQL)
        .bind(muter)
        .fetch_all(pool)
        .await?;
    Ok(rows.into_iter().map(|(muted_id,)| muted_id).collect())
}

/// The ids of the accounts `muter` still mutes *and* hides the notifications
/// of, oldest mute first.
///
/// [`muted_ids`]' set narrowed to the rows written with Mastodon's
/// `notifications` parameter left `true`: what the notification filters drop
/// and the [`Relationship`] entity's `muting_notifications` flag reads. An
/// account muted for its timeline only is in [`muted_ids`] and not here, an
/// expired mute is in neither, and a muter hiding nothing answers the empty
/// `Vec`.
///
/// [`Relationship`]: crate::entities::relationship::Relationship
///
/// # Errors
///
/// Any database failure (connection, encoding) arrives as
/// [`AppError::Database`](crate::error::AppError).
pub async fn notification_muted_ids(pool: &PgPool, muter: Uuid) -> AppResult<Vec<Uuid>> {
    let rows = sqlx::query_as::<_, (Uuid,)>(NOTIFICATION_MUTED_IDS_SQL)
        .bind(muter)
        .fetch_all(pool)
        .await?;
    Ok(rows.into_iter().map(|(muted_id,)| muted_id).collect())
}

/// The accounts `muter` still mutes, newest mute first and `page`'s `limit` at
/// a time, each entry the [`Mute`] row next to the [`User`] it names.
///
/// Like the relationship pages, the cursor and the `Link` header key on the
/// *mute row* id and not on the accounts (decision 4), which is why the row
/// comes back beside the user — and why the listing can render the row's
/// expiry as `mute_expires_at`. An expired mute is on no page.
/// [`PageParams::finish`] flips a `min_id` fetch back to newest-first, so every
/// page is answered newest mute first.
///
/// # Errors
///
/// Any database failure (connection, encoding) arrives as
/// [`AppError::Database`](crate::error::AppError).
pub async fn muting_page(
    pool: &PgPool,
    muter: Uuid,
    page: PageParams,
) -> AppResult<Vec<(Mute, User)>> {
    let mut builder = QueryBuilder::<Postgres>::new(PAGE_SQL);
    // The builder's push methods hand back `&mut Self` for chaining; binding
    // it keeps the statement from having an unused result (decision 14).
    let _bound: &mut _ = builder.push_bind(muter);
    page.apply_keyset(&mut builder, "m.id");
    let rows = builder
        .build_query_as::<MuteUserRow>()
        .fetch_all(pool)
        .await?;
    Ok(page.finish(rows).into_iter().map(into_entry).collect())
}

#[cfg(test)]
mod tests {
    use super::super::{
        MuteOptions,
        test_support::{
            TIMELINE_ONLY, mute_count, mute_expiring_in, muted_ids_of, page, row_ids_of, seed_mute,
            stored_rows, user,
        },
        unmute,
    };
    use super::*;

    /// A mute whose `expires_at` lies in the past — what the row of a mute
    /// whose duration ran out looks like until the sweep deletes it — is absent
    /// from every read.
    #[tokio::test]
    async fn an_expired_mute_is_absent_everywhere() {
        let db = crate::test_db::test_db().await.expect("test database slot");
        let pool = db.pool().clone();
        let alice = user(&pool, "alice").await;
        let bob = user(&pool, "bob").await;
        let carol = user(&pool, "carol").await;
        mute_expiring_in(&pool, &alice, &bob, -60).await;
        mute_expiring_in(&pool, &alice, &carol, 60).await;

        assert!(
            !is_muting(&pool, alice.id, bob.id).await.unwrap(),
            "the lapsed mute is not there to find"
        );
        assert_eq!(
            muted_ids(&pool, alice.id).await.unwrap(),
            vec![carol.id],
            "and its account is not on the muted list"
        );
        assert_eq!(
            notification_muted_ids(&pool, alice.id).await.unwrap(),
            vec![carol.id],
            "nor on the notification-muted one"
        );
        let entries = muting_page(&pool, alice.id, page("")).await.unwrap();
        assert_eq!(
            muted_ids_of(&entries),
            vec![carol.id],
            "and the page skips it"
        );
        assert_eq!(
            mute_count(&pool).await,
            2,
            "the row is still on the books until the sweep runs"
        );
    }

    /// Mutes are each account's own: another account's mute of the same target
    /// is a separate row, and lifting one leaves the other.
    #[tokio::test]
    async fn mutes_are_each_account_s_own() {
        let db = crate::test_db::test_db().await.expect("test database slot");
        let pool = db.pool().clone();
        let alice = user(&pool, "alice").await;
        let bob = user(&pool, "bob").await;
        let carol = user(&pool, "carol").await;
        seed_mute(&pool, &alice, &carol, MuteOptions::default()).await;
        seed_mute(&pool, &bob, &carol, TIMELINE_ONLY).await;

        assert_eq!(
            muted_ids(&pool, carol.id).await.unwrap(),
            Vec::<Uuid>::new(),
            "carol holds nothing of the mutes made against her"
        );

        let lifted = unmute(&pool, &alice, &carol)
            .await
            .expect("alice lifts her mute");
        assert!(lifted.is_some(), "and alice had a mute to lift");
        assert!(
            muted_ids(&pool, alice.id).await.unwrap().is_empty(),
            "alice's withdrawal left her own set empty"
        );
        assert_eq!(
            muted_ids(&pool, bob.id).await.unwrap(),
            vec![carol.id],
            "and bob's alone"
        );
    }

    #[tokio::test]
    async fn a_page_of_mutes_newest_first_walks_by_row_id() {
        let db = crate::test_db::test_db().await.expect("test database slot");
        let pool = db.pool().clone();
        let alice = user(&pool, "alice").await;
        let erin = user(&pool, "erin").await;
        for nickname in ["bob", "carol", "dave"] {
            let muted = user(&pool, nickname).await;
            seed_mute(&pool, &alice, &muted, MuteOptions::default()).await;
        }
        let stored = stored_rows(&pool).await;
        assert_eq!(stored.len(), 3, "three fixture mutes");

        let first = muting_page(&pool, alice.id, page("limit=2"))
            .await
            .expect("the first page reads");
        assert_eq!(
            row_ids_of(&first),
            vec![stored[0].0, stored[1].0],
            "the two newest rows"
        );
        assert_eq!(
            muted_ids_of(&first),
            vec![stored[0].2, stored[1].2],
            "and their muted accounts, newest mute first"
        );

        let older = muting_page(&pool, alice.id, page(&format!("max_id={}", stored[1].0)))
            .await
            .expect("the continuation reads");
        assert_eq!(
            row_ids_of(&older),
            vec![stored[2].0],
            "max_id continues where the page ended"
        );

        let newer = muting_page(&pool, alice.id, page(&format!("min_id={}", stored[2].0)))
            .await
            .expect("the forward page reads");
        assert_eq!(
            row_ids_of(&newer),
            vec![stored[0].0, stored[1].0],
            "a min_id page is answered newest-first too"
        );

        let tail = muting_page(&pool, alice.id, page(&format!("max_id={}", stored[2].0)))
            .await
            .expect("walking off the end is no error");
        assert!(tail.is_empty(), "and answers nothing");

        let other = muting_page(&pool, erin.id, page(""))
            .await
            .expect("a muter with nothing muted pages");
        assert!(other.is_empty(), "and holds nothing of alice's mutes");
    }
}
