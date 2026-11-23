//! The `hashtags` and `status_hashtags` tables: the tags a status carries.
//!
//! Reference: Pleroma normalizes tag names and keeps one `hashtags` row per
//! name behind an object join table (`lib/pleroma/hashtag.ex`). Here the
//! name is stored lowercase and without the leading `#` (the same shape
//! [`crate::util::entities::extract_hashtags`] yields), so the unique key is
//! plain text and the entity renders straight from storage; the reverse
//! lookup — the newest statuses under one tag — is an index seek on
//! `(hashtag_id, status_id DESC)`.
//!
//! Unlike [`crate::models::mention`] this edge carries no position: the
//! Status entity's `tags` array orders by name, so
//! [`for_statuses`] sorts on the column and [`link_to_status`] only has to
//! keep pairs unique. [`upsert_many`] is the idempotent, case-insensitive
//! write: re-using a tag points at the row that already holds it, and names
//! that are blank or, once normalized, longer than the 100-character
//! extraction bound are ignored rather than stored.
//!
//! Nothing here extracts tags from text: [`crate::models::status::insert`]
//! upserts and links what its caller computed —
//! [`crate::util::entities::extract_hashtags`] of the raw text for a local
//! status, the `tag` list of the activity for a remote one — in the
//! status's own transaction.
//!
//! [`day_counts`] is the read behind the tag endpoints' `history`: the one
//! tag's live statuses, bucketed by UTC day. [`search_prefix`] is the read
//! behind the combined search's hashtag arm: stored names, prefix-matched.
//!
//! The code is split by the question it answers behind this file: `tags`
//! upserts the tag rows and reads them by name, `search` prefix-matches the
//! stored names, `usage` counts one tag's use per day and pages its public
//! posts, and `links` owns the `status_hashtags` edge. The two row types and
//! the name shape every arm shares, `normalize_name`, live here; each item
//! the rest of the crate uses is re-exported below, so every outside path
//! stays `crate::models::hashtag::…`.

use std::sync::LazyLock;

use chrono::{DateTime, Utc};
use regex::Regex;
use uuid::Uuid;

use crate::util::entities::MAX_HASHTAG_LEN;

/// The `status_hashtags` edge: the link writes and the batched read.
mod links;
/// The prefix search over the stored tag names.
mod search;
/// The `hashtags` rows: the idempotent upsert and the reads one name answers.
mod tags;
/// One tag's usage: the per-UTC-day buckets and its public posts page.
mod usage;

/// The fixtures the children's tests share.
#[cfg(test)]
mod test_support;

pub use links::{
    for_statuses, link_names_conn, link_to_status, replace_for_status, replace_names_conn,
};
pub use search::search_prefix;
pub use tags::{find_or_create, get_by_name, upsert_many};
pub use usage::day_counts;

/// One row of `hashtags`: one tag name, shared by every status that uses it.
#[derive(Clone, Debug, PartialEq, Eq, sqlx::FromRow)]
pub struct Hashtag {
    /// `UUIDv7` primary key, minted by [`upsert_many`] (decision 4).
    pub id: Uuid,
    /// The tag name as stored: lowercase, without the leading `#`.
    pub name: String,
    /// When this tag was first created.
    pub inserted_at: DateTime<Utc>,
}

/// The joined row of [`for_statuses`]: one tag link with the [`Hashtag`] its
/// `hashtag_id` names, flattened.
#[derive(Debug, sqlx::FromRow)]
struct StatusHashtagRow {
    /// The status this row links, from `status_hashtags.status_id`.
    status_id: Uuid,
    /// The tag, from the joined `hashtags` row.
    #[sqlx(flatten)]
    hashtag: Hashtag,
}

/// The characters a stored tag name may hold: anything alphanumeric, any
/// Unicode combining mark, `_`, the middle dot `U+00B7`, the katakana middle
/// dot `U+30FB` and the zero-width non-joiner `U+200C` (Mastodon's
/// `HASHTAG_NAME_RE` set, plus the marks its Thai range covers).
const NAME_PATTERN: &str = r"^[\p{Alphabetic}\p{N}\p{M}_\x{00B7}\x{30FB}\x{200C}]+$";

/// The compiled [`NAME_PATTERN`], or `None` if the fixed pattern ever failed
/// to compile (it cannot; every name is then refused rather than stored
/// unchecked).
static NAME_RE: LazyLock<Option<Regex>> = LazyLock::new(|| Regex::new(NAME_PATTERN).ok());

/// The normalized form of one written tag name, or `None` when it is not a
/// usable tag: trimmed, leading `#`s dropped, lowercased — and non-empty,
/// at most [`MAX_HASHTAG_LEN`] characters once normalized, made only of the
/// characters [`NAME_PATTERN`] allows, and not all digits.
///
/// This is the single authority on what may be stored in `hashtags.name`:
/// the tag endpoints ([`crate::api::tags`]) and the federation note parser
/// run text through it before storing or rendering anything. `None` is the
/// endpoints' "Invalid hashtag" case, the lowercased text the entity's name.
pub(crate) fn normalize_name(name: &str) -> Option<String> {
    let trimmed = name.trim().trim_start_matches('#').trim();
    if trimmed.is_empty() {
        return None;
    }
    let tag = trimmed.to_lowercase();
    // The length bound is on the stored tag, so it is checked after the
    // lowercase (which can change a character count at the margins).
    if tag.chars().count() > MAX_HASHTAG_LEN {
        return None;
    }
    if !NAME_RE.as_ref().is_some_and(|re| re.is_match(&tag)) {
        return None;
    }
    // A purely numeric name is no tag (the extractor refuses it too).
    if tag.chars().all(char::is_numeric) {
        return None;
    }
    Some(tag)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn names_outside_the_tag_character_set_are_refused() {
        for name in ["foo bar", "a/b", "x?y", "<b>", "a#b", "../x", "a%b", "a-b"] {
            assert_eq!(normalize_name(name), None, "`{name}` is no tag");
        }
    }

    #[test]
    fn all_numeric_names_are_refused() {
        for name in ["123", "#2024", "٣٤"] {
            assert_eq!(normalize_name(name), None, "`{name}` is all digits");
        }
        assert_eq!(normalize_name("1a"), Some("1a".to_owned()), "a digit mix");
    }

    #[test]
    fn valid_names_are_lowercased_and_unhashed() {
        assert_eq!(
            normalize_name("#Rust_Lang"),
            Some("rust_lang".to_owned()),
            "hash dropped, lowercased, underscore kept"
        );
        assert_eq!(
            normalize_name("日本語"),
            Some("日本語".to_owned()),
            "letters of any script"
        );
        for name in [
            "เก็บ",
            "e\u{301}",
            "a\u{00B7}b",
            "カ\u{30FB}カ",
            "a\u{200C}b",
        ] {
            assert_eq!(normalize_name(name), Some(name.to_owned()), "`{name}`");
        }
    }

    #[test]
    fn blank_and_over_long_names_are_refused() {
        assert_eq!(normalize_name(" # "), None, "nothing left");
        assert!(normalize_name(&"a".repeat(MAX_HASHTAG_LEN)).is_some());
        assert_eq!(normalize_name(&"a".repeat(MAX_HASHTAG_LEN + 1)), None);
    }
}
