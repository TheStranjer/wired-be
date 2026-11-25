//! The C2S outbox's write path: `POST /users/{nickname}/outbox`
//! ([`update_outbox`]), its activity-type dispatch, and the `Create` and
//! `Delete` arms that fold a posted activity into the same writes the
//! Mastodon client API runs.

use axum::{
    extract::{Path, State},
    http::{HeaderValue, StatusCode, header},
    response::{IntoResponse as _, Response},
};
use serde_json::{Value, json};
use tracing::debug;

use crate::{
    auth::AuthUser,
    error::{AppError, AppResult},
    extract::ApiBody,
    federation::{json as ap_json, note_parser},
    models::{activitypub_actor, oauth_app, user::User},
    oauth::scopes::WRITE_STATUSES,
    services::{
        create_status::{self, CreateStatusParams},
        delete_status::delete_status,
    },
    state::AppState,
};

use super::c2s_misc::{activity_reference, new_activity_id, status_by_reference};
use super::outbox_relations::{post_announce, post_block, post_follow, post_like, post_undo};
use super::{FORBIDDEN, OBJECT_TYPE_NOTE, UNSUPPORTED_ACTIVITY_TYPE};

/// `POST /users/{nickname}/outbox` — the C2S write, Pleroma's
/// `update_outbox`: one `ActivityStreams` activity by the owner, folded
/// into the same writes the Mastodon client API runs. `Create` of a
/// `Note` posts through [`create_status::create_status`]; `Delete`
/// tombstones the caller's own status; `Follow` and `Block` write the
/// relationship through the models; `Like` and `Announce` are the
/// favourite and the reblog; `Undo` withdraws a `Follow`, `Like`,
/// `Announce` or `Block`, named either by the embedded activity or by the
/// id its creation answered in `Location`. Any other type is the 400
/// [`UNSUPPORTED_ACTIVITY_TYPE`]. A folded activity is not stored as
/// such — decision 6 keeps the facts relationally and renders their
/// activities from them — so the answer is the ticket's: `201` with the
/// new object/activity id in `Location` and an empty body.
///
/// The gates are [`read_inbox`](super::inbox::read_inbox)'s, on
/// `write:statuses` (Pleroma runs the
/// whole outbox under one scope, not per activity type).
///
/// # Errors
///
/// [`AppError::Unauthorized`] / [`AppError::Forbidden`] from [`AuthUser`]
/// and the scope check; [`AppError::Forbidden`] with [`FORBIDDEN`] for
/// somebody else's outbox (and for a `Delete` of a post the caller does
/// not own); [`AppError::BadRequest`] with [`UNSUPPORTED_ACTIVITY_TYPE`];
/// the inner write's own refusals — the create's 422s and reply 404, the
/// self-follow and block refusals, the boost 422 — and
/// [`AppError::NotFound`] for an object the activity names that no stored
/// row carries.
pub(super) async fn update_outbox(
    State(state): State<AppState>,
    auth: AuthUser,
    Path(nickname): Path<String>,
    ApiBody(activity): ApiBody<Value>,
) -> AppResult<Response> {
    auth.require(WRITE_STATUSES)?;
    ensure_own_actor(&auth.0, &nickname)?;
    let location = post_activity(&state, &auth, &activity).await?;
    debug!(
        actor = %auth.0.id,
        %location,
        "c2s activity accepted"
    );
    let value = HeaderValue::from_str(&location)
        .map_err(|err| AppError::Internal(format!("the activity id is not header-safe: {err}")))?;
    Ok((StatusCode::CREATED, [(header::LOCATION, value)]).into_response())
}

/// The outbox's type dispatch: the seven activity types the ticket folds,
/// each answered by the `Location` id of the activity the write
/// corresponds to, and everything else — including a body that names no
/// string type at all — by the 400.
async fn post_activity(state: &AppState, auth: &AuthUser, activity: &Value) -> AppResult<String> {
    match activity
        .get("type")
        .and_then(ap_json::first_type)
        .as_deref()
    {
        Some("Create") => post_create(state, auth, activity).await,
        Some("Delete") => post_delete(state, auth, activity).await,
        Some("Follow") => post_follow(state, auth, activity).await,
        Some("Like") => post_like(state, auth, activity).await,
        Some("Announce") => post_announce(state, auth, activity).await,
        Some("Block") => post_block(state, auth, activity).await,
        Some("Undo") => post_undo(state, auth, activity).await,
        Some(_) | None => Err(AppError::BadRequest(UNSUPPORTED_ACTIVITY_TYPE.to_owned())),
    }
}

/// The C2S owner gate of the two actor-keyed routes: the bearer's user
/// must *be* the account the path names — Pleroma matches the token's
/// nickname against the route's in the controller head, and its
/// catch-all arm answers every other pairing the 403. No lookup stands
/// between the two: the extractor already handed a live local account,
/// so a matching nickname can only name that one.
pub(super) fn ensure_own_actor(user: &User, nickname: &str) -> AppResult<()> {
    if user.nickname == nickname {
        Ok(())
    } else {
        Err(AppError::Forbidden(FORBIDDEN.to_owned()))
    }
}

/// `Create` of a `Note` — the post, through the client API's own write
/// service. The note's `content` is the status text, its `summary` the
/// content warning and its `sensitive` flag the fold; the audience is
/// read off `to`/`cc` exactly as the federation parser reads them
/// ([`note_parser::parse_visibility`]: the public addressee in `to` is
/// `public`, in `cc` alone `unlisted`, the followers collection
/// `private`, anything else `direct`); `inReplyTo` names the answered
/// status by object id or id text; and every `attachment` reference
/// carries an upload id from `POST /api/ap/upload_media` — the `media_ids`
/// the create claims, whose "the caller's own, not yet posted" rules are
/// the media model's. `Location` names the `Create` the post federates
/// as, `{BASE_URL}/activities/{status id}` (decision 6, and the id
/// [`create_activity_json`](crate::federation::note::create_activity_json) writes).
async fn post_create(state: &AppState, auth: &AuthUser, activity: &Value) -> AppResult<String> {
    let object = note_object(activity)?;
    let followers_url = activitypub_actor::followers_url(&state.db, auth.0.id).await?;
    let params = CreateStatusParams {
        status: text_field(object, "content"),
        spoiler_text: text_field(object, "summary"),
        sensitive: object.get("sensitive").and_then(Value::as_bool),
        visibility: Some(
            note_parser::parse_visibility(
                &create_addressing(object, activity),
                followers_url.as_deref(),
            )
            .as_str()
            .to_owned(),
        ),
        in_reply_to_id: replied_to(state, object).await?,
        media_ids: Some(attachment_ids(object)),
        ..CreateStatusParams::default()
    };
    let app = oauth_app::get_by_id(&state.db, auth.1.app_id).await?;
    let entity = create_status::create_status(state, &auth.0, app.as_ref(), params).await?;
    Ok(format!(
        "{}/activities/{}",
        state.config.base_url, entity.id
    ))
}

/// The `Create`'s object, when it is a `Note` document: a bare reference,
/// a missing object, or any other object type is the 400
/// [`UNSUPPORTED_ACTIVITY_TYPE`] — the outbox posts a plain note and
/// nothing else.
fn note_object(activity: &Value) -> AppResult<&Value> {
    let unsupported = || AppError::BadRequest(UNSUPPORTED_ACTIVITY_TYPE.to_owned());
    activity
        .get("object")
        .filter(|object| {
            object.is_object()
                && object.get("type").and_then(ap_json::first_type).as_deref()
                    == Some(OBJECT_TYPE_NOTE)
        })
        .ok_or_else(unsupported)
}

/// The string `object` carries under `key`: `None` for an absent, `null`
/// or non-text value, which for `content` leaves the create's own empty
/// refusal and for `summary` no content warning.
fn text_field(object: &Value, key: &str) -> Option<String> {
    object.get(key).and_then(Value::as_str).map(str::to_owned)
}

/// The `to`/`cc` the create's audience is read from: the note's own when
/// it addresses anyone, the enclosing `Create`'s otherwise — C2S clients
/// write the audience on either half, and the two places Pleroma's
/// pipeline checks. The result is the shape [`note_parser::parse_visibility`]
/// reads: one object with `to` and `cc`.
fn create_addressing(object: &Value, activity: &Value) -> Value {
    let audience = |key: &str| {
        object
            .get(key)
            .filter(|value| !is_empty_audience(value))
            .or_else(|| activity.get(key))
            .cloned()
            .unwrap_or_else(|| json!([]))
    };
    json!({ "to": audience("to"), "cc": audience("cc") })
}

/// Whether an addressing value names nobody: no non-empty reference
/// anywhere in it — `null`, the empty array, the empty string, or a
/// value that carries no reference at all all mean "this half holds no
/// audience", and the create falls back to the other half.
fn is_empty_audience(value: &Value) -> bool {
    !ap_json::references(value)
        .iter()
        .any(|recipient| !recipient.is_empty())
}

/// The note's `inReplyTo` as the create's id text: the answered status
/// must be a row this instance stores (an object id or the bare id text,
/// through the same read a `Like` uses), or the shared 404. An absent or
/// `null` reference is a root post.
async fn replied_to(state: &AppState, object: &Value) -> AppResult<Option<String>> {
    let Some(reference) = object.get("inReplyTo").and_then(ap_json::value_or_id) else {
        return Ok(None);
    };
    let row = status_by_reference(state, &reference)
        .await?
        .ok_or(AppError::NotFound)?;
    Ok(Some(row.id.to_string()))
}

/// The `attachment` array's upload ids, submission order kept: every
/// reference's last path segment — the media id `POST /api/ap/upload_media`
/// answers as the `Document`'s `id`, bare or under an id URL. The create
/// itself decides what each id may mean: the media model claims only the
/// poster's own, not-yet-posted uploads and refuses any other with its
/// 422.
fn attachment_ids(object: &Value) -> Vec<String> {
    object
        .get("attachment")
        .map_or(&[][..], ap_json::as_array)
        .iter()
        .filter_map(ap_json::value_or_id)
        .map(|reference| match reference.rsplit_once('/') {
            Some((_origin_and_path, last_segment)) => last_segment.to_owned(),
            None => reference,
        })
        .collect()
}

/// `Delete` of the caller's own status, named by object id or id text:
/// the same write `DELETE /api/v1/statuses/{id}` runs — the tombstone of
/// the row and its reblogs, the counter steps and the post-delete hooks
/// ([`delete_status`]). Somebody else's post is the 403, Pleroma's
/// permission arm of `fix_user_message`; an unknown or already deleted
/// one is the shared 404. `Location` names the `Delete` itself: a fresh
/// `{BASE_URL}/activities/{uuid}` id, since a withdrawal creates no row
/// to key one on (decision 6).
async fn post_delete(state: &AppState, auth: &AuthUser, activity: &Value) -> AppResult<String> {
    let reference = activity_reference(activity)?;
    let row = status_by_reference(state, &reference)
        .await?
        .ok_or(AppError::NotFound)?;
    if row.user_id != auth.0.id {
        return Err(AppError::Forbidden(FORBIDDEN.to_owned()));
    }
    // The write is idempotent: the `false` is the concurrent delete that
    // turned the row into a tombstone between the read and this write —
    // the fact already withdrawn, and the same accepted answer.
    let _restated = delete_status(state, &row).await?;
    Ok(new_activity_id(&state.config))
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The audience rule of the C2S create: only values the visibility
    /// parser can read a recipient out of count as an audience.
    #[test]
    fn an_audience_names_nobody_when_it_carries_no_reference() {
        for empty in [Value::Null, json!([]), json!("")] {
            assert!(is_empty_audience(&empty), "`{empty}` addresses nobody");
        }
        for filled in [
            json!("https://test.example/users/bob"),
            json!(["https://test.example/users/bob"]),
            json!([{"id": "https://test.example/users/bob"}]),
        ] {
            assert!(
                !is_empty_audience(&filled),
                "`{filled}` names at least one recipient"
            );
        }
    }

    /// The create's audience is the note's own when it carries one and the
    /// enclosing activity's otherwise — the two places C2S clients write
    /// `to`/`cc`.
    #[test]
    fn the_addressing_falls_back_from_the_object_to_the_activity() {
        let activity = json!({
            "type": "Create",
            "to": ["https://www.w3.org/ns/activitystreams#Public"],
            "cc": ["https://test.example/users/alice/followers"],
        });
        let own = json!({ "type": "Note", "to": ["https://test.example/users/bob"] });
        let merged = create_addressing(&own, &activity);
        assert_eq!(
            merged["to"],
            json!(["https://test.example/users/bob"]),
            "the object's own audience wins"
        );
        assert_eq!(
            merged["cc"],
            json!(["https://test.example/users/alice/followers"]),
            "and each half falls back on its own"
        );

        let empty = json!({ "type": "Note", "to": [], "cc": Value::Null });
        let echoed = create_addressing(&empty, &activity);
        assert_eq!(
            echoed["to"], activity["to"],
            "an empty array is no audience: the activity's applies"
        );
        assert_eq!(echoed["cc"], activity["cc"], "and so is a null one");

        let naked = json!({ "type": "Note" });
        let bare = create_addressing(&naked, &json!({"type": "Create"}));
        assert_eq!(
            bare,
            json!({ "to": [], "cc": [] }),
            "an activity that addresses nobody reads as addressing nobody"
        );
    }

    /// The attachment references a posted create may carry — the bare
    /// upload id or a URL ending in it — all read as the ids the create's
    /// `media_ids` wants, in submission order.
    #[test]
    fn attachment_references_read_as_upload_ids_in_order() {
        let object = json!({
            "type": "Note",
            "attachment": [
                {"type": "Document", "id": "01934567-89ab-7cde-f012-3456789abcde"},
                {"id": "https://test.example/api/v1/media/01934567-89ab-7cde-f012-4456789abcde"},
                "01934567-89ab-7cde-f012-5456789abcde",
                {"type": "Document"},
                {"name": "no reference at all"},
            ],
        });
        assert_eq!(
            attachment_ids(&object),
            [
                "01934567-89ab-7cde-f012-3456789abcde",
                "01934567-89ab-7cde-f012-4456789abcde",
                "01934567-89ab-7cde-f012-5456789abcde",
            ],
            "ids bare and as the last path segment; reference-less entries drop out"
        );
        assert!(
            attachment_ids(&json!({"type": "Note"})).is_empty(),
            "a note with no attachments claims no uploads"
        );
    }

    /// A posted `Create` folds only a `Note` object: every other shape is
    /// the 400 `Unsupported activity type`.
    #[test]
    fn only_a_note_document_is_a_creatable_object() {
        for object in [
            json!({"type": "Note", "content": "hi"}),
            json!({"type": ["Note", "Article"], "content": "hi"}),
        ] {
            let activity = json!({"type": "Create", "object": object});
            let note = note_object(&activity).expect("`Note` is the supported object type");
            assert_eq!(
                note["content"], "hi",
                "and it is the object that comes back"
            );
        }
        for activity in [
            json!({"type": "Create"}),
            json!({"type": "Create", "object": "https://test.example/objects/1"}),
            json!({"type": "Create", "object": {"type": "Question"}}),
            json!({"type": "Create", "object": {"content": "no type"}}),
            json!({"type": "Create", "object": Value::Null}),
        ] {
            let err = note_object(&activity)
                .expect_err("anything but an embedded Note is not a creatable object");
            assert_eq!(
                err.to_string(),
                UNSUPPORTED_ACTIVITY_TYPE,
                "and it is refused as the unsupported shape it is"
            );
        }
    }
}
