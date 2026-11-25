//! The Mastodon v1 `Filter` entity; the v2 entities live in [`v2`].
//!
//! The reference shape is Pleroma's `FilterView.render("show.json", …)`
//! (`lib/pleroma/web/mastodon_api/views/filter_view.ex`): `id`, `phrase`,
//! `context`, `expires_at`, `irreversible` and `whole_word`, in that order.
//! Two keys read differently here than there. `id` is Pleroma's per-user
//! integer counter; per `docs/ARCHITECTURE.md` decision 4 this entity
//! carries the row's `UUIDv7` as lowercase hyphenated text instead.
//! `irreversible` is the filter's `hide` action: the API has always
//! called it `irreversible`.
//!
//! As in Mastodon, a v1 filter is one *keyword* of a v2 filter, so
//! [`Filter::from_phrase`] renders a [`Phrase`] — the keyword with its
//! parent's contexts, action and expiry — and nothing else is read: the
//! owner, the row's timestamps and the expiry rule live behind it.
//! `expires_at` is `null` for a filter that never expires, in the Mastodon
//! time format otherwise (decision 5).

use chrono::{DateTime, Utc};
use serde::Serialize;

use crate::models::filter::phrase::Phrase;

pub mod v2;

pub use self::v2::FilterResult;

/// The Mastodon v1 `Filter` entity.
///
/// Build one with [`Filter::from_phrase`]; its [`Serialize`] impl emits `id`,
/// `phrase`, `context`, `expires_at`, `irreversible` and `whole_word`.
#[derive(Clone, Debug, Serialize)]
pub struct Filter {
    /// The keyword id as the hyphenated lowercase `UUIDv7` string
    /// (decision 4).
    pub id: String,
    /// The phrase to look for, verbatim as its owner wrote it.
    pub phrase: String,
    /// The contexts the filter applies in, as stored.
    pub context: Vec<String>,
    /// When the filter stops being active, in the Mastodon time format
    /// (decision 5); `null` when it never does.
    #[serde(with = "crate::util::time::serde_mastodon_time::option")]
    pub expires_at: Option<DateTime<Utc>>,
    /// Whether clients hide matching posts instead of warning.
    pub irreversible: bool,
    /// Whether the phrase must appear as a whole word.
    pub whole_word: bool,
}

impl Filter {
    /// Renders `phrase` as the entity.
    #[must_use]
    pub fn from_phrase(phrase: &Phrase) -> Self {
        Self {
            id: phrase.keyword.id.to_string(),
            phrase: phrase.keyword.phrase.clone(),
            context: phrase.context.clone(),
            expires_at: phrase.expires_at,
            irreversible: phrase.irreversible(),
            whole_word: phrase.keyword.whole_word,
        }
    }
}

#[cfg(test)]
mod tests;
