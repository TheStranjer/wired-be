//! The `scrobbles` table: what an account listened to, one row per listen.
//!
//! Reference: Pleroma's `Listen` activities
//! (`lib/pleroma/web/pleroma_api/controllers/scrobble_controller.ex`), whose
//! media metadata lives in a JSONB `Audio` object. Per
//! `docs/ARCHITECTURE.md` decision 6 that object becomes columns here, like
//! statuses: the object's `title`/`artist`/`album`/`length`/`externalLink`
//! are `title`/`artist`/`album`/`length`/`external_link`, its actor is
//! `user_id` and its `published` time is `inserted_at`. A scrobble federates as a
//! `Listen` whose id is `ap_id` (`{BASE_URL}/activities/{id}`, decision 6)
//! around an `Audio` object at `{BASE_URL}/objects/{id}`; since a scrobble
//! is never edited it has no `updated_at`.
//!
//! Visibility is the same four-valued thing a status has, so this module
//! reuses [`Visibility`] rather than minting a second enum; `local` is a
//! status-only visibility and the column's `CHECK` refuses it. The two
//! private levels are what makes [`page_for_user`] take a viewer: Pleroma's
//! index runs its activities through `fetch_user_abstract_activities`, which
//! drops everything the reading user is not addressed to, so a stranger
//! listing someone's scrobbles never sees their `private` or `direct` ones.
//!
//! [`Visibility`]: crate::models::status::Visibility

use chrono::{DateTime, Utc};
use sqlx::{PgPool, Postgres, QueryBuilder};
use uuid::Uuid;

use crate::{
    error::AppResult,
    models::status::Visibility,
    pagination::PageParams,
    util::id::{activity_ap_id, new_id},
};

/// [`insert`]: the id this module mints (decision 4) and the `Listen` id
/// built from it, the owner and what the caller decided, with `inserted_at`
/// left to the column default. `RETURNING *` brings the row back with that
/// stamp.
const INSERT_SQL: &str = "INSERT INTO scrobbles (
     id, ap_id, user_id, title, artist, album, length, external_link, visibility
) VALUES ($1, $2, $3, $4, $5, $6, $7, $8, $9) RETURNING *";

/// [`get_by_id`]: one scrobble by its primary key.
const GET_SQL: &str = "SELECT * FROM scrobbles WHERE id = $1";

/// The start of [`page_for_user`]'s statement: one account's scrobbles, the
/// owner bound next and the page's conditions appended after it.
const PAGE_SQL: &str = "SELECT * FROM scrobbles WHERE user_id = ";

/// The condition [`page_for_user`] adds for a reader who is not the owner:
/// the two private visibilities stay hidden from everyone else, exactly as
/// Pleroma's address-based filter drops a `Listen` the reader is not
/// addressed to. Plain SQL, nothing user-supplied in it.
const OWNER_ONLY_SQL: &str = " AND visibility NOT IN ('private', 'direct')";

/// One row of `scrobbles`: one listen, posted by one account.
#[derive(Clone, Debug, PartialEq, Eq, sqlx::FromRow)]
pub struct Scrobble {
    /// `UUIDv7` primary key, minted by [`insert`] (decision 4); its text is
    /// the creation order the API lists scrobbles in and the cursor
    /// [`PageParams`] compares.
    pub id: Uuid,
    /// The `Listen` activity's id, `{BASE_URL}/activities/{id}`; `None` for
    /// a row that predates federation.
    pub ap_id: Option<String>,
    /// The account that listened; every read and write is scoped to it.
    pub user_id: Uuid,
    /// The title of the media playing, verbatim as the client sent it.
    pub title: String,
    /// The artist, `None` when the client named none.
    pub artist: Option<String>,
    /// The album, `None` when the client named none.
    pub album: Option<String>,
    /// Length of the media in milliseconds, `None` when the client named
    /// none.
    pub length: Option<i32>,
    /// URL referencing the media, `None` when the client sent none.
    pub external_link: Option<String>,
    /// Who may see this scrobble; see the [module docs](self) for the three
    /// levels [`page_for_user`] acts on.
    pub visibility: Visibility,
    /// When the row was created, which is when the listen happened: the
    /// entity's `created_at`.
    pub inserted_at: DateTime<Utc>,
}

/// What to create: every `scrobbles` column except the `id` [`insert`]
/// mints, the `user_id` the handler takes from its caller and the
/// `inserted_at` the database stamps.
///
/// [`Default::default`] mirrors the column defaults, so a caller sets only
/// what it decides on; the usual minimum is the title.
#[derive(Clone, Debug)]
pub struct NewScrobble {
    /// The title; empty in the default, and the API's only required
    /// parameter.
    pub title: String,
    /// The artist; `None` by default, like the column.
    pub artist: Option<String>,
    /// The album; `None` by default, like the column.
    pub album: Option<String>,
    /// Length in milliseconds; `None` by default, like the column.
    pub length: Option<i32>,
    /// The external link; `None` by default, like the column.
    pub external_link: Option<String>,
    /// Who may see it; [`Visibility::Public`] by default, like the column.
    pub visibility: Visibility,
}

impl Default for NewScrobble {
    /// The column defaults: empty title, nothing else, public.
    fn default() -> Self {
        Self {
            title: String::new(),
            artist: None,
            album: None,
            length: None,
            external_link: None,
            visibility: Visibility::Public,
        }
    }
}

/// Inserts `scrobble` for `user_id` and returns the stored row.
///
/// The primary key is minted here (decision 4), so a scrobble's place in the
/// newest-first listing is its creation time, and the row's `ap_id` is the
/// `Listen` id [`activity_ap_id`] builds from it under `base_url`.
///
/// # Errors
///
/// Any database failure (connection, encoding) arrives as
/// [`AppError::Database`](crate::error::AppError).
pub async fn insert(
    pool: &PgPool,
    base_url: &str,
    user_id: Uuid,
    scrobble: &NewScrobble,
) -> AppResult<Scrobble> {
    let id = new_id();
    let row = sqlx::query_as::<_, Scrobble>(INSERT_SQL)
        .bind(id)
        .bind(activity_ap_id(base_url, id))
        .bind(user_id)
        .bind(&scrobble.title)
        .bind(scrobble.artist.as_deref())
        .bind(scrobble.album.as_deref())
        .bind(scrobble.length)
        .bind(scrobble.external_link.as_deref())
        .bind(scrobble.visibility)
        .fetch_one(pool)
        .await?;
    Ok(row)
}

/// The scrobble with primary key `id`, `None` when there is none.
///
/// # Errors
///
/// Any database failure (connection, decoding) arrives as
/// [`AppError::Database`](crate::error::AppError).
pub async fn get_by_id(pool: &PgPool, id: Uuid) -> AppResult<Option<Scrobble>> {
    let row = sqlx::query_as::<_, Scrobble>(GET_SQL)
        .bind(id)
        .fetch_optional(pool)
        .await?;
    Ok(row)
}

/// The scrobbles of `owner_id`, newest first, `page`'s `limit` at a time —
/// what `GET /api/v1/pleroma/accounts/{id}/scrobbles` answers.
///
/// `viewer_id` is the reader behind an optional token (`None` for an
/// anonymous request): unless the reader *is* the owner, the `private` and
/// `direct` rows are left out, so a stranger sees only what the account
/// scrobbled in the open. Whether `owner_id` names a live user is the API's
/// question, not this query's.
///
/// # Errors
///
/// Any database failure (connection, encoding) arrives as
/// [`AppError::Database`](crate::error::AppError).
pub async fn page_for_user(
    pool: &PgPool,
    owner_id: Uuid,
    viewer_id: Option<Uuid>,
    page: PageParams,
) -> AppResult<Vec<Scrobble>> {
    let mut builder = QueryBuilder::<Postgres>::new(PAGE_SQL);
    // The builder's push methods hand back `&mut Self` for chaining; binding
    // the result keeps these statements from being unused (decision 14).
    let _bound: &mut _ = builder.push_bind(owner_id);
    if viewer_id != Some(owner_id) {
        let _private_hidden: &mut _ = builder.push(OWNER_ONLY_SQL);
    }
    page.apply_keyset(&mut builder, "id");
    let rows = builder.build_query_as::<Scrobble>().fetch_all(pool).await?;
    Ok(page.finish(rows))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{
        config::TEST_BASE_URL, error::AppError, models::user::User, pagination::MAX_LIMIT,
        test_fixtures::local_user as user,
    };

    /// A scrobble of `title`, the column defaults otherwise.
    fn titled(title: &str) -> NewScrobble {
        NewScrobble {
            title: title.to_owned(),
            ..NewScrobble::default()
        }
    }

    /// A scrobble of `title` and `visibility`.
    fn visible(title: &str, visibility: Visibility) -> NewScrobble {
        NewScrobble {
            visibility,
            ..titled(title)
        }
    }

    /// Inserts a scrobble of `title` for `owner` and returns its row.
    async fn seed(pool: &PgPool, owner: &User, title: &str) -> Scrobble {
        insert(pool, TEST_BASE_URL, owner.id, &titled(title))
            .await
            .expect("the fixture scrobble inserts")
    }

    /// Inserts a scrobble of `title` and `visibility` for `owner`.
    async fn seed_visible(pool: &PgPool, owner: &User, title: &str, visibility: Visibility) {
        let _stored: Scrobble = insert(pool, TEST_BASE_URL, owner.id, &visible(title, visibility))
            .await
            .expect("the fixture scrobble inserts");
    }

    /// The default page parameters: limit 20, no cursors.
    fn page() -> PageParams {
        PageParams::from_query(None, MAX_LIMIT).expect("an empty query paginates")
    }

    /// The page parameters of `query`.
    fn page_of(query: &str) -> PageParams {
        PageParams::from_query(Some(query), MAX_LIMIT).expect("the fixture query paginates")
    }

    /// The titles of `rows`, in row order.
    fn titles(rows: &[Scrobble]) -> Vec<&str> {
        rows.iter().map(|row| row.title.as_str()).collect()
    }

    /// How many `scrobbles` rows the database holds.
    async fn count_scrobbles(pool: &PgPool) -> i64 {
        sqlx::query_scalar::<_, i64>("SELECT count(*) FROM scrobbles")
            .fetch_one(pool)
            .await
            .expect("counting scrobbles succeeds")
    }

    #[tokio::test]
    async fn insert_round_trips_every_column() {
        let db = crate::test_db::test_db().await.expect("test database slot");
        let pool = db.pool().clone();
        let alice = user(&pool, "alice").await;
        let stored = insert(
            &pool,
            TEST_BASE_URL,
            alice.id,
            &NewScrobble {
                title: "Midnight City".to_owned(),
                artist: Some("M83".to_owned()),
                album: Some("Hurry Up, We're Dreaming".to_owned()),
                length: Some(243_000),
                external_link: Some("https://example.test/midnight-city".to_owned()),
                visibility: Visibility::Unlisted,
            },
        )
        .await
        .expect("a scrobble creates");

        assert!(
            stored.id.get_version_num() == 7,
            "id is a UUIDv7: {}",
            stored.id
        );
        assert_eq!(stored.user_id, alice.id, "the owner is the argument");
        assert_eq!(stored.title, "Midnight City", "the title is kept verbatim");
        assert_eq!(stored.artist.as_deref(), Some("M83"), "the artist");
        assert_eq!(
            stored.album.as_deref(),
            Some("Hurry Up, We're Dreaming"),
            "the album"
        );
        assert_eq!(stored.length, Some(243_000), "the length in milliseconds");
        assert_eq!(
            stored.external_link.as_deref(),
            Some("https://example.test/midnight-city"),
            "the external link"
        );
        assert_eq!(
            stored.visibility,
            Visibility::Unlisted,
            "the visibility round trips"
        );
        assert!(
            stored.inserted_at <= Utc::now(),
            "the creation stamp is now or earlier"
        );
        assert_eq!(
            stored.ap_id,
            Some(format!("{TEST_BASE_URL}/activities/{}", stored.id)),
            "the Listen id is minted from the row id"
        );
    }

    #[tokio::test]
    async fn get_by_id_reads_the_row_back_or_nothing() {
        let db = crate::test_db::test_db().await.expect("test database slot");
        let pool = db.pool().clone();
        let alice = user(&pool, "alice").await;
        let stored = seed(&pool, &alice, "Midnight City").await;

        let found = get_by_id(&pool, stored.id)
            .await
            .expect("the read succeeds")
            .expect("the row exists");
        assert_eq!(found, stored, "the row comes back whole");
        assert!(
            get_by_id(&pool, new_id())
                .await
                .expect("the read succeeds")
                .is_none(),
            "an unknown id is None"
        );
    }

    #[tokio::test]
    async fn the_optional_columns_stay_null_and_visibility_defaults() {
        let db = crate::test_db::test_db().await.expect("test database slot");
        let pool = db.pool().clone();
        let alice = user(&pool, "alice").await;
        let stored = insert(&pool, TEST_BASE_URL, alice.id, &titled("only a title"))
            .await
            .expect("a title-only scrobble inserts");

        assert!(stored.artist.is_none(), "no artist, no value");
        assert!(stored.album.is_none(), "no album, no value");
        assert!(stored.length.is_none(), "no length, no value");
        assert!(stored.external_link.is_none(), "no external link, no value");
        assert_eq!(
            stored.visibility,
            Visibility::Public,
            "and the default is public, like the column"
        );
    }

    #[tokio::test]
    async fn the_column_check_refuses_a_status_only_visibility() {
        let db = crate::test_db::test_db().await.expect("test database slot");
        let pool = db.pool().clone();
        let alice = user(&pool, "alice").await;
        let err = insert(
            &pool,
            TEST_BASE_URL,
            alice.id,
            &visible("nope", Visibility::Local),
        )
        .await
        .expect_err("`local` is not a scrobble visibility");
        assert!(
            matches!(err, AppError::Database(_)),
            "the database itself refuses it: {err}"
        );
        assert_eq!(count_scrobbles(&pool).await, 0, "and no row went in");
    }

    #[tokio::test]
    async fn page_for_user_is_newest_first_and_the_owners_own() {
        let db = crate::test_db::test_db().await.expect("test database slot");
        let pool = db.pool().clone();
        let alice = user(&pool, "alice").await;
        let bob = user(&pool, "bob").await;
        let first = seed(&pool, &alice, "first").await;
        let _second = seed(&pool, &alice, "second").await;
        let newest = seed(&pool, &alice, "third").await;
        let _theirs: Scrobble = seed(&pool, &bob, "bob's own").await;
        assert_eq!(first.title, "first", "the fixtures are in place");

        let listed = page_for_user(&pool, alice.id, None, page())
            .await
            .expect("the page reads");
        assert_eq!(
            titles(&listed),
            ["third", "second", "first"],
            "alice's three scrobbles newest first, bob's nowhere"
        );
        assert_eq!(
            listed.first().expect("a nonempty page").id,
            newest.id,
            "the newest id leads"
        );

        let single = page_for_user(&pool, bob.id, Some(alice.id), page_of("limit=1"))
            .await
            .expect("a one-row page reads");
        assert_eq!(
            titles(&single),
            ["bob's own"],
            "`limit=1` answers the newest row only"
        );

        let nobody = page_for_user(&pool, Uuid::now_v7(), None, page())
            .await
            .expect("reading an unknown account is not a failure");
        assert!(nobody.is_empty(), "and it has no scrobbles");
    }

    #[tokio::test]
    async fn private_scrobbles_reach_their_owner_only() {
        let db = crate::test_db::test_db().await.expect("test database slot");
        let pool = db.pool().clone();
        let alice = user(&pool, "alice").await;
        let bob = user(&pool, "bob").await;
        seed_visible(&pool, &alice, "public one", Visibility::Public).await;
        seed_visible(&pool, &alice, "unlisted one", Visibility::Unlisted).await;
        seed_visible(&pool, &alice, "private one", Visibility::Private).await;
        seed_visible(&pool, &alice, "direct one", Visibility::Direct).await;

        let hers = page_for_user(&pool, alice.id, Some(alice.id), page())
            .await
            .expect("the owner's page reads");
        assert_eq!(
            titles(&hers),
            ["direct one", "private one", "unlisted one", "public one"],
            "the owner sees all four, newest first"
        );

        let anonymous = page_for_user(&pool, alice.id, None, page())
            .await
            .expect("the anonymous page reads");
        let stranger = page_for_user(&pool, alice.id, Some(bob.id), page())
            .await
            .expect("the stranger's page reads");
        assert_eq!(
            titles(&stranger),
            ["unlisted one", "public one"],
            "a stranger sees the two open ones only"
        );
        assert_eq!(titles(&anonymous), titles(&stranger), "and so does nobody");
    }

    #[tokio::test]
    async fn the_page_walks_by_id_cursors() {
        let db = crate::test_db::test_db().await.expect("test database slot");
        let pool = db.pool().clone();
        let alice = user(&pool, "alice").await;
        let mut ids: Vec<Uuid> = Vec::new();
        for n in 0..5 {
            let row = seed(&pool, &alice, &format!("track {n}")).await;
            ids.push(row.id);
        }

        let middle = page_for_user(&pool, alice.id, None, page_of("limit=2"))
            .await
            .expect("the first page reads");
        assert_eq!(titles(&middle), ["track 4", "track 3"], "two newest first");

        let older = page_for_user(
            &pool,
            alice.id,
            None,
            page_of(&format!(
                "limit=2&max_id={}",
                middle.last().expect("a full page").id
            )),
        )
        .await
        .expect("the cursor page reads");
        assert_eq!(titles(&older), ["track 2", "track 1"], "max_id walks back");

        let newer = page_for_user(
            &pool,
            alice.id,
            None,
            page_of(&format!("limit=2&min_id={}", ids[2])),
        )
        .await
        .expect("the forward page reads");
        assert_eq!(
            titles(&newer),
            ["track 4", "track 3"],
            "min_id answers newest-first too, after the flip"
        );

        let unseen = page_for_user(
            &pool,
            alice.id,
            None,
            page_of(&format!("since_id={}", ids[3])),
        )
        .await
        .expect("the polling page reads");
        assert_eq!(titles(&unseen), ["track 4"], "since_id gives the new ones");
    }

    #[tokio::test]
    async fn deleting_a_user_takes_their_scrobbles() {
        let db = crate::test_db::test_db().await.expect("test database slot");
        let pool = db.pool().clone();
        let alice = user(&pool, "alice").await;
        let bob = user(&pool, "bob").await;
        let _hers: Scrobble = seed(&pool, &alice, "hers").await;
        let _his: Scrobble = seed(&pool, &bob, "his").await;

        let deleted = sqlx::query("DELETE FROM users WHERE id = $1")
            .bind(alice.id)
            .execute(&pool)
            .await
            .expect("the fixture user deletes");
        assert_eq!(deleted.rows_affected(), 1, "one account went");
        assert_eq!(count_scrobbles(&pool).await, 1, "and her scrobble with it");

        let left = page_for_user(&pool, bob.id, None, page())
            .await
            .expect("bob's page still reads");
        assert_eq!(titles(&left), ["his"], "and nothing of his went");
    }
}
