//! What a reader does not want to see, read once when the socket opens and
//! refreshed every [`FILTER_TTL`]: the suppression sets of [`Filters`], the
//! fifth of them the reader's active irreversible word filters, and the two
//! helpers that read an entity's text and JSON for them.
//!
//! Pleroma recomputes the same sets per event in `Streamer.filtered_by_user?`
//! (`lib/pleroma/web/streamer.ex`) against its cache; one set per connection
//! is the same answer for a fraction of the queries, and a change lands on a
//! live socket one refresh later at the latest.
//!
//! [`FILTER_TTL`]: super::FILTER_TTL

use std::collections::HashSet;

use serde::Serialize;
use serde_json::Value;
use sqlx::PgPool;
use tracing::{debug, warn};
use uuid::Uuid;

use crate::{
    error::AppResult,
    models::{block, conversation_mute, domain_block, filter, mute},
};

use super::facts::Facts;

/// What a reader does not want to see, read once when the socket opens and
/// refreshed every [`FILTER_TTL`]: the accounts it blocked and muted, the
/// domains it blocked, the threads it muted, and its active irreversible
/// word filters in the streaming contexts.
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
    /// The reader's active, irreversible word filters in the streaming
    /// contexts ([`STREAMING_CONTEXTS`]), verbatim [`filter::Filter`] rows
    /// for [`filter::matches`] to run.
    pub(super) words: Vec<filter::Filter>,
}

/// The filter contexts streaming answers: `home` for the reader's own,
/// direct and list timelines, `notifications` for the notification stream
/// and `public` for the public, media, remote and hashtag timelines. A
/// filter active in any one of them judges every entity a socket renders;
/// the `thread` and `account` contexts belong to the HTTP reads that name
/// them, not to a stream.
const STREAMING_CONTEXTS: [&str; 3] = ["home", "notifications", "public"];

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
        let words = word_filters(pool, reader).await?;
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
    /// object to. The payload judged is the one [`frame_for`] resolved, so
    /// a render-hinted event is weighed by its rendered entity too.
    ///
    /// [`frame_for`]: super::connection::Connection::frame_for
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

    /// Whether the text a rendered entity carries matches one of the
    /// reader's active irreversible word filters — the streaming drop for
    /// a phrase the reader refuses to see even once. Only rendered
    /// entities are weighed: a pre-serialized payload event stays with
    /// [`Filters::suppresses`], the suppression it has always had.
    pub(super) fn suppresses_words(&self, payload: &str) -> bool {
        if self.words.is_empty() {
            return false;
        }
        let texts = entity_texts(payload);
        texts
            .iter()
            .any(|text| self.words.iter().any(|word| filter::matches(word, text)))
    }
}

/// The reader's active word filters that apply to streaming: the
/// [`filter::list_active`] rows that are irreversible (a warn filter only
/// tells the client to cover the post, it never drops it) and name at
/// least one of the [`STREAMING_CONTEXTS`].
async fn word_filters(pool: &PgPool, reader: Uuid) -> AppResult<Vec<filter::Filter>> {
    let active = filter::list_active(pool, reader).await?;
    Ok(active
        .into_iter()
        .filter(|row| {
            row.irreversible
                && row
                    .context
                    .iter()
                    .any(|context| STREAMING_CONTEXTS.contains(&context.as_str()))
        })
        .collect())
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

/// The filterable text a rendered entity payload states: every
/// `content` and `spoiler_text` string it carries, nested entities
/// included — a reblog's original, a notification's status, a chat's last
/// message. A payload that is no JSON object, or holds neither key,
/// states no text and matches nothing.
fn entity_texts(payload: &str) -> Vec<String> {
    let Ok(value) = serde_json::from_str::<Value>(payload) else {
        debug!("streaming render payload is no JSON");
        return Vec::new();
    };
    let mut texts = Vec::new();
    collect_texts(&value, &mut texts);
    texts
}

/// Records the `content` / `spoiler_text` strings of `value`, walking
/// every nested object and array so one pass reaches the whole entity.
fn collect_texts(value: &Value, texts: &mut Vec<String>) {
    match value {
        Value::Object(map) => {
            for key in ["content", "spoiler_text"] {
                if let Some(text) = map.get(key).and_then(Value::as_str) {
                    texts.push(text.to_owned());
                }
            }
            for nested in map.values() {
                collect_texts(nested, texts);
            }
        }
        Value::Array(items) => {
            for item in items {
                collect_texts(item, texts);
            }
        }
        Value::Null | Value::Bool(_) | Value::Number(_) | Value::String(_) => {}
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

    /// The fifth read keeps only what streaming hides outright: an
    /// irreversible filter of a streaming context. A warn filter stays out
    /// (it covers a post, it never drops one) and so does an irreversible
    /// filter of a non-streaming context.
    #[tokio::test]
    async fn the_word_filter_read_keeps_irreversible_streaming_rows() {
        let (_db, state) = state().await;
        let reader = account(&state, "wordfilter-loader").await;
        seed_word_filter(&state, &reader, "spoilers", "home", true).await;
        seed_word_filter(&state, &reader, "maybe", "public", false).await;
        seed_word_filter(&state, &reader, "profile-only", "account", true).await;
        let _expired = filter::create(
            &state.db,
            reader.id,
            &filter::NewFilter {
                phrase: "past-due".to_owned(),
                context: vec!["home".to_owned()],
                irreversible: true,
                expires_at: Some(chrono::Utc::now() - chrono::Duration::days(1)),
                ..filter::NewFilter::default()
            },
        )
        .await
        .expect("the expired filter fixture creates");

        let filters = Filters::load(&state.db, Some(reader.id))
            .await
            .expect("the five reads run");

        assert_eq!(
            filters
                .words
                .iter()
                .map(|row| row.phrase.as_str())
                .collect::<Vec<&str>>(),
            ["spoilers"],
            "the irreversible streaming row, newest-first as [`filter::list_active`] reads it"
        );
        let anonymous = Filters::load(&state.db, None)
            .await
            .expect("the anonymous load runs no read");
        assert!(
            anonymous.words.is_empty(),
            "word filters are a reader's own, and the anonymous reader has none"
        );
    }

    /// The word filter reads the whole rendered entity, not just its top
    /// level: the status's own content and spoiler, a reblog's original,
    /// a notification's status and a chat's last message all state text.
    #[test]
    fn the_filterable_text_reaches_every_nested_entity() {
        let payload = r#"{"id":"1","content":"outer","spoiler_text":"subject",
             "reblog":{"content":"inner"},
             "pleroma":{"content":{"text/plain":"raw"},"conversation_id":"c"},
             "status":{"spoiler_text":"nested subject"},
             "last_message":{"content":"chat text"},
             "media_attachments":[{"description":"not filterable"}]}"#;

        let mut texts = entity_texts(payload);
        texts.sort();

        assert_eq!(
            texts,
            ["chat text", "inner", "nested subject", "outer", "subject"],
            "content and spoiler_text at every depth, and nothing else"
        );
    }

    #[test]
    fn a_payload_with_no_entity_states_no_text() {
        assert!(
            entity_texts("\"just-a-status-id\"").is_empty(),
            "a delete's id payload matches no phrase"
        );
        assert!(
            entity_texts("not json at all").is_empty(),
            "and neither does a body that is no JSON"
        );
    }
}
