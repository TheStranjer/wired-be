//! The Mastodon `Translation` entity: what a status looks like after a
//! provider has translated it.
//!
//! The reference shape is Pleroma's `StatusView.render("translation.json", …)`
//! (`lib/pleroma/web/mastodon_api/views/status_view.ex`), which answers
//! `content`, `detected_source_language` and `provider`, widened to Mastodon's
//! own entity by the three keys a client reads for the parts of a post a text
//! translator cannot carry: `spoiler_text`, `poll` and `media_attachments`.
//! wired-be translates one thing, the status's HTML `content`, so this answer
//! carries the content warning as its author wrote it and an empty poll and
//! attachment list — Pleroma's renderer omits those three keys altogether and
//! Mastodon's fills them with translated parts; the ticket's document is the
//! middle path, Mastodon's keys with the one piece this instance translates.
//!
//! Like every entity (`docs/ARCHITECTURE.md` decision 3) this struct is pure
//! data: [`Translation::for_status`] wraps one provider answer beside the row
//! it translated, and nothing here touches the database or the network — the
//! provider call is [`crate::services::translate`].

use serde::Serialize;
use serde_json::Value;

use crate::{models::status::Status as StatusRow, services::translate::Translation as Provider};

/// The answer of `POST /api/v1/statuses/{id}/translate`.
#[derive(Clone, Debug, Serialize)]
pub struct Translation {
    /// The translated status content: sanitized HTML, from the provider the
    /// instance has configured.
    pub content: String,
    /// The status's content warning, as its author wrote it: the spoiler is
    /// not part of the translated text.
    pub spoiler_text: String,
    /// A translated poll; wired-be translates content only, so JSON `null`.
    pub poll: Value,
    /// Media attachments with translated descriptions; empty for the same
    /// reason.
    pub media_attachments: Vec<Value>,
    /// The language the provider says the original was written in, when it
    /// said: `None` for a status with no language and a provider that
    /// detected none either.
    pub detected_source_language: Option<String>,
    /// Which provider answered, e.g. `LibreTranslate`.
    pub provider: &'static str,
}

impl Translation {
    /// The entity of `translated`, the provider's answer for the status
    /// `row`: its content and detected language, the row's own content
    /// warning, and nothing for the parts a content translator does not
    /// touch.
    #[must_use]
    pub fn for_status(row: &StatusRow, translated: Provider) -> Self {
        Self {
            content: translated.content,
            spoiler_text: row.spoiler_text.clone(),
            poll: Value::Null,
            media_attachments: Vec::new(),
            detected_source_language: translated.detected_source_language,
            provider: translated.provider,
        }
    }
}

#[cfg(test)]
mod tests {
    use chrono::{DateTime, Utc};
    use serde_json::json;
    use uuid::Uuid;

    use super::*;
    use crate::{
        config::TEST_BASE_URL,
        models::status::{self, Visibility},
        util::time::parse_time,
    };

    /// The instant every field of these fixtures is stamped with.
    fn stamped() -> DateTime<Utc> {
        parse_time("2024-01-31T12:34:56.789Z").expect("the fixture timestamp parses")
    }

    /// A stored row carrying `content` and `spoiler_text`, nothing else of
    /// interest.
    fn row(content: &str, spoiler_text: &str) -> StatusRow {
        let id = Uuid::now_v7();
        StatusRow {
            id,
            uri: format!("{TEST_BASE_URL}/objects/{id}"),
            url: None,
            user_id: Uuid::nil(),
            content: content.to_owned(),
            raw_text: String::new(),
            content_type: status::DEFAULT_CONTENT_TYPE.to_owned(),
            spoiler_text: spoiler_text.to_owned(),
            sensitive: false,
            visibility: Visibility::Public,
            language: Some("en".to_owned()),
            in_reply_to_id: None,
            in_reply_to_account_id: None,
            in_reply_to_ap_id: None,
            conversation_id: id,
            reblog_of_id: None,
            quote_of_id: None,
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

    /// One provider answer to wrap.
    fn provider(content: &str, detected: Option<&str>) -> Provider {
        Provider {
            content: content.to_owned(),
            detected_source_language: detected.map(str::to_owned),
            provider: "LibreTranslate",
        }
    }

    #[test]
    fn the_answer_is_the_provider_text_beside_the_row_s_poiler() {
        let entity = Translation::for_status(
            &row("<p>Hello</p>", "spoilers"),
            provider("<p>Hallo</p>", Some("en")),
        );
        assert_eq!(
            serde_json::to_value(&entity).unwrap(),
            json!({
                "content": "<p>Hallo</p>",
                "spoiler_text": "spoilers",
                "poll": null,
                "media_attachments": [],
                "detected_source_language": "en",
                "provider": "LibreTranslate",
            }),
            "the ticket's key set, with the untranslated warning of the row"
        );
    }

    #[test]
    fn a_provider_that_detected_nothing_answers_null() {
        let entity =
            Translation::for_status(&row("<p>Hello</p>", ""), provider("<p>Hallo</p>", None));
        let value = serde_json::to_value(&entity).unwrap();
        assert_eq!(
            value["detected_source_language"],
            Value::Null,
            "no detection"
        );
        assert_eq!(value["spoiler_text"], "", "a post without a warning");
    }
}
