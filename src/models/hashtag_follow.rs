//! The `hashtag_follows` table: which user follows which hashtag, the edge
//! behind `POST /api/v1/tags/{name}/follow`, `…/unfollow` and
//! `GET /api/v1/followed_tags`.
//!
//! Reference: Pleroma's `Pleroma.User.HashtagFollow`
//! (`lib/pleroma/user/hashtag_follow.ex`): one row per `(user, tag)` pair,
//! and the pair's uniqueness is the whole storage fact — `new` inserts
//! `on_conflict: :nothing` and `delete` is content with finding nothing.
//! [`follow`] and [`unfollow`] are those two writes, so both endpoints are
//! idempotent by construction: following twice leaves one row and reports
//! zero new ones, unfollowing twice removes one and reports the second
//! attempt removed nothing.
//!
//! The pair's insertion order is a fact no `UUIDv7` column can state (one
//! user's follow of an old tag is newer than their follow of a freshly
//! created one, and the tag id says nothing about it either), so
//! `hashtag_follows.position` — the `bigserial` ordinal its migration adds,
//! the same device `list_members.position` is for list members — is what
//! [`page`] keys its `max_id`/`min_id`/`since_id` cursors and the endpoint's
//! `Link` header on.

use sqlx::{PgPool, QueryBuilder, postgres::Postgres};
use uuid::Uuid;

use crate::{error::AppResult, models::hashtag::Hashtag, pagination::OrdinalPageParams};

/// [`follow`]: one pair, and the pair it already holds conflicts on the
/// primary key and is left exactly where it was — the insert that changes
/// nothing, which is what makes following twice one follow.
const FOLLOW_SQL: &str = "INSERT INTO hashtag_follows (user_id, hashtag_id)
     VALUES ($1, $2)
ON CONFLICT (user_id, hashtag_id) DO NOTHING";

/// [`unfollow`]: the pair's row; an unfollow of a tag the user does not
/// follow matches nothing and changes nothing.
const UNFOLLOW_SQL: &str = "DELETE FROM hashtag_follows
     WHERE user_id = $1 AND hashtag_id = $2";

/// [`is_following`]: does the pair's row exist? `EXISTS` answers the boolean
/// without fetching the row.
const IS_FOLLOWING_SQL: &str = "SELECT EXISTS (
     SELECT 1 FROM hashtag_follows WHERE user_id = $1 AND hashtag_id = $2)";

/// The head of [`page`]: one user's followed tags as their `hashtags` rows,
/// each entry led by the `position` of the follow row behind it — the
/// ordinal the page keys on. The text stops where its one placeholder
/// belongs, because that is where [`page`] pushes its bind and the builder
/// is what numbers it `$1`; the keyset it then appends runs on
/// `hf.position`.
const PAGE_SQL: &str = "SELECT hf.position, h.* FROM hashtag_follows hf
     JOIN hashtags h ON h.id = hf.hashtag_id
     WHERE hf.user_id = ";

/// One entry of a paged followed-tag list: the [`Hashtag`] row plus the
/// `position` of the `hashtag_follows` row behind it.
///
/// The follow ordinal — not the tag's id — is what [`page`] keys on (see the
/// [module docs](self)), so the endpoint's cursors and `Link` header carry
/// [`Entry::position`] values, as [`crate::models::list::MemberEntry`]
/// carries list-membership positions.
#[derive(Debug, sqlx::FromRow)]
pub struct Entry {
    /// `hashtag_follows.position`: the sequence ordinal the follow was
    /// inserted at, and so its place in the user's own follow order.
    pub position: i64,
    /// The followed tag's row.
    #[sqlx(flatten)]
    pub hashtag: Hashtag,
}

/// Records that `user_id` follows `hashtag_id`, returning how many rows were
/// new (`0` or `1`).
///
/// A pair that already follows is left exactly where it was — its `position`
/// included — which is why re-following is idempotent and does not move the
/// tag to the end of the user's list.
///
/// # Errors
///
/// Any database failure (connection, encoding) arrives as
/// [`AppError::Database`](crate::error::AppError); an unknown `user_id` or
/// `hashtag_id` is the foreign-key violation the database reports.
pub async fn follow(pool: &PgPool, user_id: Uuid, hashtag_id: Uuid) -> AppResult<u64> {
    let inserted = sqlx::query(FOLLOW_SQL)
        .bind(user_id)
        .bind(hashtag_id)
        .execute(pool)
        .await?;
    Ok(inserted.rows_affected())
}

/// Removes `user_id`'s follow of `hashtag_id`, reporting whether one went.
///
/// A pair that did not follow — or never existed — matches no row and
/// changes nothing (Pleroma's `HashtagFollow.delete` on a `nil` lookup), so
/// unfollowing twice unfollows once and the second call reports `false`.
///
/// # Errors
///
/// Any database failure (connection, encoding) arrives as
/// [`AppError::Database`](crate::error::AppError).
pub async fn unfollow(pool: &PgPool, user_id: Uuid, hashtag_id: Uuid) -> AppResult<bool> {
    let deleted = sqlx::query(UNFOLLOW_SQL)
        .bind(user_id)
        .bind(hashtag_id)
        .execute(pool)
        .await?;
    Ok(deleted.rows_affected() > 0)
}

/// Whether `user_id` follows `hashtag_id` right now.
///
/// # Errors
///
/// Any database failure (connection, encoding) arrives as
/// [`AppError::Database`](crate::error::AppError).
pub async fn is_following(pool: &PgPool, user_id: Uuid, hashtag_id: Uuid) -> AppResult<bool> {
    let (following,) = sqlx::query_as::<_, (bool,)>(IS_FOLLOWING_SQL)
        .bind(user_id)
        .bind(hashtag_id)
        .fetch_one(pool)
        .await?;
    Ok(following)
}

/// `user_id`'s followed tags, newest follow first and `page`'s `limit` at a
/// time, each entry carrying its [`Entry::position`] next to the [`Hashtag`]
/// (see [`Entry`]).
///
/// [`OrdinalPageParams`]'s cursors are follow ordinals and the page is
/// ordered by them: the order the tags were followed in, newest first — not
/// the tags' `UUIDv7` ids (decision 4's rule applies to id-keyed listings).
/// `limit=0` answers every followed tag.
///
/// # Errors
///
/// Any database failure (connection, encoding) arrives as
/// [`AppError::Database`](crate::error::AppError).
pub async fn page(pool: &PgPool, user_id: Uuid, page: OrdinalPageParams) -> AppResult<Vec<Entry>> {
    let mut builder = QueryBuilder::<Postgres>::new(PAGE_SQL);
    // The builder's push methods hand back `&mut Self` for chaining; binding
    // it keeps the statement from having an unused result (decision 14).
    let _bound: &mut _ = builder.push_bind(user_id);
    page.apply_keyset(&mut builder, "hf.position");
    let rows = builder.build_query_as::<Entry>().fetch_all(pool).await?;
    Ok(page.finish(rows))
}

#[cfg(test)]
mod tests {
    use crate::{
        error::AppError, models::hashtag, pagination::MAX_LIMIT, test_fixtures::local_user,
    };

    use super::*;

    /// The names of the tags on `entries`, in page order.
    fn names(entries: &[Entry]) -> Vec<String> {
        entries
            .iter()
            .map(|entry| entry.hashtag.name.clone())
            .collect()
    }

    /// A fixture tag named `name`.
    async fn tag(pool: &PgPool, name: &str) -> Hashtag {
        hashtag::find_or_create(pool, name)
            .await
            .expect("fixture tag creates")
            .expect("the fixture name is usable")
    }

    /// The id of a local user named `nickname`.
    async fn user(pool: &PgPool, nickname: &str) -> Uuid {
        local_user(pool, nickname).await.id
    }

    /// The page parameters of `query`, for the ordinal page tests.
    fn page_params(query: &str) -> OrdinalPageParams {
        OrdinalPageParams::from_query(Some(query), MAX_LIMIT).expect("the fixture query parses")
    }

    #[tokio::test]
    async fn following_is_idempotent_and_unfollowing_reports() {
        let db = crate::test_db::test_db().await.expect("test database slot");
        let pool = db.pool().clone();
        let alice = user(&pool, "alice").await;
        let foo = tag(&pool, "foo").await;

        assert!(
            !is_following(&pool, alice, foo.id).await.unwrap(),
            "a fresh pair follows nothing"
        );
        assert_eq!(
            follow(&pool, alice, foo.id).await.unwrap(),
            1,
            "the first follow writes a row"
        );
        assert!(is_following(&pool, alice, foo.id).await.unwrap());
        assert_eq!(
            follow(&pool, alice, foo.id).await.unwrap(),
            0,
            "and the second one writes nothing"
        );

        assert!(
            unfollow(&pool, alice, foo.id).await.unwrap(),
            "the first unfollow removes the row"
        );
        assert!(
            !is_following(&pool, alice, foo.id).await.unwrap(),
            "and the pair is gone"
        );
        assert!(
            !unfollow(&pool, alice, foo.id).await.unwrap(),
            "unfollow twice unfollows once"
        );
    }

    #[tokio::test]
    async fn an_unknown_side_of_a_follow_is_the_foreign_key() {
        let db = crate::test_db::test_db().await.expect("test database slot");
        let pool = db.pool().clone();
        let alice = user(&pool, "alice").await;
        let err = follow(&pool, alice, Uuid::now_v7())
            .await
            .expect_err("the foreign key refuses an unknown tag");
        assert!(
            matches!(err, AppError::Database(_)),
            "a missing tag is a database error: {err}"
        );

        let user_err = follow(&pool, Uuid::now_v7(), tag(&pool, "foo").await.id)
            .await
            .expect_err("and so is an unknown user");
        assert!(
            matches!(user_err, AppError::Database(_)),
            "the other side is checked too: {user_err}"
        );
    }

    #[tokio::test]
    async fn deleting_a_user_or_tag_cascades_the_follows() {
        let db = crate::test_db::test_db().await.expect("test database slot");
        let pool = db.pool().clone();
        let alice = user(&pool, "alice").await;
        let bob = user(&pool, "bob").await;
        let foo = tag(&pool, "foo").await;
        let bar = tag(&pool, "bar").await;
        assert_eq!(follow(&pool, alice, foo.id).await.unwrap(), 1);
        assert_eq!(follow(&pool, alice, bar.id).await.unwrap(), 1);
        assert_eq!(follow(&pool, bob, foo.id).await.unwrap(), 1);

        let alice_gone = sqlx::query("DELETE FROM users WHERE id = $1")
            .bind(alice)
            .execute(&pool)
            .await
            .expect("alice hard-deletes");
        assert_eq!(alice_gone.rows_affected(), 1, "one user went");
        assert!(
            page(&pool, alice, page_params("limit=0"))
                .await
                .unwrap()
                .is_empty(),
            "alice's follows went with her"
        );
        assert!(
            is_following(&pool, bob, foo.id).await.unwrap(),
            "while bob's stayed"
        );

        let foo_gone = sqlx::query("DELETE FROM hashtags WHERE id = $1")
            .bind(foo.id)
            .execute(&pool)
            .await
            .expect("the tag hard-deletes");
        assert_eq!(foo_gone.rows_affected(), 1, "one tag went");
        assert!(
            !is_following(&pool, bob, foo.id).await.unwrap(),
            "and bob's follow of it cascaded"
        );
    }

    #[tokio::test]
    async fn page_orders_by_follow_insertion_and_pages_over_position() {
        let db = crate::test_db::test_db().await.expect("test database slot");
        let pool = db.pool().clone();
        let alice = user(&pool, "alice").await;
        let stranger = user(&pool, "stranger").await;
        for name in ["alpha", "beta", "gamma"] {
            let tag = tag(&pool, name).await;
            assert_eq!(follow(&pool, alice, tag.id).await.unwrap(), 1);
        }
        assert_eq!(
            follow(&pool, stranger, tag(&pool, "alpha").await.id)
                .await
                .unwrap(),
            1,
            "the stranger follows alpha too"
        );

        let every = page(&pool, alice, page_params("limit=0")).await.unwrap();
        assert_eq!(
            names(&every),
            vec!["gamma", "beta", "alpha"],
            "newest follow first, and only alice's own tags"
        );
        let positions: Vec<i64> = every.iter().map(|entry| entry.position).collect();
        let descending = positions.windows(2).all(|pair| {
            pair.first()
                .and_then(|&newer| pair.last().map(|&older| (newer, older)))
                .is_some_and(|(newer, older)| newer > older)
        });
        assert!(
            descending,
            "the page descends on the follow ordinal: {positions:?}"
        );
        let middle = *positions
            .get(1)
            .expect("three follows have three positions");

        let older = page(&pool, alice, page_params(&format!("max_id={middle}")))
            .await
            .unwrap();
        assert_eq!(
            names(&older),
            vec!["alpha"],
            "max_id walks to the follows made before the cursor"
        );
        let newer = page(&pool, alice, page_params(&format!("since_id={middle}")))
            .await
            .unwrap();
        assert_eq!(
            names(&newer),
            vec!["gamma"],
            "since_id reads the newer follows, newest first"
        );
        let back = page(&pool, alice, page_params(&format!("min_id={middle}")))
            .await
            .unwrap();
        assert_eq!(
            names(&back),
            vec!["gamma"],
            "a min_id page reads just after the cursor and is answered \
             newest-first"
        );
    }
}
