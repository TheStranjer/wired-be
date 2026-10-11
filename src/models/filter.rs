//! The `filters` table: per-user phrase filters with Mastodon v1 semantics.
//!
//! Reference: Pleroma's `Pleroma.Filter` (`lib/pleroma/filter.ex`). Pleroma
//! gives each filter a per-user integer `filter_id` beside its row id and
//! schedules an Oban job to purge it at expiry; neither is needed here: the
//! row's `UUIDv7` id (decision 4) is what the API names, and expiry is a
//! plain query condition — [`list_active`] reads
//! `(expires_at IS NULL OR expires_at > now())`, which keeps the active set
//! correct without a sweeper job.
//!
//! Filters are strictly per-user: [`get_owned`], [`update`] and [`delete`]
//! all carry the owner in their `WHERE` clause, so a foreign id is
//! indistinguishable from a missing one (both answer `None`/`false`), which
//! is exactly the 404 the API owes. Deleting a user takes their filters
//! with it through the foreign key's cascade.
//!
//! [`matches()`] is the pure phrase test the timeline ticket will apply:
//! case-insensitive, and when [`whole_word`](Filter::whole_word) is set the
//! phrase must sit at word boundaries — except, as Mastodon relaxes it, at
//! the edges where the phrase itself does not begin or end with a word
//! character, where no boundary is required there.

use chrono::{DateTime, Utc};
use sqlx::PgPool;
use uuid::Uuid;

use crate::{error::AppResult, util::id::new_id};

/// [`create`]: the id this module mints (decision 4), the owner and what the
/// caller decided, with `inserted_at` and `updated_at` left to the column
/// defaults. `RETURNING *` brings the row back with those stamps.
const INSERT_SQL: &str = "INSERT INTO filters (
     id, user_id, phrase, context, whole_word, irreversible, expires_at
) VALUES ($1, $2, $3, $4, $5, $6, $7) RETURNING *";

/// [`get_owned`]: one filter, only of its owner — a foreign id reads as
/// absent.
const GET_OWNED_SQL: &str = "SELECT * FROM filters WHERE id = $1 AND user_id = $2";

/// [`list_active`]: the owner's filters that have not expired, newest first
/// on the id whose text sorts in creation order (decision 4). A `NULL`
/// expiry is an open end: the filter never stops being active.
const LIST_ACTIVE_SQL: &str = "SELECT * FROM filters
     WHERE user_id = $1
       AND (expires_at IS NULL OR expires_at > now())
  ORDER BY id DESC";

/// [`update`]: every mutable column of one owned filter, and the
/// `updated_at` bump Pleroma's `Repo.update` stamps too. The owner is part
/// of the `WHERE` clause, so [`update`] reports a foreign or missing row as
/// `None` without a separate existence read.
const UPDATE_SQL: &str = "UPDATE filters
     SET phrase = $3, context = $4, whole_word = $5, irreversible = $6,
         expires_at = $7, updated_at = now()
     WHERE id = $1 AND user_id = $2
  RETURNING *";

/// [`delete`]: one owned filter; a foreign or missing id deletes nothing.
const DELETE_SQL: &str = "DELETE FROM filters WHERE id = $1 AND user_id = $2";

/// One row of `filters`: a phrase one account wants kept out of (or hidden
/// from) their timelines.
#[derive(Clone, Debug, PartialEq, Eq, sqlx::FromRow)]
pub struct Filter {
    /// `UUIDv7` primary key, minted by [`create`] (decision 4); also the
    /// creation order lists are read in.
    pub id: Uuid,
    /// The owning account; every read and write of a filter is scoped to it.
    pub user_id: Uuid,
    /// The phrase to look for, verbatim as its owner wrote it
    /// (case-insensitivity belongs to [`matches()`], not to storage).
    pub phrase: String,
    /// Where the filter applies: a non-empty subset of `home`,
    /// `notifications`, `public`, `thread` and `account` (the API
    /// validates; the column stores what passed).
    pub context: Vec<String>,
    /// Whether the phrase must appear as a whole word; see [`matches()`].
    pub whole_word: bool,
    /// Whether clients hide matching posts instead of warning
    /// (Mastodon's `irreversible`, Pleroma's `hide`).
    pub irreversible: bool,
    /// When the filter stops being active; `None` means it never does.
    pub expires_at: Option<DateTime<Utc>>,
    /// When the row was created.
    pub inserted_at: DateTime<Utc>,
    /// When the row last changed; refreshed by every [`update`].
    pub updated_at: DateTime<Utc>,
}

/// What to create, or to replace a filter's content with: every `filters`
/// column except the `id` [`create`] mints, the `user_id` the handler takes
/// from its caller and the timestamps the database stamps.
///
/// [`Default::default`] mirrors the column defaults, so a caller sets only
/// what it decides on; the usual minimum is the phrase and the contexts.
#[derive(Clone, Debug)]
pub struct NewFilter {
    /// The phrase to look for; empty in the default.
    pub phrase: String,
    /// The contexts the filter applies in; empty in the default.
    pub context: Vec<String>,
    /// Whole-word matching; `true` by default, like the column.
    pub whole_word: bool,
    /// Hide rather than warn; `false` by default, like the column.
    pub irreversible: bool,
    /// When the filter expires; `None` (never) by default, like the column.
    pub expires_at: Option<DateTime<Utc>>,
}

impl Default for NewFilter {
    /// The column defaults: empty phrase and contexts, whole-word on,
    /// irreversible off and no expiry.
    fn default() -> Self {
        Self {
            phrase: String::new(),
            context: Vec::new(),
            whole_word: true,
            irreversible: false,
            expires_at: None,
        }
    }
}

/// Inserts `filter` owned by `user_id` and returns the stored row.
///
/// The primary key is minted here (decision 4), so the row's place in the
/// newest-first list is its creation time.
///
/// # Errors
///
/// Any database failure (connection, encoding) arrives as
/// [`AppError::Database`](crate::error::AppError).
pub async fn create(pool: &PgPool, user_id: Uuid, filter: &NewFilter) -> AppResult<Filter> {
    let row = sqlx::query_as::<_, Filter>(INSERT_SQL)
        .bind(new_id())
        .bind(user_id)
        .bind(&filter.phrase)
        .bind(&filter.context)
        .bind(filter.whole_word)
        .bind(filter.irreversible)
        .bind(filter.expires_at)
        .fetch_one(pool)
        .await?;
    Ok(row)
}

/// Fetches the filter `id` if `user_id` owns it; `None` for a missing or
/// foreign id, which the API renders as one and the same 404. Expiry is not
/// consulted: an expired filter is still a row its owner can read.
///
/// # Errors
///
/// Any database failure (connection, encoding) arrives as
/// [`AppError::Database`](crate::error::AppError).
pub async fn get_owned(pool: &PgPool, id: Uuid, user_id: Uuid) -> AppResult<Option<Filter>> {
    let row = sqlx::query_as::<_, Filter>(GET_OWNED_SQL)
        .bind(id)
        .bind(user_id)
        .fetch_optional(pool)
        .await?;
    Ok(row)
}

/// The filters of `user_id` that are active *now* — every one whose
/// [`expires_at`](Filter::expires_at) is `NULL` or still in the future —
/// newest first. Expired rows keep existing for [`get_owned`] and
/// [`update`]; they simply leave this list.
///
/// # Errors
///
/// Any database failure (connection, encoding) arrives as
/// [`AppError::Database`](crate::error::AppError).
pub async fn list_active(pool: &PgPool, user_id: Uuid) -> AppResult<Vec<Filter>> {
    let rows = sqlx::query_as::<_, Filter>(LIST_ACTIVE_SQL)
        .bind(user_id)
        .fetch_all(pool)
        .await?;
    Ok(rows)
}

/// Replaces every mutable column of the filter `id` of `user_id` with
/// `filter` — the whole-row write the API's `PUT` performs — and returns
/// the rewritten row, `None` when no owned filter carries that id.
///
/// # Errors
///
/// Any database failure (connection, encoding) arrives as
/// [`AppError::Database`](crate::error::AppError).
pub async fn update(
    pool: &PgPool,
    id: Uuid,
    user_id: Uuid,
    filter: &NewFilter,
) -> AppResult<Option<Filter>> {
    let row = sqlx::query_as::<_, Filter>(UPDATE_SQL)
        .bind(id)
        .bind(user_id)
        .bind(&filter.phrase)
        .bind(&filter.context)
        .bind(filter.whole_word)
        .bind(filter.irreversible)
        .bind(filter.expires_at)
        .fetch_optional(pool)
        .await?;
    Ok(row)
}

/// Deletes the filter `id` of `user_id`, reporting whether one went; a
/// foreign or missing id deletes nothing and reports `false`.
///
/// # Errors
///
/// Any database failure (connection, encoding) arrives as
/// [`AppError::Database`](crate::error::AppError).
pub async fn delete(pool: &PgPool, id: Uuid, user_id: Uuid) -> AppResult<bool> {
    let deleted = sqlx::query(DELETE_SQL)
        .bind(id)
        .bind(user_id)
        .execute(pool)
        .await?;
    Ok(deleted.rows_affected() > 0)
}

/// Whether `text` contains `filter`'s phrase, case-insensitively; when
/// [`whole_word`](Filter::whole_word) is set the occurrence must sit at
/// word boundaries.
///
/// A boundary is required only at an edge where the phrase itself starts
/// (respectively ends) with a word character — the relaxation Mastodon
/// applies to punctuation phrases: `"?!?"` needs nothing around it,
/// `"#tag"` needs a boundary after `tag` but not before `#`. A word
/// character is a letter, a digit or an underscore, Unicode-aware, the
/// same set PostgreSQL's `\y` draws its boundaries from. An empty phrase
/// matches nothing; there is no such row behind a validated API.
#[must_use]
pub fn matches(filter: &Filter, text: &str) -> bool {
    let phrase = filter.phrase.to_lowercase();
    if phrase.is_empty() {
        return false;
    }
    let hay = text.to_lowercase();
    let mut offset = 0_usize;
    while let Some(found) = hay.get(offset..).and_then(|rest| rest.find(&phrase)) {
        let start = offset + found;
        let end = start + phrase.len();
        if !filter.whole_word || at_relaxed_boundaries(&hay, start, end, &phrase) {
            return true;
        }
        // Resume after this occurrence's first character: the search never
        // re-reads bytes and never stops mid-character.
        let step = hay
            .get(start..)
            .and_then(|rest| rest.chars().next())
            .map_or(1, char::len_utf8);
        offset = start + step;
    }
    false
}

/// Whether the occurrence of `phrase` at `start..end` of `hay` sits at word
/// boundaries — where a boundary is only asked for at phrase edges that
/// are themselves word characters (see [`matches()`]).
fn at_relaxed_boundaries(hay: &str, start: usize, end: usize, phrase: &str) -> bool {
    let before_ok =
        !starts_with_word_char(phrase) || char_before(hay, start).is_none_or(|c| !is_word_char(c));
    let after_ok =
        !ends_with_word_char(phrase) || char_after(hay, end).is_none_or(|c| !is_word_char(c));
    before_ok && after_ok
}

/// Whether `phrase` begins with a word character (nothing does when empty).
fn starts_with_word_char(phrase: &str) -> bool {
    phrase.chars().next().is_some_and(is_word_char)
}

/// Whether `phrase` ends with a word character (nothing does when empty).
fn ends_with_word_char(phrase: &str) -> bool {
    phrase.chars().next_back().is_some_and(is_word_char)
}

/// The character just before index `start`, `None` at the beginning of the
/// string (a string start is a boundary).
fn char_before(hay: &str, start: usize) -> Option<char> {
    hay.get(..start)?.chars().next_back()
}

/// The character at index `end` and later, `None` at the end of the string
/// (a string end is a boundary).
fn char_after(hay: &str, end: usize) -> Option<char> {
    hay.get(end..)?.chars().next()
}

/// Whether `c` is a word character: a letter, a digit or an underscore,
/// Unicode-aware (PostgreSQL's `\w`, the set `\y` boundaries are drawn
/// between).
fn is_word_char(c: char) -> bool {
    c.is_alphanumeric() || c == '_'
}

#[cfg(test)]
mod tests {
    use chrono::Duration;

    use super::*;
    use crate::{
        config::TEST_BASE_URL,
        models::user::{NewUser, User, insert, local_ap_id},
    };

    /// An instant `days` from now, negative ones for the past, with the
    /// sub-microsecond part dropped so it survives PostgreSQL's
    /// microsecond resolution unchanged. Only ever used to decide which
    /// side of *now* an expiry falls on.
    fn offset(days: i64) -> DateTime<Utc> {
        use chrono::Timelike as _;
        (Utc::now() + Duration::days(days))
            .with_nanosecond(0)
            .expect("zero nanoseconds is always in range")
    }

    /// A filter named `phrase` applying in `home`, with the defaults
    /// otherwise.
    fn phrase(text: &str) -> NewFilter {
        NewFilter {
            phrase: text.to_owned(),
            context: vec!["home".to_owned()],
            ..NewFilter::default()
        }
    }

    /// Inserts a filter named `phrase` for `user`, dropping the row.
    async fn seed(pool: &PgPool, user: &User, text: &str) {
        let _row: Filter = create(pool, user.id, &phrase(text)).await.unwrap();
    }

    /// Inserts a filter named `text` for `user` expiring at `expires_at`.
    async fn seed_expiry(
        pool: &PgPool,
        user: &User,
        text: &str,
        expires_at: Option<DateTime<Utc>>,
    ) {
        let _row: Filter = create(
            pool,
            user.id,
            &NewFilter {
                expires_at,
                ..phrase(text)
            },
        )
        .await
        .unwrap();
    }

    /// Inserts a local user named `nickname`.
    async fn user(pool: &PgPool, nickname: &str) -> User {
        insert(
            pool,
            &NewUser::local(nickname, &local_ap_id(TEST_BASE_URL, nickname)),
        )
        .await
        .expect("the local fixture inserts")
    }

    /// The phrases of `rows`, in row order — what a list test compares
    /// against.
    fn phrases(rows: &[Filter]) -> Vec<&str> {
        rows.iter().map(|row| row.phrase.as_str()).collect()
    }

    /// A stored filter of `phrase(text)`'s shape, for [`matches()`] tests.
    fn stored(text: &str, whole_word: bool) -> Filter {
        Filter {
            id: Uuid::now_v7(),
            user_id: Uuid::now_v7(),
            phrase: text.to_owned(),
            context: vec!["home".to_owned()],
            whole_word,
            irreversible: false,
            expires_at: None,
            inserted_at: Utc::now(),
            updated_at: Utc::now(),
        }
    }

    #[sqlx::test(migrator = "crate::MIGRATOR")]
    async fn create_round_trips_every_column(pool: PgPool) {
        let alice = user(&pool, "alice").await;
        let expires = offset(2);
        let stored = create(
            &pool,
            alice.id,
            &NewFilter {
                phrase: "Spoilers".to_owned(),
                context: vec!["home".to_owned(), "public".to_owned()],
                whole_word: false,
                irreversible: true,
                expires_at: Some(expires),
            },
        )
        .await
        .expect("a filter creates");

        assert!(stored.id.get_version_num() == 7, "id is a UUIDv7");
        assert_eq!(stored.user_id, alice.id, "the owner is the argument");
        assert_eq!(stored.phrase, "Spoilers", "the phrase is stored verbatim");
        assert_eq!(
            stored.context,
            ["home", "public"],
            "the contexts round trip in order"
        );
        assert!(!stored.whole_word, "the flags round trip");
        assert!(stored.irreversible, "both of them");
        assert_eq!(stored.expires_at, Some(expires), "and so does the expiry");
        assert_eq!(
            stored.inserted_at, stored.updated_at,
            "a fresh row is stamped once"
        );
    }

    #[sqlx::test(migrator = "crate::MIGRATOR")]
    async fn create_defaults_match_the_column_defaults(pool: PgPool) {
        let alice = user(&pool, "alice").await;
        let stored = create(&pool, alice.id, &phrase("x"))
            .await
            .expect("a bare filter creates");
        assert!(stored.whole_word, "whole word is on by default");
        assert!(
            !stored.irreversible,
            "irreversible is off by default, like the column"
        );
        assert!(stored.expires_at.is_none(), "and a filter never expires");

        let empty = NewFilter::default();
        assert!(
            empty.phrase.is_empty() && empty.context.is_empty(),
            "the default text and contexts are empty, not missing"
        );
    }

    #[sqlx::test(migrator = "crate::MIGRATOR")]
    async fn list_active_is_newest_first_and_the_owners_own(pool: PgPool) {
        let alice = user(&pool, "alice").await;
        let bob = user(&pool, "bob").await;
        seed(&pool, &alice, "first").await;
        seed(&pool, &alice, "second").await;
        seed(&pool, &bob, "theirs").await;

        let listed = list_active(&pool, alice.id).await.expect("the list reads");
        assert_eq!(
            phrases(&listed),
            ["second", "first"],
            "alice's two filters, newest first; bob's stays his"
        );
    }

    #[sqlx::test(migrator = "crate::MIGRATOR")]
    async fn list_active_excludes_expired_filters(pool: PgPool) {
        let alice = user(&pool, "alice").await;
        seed(&pool, &alice, "never").await;
        seed_expiry(&pool, &alice, "future", Some(offset(2))).await;
        seed_expiry(&pool, &alice, "past", Some(offset(-2))).await;

        let listed = list_active(&pool, alice.id).await.expect("the list reads");
        assert_eq!(
            phrases(&listed),
            ["future", "never"],
            "the expired row leaves the list; an open end and a future one stay"
        );
    }

    #[sqlx::test(migrator = "crate::MIGRATOR")]
    async fn get_owned_reads_the_owners_row_and_nobody_elses(pool: PgPool) {
        let alice = user(&pool, "alice").await;
        let bob = user(&pool, "bob").await;
        let hers = create(&pool, alice.id, &phrase("mine"))
            .await
            .expect("the fixture creates");

        let fetched = get_owned(&pool, hers.id, alice.id)
            .await
            .expect("the owner's read succeeds");
        assert_eq!(fetched, Some(hers.clone()), "the owner gets the whole row");

        let foreign = get_owned(&pool, hers.id, bob.id)
            .await
            .expect("a foreign read succeeds too");
        assert!(foreign.is_none(), "and reports the row as absent");

        let missing = get_owned(&pool, Uuid::now_v7(), alice.id)
            .await
            .expect("reading nothing is not a failure");
        assert!(missing.is_none(), "an unknown id reads the same way");
    }

    #[sqlx::test(migrator = "crate::MIGRATOR")]
    async fn get_owned_serves_an_expired_filter(pool: PgPool) {
        let alice = user(&pool, "alice").await;
        seed_expiry(&pool, &alice, "expired", Some(offset(-1))).await;
        let listed = list_active(&pool, alice.id).await.expect("list reads");
        assert!(listed.is_empty(), "the list hides it");

        // An expired filter is still a row its owner can read by id: the
        // API's `GET /filters/:id` is a row lookup, not an active list.
        let found = sqlx::query_as::<_, Filter>("SELECT * FROM filters WHERE user_id = $1")
            .bind(alice.id)
            .fetch_one(&pool)
            .await
            .expect("the seeded row reads");
        let fetched = get_owned(&pool, found.id, alice.id)
            .await
            .expect("the read succeeds");
        assert_eq!(
            fetched.map(|row| row.phrase),
            Some("expired".to_owned()),
            "and get_owned serves it"
        );
    }

    #[sqlx::test(migrator = "crate::MIGRATOR")]
    async fn update_rewrites_every_column_once(pool: PgPool) {
        let alice = user(&pool, "alice").await;
        let stored = create(&pool, alice.id, &phrase("before"))
            .await
            .expect("the fixture creates");
        let expires = offset(3);
        let rewritten = update(
            &pool,
            stored.id,
            alice.id,
            &NewFilter {
                phrase: "after".to_owned(),
                context: vec!["thread".to_owned()],
                whole_word: false,
                irreversible: true,
                expires_at: Some(expires),
            },
        )
        .await
        .expect("a filter updates")
        .expect("the fixture filter exists");

        assert_eq!(rewritten.id, stored.id, "the row is rewritten in place");
        assert_eq!(rewritten.user_id, alice.id, "and stays owned");
        assert_eq!(rewritten.phrase, "after", "the phrase is replaced");
        assert_eq!(rewritten.context, ["thread"], "the contexts too");
        assert!(!rewritten.whole_word, "the flags too");
        assert!(rewritten.irreversible, "both");
        assert_eq!(rewritten.expires_at, Some(expires), "and the expiry");
        assert_eq!(
            rewritten.inserted_at, stored.inserted_at,
            "the creation stamp stays"
        );
        assert!(
            rewritten.updated_at >= stored.updated_at,
            "the update stamp moves forward"
        );
    }

    #[sqlx::test(migrator = "crate::MIGRATOR")]
    async fn update_can_clear_the_expiry(pool: PgPool) {
        let alice = user(&pool, "alice").await;
        let stored = create(
            &pool,
            alice.id,
            &NewFilter {
                expires_at: Some(offset(1)),
                ..phrase("soon")
            },
        )
        .await
        .expect("the fixture creates");

        let rewritten = update(&pool, stored.id, alice.id, &phrase("forever"))
            .await
            .expect("the update succeeds")
            .expect("the fixture exists");
        assert_eq!(
            rewritten.expires_at, None,
            "a whole-row write with no expiry clears the old one"
        );
    }

    #[sqlx::test(migrator = "crate::MIGRATOR")]
    async fn update_and_delete_of_a_foreign_or_missing_id_do_nothing(pool: PgPool) {
        let alice = user(&pool, "alice").await;
        let bob = user(&pool, "bob").await;
        let hers = create(&pool, alice.id, &phrase("mine"))
            .await
            .expect("the fixture creates");

        let updated = update(&pool, hers.id, bob.id, &phrase("stolen"))
            .await
            .expect("a foreign update is not a failure");
        assert!(updated.is_none(), "and rewrites nothing");

        let missing = Uuid::now_v7();
        assert!(
            update(&pool, missing, alice.id, &phrase("ghost"))
                .await
                .expect("updating nothing is not a failure")
                .is_none(),
            "no row, no rewrite"
        );

        assert!(
            !(delete(&pool, hers.id, bob.id)
                .await
                .expect("a foreign delete is not a failure")),
            "bob deletes nothing of alice's"
        );
        assert!(
            !(delete(&pool, missing, alice.id)
                .await
                .expect("deleting nothing is not a failure")),
            "and an unknown id deletes nothing"
        );
        assert!(
            get_owned(&pool, hers.id, alice.id)
                .await
                .expect("the row reads")
                .is_some(),
            "alice's filter survived every failed attempt on it"
        );
    }

    #[sqlx::test(migrator = "crate::MIGRATOR")]
    async fn delete_takes_a_filter_once(pool: PgPool) {
        let alice = user(&pool, "alice").await;
        let doomed = create(&pool, alice.id, &phrase("doomed"))
            .await
            .expect("the fixture creates");

        assert!(
            delete(&pool, doomed.id, alice.id)
                .await
                .expect("the filter deletes"),
            "the first delete finds it"
        );
        assert!(
            !(delete(&pool, doomed.id, alice.id)
                .await
                .expect("deleting nothing is not a failure")),
            "the second finds nothing"
        );
        assert!(
            get_owned(&pool, doomed.id, alice.id)
                .await
                .expect("the row reads")
                .is_none(),
            "and it is gone"
        );
    }

    #[sqlx::test(migrator = "crate::MIGRATOR")]
    async fn deleting_a_user_takes_their_filters(pool: PgPool) {
        let alice = user(&pool, "alice").await;
        seed(&pool, &alice, "hers").await;

        let deleted = sqlx::query("DELETE FROM users WHERE id = $1")
            .bind(alice.id)
            .execute(&pool)
            .await
            .expect("the fixture user deletes");
        assert_eq!(deleted.rows_affected(), 1, "one account went");

        let left = sqlx::query_scalar::<_, i64>("SELECT count(*) FROM filters")
            .fetch_one(&pool)
            .await
            .expect("the filters count");
        assert_eq!(left, 0, "and their filters with it");
    }

    #[test]
    fn whole_word_requires_boundaries_at_word_character_edges() {
        let filter = stored("foo", true);
        assert!(matches(&filter, "a foo bar"), "surrounded by spaces");
        assert!(matches(&filter, "foo"), "a string edge bounds at both ends");
        assert!(
            matches(&filter, "say (foo) here"),
            "punctuation breaks a word"
        );
        assert!(!matches(&filter, "foobar"), "glued to a word behind");
        assert!(!matches(&filter, "barfoo"), "glued to a word ahead");
        assert!(
            !matches(&filter, "foo_bar"),
            "an underscore is a word character"
        );
        assert!(!matches(&filter, "1foo"), "a digit is a word character");
        assert!(
            !matches(&filter, "f o o"),
            "letters split by spaces are no occurrence of the phrase"
        );
    }

    #[test]
    fn matching_is_case_insensitive() {
        let upper = stored("FOO", true);
        assert!(
            matches(&upper, "a foo bar"),
            "the phrase case differs from the text"
        );
        let lower = stored("foo", true);
        assert!(matches(&lower, "FOO!"), "and the other way around");
        assert!(matches(&lower, "FoO BaR"), "mixed case too");
    }

    #[test]
    fn whole_word_off_is_a_plain_substring_search() {
        let filter = stored("foo", false);
        assert!(matches(&filter, "foobar"), "no boundary is asked for");
        assert!(matches(&filter, "barFOO"), "case still does not matter");
        assert!(
            !matches(&filter, "f o o"),
            "but the letters must be adjacent"
        );
    }

    #[test]
    fn punctuation_phrases_need_no_boundary_where_they_are_not_words() {
        // Mastodon's relaxation: a boundary is required only at the edges
        // of the phrase where the phrase has word characters.
        let shout = stored("?!?", true);
        assert!(
            matches(&shout, "what?!?yes"),
            "no word edges, no boundaries"
        );
        let tag = stored("#hashtag", true);
        assert!(
            matches(&tag, "text#hashtag here"),
            "the `#` edge needs nothing"
        );
        assert!(!matches(&tag, "#hashtagger"), "but the `t` edge still does");
        let dots = stored("...", true);
        assert!(
            matches(&dots, "wait...then"),
            "a dotted phrase is all edges"
        );
        let colon = stored(":)", true);
        assert!(
            matches(&colon, "hi :)x"),
            "even the trailing edge of `:)` is not a word"
        );
    }

    #[test]
    fn boundaries_are_unicode_aware() {
        let cafe = stored("café", true);
        assert!(matches(&cafe, "un café!"), "the accent is part of the word");
        assert!(
            !matches(&cafe, "cafés"),
            "so a letter after it breaks the boundary"
        );
        let naive = stored("naïve", true);
        assert!(!matches(&naive, "énaïve"), "and a letter before it does");
        assert!(
            matches(&naive, "très naïve"),
            "accents on both sides still bound"
        );
        let umlaut = stored("ÄÖ", true);
        assert!(
            matches(&umlaut, "äö",),
            "case-folding reaches non-ASCII too"
        );
    }

    #[test]
    fn multi_byte_text_offsets_find_the_right_boundaries() {
        // The boundary checks run on byte offsets into multi-byte text;
        // these pin the arithmetic down.
        let filter = stored("foo", true);
        assert!(
            matches(&filter, "überläuft foo"),
            "the hit sits behind multi-byte text"
        );
        assert!(
            !matches(&filter, "überläuft barfoo"),
            "a glued occurrence behind multi-byte text still fails"
        );
        assert!(
            matches(&filter, "überläuft foobar foo"),
            "scanning continues past a rejected occurrence"
        );
    }

    #[test]
    fn nothing_finds_nothing() {
        assert!(
            !matches(&stored("", true), "anything"),
            "an empty phrase matches no text"
        );
        assert!(
            !matches(&stored("foo", true), ""),
            "empty text holds no phrase"
        );
        assert!(
            !matches(&stored("foo", false), "bar"),
            "a plain miss is a miss in both modes"
        );
    }
}
