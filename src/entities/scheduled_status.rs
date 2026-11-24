//! The Mastodon `ScheduledStatus` entity: a create-status request parked
//! until a future moment.
//!
//! The reference shape is Pleroma's `ScheduledActivityView.render("show.json")`
//! (`lib/pleroma/web/mastodon_api/views/scheduled_activity_view.ex`): the
//! schedule's id, the moment it is due, the request params re-keyed the way
//! the API names them, and the attachments the schedule carries. Like
//! Pleroma's `status_params/1`, [`ScheduledStatusParams`] reads the
//! request-shaped keys the [`scheduled_status`](crate::models::scheduled_status)
//! model stores (`status` arrives as `text`) and echoes what it cannot fill
//! as JSON `null`: `poll` and `scheduled_at` always — the stored params
//! carry neither — and `idempotency` and `application_id` unless the stored
//! object holds those keys of its own (the endpoint stores them when the
//! request had them). A missing `media_ids` is `[]`, never `null`.
//!
//! Pure (decision 3): [`ScheduledStatus::from_row`] consumes one row, the
//! attachment rows loaded for it and the instance [`Config`] the media
//! render needs — nothing here touches the database or a request. The media
//! render through the existing
//! [`MediaAttachment`](crate::entities::media_attachment::MediaAttachment)
//! entity in the order given. The top-level `scheduled_at` is the row's own
//! due time in the Mastodon format (decision 5), distinct from the params
//! echo, which stays `null`.

use serde::Serialize;
use serde_json::Value;

use crate::{
    config::Config,
    models::{media_attachment, scheduled_status::ScheduledStatus as ModelScheduledStatus},
    util::time::mastodon_time,
};

/// The stored create-status params, rendered the way the API names them.
///
/// The request keys are re-labelled, and every key the entity cannot fill
/// is present as JSON `null` — Pleroma's
/// `ScheduledActivityView.status_params/1` behaviour.
#[derive(Clone, Debug, Serialize)]
pub struct ScheduledStatusParams {
    /// The status text: the stored `status` key, under the name the API
    /// gives it; `null` when the params carry none.
    pub text: Value,
    /// The stored `visibility` verbatim (`public`, `unlisted`, `private`,
    /// `direct`); `null` when absent.
    pub visibility: Value,
    /// Whether the post folds behind its warning; the stored `sensitive`
    /// as submitted, `null` when absent.
    pub sensitive: Value,
    /// The content warning: the stored `spoiler_text`; `null` when absent.
    pub spoiler_text: Value,
    /// The status the request answers, as the id string the endpoint
    /// stored; `null` when the post is not a reply.
    pub in_reply_to_id: Value,
    /// The attachment ids the request named, in submission order; a
    /// missing stored `media_ids` is `[]`, never `null`.
    pub media_ids: Vec<String>,
    /// The poll params; always `null` here — the stored params hold no
    /// poll part and the polls entity is a later ticket.
    pub poll: Value,
    /// The idempotency key of the request that made this schedule, echoed
    /// when the stored params carry an `idempotency` key of their own;
    /// `null` when the request sent none.
    pub idempotency: Value,
    /// The due time as the client submitted it; always `null` here — the
    /// row keeps the fire moment in its own column, never in the params,
    /// and the entity answers it as the top-level
    /// [`scheduled_at`](ScheduledStatus::scheduled_at).
    pub scheduled_at: Value,
    /// The application the posting token belongs to, echoed when the
    /// stored params carry an `application_id` key of their own; `null`
    /// when the endpoint stored none.
    pub application_id: Value,
    /// The post's language code as submitted; `null` when absent.
    pub language: Value,
    /// The submitted content type (`text/plain`, `text/markdown`, …);
    /// `null` when absent.
    pub content_type: Value,
    /// The publish-after delay in seconds, as submitted; `null` when
    /// absent.
    pub expires_in: Value,
}

impl ScheduledStatusParams {
    /// Re-keys the stored request-shaped params into the API shape.
    ///
    /// The keys the endpoint stored are read by their request names;
    /// everything else follows Pleroma's echo-as-null behaviour, with
    /// `media_ids` the one key that defaults to the empty list instead.
    fn from_stored(stored: &Value) -> Self {
        Self {
            text: stored_param(stored, "status"),
            visibility: stored_param(stored, "visibility"),
            sensitive: stored_param(stored, "sensitive"),
            spoiler_text: stored_param(stored, "spoiler_text"),
            in_reply_to_id: stored_param(stored, "in_reply_to_id"),
            media_ids: stored_media_ids(stored),
            poll: Value::Null,
            idempotency: stored_param(stored, "idempotency"),
            scheduled_at: Value::Null,
            application_id: stored_param(stored, "application_id"),
            language: stored_param(stored, "language"),
            content_type: stored_param(stored, "content_type"),
            expires_in: stored_param(stored, "expires_in"),
        }
    }
}

/// The Mastodon `ScheduledStatus` entity: one schedule as the
/// scheduled-statuses endpoints answer it.
#[derive(Debug, Serialize)]
pub struct ScheduledStatus {
    /// The row's `UUIDv7` primary key as the hyphenated lowercase string
    /// (decision 4).
    pub id: String,
    /// The moment the status is due to fire, in the Mastodon time format
    /// (decision 5): `2024-01-31T12:34:56.789Z`.
    pub scheduled_at: String,
    /// The stored request params in the API shape; see
    /// [`ScheduledStatusParams`].
    pub params: ScheduledStatusParams,
    /// The schedule's attachments rendered through the
    /// [`MediaAttachment`](crate::entities::media_attachment::MediaAttachment)
    /// entity, in the order the rows were handed in.
    pub media_attachments: Vec<crate::entities::media_attachment::MediaAttachment>,
}

impl ScheduledStatus {
    /// Renders `row` as the entity, with `media` — the attachment rows
    /// stored for the schedule — rendered through the existing
    /// [`MediaAttachment::from_model`](crate::entities::media_attachment::MediaAttachment::from_model)
    /// in the order given, and the row's params jsonb re-keyed into
    /// [`ScheduledStatusParams`] the way Pleroma's
    /// `ScheduledActivityView.status_params/1` reads it.
    #[must_use]
    pub fn from_row(
        row: &ModelScheduledStatus,
        media: &[media_attachment::MediaAttachment],
        config: &Config,
    ) -> Self {
        Self {
            id: row.id.to_string(),
            scheduled_at: mastodon_time(&row.scheduled_at),
            params: ScheduledStatusParams::from_stored(&row.params),
            media_attachments: media
                .iter()
                .map(|attachment| {
                    crate::entities::media_attachment::MediaAttachment::from_model(
                        attachment, config,
                    )
                })
                .collect(),
        }
    }
}

/// The stored params' value under `key`, or JSON `null` when the object
/// does not name it: Pleroma's `params[key]` read, which is `nil` for a
/// missing key and renders as `null`.
fn stored_param(stored: &Value, key: &str) -> Value {
    stored.get(key).cloned().unwrap_or_default()
}

/// The stored `media_ids` as id strings, keeping submission order. A
/// value that is not an array — a missing key included — is the empty
/// list, never `null`.
fn stored_media_ids(stored: &Value) -> Vec<String> {
    stored
        .get("media_ids")
        .and_then(Value::as_array)
        .map_or_default(|ids| ids.iter().map(media_id_text).collect())
}

/// One stored `media_ids` element as text: a string passes through
/// verbatim, anything else keeps its JSON text. The endpoint stores id
/// strings, so the fallback only speaks for a hand-written row.
fn media_id_text(id: &Value) -> String {
    id.as_str().map_or_else(|| id.to_string(), str::to_owned)
}

#[cfg(test)]
mod tests {
    use chrono::{DateTime, Utc};
    use serde_json::json;
    use uuid::Uuid;

    use super::*;
    use crate::util::time::parse_time;

    /// The hyphenated lowercase id every fixture row carries.
    const ROW_ID: &str = "01934567-89ab-7cde-f012-3456789abcde";

    /// The params keys in the order the ticket lists them, which is the
    /// order the serializer must emit them.
    const PARAM_KEY_ORDER: [&str; 13] = [
        "text",
        "visibility",
        "sensitive",
        "spoiler_text",
        "in_reply_to_id",
        "media_ids",
        "poll",
        "idempotency",
        "scheduled_at",
        "application_id",
        "language",
        "content_type",
        "expires_in",
    ];

    /// The entity's own keys, in the order the ticket lists them.
    const ENTITY_KEY_ORDER: [&str; 4] = ["id", "scheduled_at", "params", "media_attachments"];

    /// The due instant of every fixture row: 2026-05-18T09:00:00Z, whole
    /// seconds, so the render must gain the `.000Z` millisecond form.
    fn due() -> DateTime<Utc> {
        parse_time("2026-05-18T09:00:00Z").expect("the fixture timestamp parses")
    }

    /// A `scheduled_statuses` row parking `params` until [`due`].
    fn model_row(params: Value) -> ModelScheduledStatus {
        ModelScheduledStatus {
            id: Uuid::parse_str(ROW_ID).expect("the fixture id parses"),
            user_id: Uuid::now_v7(),
            scheduled_at: due(),
            params,
            inserted_at: due(),
            updated_at: due(),
            idempotency_key: None,
        }
    }

    /// A schedule with `params` and no attachments.
    fn render(params: Value) -> ScheduledStatus {
        ScheduledStatus::from_row(&model_row(params), &[], &Config::for_tests())
    }

    /// A stored attachment row for `id`, bare enough to render.
    fn attachment_row(id: Uuid) -> media_attachment::MediaAttachment {
        media_attachment::MediaAttachment {
            id,
            user_id: Some(Uuid::now_v7()),
            status_id: None,
            position: 0,
            storage_key: Some("media/x.png".to_owned()),
            remote_url: None,
            url: "https://test.example/media/x.png".to_owned(),
            preview_url: None,
            content_type: "image/png".to_owned(),
            description: None,
            blurhash: None,
            width: None,
            height: None,
            preview_width: None,
            preview_height: None,
            duration: None,
            focus_x: None,
            focus_y: None,
            file_size: None,
            sensitive_hint: false,
            inserted_at: due(),
            updated_at: due(),
        }
    }

    /// Asserts the serializer emits `keys` in exactly this order. A
    /// `Value` round trip would sort them (`serde_json` is compiled
    /// without `preserve_order`), so this inspects the direct struct-to-
    /// text render: each key's first occurrence sits after the previous
    /// key's. The key set itself is pinned by exact-object assertions.
    fn assert_key_order<T: Serialize + ?Sized>(value: &T, keys: &[&str]) {
        let text = serde_json::to_string(value).expect("the entity serializes");
        let mut previous = 0;
        for key in keys {
            let needle = format!("\"{key}\":");
            let position = text
                .find(&needle)
                .unwrap_or_else(|| panic!("`{key}` should be emitted in {text}"));
            assert!(
                position > previous,
                "`{key}` follows the earlier keys in declaration order: {text}"
            );
            previous = position;
        }
    }

    #[test]
    fn fully_populated_params_render_the_exact_entity() {
        let params = json!({
            "status": "hello tomorrow",
            "visibility": "unlisted",
            "sensitive": true,
            "spoiler_text": "spoilers ahead",
            "in_reply_to_id": "01934567-89ab-7000-8000-000000000001",
            "media_ids": [
                "01934567-89ab-7000-8000-00000000000a",
                "01934567-89ab-7000-8000-00000000000b",
            ],
            "language": "en",
            "content_type": "text/markdown",
            "expires_in": 3600,
        });
        let entity = render(params);
        let value = serde_json::to_value(&entity).expect("the entity serializes");
        assert_eq!(
            value,
            json!({
                "id": ROW_ID,
                "scheduled_at": "2026-05-18T09:00:00.000Z",
                "params": {
                    "text": "hello tomorrow",
                    "visibility": "unlisted",
                    "sensitive": true,
                    "spoiler_text": "spoilers ahead",
                    "in_reply_to_id": "01934567-89ab-7000-8000-000000000001",
                    "media_ids": [
                        "01934567-89ab-7000-8000-00000000000a",
                        "01934567-89ab-7000-8000-00000000000b",
                    ],
                    "poll": null,
                    "idempotency": null,
                    "scheduled_at": null,
                    "application_id": null,
                    "language": "en",
                    "content_type": "text/markdown",
                    "expires_in": 3600,
                },
                "media_attachments": [],
            }),
            "the whole entity, key for key"
        );
        assert_eq!(value["id"], ROW_ID, "the uuid as hyphenated text");
        assert_eq!(
            value["params"]["text"], "hello tomorrow",
            "`status` arrives re-named as `text`"
        );
        assert_eq!(
            value["params"]["sensitive"], true,
            "a stored boolean passes through as one"
        );
        assert_eq!(
            value["params"]["expires_in"], 3600,
            "a stored number passes through as one"
        );
        assert_eq!(
            value["params"]["media_ids"],
            json!([
                "01934567-89ab-7000-8000-00000000000a",
                "01934567-89ab-7000-8000-00000000000b",
            ]),
            "the id strings keep their order"
        );
        assert_eq!(
            value["params"]["poll"],
            Value::Null,
            "polls are a later ticket"
        );
        assert_eq!(
            value["params"]["scheduled_at"],
            Value::Null,
            "the due time lives in the row column, not the params"
        );
        assert_key_order(&entity, &ENTITY_KEY_ORDER);
        assert_key_order(&entity.params, &PARAM_KEY_ORDER);
    }

    #[test]
    fn empty_params_echo_their_nulls_and_an_empty_media_ids() {
        let entity = render(json!({}));
        let value = serde_json::to_value(&entity).expect("the entity serializes");
        assert_eq!(
            value["params"],
            json!({
                "text": null,
                "visibility": null,
                "sensitive": null,
                "spoiler_text": null,
                "in_reply_to_id": null,
                "media_ids": [],
                "poll": null,
                "idempotency": null,
                "scheduled_at": null,
                "application_id": null,
                "language": null,
                "content_type": null,
                "expires_in": null,
            }),
            "every key is present, the unfilled ones are null, media_ids is []"
        );
        assert_eq!(
            value["media_attachments"],
            json!([]),
            "a schedule with no media answers the empty list"
        );
    }

    #[test]
    fn the_due_time_renders_in_the_mastodon_millisecond_format() {
        let value =
            serde_json::to_value(render(json!({"status": "soon"}))).expect("the entity serializes");
        assert_eq!(
            value["scheduled_at"], "2026-05-18T09:00:00.000Z",
            "a whole second gains the .000Z millisecond form (decision 5)"
        );
    }

    #[test]
    fn stored_idempotency_and_application_id_are_echoed() {
        let entity = render(json!({
            "status": "once more",
            "idempotency": "req-42",
            "application_id": "01934567-89ab-7000-8000-00000000000c",
            "media_ids": ["01934567-89ab-7000-8000-00000000000a", 42],
        }));
        let value = serde_json::to_value(&entity).expect("the entity serializes");
        assert_eq!(
            value["params"]["idempotency"], "req-42",
            "the key the request sent rides through"
        );
        assert_eq!(
            value["params"]["application_id"], "01934567-89ab-7000-8000-00000000000c",
            "and so does the application the token belongs to"
        );
        assert_eq!(
            value["params"]["media_ids"],
            json!(["01934567-89ab-7000-8000-00000000000a", "42"]),
            "a non-string id keeps its JSON text"
        );
    }

    #[test]
    fn attachments_render_as_entities_in_the_order_given() {
        let first =
            Uuid::parse_str("01934567-89ab-7000-8000-000000000001").expect("the fixture id parses");
        let second =
            Uuid::parse_str("01934567-89ab-7000-8000-000000000002").expect("the fixture id parses");
        let row = model_row(json!({"status": "with pictures"}));
        let media = vec![attachment_row(first), attachment_row(second)];
        let entity = ScheduledStatus::from_row(&row, &media, &Config::for_tests());
        let value = serde_json::to_value(&entity).expect("the entity serializes");
        let attachments = value["media_attachments"]
            .as_array()
            .expect("media_attachments is an array");
        assert_eq!(attachments.len(), 2, "one entry per stored row");
        assert_eq!(
            attachments[0]["id"],
            first.to_string(),
            "the rows render in the order handed in"
        );
        assert_eq!(attachments[1]["id"], second.to_string(), "and not sorted");
        assert_eq!(
            attachments[0]["type"], "image",
            "each entry is the full Attachment entity"
        );
        assert_eq!(
            attachments[0]["url"], "https://test.example/media/x.png",
            "on the test instance's URLs"
        );
    }
}
