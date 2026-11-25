//! What a reader does not want to see, read once when the socket opens and
//! refreshed every [`FILTER_TTL`]: the suppression sets of [`Filters`], the
//! fifth of them the reader's active word filters, and the helper that
//! serializes a rendered entity. The phrase matching itself is
//! [`services::filter`](crate::services::filter)'s, run on each rendered
//! entity by the delivery step.
//!
//! Pleroma recomputes the same sets per event in `Streamer.filtered_by_user?`
//! (`lib/pleroma/web/streamer.ex`) against its cache; one set per connection
//! is the same answer for a fraction of the queries, and a change lands on a
//! live socket one refresh later at the latest.
//!
//! [`FILTER_TTL`]: super::FILTER_TTL

use std::collections::HashSet;

use serde::Serialize;
use sqlx::PgPool;
use tracing::warn;
use uuid::Uuid;

use crate::{
    error::AppResult,
    models::{block, conversation_mute, domain_block, filter, mute},
};

use super::facts::Facts;

/// What a reader does not want to see, read once when the socket opens and
/// refreshed every [`FILTER_TTL`]: the accounts it blocked and muted, the
/// domains it blocked, the threads it muted, and its active word filters.
///
/// Pleroma recomputes the same sets per event in `Streamer.filtered_by_user?`
/// (`lib/pleroma/web/streamer.ex`) against its cache; one set per connection
/// is the same answer for a fraction of the queries, and a change lands on a
/// live socket one refresh later at the latest.
#[derive(Debug, Default)]
pub(super) struct Filters {
    /// Accounts the reader blocked.
    pub(super) blocked: HashSet<Uuid>,
    /// Accounts the reader muted.
    pub(super) muted: HashSet<Uuid>,
    /// Hosts the reader blocked, in [`domain_block::normalize_domain`]'s
    /// canonical spelling.
    pub(super) domains: HashSet<String>,
    /// Threads the reader muted (`conversation_mutes`).
    pub(super) threads: HashSet<Uuid>,
    /// The reader's active word filters, every context and both actions,
    /// verbatim [`filter::Filter`] rows for
    /// [`apply_to_entity`](crate::services::filter::apply_to_entity) to
    /// scope to the stream an entity is delivered on.
    pub(super) words: Vec<filter::Filter>,
}

impl Filters {
    /// The suppression sets of `reader`, read in five queries. An anonymous
    /// reader suppresses nothing and asks the database nothing.
    ///
    /// # Errors
    ///
    /// Whatever any of the five reads fails on.
    pub(super) async fn load(pool: &PgPool, reader: Option<Uuid>) -> AppResult<Self> {
        let Some(reader) = reader else {
            return Ok(Self::default());
        };
        let blocked = block::blocked_ids(pool, reader).await?;
        let muted = mute::muted_ids(pool, reader).await?;
        let domains = domain_block::blocked_domains(pool, reader).await?;
        let threads = conversation_mute::muted_ids(pool, reader).await?;
        let words = filter::list_active(pool, reader).await?;
        Ok(Self {
            blocked: blocked.into_iter().collect(),
            muted: muted.into_iter().collect(),
            domains: domains.into_iter().collect(),
            threads: threads.into_iter().collect(),
            words,
        })
    }

    /// Whether the reader has no relationship suppression at all, so no
    /// payload needs reading for [`Filters::suppresses`]. The word filters
    /// are a separate gate with a guard of their own.
    fn is_empty(&self) -> bool {
        self.blocked.is_empty()
            && self.muted.is_empty()
            && self.domains.is_empty()
            && self.threads.is_empty()
    }

    /// Whether the payload one frame would carry is something this reader
    /// suppresses: an entity authored by a blocked or muted account, posted
    /// from a blocked domain, or part of a muted thread. A payload that
    /// names none of those — a `delete`, whose payload is only a status id,
    /// or a body-less event — is never suppressed: there is nothing to
    /// object to. The payload judged is the one [`frames_for`] resolved, so
    /// a render-hinted event is weighed by its rendered entity too.
    ///
    /// [`frames_for`]: super::connection::Connection::frames_for
    pub(super) fn suppresses(&self, payload: Option<&str>) -> bool {
        if self.is_empty() {
            return false;
        }
        let facts = Facts::of(payload);
        self.suppressed_author(&facts)
            || facts.hosts.iter().any(|host| self.domains.contains(host))
            || facts
                .threads
                .iter()
                .any(|thread| self.threads.contains(thread))
    }

    /// Whether any author of the entity is blocked or muted. A reblog is
    /// judged by both its own account and the one it boosted, which is what
    /// Pleroma's `filtered_by_user?` asks of `item` and its `parent`.
    fn suppressed_author(&self, facts: &Facts) -> bool {
        facts
            .authors
            .iter()
            .any(|author| self.blocked.contains(author) || self.muted.contains(author))
    }
}

/// The serialized entity JSON of a rendered hint, `None` with a log when
/// it would not serialize.
pub(super) fn entity_json<T: Serialize>(entity: &T, id: Uuid) -> Option<String> {
    match serde_json::to_string(entity) {
        Ok(text) => Some(text),
        Err(err) => {
            warn!(%err, %id, "a streamed entity could not be serialized");
            None
        }
    }
}

#[cfg(test)]
mod tests {
    use crate::api::streaming::test_support::{
        account, block_author, mute_author, one, seed_word_filter, state, status_payload,
    };

    use super::*;

    #[tokio::test]
    async fn the_suppression_sets_are_read_from_the_five_tables() {
        let (_db, state) = state().await;
        let reader = account(&state, "filter-reader").await;
        let blocked = account(&state, "filter-blocked").await;
        let muted = account(&state, "filter-muted").await;
        let thread = Uuid::now_v7();
        block_author(&state, &reader, &blocked).await;
        mute_author(&state, &reader, &muted).await;
        domain_block::block_domain(&state.db, reader.id, "spam.example")
            .await
            .expect("the domain block fixture writes");
        conversation_mute::mute(&state.db, reader.id, thread)
            .await
            .expect("the thread mute fixture writes");

        let filters = Filters::load(&state.db, Some(reader.id))
            .await
            .expect("the five reads run");

        assert_eq!(
            filters.blocked,
            one(blocked.id),
            "the blocked account is in"
        );
        assert!(filters.muted.contains(&muted.id), "so is the muted one");
        assert_eq!(
            filters.domains,
            one("spam.example".to_owned()),
            "and the blocked domain"
        );
        assert!(filters.threads.contains(&thread), "and the muted thread");

        let body = status_payload(muted.id, "muted@spam.example", thread);
        assert!(
            filters.suppresses(Some(body.as_str())),
            "one entity can be suppressed on three counts at once"
        );
        assert!(
            Filters::load(&state.db, None).await.unwrap().is_empty(),
            "and an anonymous reader suppresses nothing"
        );
    }

    /// The fifth read keeps every active filter, whatever its action or
    /// contexts: scoping to the stream is [`apply_to_entity`]'s job. Only
    /// an expired row stays out.
    ///
    /// [`apply_to_entity`]: crate::services::filter::apply_to_entity
    #[tokio::test]
    async fn the_word_filter_read_keeps_every_active_row() {
        let (_db, state) = state().await;
        let reader = account(&state, "wordfilter-loader").await;
        seed_word_filter(&state, &reader, "spoilers", "home", true).await;
        seed_word_filter(&state, &reader, "maybe", "public", false).await;
        seed_word_filter(&state, &reader, "profile-only", "account", true).await;
        let _expired = filter::phrase::create_phrase(
            &state.db,
            reader.id,
            &filter::phrase::NewPhrase {
                phrase: "past-due".to_owned(),
                context: vec!["home".to_owned()],
                irreversible: true,
                expires_at: Some(chrono::Utc::now() - chrono::Duration::days(1)),
                ..filter::phrase::NewPhrase::default()
            },
        )
        .await
        .expect("the expired filter fixture creates");

        let filters = Filters::load(&state.db, Some(reader.id))
            .await
            .expect("the five reads run");

        let mut phrases = filters
            .words
            .iter()
            .map(|row| row.title.as_str())
            .collect::<Vec<&str>>();
        phrases.sort_unstable();
        assert_eq!(
            phrases,
            ["maybe", "profile-only", "spoilers"],
            "every active row, warn or irreversible, in any context; the expired one is out"
        );
        let anonymous = Filters::load(&state.db, None)
            .await
            .expect("the anonymous load runs no read");
        assert!(
            anonymous.words.is_empty(),
            "word filters are a reader's own, and the anonymous reader has none"
        );
    }

    #[test]
    fn an_entity_serializes_to_its_json() {
        let json = entity_json(&serde_json::json!({"id": "1"}), Uuid::now_v7());
        assert_eq!(
            json.as_deref(),
            Some(r#"{"id":"1"}"#),
            "the entity's own JSON, compact"
        );
    }
}
