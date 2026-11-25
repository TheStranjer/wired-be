//! Test fixtures shared by the statuses unit tests: the stamped stored
//! row the ordering, gating and silencing tests build their cases on,
//! and the small reads over it.

use chrono::{DateTime, Utc};
use uuid::Uuid;

use crate::{
    config::TEST_BASE_URL,
    models::status::{self, Visibility},
    util::time::parse_time,
};

/// The instant every fixture row in these tests is stamped with.
fn stamped() -> DateTime<Utc> {
    parse_time("2024-01-31T12:34:56.789Z").expect("the fixture timestamp parses")
}

/// A stored row whose only fact of interest is its `id`, so the
/// ordering helper is tested on the ordering and nothing else.
pub(super) fn row(id: Uuid) -> status::Status {
    status::Status {
        id,
        uri: format!("{TEST_BASE_URL}/objects/{id}"),
        url: None,
        user_id: Uuid::nil(),
        content: String::new(),
        raw_text: String::new(),
        content_type: status::DEFAULT_CONTENT_TYPE.to_owned(),
        spoiler_text: String::new(),
        sensitive: false,
        visibility: Visibility::Public,
        language: None,
        in_reply_to_id: None,
        in_reply_to_account_id: None,
        in_reply_to_ap_id: None,
        conversation_id: id,
        ap_context: None,
        direct_conversation_id: None,
        reblog_of_id: None,
        quote_of_id: None,
        quote_policy: None,
        quote_approval_policy: None,
        local: true,
        application_name: None,
        application_website: None,
        replies_count: 0,
        reblogs_count: 0,
        favourites_count: 0,
        quotes_count: 0,
        edited_at: None,
        deleted_at: None,
        expires_at: None,
        inserted_at: stamped(),
        updated_at: stamped(),
    }
}

/// The ids of `rows`, in order.
pub(super) fn ids(rows: &[status::Status]) -> Vec<Uuid> {
    rows.iter().map(|row| row.id).collect()
}

/// `row(id)` as `author` wrote it, public: the row the pin gate asks
/// about, before a test narrows it to the case it names.
pub(super) fn authored(id: Uuid, author: Uuid) -> status::Status {
    let mut own = row(id);
    own.user_id = author;
    own
}
