//! The [`CreateStatusParams`] body of `POST /api/v1/statuses` as the client
//! wrote it.
//!
//! Every submitted parameter with its serde aliases and [`flex`]
//! deserializers, the nested [`PollRequest`] of the `poll[…]` keys, and the
//! [`CheckedPoll`] [`check_body`] accepted. The [`Serialize`] half is the
//! schedule's: [`crate::services::schedule_status`] parks this shape and
//! the publish replays it.
//!
//! [`check_body`]: crate::services::create_status::check_body
//! [`flex`]: crate::extract::flex

use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};

/// The body of `POST /api/v1/statuses`, as the client wrote it.
///
/// Every field is an `Option` with a serde `default`, so a key the body omits
/// or sends as `null` stays this module's decision instead of becoming a
/// deserialization error. `poll` joined the struct with the polls ticket:
/// [`crate::services::create_status::check_body`] runs Pleroma's poll rules over it and both create arms
/// store the accepted question beside the status row (`media_ids` beside a
/// poll is the 422 `Polls cannot have attachments`; the ids themselves mean
/// the post's attachments — see the struct's own `media_ids` field).
/// `scheduled_at` joined the struct with the scheduling ticket: a body
/// that sends it is not a create at all — see that field below. The
/// `sensitive` field is a [`flex`] boolean, so a form body's
/// `sensitive=on` spells what a JSON body's `true` spells.
///
/// The [`Serialize`] half is the schedule's: [`crate::services::schedule_status`]
/// stores the normalised params as the pending row's jsonb, and the
/// [`scheduled_statuses.publish`](crate::jobs::scheduled_statuses::KIND_PUBLISH)
/// job reads them back through this same shape.
///
/// [`flex`]: crate::extract::flex
#[derive(Debug, Default, Deserialize, Serialize)]
pub struct CreateStatusParams {
    /// The post text; trimmed, and required to leave something behind.
    #[serde(default)]
    pub status: Option<String>,
    /// The content warning shown in place of the post; at most
    /// `MAX_SPOILER_CHARS` characters, and the empty string when absent.
    #[serde(default)]
    pub spoiler_text: Option<String>,
    /// Whether the post starts folded; the author's own default when the body
    /// does not say.
    #[serde(default, deserialize_with = "crate::extract::flex::opt_bool")]
    pub sensitive: Option<bool>,
    /// The requested audience, one of the five [`crate::models::status::Visibility`] names; the
    /// author's default scope when absent.
    #[serde(default)]
    pub visibility: Option<String>,
    /// The content language tag; at most `MAX_LANGUAGE_CHARS` characters,
    /// and no tag at all when absent or empty.
    #[serde(default)]
    pub language: Option<String>,
    /// The MIME type of the `status` text; [`crate::models::status::DEFAULT_CONTENT_TYPE`] when
    /// absent.
    #[serde(default)]
    pub content_type: Option<String>,
    /// The status to quote, named by id; absent or `null` posts no quote.
    /// Pleroma's older `quoted_status_id` spelling is accepted as an alias,
    /// in JSON and form bodies alike through the [`ApiBody`] extractor
    /// (decision 10). The service's private `quote_target` step checks the
    /// target and hands the original's id to the stored row's `quote_of_id`.
    ///
    /// [`ApiBody`]: crate::extract::ApiBody
    #[serde(default, alias = "quoted_status_id")]
    pub quote_id: Option<String>,
    /// Mastodon's `quote_approval_policy`: who may quote the post, one of
    /// `public`, `followers` or `nobody`. It only narrows what the
    /// visibility allows; absent or empty is the default. Anything else is
    /// the 422 `Invalid quote_approval_policy`.
    #[serde(default)]
    pub quote_approval_policy: Option<String>,
    /// Whether this is a preview: validate and render, answer the entity a
    /// create would have, write nothing — see
    /// [`crate::services::create_status::preview_status`]. Absent,
    /// null and every [`flex`] false spelling are a real post.
    ///
    /// [`flex`]: crate::extract::flex
    #[serde(default, deserialize_with = "crate::extract::flex::opt_bool")]
    pub preview: Option<bool>,
    /// Pleroma's expiration extension: seconds from now after which the
    /// post deletes itself. A [`flex`] integer — a form body's
    /// `expires_in=3600` spells what JSON's `3600` spells, and anything
    /// that is no integer at all is the extractor's 422. Absent or null is
    /// a post that never expires; a value under
    /// [`min_expiration_secs`](crate::config::Config::min_expiration_secs)
    /// or too large to land on the calendar is the service's — see
    /// `checked_expiration`.
    ///
    /// [`flex`]: crate::extract::flex
    #[serde(default, deserialize_with = "crate::extract::flex::opt_i64")]
    pub expires_in: Option<i64>,
    /// The status this one answers, as id text. A named parent is read
    /// with the same viewer-visibility rules as the show route's shared
    /// gate, so a malformed, unknown, deleted or poster-invisible parent
    /// is the shared 404, and a reblog row folds to the original it
    /// boosts; the reply then carries the parent's thread id, author
    /// link, audience cap and fallback language, and auto-mentions the
    /// parent's author and the accounts the parent mentioned — the post
    /// text itself is never modified. Absent, null or empty is a root
    /// post, Pleroma's own empty-string "no reply" arm.
    #[serde(default)]
    pub in_reply_to_id: Option<String>,
    /// Pleroma's conversation-reply arm: the id text of the poster's own
    /// [`conversation_participation`](crate::models::conversation::Participation)
    /// row. A named participation makes the post a `direct` reply into
    /// that DM thread: the participation's recipients join its mentions,
    /// the visibility is forced to `direct` however the body spelled it,
    /// and the new row lands in the participation's conversation, which
    /// is what `after_status_created`'s
    /// [`participate`](crate::models::conversation::participate) hook then
    /// reads back. A malformed, unknown, or someone-else's id is the
    /// shared 404. Absent, null or empty names no conversation, and the
    /// post stands on [`in_reply_to_id`](Self::in_reply_to_id) alone.
    #[serde(default)]
    pub in_reply_to_conversation_id: Option<String>,
    /// The ids of the poster's own unposted uploads this status carries,
    /// in the order they want them to render — decision 10's id-array:
    /// JSON's `["a","b"]` or the form's `media_ids[]=a&media_ids[]=b`,
    /// both read as id text the way
    /// [`crate::services::create_status::EditStatusParams`] takes them.
    /// More than [`crate::config::Config::max_media_attachments`] is 422 `Too many
    /// attachments`; an entry that is no id, or an id that names no row
    /// the poster owns and no status has claimed yet, is 422
    /// `Media not found` — the write's own gate, through
    /// [`crate::models::media_attachment::attach_to_status`]. With at
    /// least one id here, an empty [`status`](Self::status) is a
    /// media-only post rather than a refusal; a poll sent beside a
    /// non-empty list is 422 `Polls cannot have attachments`. Absent or
    /// null attaches nothing.
    ///
    /// [`EditStatusParams`]: crate::services::create_status::EditStatusParams
    #[serde(default)]
    pub media_ids: Option<Vec<String>>,
    /// The poll the body asks for, in the shape the [`ApiBody`]
    /// extractor nests out of `poll[options][]`, `poll[expires_in]`,
    /// `poll[multiple]` and `poll[hide_totals]` form keys — or out of
    /// the equivalent JSON object.
    /// [`crate::services::create_status::check_body`] runs Pleroma's
    /// `CommonAPI` rules over it and carries the result as
    /// [`crate::services::create_status::Checked::poll`]; both create arms store it beside the status
    /// row through [`crate::models::poll::create`], and an accepted
    /// poll answers its [`Poll`] entity on the created post.
    ///
    /// [`ApiBody`]: crate::extract::ApiBody
    /// [`Poll`]: crate::entities::poll::Poll
    #[serde(default)]
    pub poll: Option<PollRequest>,
    /// Pleroma's scheduling arm: the instant the post should fire, as
    /// RFC3339 text. A body that sends it is not a create at all: the
    /// endpoint hands the whole request to
    /// [`crate::services::schedule_status`], which validates it exactly
    /// as a create is validated, parks the normalised params (without
    /// this key) until that moment, and answers a
    /// [`ScheduledStatus`](crate::entities::scheduled_status::ScheduledStatus)
    /// entity instead of a [`Status`]. Absent or null posts now; present
    /// text that is no RFC3339 instant is the schedule's 422. This key
    /// is handled by that arm and never by a create — the stored params
    /// the publish replays carry no `scheduled_at`.
    ///
    /// [`Status`]: crate::entities::status::Status
    #[serde(default)]
    pub scheduled_at: Option<String>,
}

/// The `poll` half of a [`CreateStatusParams`] body, as the client wrote
/// it.
///
/// This is the shape the [`ApiBody`] extractor nests out of
/// `poll[options][]`, `poll[expires_in]`, `poll[multiple]` and
/// `poll[hide_totals]` form keys (see [`crate::extract::multipart`]'s
/// bracket-key test) or out of the equivalent JSON object. `expires_in`,
/// `multiple` and `hide_totals` are [`flex`] fields, so a form body's
/// `poll[expires_in]=3600`, `poll[multiple]=true` and
/// `poll[hide_totals]=false` strings spell what JSON's number and booleans
/// spell. `checked_poll` applies Pleroma's `CommonAPI.Utils` poll rules
/// and answers the [`CheckedPoll`] both create arms store.
///
/// The [`Serialize`] half is the schedule's: a parked create keeps this
/// shape in its pending row's jsonb and the publish replays it.
///
/// [`ApiBody`]: crate::extract::ApiBody
/// [`flex`]: crate::extract::flex
#[derive(Debug, Deserialize, Serialize)]
pub struct PollRequest {
    /// The options as posted, in order. `#[serde(default)]` lets a poll
    /// that names no options at all reach `checked_poll`'s minimum of
    /// two rather than failing the extractor.
    #[serde(default)]
    pub options: Vec<String>,
    /// Seconds from now until the poll closes; a [`flex`] integer, with
    /// absent or null meaning a poll that never closes. Accepted values
    /// run from [`crate::config::Config::min_poll_expiration_secs`] to
    /// [`crate::config::Config::max_poll_expiration_secs`].
    ///
    /// [`flex`]: crate::extract::flex
    #[serde(default, deserialize_with = "crate::extract::flex::opt_i64")]
    pub expires_in: Option<i64>,
    /// Whether one voter may mark several options; a [`flex`] boolean,
    /// `false` when absent or null.
    ///
    /// [`flex`]: crate::extract::flex
    #[serde(default, deserialize_with = "crate::extract::flex::opt_bool")]
    pub multiple: Option<bool>,
    /// Whether the per-option counts stay hidden while the poll runs; a
    /// [`flex`] boolean, `false` when absent or null.
    ///
    /// [`flex`]: crate::extract::flex
    #[serde(default, deserialize_with = "crate::extract::flex::opt_bool")]
    pub hide_totals: Option<bool>,
}

/// The poll [`crate::services::create_status::check_body`] accepted.
///
/// What its `checked_poll` step measured and computed, handed by both
/// create arms to [`crate::models::poll::create`] beside the status row
/// they insert on the same transaction.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct CheckedPoll {
    /// The options as posted, in order; duplicates are the poster's
    /// business (Pleroma's own rules allow them) and every count and
    /// length bound `checked_poll` vouches for.
    pub options: Vec<String>,
    /// The moment voting closes: `now` plus the posted `expires_in`, or
    /// `None` when the body named none and the poll never closes.
    pub expires_at: Option<DateTime<Utc>>,
    /// Whether a voter may mark more than one option: the posted flag,
    /// `false` when the body left it out.
    pub multiple: bool,
    /// Whether the per-option counts stay hidden: the posted flag,
    /// `false` when the body left it out.
    pub hide_totals: bool,
}

impl CreateStatusParams {
    /// Whether this body asks for a preview
    /// ([`crate::services::create_status::preview_status`]) rather
    /// than a post: the submitted flag, `false` when the body leaves it
    /// out, sends `null`, or spells any of the [`flex`] false forms. The
    /// endpoint asks this before it decides which service arm to run; the
    /// field itself plays no part in
    /// [`crate::services::create_status::check_body`]'s validation.
    ///
    /// [`flex`]: crate::extract::flex
    #[must_use]
    pub const fn previews(&self) -> bool {
        matches!(self.preview, Some(true))
    }
}

#[cfg(test)]
mod tests {
    use serde_json::json;

    use super::*;
    use crate::services::create_status::test_support::{accepted, params};
    #[test]
    fn a_sensitive_flag_accepts_the_form_spellings() {
        let on = params(json!({"status": "hi", "sensitive": "on"}));
        assert!(accepted(&on).sensitive, "`\"on\"` means true");
        let zero = params(json!({"status": "hi", "sensitive": "0"}));
        assert!(!accepted(&zero).sensitive, "`\"0\"` means false");
        let null = params(json!({"status": "hi", "sensitive": null}));
        assert!(!accepted(&null).sensitive, "a null is as absent as no key");
    }

    #[test]
    fn a_quote_id_accepts_both_spellings_and_its_absence() {
        let id = "01934567-89ab-7cde-f012-3456789abcde";
        let quoted = params(json!({"status": "hi", "quote_id": id}));
        assert_eq!(
            quoted.quote_id.as_deref(),
            Some(id),
            "the Mastodon spelling arrives"
        );
        let alias = params(json!({"status": "hi", "quoted_status_id": id}));
        assert_eq!(
            alias.quote_id.as_deref(),
            Some(id),
            "and so does Pleroma's older alias, into the same field"
        );
        for value in [
            json!({"status": "hi"}),
            json!({"status": "hi", "quote_id": null}),
        ] {
            let body = params(value);
            assert!(
                body.quote_id.is_none(),
                "an absent or null key is no quote at all"
            );
        }
    }

    /// The `flex` reading of the parameter: a form body's numeric string is
    /// the same expiry as JSON's number, and everything that is no integer
    /// at all — text, a boolean, a fraction, a container — is the
    /// extractor's 422 before any service runs.
    #[test]
    fn only_integers_deserialize_and_a_numeric_string_counts_as_one() {
        let body = params(json!({"status": "hi", "expires_in": "3600"}));
        assert!(
            accepted(&body).expires_at.is_some(),
            "`expires_in=3600` from a form body is an expiry"
        );
        for value in [
            json!("soon"),
            json!(""),
            json!("60.5"),
            json!(60.5),
            json!(true),
            json!([3600]),
            json!({"secs": 3600}),
        ] {
            let parsed = crate::extract::into_params::<CreateStatusParams>(
                json!({"status": "hi", "expires_in": value}),
            );
            assert!(
                parsed.is_err(),
                "`{value}` is no integer and must not deserialize"
            );
        }
    }

    /// The parameter arrives as id text or not at all: absent, null and the
    /// form body's plain string all deserialize into the optional field.
    #[test]
    fn the_reply_parameter_deserializes_as_id_text() {
        let absent = params(json!({"status": "hi"}));
        assert_eq!(
            absent.in_reply_to_id, None,
            "no key is a root post's parameter"
        );
        let null = params(json!({"status": "hi", "in_reply_to_id": null}));
        assert_eq!(null.in_reply_to_id, None, "a null is as absent as no key");
        let one = params(json!({
            "status": "hi",
            "in_reply_to_id": "01934567-89ab-7cde-f012-3456789abcde",
        }));
        assert_eq!(
            one.in_reply_to_id.as_deref(),
            Some("01934567-89ab-7cde-f012-3456789abcde"),
            "id text is stored as written"
        );
    }

    #[test]
    fn the_media_ids_parameter_deserializes_as_id_text() {
        let absent = params(json!({"status": "hi"}));
        assert_eq!(absent.media_ids, None, "no key says nothing about media");
        let null = params(json!({"status": "hi", "media_ids": null}));
        assert_eq!(null.media_ids, None, "a null is as absent as no key");
        let list = params(json!({
            "status": "hi",
            "media_ids": ["01934567-89ab-7cde-f012-3456789abcde", "junk"],
        }));
        assert_eq!(
            list.media_ids,
            Some(vec![
                "01934567-89ab-7cde-f012-3456789abcde".to_owned(),
                "junk".to_owned(),
            ]),
            "decision 10's array arrives as id text, JSON or form alike —
             the extractor folds `media_ids[]=a&media_ids[]=b` to the same list"
        );
    }

    /// The poll arrives the same from JSON and from form keys: the
    /// extractor nests `poll[options][]` into the option list and every
    /// other `poll[...]` value is a [`flex`] string the deserializers
    /// read; anything that is no poll shape at all stays the extractor's
    /// 422.
    ///
    /// [`flex`]: crate::extract::flex
    #[test]
    fn the_poll_deserializes_from_the_json_and_form_spellings() {
        let from_json = params(json!({
            "status": "pick",
            "poll": {
                "options": ["a", "b"],
                "expires_in": 3600,
                "multiple": true,
                "hide_totals": false,
            },
        }));
        assert!(from_json.poll.is_some(), "the json shape is a poll request");

        let form = crate::extract::parse_pairs([
            ("status", "pick"),
            ("poll[options][]", "a"),
            ("poll[options][]", "b"),
            ("poll[expires_in]", "3600"),
            ("poll[multiple]", "true"),
            ("poll[hide_totals]", "on"),
            ("media_ids[]", "id-1"),
        ])
        .expect("the keys nest shallowly");
        let from_form: CreateStatusParams =
            crate::extract::into_params(form).expect("the form shape fits the same type");
        let poll = from_form.poll.expect("the nested keys became a poll");
        assert_eq!(
            poll.options,
            vec!["a", "b"],
            "the append keys are the option list"
        );
        assert_eq!(
            poll.expires_in,
            Some(3_600),
            "the string number is the flex integer"
        );
        assert_eq!(
            poll.multiple,
            Some(true),
            "the string boolean is the flex boolean"
        );
        assert_eq!(poll.hide_totals, Some(true), "`on` means true");
        assert_eq!(
            from_form.media_ids,
            Some(vec!["id-1".to_owned()]),
            "and `media_ids[]` nests beside it"
        );

        for broken in [
            json!("yes"),
            json!({"options": "ab"}),
            json!({"options": ["a", "b"], "expires_in": "soon"}),
            json!({"options": ["a", "b"], "multiple": "maybe"}),
        ] {
            let parsed = crate::extract::into_params::<CreateStatusParams>(
                json!({"status": "hi", "poll": broken}),
            );
            assert!(
                parsed.is_err(),
                "`{broken}` is no poll and must not deserialize"
            );
        }
    }
}
