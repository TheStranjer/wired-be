//! The profile directory page: which accounts may be listed at all, the
//! two orders [`DirectoryOrder`] walks them in, and [`directory_page`]
//! paging one [`PageParams`] window at a time.

use sqlx::{PgPool, Postgres, QueryBuilder};

use super::User;
use crate::{error::AppResult, pagination::PageParams};

/// The order the profile directory walks its accounts in
/// (`GET /api/v1/directory`'s `order` parameter).
///
/// Which column the listing sorts on is the model's business; reading the
/// client's `active`/`new` text and defaulting an unrecognised value is the
/// endpoint's (Pleroma's `order_by_creation_date/2` matches `"new"` exactly
/// and falls back to the active order for anything else).
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum DirectoryOrder {
    /// Most recently active first: `last_status_at` descending, the accounts
    /// that never posted last (`DIRECTORY_ACTIVE_SQL`).
    #[default]
    Active,
    /// Newest account first: `inserted_at` descending
    /// (`DIRECTORY_NEW_SQL`).
    New,
}

impl DirectoryOrder {
    /// The `ORDER BY` clause of this order, with `id DESC` as tiebreak: two
    /// accounts sharing a timestamp must not land on both sides of an
    /// `OFFSET` boundary, which is how the directory pages.
    const fn order_by(self) -> &'static str {
        match self {
            Self::Active => DIRECTORY_ACTIVE_SQL,
            Self::New => DIRECTORY_NEW_SQL,
        }
    }
}

/// The shared head of [`directory_page`]: the accounts the directory may
/// answer with at all — active, and opted into discovery. The text stops
/// where the optional `local` filter belongs, because that is what
/// [`directory_page`] appends next; [`DIRECTORY_ACTIVE_SQL`] and
/// [`DIRECTORY_NEW_SQL`] carry the order, and the page's `LIMIT` and
/// `OFFSET` are binds the function pushes after them.
const DIRECTORY_SQL: &str = "SELECT * FROM users WHERE is_active AND discoverable";

/// The `active` order: newest `last_status_at` first, `NULL` (an account
/// that never posted) last, exactly Pleroma's `desc_nulls_last`.
const DIRECTORY_ACTIVE_SQL: &str = " ORDER BY last_status_at DESC NULLS LAST, id DESC";

/// The `new` order: newest account first.
const DIRECTORY_NEW_SQL: &str = " ORDER BY inserted_at DESC, id DESC";

/// One [`directory_page`] offset: the row to start at, `0` for the first
/// page — a directory has no pages before the first row.
const NO_OFFSET: i64 = 0;

/// The directory's discoverable, active accounts, `page`'s `limit` at a
/// time starting at its `offset`, in `order`'s order.
///
/// `local_only` is the caller's `local` flag: `true` restricts the listing
/// to accounts this instance owns (Pleroma's `exclude_remote`), `false`
/// leaves the remote mirrors in. Of [`PageParams`], only
/// [`limit`](PageParams::limit) and [`offset`](PageParams::offset) mean
/// anything here: a directory has no id cursor to walk, so
/// [`max_id`](PageParams::max_id) and its siblings are ignored, and
/// `offset` absent is the first page.
///
/// # Errors
///
/// Any database failure (connection, encoding) arrives as
/// [`AppError::Database`](crate::error::AppError).
pub async fn directory_page(
    pool: &PgPool,
    order: DirectoryOrder,
    local_only: bool,
    page: PageParams,
) -> AppResult<Vec<User>> {
    let mut builder = QueryBuilder::<Postgres>::new(DIRECTORY_SQL);
    if local_only {
        let _local: &mut _ = builder.push(" AND local");
    }
    let _ordered: &mut _ = builder.push(order.order_by());
    let _limited: &mut _ = builder
        .push(" LIMIT ")
        .push_bind(page.limit)
        .push(" OFFSET ")
        .push_bind(page.offset.unwrap_or(NO_OFFSET));
    let users = builder.build_query_as::<User>().fetch_all(pool).await?;
    Ok(users)
}

#[cfg(test)]
mod tests {
    use chrono::{DateTime, Utc};

    use super::*;
    use crate::models::user::{Flag, NewUser, insert, test_support::*};

    /// A discoverable local user named `nickname` whose newest status was
    /// posted at `active` (`None`: never).
    async fn discoverable(pool: &PgPool, nickname: &str, active: Option<DateTime<Utc>>) -> User {
        insert(
            pool,
            &NewUser {
                discoverable: Flag::new(true),
                last_status_at: active,
                ..local_new(nickname)
            },
        )
        .await
        .expect("the discoverable fixture inserts")
    }

    /// Moves `target`'s `inserted_at` back to `days` ago, so an
    /// `order = new` expectation does not depend on how fast the fixture
    /// loop ran.
    async fn insert_days_ago(pool: &PgPool, target: &User, days: i64) {
        let moved = sqlx::query(
            "UPDATE users SET inserted_at = now() - $1::bigint * interval '1 day' WHERE id = $2",
        )
        .bind(days)
        .bind(target.id)
        .execute(pool)
        .await
        .expect("the fixture backdates the account");
        assert_eq!(moved.rows_affected(), 1, "the row was there to move");
    }

    /// The directory page `query` asks for, with this endpoint's page sizes.
    fn directory_page_params(query: &str) -> PageParams {
        PageParams::from_query_with_default(Some(query), 40, 80).expect("the fixture page parses")
    }

    #[tokio::test]
    async fn directory_page_needs_discoverable_and_active() {
        let db = crate::test_db::test_db().await.expect("test database slot");
        let pool = db.pool().clone();
        let listed = discoverable(&pool, "listed", None).await;
        let hidden = insert(&pool, &local_new("hidden")).await.unwrap();
        let gone = discoverable(&pool, "gone", None).await;
        let _deactivated = sqlx::query("UPDATE users SET is_active = false WHERE id = $1")
            .bind(gone.id)
            .execute(&pool)
            .await
            .expect("the fixture deactivates");

        let seen = directory_page(
            &pool,
            DirectoryOrder::Active,
            false,
            directory_page_params(""),
        )
        .await
        .unwrap();
        assert_eq!(ids_of(&seen), vec![listed.id], "opted in and active");
        assert!(
            !ids_of(&seen).contains(&hidden.id),
            "an account that never opted into discovery is not listed"
        );
        assert!(
            !ids_of(&seen).contains(&gone.id),
            "and neither is a deactivated one"
        );
    }

    #[tokio::test]
    async fn directory_page_lists_remote_accounts_unless_asked_locally() {
        let db = crate::test_db::test_db().await.expect("test database slot");
        let pool = db.pool().clone();
        let local = discoverable(&pool, "local-one", None).await;
        let ap_id = "https://remote.example/users/mirror";
        let mirror = insert(
            &pool,
            &NewUser {
                nickname: "mirror@remote.example".to_owned(),
                ap_id: ap_id.to_owned(),
                domain: Some("remote.example".to_owned()),
                discoverable: Flag::new(true),
                ..NewUser::default()
            },
        )
        .await
        .expect("the remote fixture inserts");

        let everyone = directory_page(
            &pool,
            DirectoryOrder::Active,
            false,
            directory_page_params(""),
        )
        .await
        .unwrap();
        assert_eq!(
            ids_of(&everyone).len(),
            2,
            "with no `local` flag the mirrors stay in, as Pleroma's \
             `exclude_remote` only fires on true"
        );
        let local_only = directory_page(
            &pool,
            DirectoryOrder::Active,
            true,
            directory_page_params(""),
        )
        .await
        .unwrap();
        assert_eq!(
            ids_of(&local_only),
            vec![local.id],
            "`local` true answers this instance's own accounts"
        );
        assert!(!ids_of(&local_only).contains(&mirror.id), "and no mirror");
    }

    #[tokio::test]
    async fn directory_active_order_puts_never_posted_accounts_last() {
        let db = crate::test_db::test_db().await.expect("test database slot");
        let pool = db.pool().clone();
        let quiet = discoverable(&pool, "quiet", None).await;
        let older =
            discoverable(&pool, "older", Some(Utc::now() - chrono::Duration::days(1))).await;
        let newest = discoverable(&pool, "newest", Some(Utc::now())).await;

        let seen = directory_page(
            &pool,
            DirectoryOrder::Active,
            false,
            directory_page_params(""),
        )
        .await
        .unwrap();
        assert_eq!(
            ids_of(&seen),
            vec![newest.id, older.id, quiet.id],
            "newest status first, the accounts that never posted last"
        );
    }

    #[tokio::test]
    async fn directory_new_order_sorts_on_the_account_itself() {
        let db = crate::test_db::test_db().await.expect("test database slot");
        let pool = db.pool().clone();
        // The newest account posted long ago: the two orders must disagree.
        let fresh =
            discoverable(&pool, "fresh", Some(Utc::now() - chrono::Duration::days(9))).await;
        let veteran = discoverable(
            &pool,
            "veteran",
            Some(Utc::now() - chrono::Duration::days(1)),
        )
        .await;
        insert_days_ago(&pool, &veteran, 5).await;
        insert_days_ago(&pool, &fresh, 1).await;

        let seen = directory_page(&pool, DirectoryOrder::New, false, directory_page_params(""))
            .await
            .unwrap();
        assert_eq!(
            ids_of(&seen),
            vec![fresh.id, veteran.id],
            "`new` sorts on `inserted_at`, ignoring last activity"
        );
        let by_activity = directory_page(
            &pool,
            DirectoryOrder::Active,
            false,
            directory_page_params(""),
        )
        .await
        .unwrap();
        assert_eq!(
            ids_of(&by_activity),
            vec![veteran.id, fresh.id],
            "and the default order is the other way round for the same rows"
        );
    }

    #[tokio::test]
    async fn directory_pages_by_limit_and_offset() {
        let db = crate::test_db::test_db().await.expect("test database slot");
        let pool = db.pool().clone();
        for nickname in ["one", "two", "three", "four", "five"] {
            let posted = discoverable(&pool, nickname, Some(Utc::now())).await;
            assert!(posted.discoverable.get(), "{nickname} is listed");
        }
        let all = directory_page(&pool, DirectoryOrder::New, false, directory_page_params(""))
            .await
            .unwrap();
        assert_eq!(ids_of(&all).len(), 5, "the whole listing");

        let second = directory_page(
            &pool,
            DirectoryOrder::New,
            false,
            directory_page_params("limit=2&offset=2"),
        )
        .await
        .unwrap();
        assert_eq!(
            ids_of(&second),
            ids_of(&all)[2..4],
            "offset 2 skips the first two of the same order"
        );
        let tail = directory_page(
            &pool,
            DirectoryOrder::New,
            false,
            directory_page_params("limit=2&offset=4"),
        )
        .await
        .unwrap();
        assert_eq!(tail.len(), 1, "and the last page is what is left");
        assert_eq!(ids_of(&tail), ids_of(&all)[4..], "the fifth account");
        let past = directory_page(
            &pool,
            DirectoryOrder::New,
            false,
            directory_page_params("offset=99"),
        )
        .await
        .unwrap();
        assert!(past.is_empty(), "an offset past the end answers nothing");
    }
}
