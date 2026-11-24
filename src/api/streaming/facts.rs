//! What one event's payload says about the entity it carries, as far as a
//! reader's [`Filters`] can read it: the accounts that wrote it, the hosts
//! those accounts live on, and the threads they belong to.
//!
//! The hub carries no authorship of its own, so all of it is parsed out of the
//! entity JSON, whose shape is the Status entity's: `account` (and a reblog's
//! own `account`) name the authors, `acct` then `url` name the host, and
//! `conversation_id` name the thread — the column this schema keys a thread on
//! and the `pleroma.conversation_id` key of Pleroma's Status entity
//! (`lib/pleroma/web/mastodon_api/views/status_view.ex`). A payload that is no
//! JSON object names nothing and suppresses nothing.
//!
//! [`Filters`]: super::filters::Filters

use serde_json::{Map, Value};
use tracing::debug;
use uuid::Uuid;

use crate::{models::domain_block, util::url::http_host};

/// What one event's payload says about the entity it carries, as far as a
/// reader's [`Filters`] can read it: the accounts that wrote it, the hosts
/// those accounts live on, and the threads they belong to.
///
/// The hub carries no authorship of its own, so all of it is parsed out of the
/// entity JSON, whose shape is the Status entity's: `account` (and a reblog's
/// own `account`) name the authors, `acct` then `url` name the host, and
/// `conversation_id` name the thread — the column this schema keys a thread on
/// and the `pleroma.conversation_id` key of Pleroma's Status entity
/// (`lib/pleroma/web/mastodon_api/views/status_view.ex`). A payload that is no
/// JSON object names nothing and suppresses nothing.
#[derive(Debug, Default)]
pub(super) struct Facts {
    /// The author account ids: the status's own first, a reblog's second.
    pub(super) authors: Vec<Uuid>,
    /// The hosts of those same accounts; empty for local authors, whose bare
    /// `acct` names no domain.
    pub(super) hosts: Vec<String>,
    /// The threads the entity belongs to: the status's and the reblog's.
    pub(super) threads: Vec<Uuid>,
}

impl Facts {
    /// The facts one frame payload states.
    pub(super) fn of(payload: Option<&str>) -> Self {
        let mut facts = Self::default();
        let Some(payload) = payload else {
            return facts;
        };
        match serde_json::from_str::<Value>(payload) {
            Ok(Value::Object(entity)) => facts.note_entity(&entity),
            Ok(_scalar_or_list) => debug!("streaming payload is no entity object"),
            Err(err) => debug!(%err, "streaming payload is no JSON"),
        }
        facts
    }

    /// Records what one status-shaped object says — its author account and the
    /// thread it belongs to — and, when it is a reblog, what the boosted
    /// status says too. Pleroma's `filtered_by_user?` asks the same pair of
    /// `item` and its `parent`.
    fn note_entity(&mut self, entity: &Map<String, Value>) {
        self.note_account(entity.get("account"));
        self.threads.extend(thread_of(entity));
        let pleroma = entity.get("pleroma").and_then(Value::as_object);
        self.threads.extend(pleroma.and_then(thread_of));
        if let Some(Value::Object(reblog)) = entity.get("reblog") {
            self.note_entity(reblog);
        }
    }

    /// Records the author and the host of one `account` object, if the value
    /// is one.
    fn note_account(&mut self, account: Option<&Value>) {
        let Some(Value::Object(account)) = account else {
            return;
        };
        if let Some(id) = account.get("id").and_then(value_uuid) {
            self.authors.push(id);
        }
        self.hosts.extend(account_host(account));
    }
}

/// The thread `entity` names: `conversation_id`, a uuid string (decision 4).
fn thread_of(entity: &Map<String, Value>) -> Option<Uuid> {
    entity.get("conversation_id").and_then(value_uuid)
}

/// The host an account object names: the domain of its `acct` handle
/// (`user@host`, decision 7's shape for a remote account) and, when the handle
/// carries no domain, the authority of its profile `url`. A remote account
/// reports its own instance either way; a local one reports this instance,
/// which a reader suppresses only by blocking it.
fn account_host(account: &Map<String, Value>) -> Option<String> {
    let handle = account
        .get("acct")
        .and_then(Value::as_str)
        .and_then(|acct| acct.rsplit_once('@').map(|(_, host)| host));
    let url = account
        .get("url")
        .and_then(Value::as_str)
        .and_then(http_host);
    domain_block::normalize_domain(handle.or(url.as_deref())?)
}

/// A uuid out of a JSON value: the hyphenated string every id in this API is
/// (decision 4).
fn value_uuid(value: &Value) -> Option<Uuid> {
    let text = value.as_str()?;
    parse_uuid(text)
}

/// A uuid out of a query parameter or a message field.
pub(super) fn parse_uuid(text: &str) -> Option<Uuid> {
    Uuid::parse_str(text).ok()
}

#[cfg(test)]
mod tests {
    use crate::api::streaming::{filters::Filters, test_support::one};

    use super::*;

    #[test]
    fn a_reblog_is_judged_by_both_authors() {
        let booster = Uuid::now_v7();
        let original = Uuid::now_v7();
        let thread = Uuid::now_v7();
        let body = format!(
            r#"{{"account":{{"id":"{booster}","acct":"booster"}},"reblog":{{"account":{{"id":"{original}","acct":"original"}},"conversation_id":"{thread}"}}}}"#
        );
        let facts = Facts::of(Some(body.as_str()));

        assert_eq!(
            facts.authors,
            vec![booster, original],
            "the boost and what it boosted are both read"
        );
        let blocking_the_original = Filters {
            blocked: one(original),
            ..Filters::default()
        };
        assert!(
            blocking_the_original.suppresses(Some(body.as_str())),
            "and blocking either one suppresses the reblog"
        );
    }

    #[test]
    fn a_delete_event_names_nobody_and_is_never_suppressed() {
        let body = format!("\"{}\"", Uuid::now_v7());
        let filters = Filters {
            muted: one(Uuid::now_v7()),
            ..Filters::default()
        };

        let facts = Facts::of(Some(body.as_str()));

        assert!(
            facts.authors.is_empty() && facts.hosts.is_empty() && facts.threads.is_empty(),
            "a payload that is no entity object states no facts"
        );
        assert!(
            !filters.suppresses(Some(body.as_str())),
            "so there is nothing to object to and the delete goes out"
        );
    }

    #[test]
    fn an_account_object_names_its_host_by_acct_then_url() {
        let remote: Value = serde_json::from_str(
            r#"{"acct":"user@Remote.Example","url":"https://other.example/@user"}"#,
        )
        .expect("test fixture JSON");
        let local: Value =
            serde_json::from_str(r#"{"acct":"user","url":"https://test.example/users/user"}"#)
                .expect("test fixture JSON");
        let by_url: Value = serde_json::from_str(r#"{"url":"https://Fallback.Example/@user"}"#)
            .expect("fixture JSON");

        assert_eq!(
            account_host(remote.as_object().expect("object")).as_deref(),
            Some("remote.example"),
            "the acct handle speaks first, lowercased"
        );
        assert_eq!(
            account_host(local.as_object().expect("object")).as_deref(),
            Some("test.example"),
            "a local account reports this instance through its profile url"
        );
        assert_eq!(
            account_host(by_url.as_object().expect("object")).as_deref(),
            Some("fallback.example"),
            "and a handle without a domain falls back to the profile url"
        );
    }

    #[test]
    fn the_thread_is_read_from_either_spelling() {
        let id = Uuid::now_v7();
        let column: Value = serde_json::from_str(&format!(r#"{{"conversation_id":"{id}"}}"#))
            .expect("fixture JSON");
        let entity: Value =
            serde_json::from_str(&format!(r#"{{"pleroma":{{"conversation_id":"{id}"}}}}"#))
                .expect("fixture JSON");
        let mut facts = Facts::default();

        facts.note_entity(column.as_object().expect("object"));
        facts.note_entity(entity.as_object().expect("object"));

        assert_eq!(
            facts.threads,
            vec![id, id],
            "the status column and the pleroma extension both name the thread"
        );
    }
}
