//! The `custom_filter_keywords` table: the phrases one v2 filter looks for.
//!
//! Reference: Mastodon's `CustomFilterKeyword`. A keyword belongs to one
//! filter, and ownership is the filter's: every owned read and write here
//! joins `custom_filters` on `user_id`, so another account's keyword id is
//! indistinguishable from a missing one (both answer `None`/`false`, the 404
//! the API owes).
//!
//! [`matches()`] is the pure phrase test the filter application uses:
//! case-insensitive, and when [`whole_word`](Keyword::whole_word) is set the
//! phrase must sit at word boundaries — except, as Mastodon relaxes it, at
//! the edges where the phrase itself does not begin or end with a word
//! character, where no boundary is required there.

use chrono::{DateTime, Utc};
use sqlx::{PgConnection, PgPool};
use uuid::Uuid;

use crate::{error::AppResult, util::id::new_id};

/// [`insert`]: one keyword of an existing filter.
const INSERT_SQL: &str = "INSERT INTO custom_filter_keywords (id, filter_id, keyword, whole_word)
     VALUES ($1, $2, $3, $4) RETURNING *";

/// [`for_filters`]: every keyword of the named filters, oldest first.
const FOR_FILTERS_SQL: &str =
    "SELECT * FROM custom_filter_keywords WHERE filter_id = ANY($1) ORDER BY id";

/// [`get_owned`]: one keyword, only of its filter's owner.
const GET_OWNED_SQL: &str = "SELECT k.* FROM custom_filter_keywords k
     JOIN custom_filters f ON f.id = k.filter_id
     WHERE k.id = $1 AND f.user_id = $2";

/// [`update_owned`]: the given columns of one owned keyword; a `NULL`
/// argument keeps the stored value.
const UPDATE_OWNED_SQL: &str = "UPDATE custom_filter_keywords k
     SET keyword = COALESCE($3, k.keyword),
         whole_word = COALESCE($4, k.whole_word),
         updated_at = now()
     FROM custom_filters f
     WHERE k.filter_id = f.id AND k.id = $1 AND f.user_id = $2
  RETURNING k.*";

/// [`delete_owned`]: one keyword, only of its filter's owner.
const DELETE_OWNED_SQL: &str = "DELETE FROM custom_filter_keywords k
     USING custom_filters f
     WHERE k.filter_id = f.id AND k.id = $1 AND f.user_id = $2";

/// One row of `custom_filter_keywords`.
#[derive(Clone, Debug, PartialEq, Eq, sqlx::FromRow)]
pub struct Keyword {
    /// `UUIDv7` primary key; for a migrated v1 filter, the v1 filter's id.
    pub id: Uuid,
    /// The filter the keyword belongs to.
    pub filter_id: Uuid,
    /// The phrase to look for, verbatim as its owner wrote it
    /// (case-insensitivity belongs to [`matches()`], not to storage).
    #[sqlx(rename = "keyword")]
    pub phrase: String,
    /// Whether the phrase must appear as a whole word; see [`matches()`].
    pub whole_word: bool,
    /// When the row was created.
    pub inserted_at: DateTime<Utc>,
    /// When the row last changed.
    pub updated_at: DateTime<Utc>,
}

/// What to add as a keyword: the phrase and the word-boundary flag.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct NewKeyword {
    /// The phrase to look for.
    pub keyword: String,
    /// Whole-word matching; `true` by default, like the column.
    pub whole_word: bool,
}

impl NewKeyword {
    /// A keyword for `keyword` with whole-word matching on.
    #[must_use]
    pub fn new(keyword: &str) -> Self {
        Self {
            keyword: keyword.to_owned(),
            whole_word: true,
        }
    }
}

/// Inserts `keyword` into the filter `filter_id` on `conn`, minting its id.
///
/// The caller has checked that the filter exists and who owns it.
///
/// # Errors
///
/// Any database failure arrives as
/// [`AppError::Database`](crate::error::AppError).
pub async fn insert(
    conn: &mut PgConnection,
    filter_id: Uuid,
    keyword: &NewKeyword,
) -> AppResult<Keyword> {
    insert_with_id(conn, new_id(), filter_id, keyword).await
}

/// [`insert`] under a caller-chosen `id` — the v1 phrase filters mint one id
/// for the filter and its keyword.
///
/// # Errors
///
/// As [`insert`].
pub async fn insert_with_id(
    conn: &mut PgConnection,
    id: Uuid,
    filter_id: Uuid,
    keyword: &NewKeyword,
) -> AppResult<Keyword> {
    let row = sqlx::query_as::<_, Keyword>(INSERT_SQL)
        .bind(id)
        .bind(filter_id)
        .bind(&keyword.keyword)
        .bind(keyword.whole_word)
        .fetch_one(conn)
        .await?;
    Ok(row)
}

/// Every keyword of the filters `filter_ids`, oldest first.
///
/// # Errors
///
/// Any database failure arrives as
/// [`AppError::Database`](crate::error::AppError).
pub async fn for_filters(pool: &PgPool, filter_ids: &[Uuid]) -> AppResult<Vec<Keyword>> {
    let rows = sqlx::query_as::<_, Keyword>(FOR_FILTERS_SQL)
        .bind(filter_ids)
        .fetch_all(pool)
        .await?;
    Ok(rows)
}

/// The keyword `id` if the owner of its filter is `user_id`; `None` for a
/// missing or foreign id.
///
/// # Errors
///
/// Any database failure arrives as
/// [`AppError::Database`](crate::error::AppError).
pub async fn get_owned(pool: &PgPool, id: Uuid, user_id: Uuid) -> AppResult<Option<Keyword>> {
    let row = sqlx::query_as::<_, Keyword>(GET_OWNED_SQL)
        .bind(id)
        .bind(user_id)
        .fetch_optional(pool)
        .await?;
    Ok(row)
}

/// Changes the keyword `id` of `user_id`'s filter: `keyword` and
/// `whole_word` replace the stored values when given. `None` when no owned
/// keyword carries that id.
///
/// # Errors
///
/// Any database failure arrives as
/// [`AppError::Database`](crate::error::AppError).
pub async fn update_owned(
    conn: &mut PgConnection,
    id: Uuid,
    user_id: Uuid,
    changes: &KeywordPatch,
) -> AppResult<Option<Keyword>> {
    let row = sqlx::query_as::<_, Keyword>(UPDATE_OWNED_SQL)
        .bind(id)
        .bind(user_id)
        .bind(changes.keyword.as_deref())
        .bind(changes.whole_word)
        .fetch_optional(conn)
        .await?;
    Ok(row)
}

/// The columns [`update_owned`] may change; a `None` keeps the stored value.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct KeywordPatch {
    /// The new phrase.
    pub keyword: Option<String>,
    /// The new whole-word flag.
    pub whole_word: Option<bool>,
}

/// Deletes the keyword `id` of `user_id`'s filter, reporting whether one
/// went.
///
/// # Errors
///
/// Any database failure arrives as
/// [`AppError::Database`](crate::error::AppError).
pub async fn delete_owned(conn: &mut PgConnection, id: Uuid, user_id: Uuid) -> AppResult<bool> {
    let deleted = sqlx::query(DELETE_OWNED_SQL)
        .bind(id)
        .bind(user_id)
        .execute(conn)
        .await?;
    Ok(deleted.rows_affected() > 0)
}

/// Whether `text` contains `keyword`'s phrase, case-insensitively; when
/// [`whole_word`](Keyword::whole_word) is set the occurrence must sit at
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
pub fn matches(keyword: &Keyword, text: &str) -> bool {
    let phrase = keyword.phrase.to_lowercase();
    if phrase.is_empty() {
        return false;
    }
    let hay = text.to_lowercase();
    let mut offset = 0_usize;
    while let Some(found) = hay.get(offset..).and_then(|rest| rest.find(&phrase)) {
        let start = offset + found;
        let end = start + phrase.len();
        if !keyword.whole_word || at_relaxed_boundaries(&hay, start, end, &phrase) {
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
    use super::*;

    /// A stored keyword for [`matches()`] tests.
    fn stored(text: &str, whole_word: bool) -> Keyword {
        Keyword {
            id: Uuid::now_v7(),
            filter_id: Uuid::now_v7(),
            phrase: text.to_owned(),
            whole_word,
            inserted_at: Utc::now(),
            updated_at: Utc::now(),
        }
    }

    #[test]
    fn whole_word_requires_boundaries_at_word_character_edges() {
        let keyword = stored("foo", true);
        assert!(matches(&keyword, "a foo bar"), "surrounded by spaces");
        assert!(
            matches(&keyword, "foo"),
            "a string edge bounds at both ends"
        );
        assert!(
            matches(&keyword, "say (foo) here"),
            "punctuation breaks a word"
        );
        assert!(!matches(&keyword, "foobar"), "glued to a word behind");
        assert!(!matches(&keyword, "barfoo"), "glued to a word ahead");
        assert!(
            !matches(&keyword, "foo_bar"),
            "an underscore is a word character"
        );
        assert!(!matches(&keyword, "1foo"), "a digit is a word character");
        assert!(
            !matches(&keyword, "f o o"),
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
        let keyword = stored("foo", false);
        assert!(matches(&keyword, "foobar"), "no boundary is asked for");
        assert!(matches(&keyword, "barFOO"), "case still does not matter");
        assert!(
            !matches(&keyword, "f o o"),
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
        assert!(matches(&umlaut, "äö"), "case-folding reaches non-ASCII too");
    }

    #[test]
    fn multi_byte_text_offsets_find_the_right_boundaries() {
        // The boundary checks run on byte offsets into multi-byte text;
        // these pin the arithmetic down.
        let keyword = stored("foo", true);
        assert!(
            matches(&keyword, "überläuft foo"),
            "the hit sits behind multi-byte text"
        );
        assert!(
            !matches(&keyword, "überläuft barfoo"),
            "a glued occurrence behind multi-byte text still fails"
        );
        assert!(
            matches(&keyword, "überläuft foobar foo"),
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

    #[test]
    fn a_new_keyword_is_whole_word_by_default() {
        let fresh = NewKeyword::new("foo");
        assert_eq!(fresh.keyword, "foo", "the phrase is kept");
        assert!(fresh.whole_word, "whole word is on, like the column");
    }
}
