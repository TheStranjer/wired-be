//! The remaining client-to-server routes and their shared helpers:
//! `GET /api/ap/whoami`, `POST /api/ap/upload_media`, the `Document`
//! answer the upload sends, and the reference-resolution and id-minting
//! helpers the outbox's activity arms fold their objects through.

use axum::{Json, extract::State, response::Response};
use serde_json::{Value, json};
use uuid::Uuid;

use crate::{
    api::media::{UploadParams, store_upload},
    auth::AuthUser,
    config::Config,
    error::{AppError, AppResult},
    extract::multipart::MultipartForm,
    federation::{
        actor::{actor_json, profile_emojis},
        json as ap_json,
    },
    models::{
        media_attachment,
        status::{self, Status},
        status_visibility,
        user::{self, User, get_by_id},
    },
    oauth::scopes::{READ, WRITE_MEDIA},
    state::AppState,
    util::id::{new_activity_ap_id, parse_id},
};

use super::negotiation::document_response;

/// `GET /api/ap/whoami` — the authenticated user's own actor document:
/// Pleroma's `whoami` rendering `user.json` for the token's user, the
/// very bytes [`get_actor`](super::actors::get_actor) serves a federation
/// fetch. The gate is a
/// bearer token granting `read`.
///
/// # Errors
///
/// [`AppError::Unauthorized`] / [`AppError::Forbidden`] from [`AuthUser`]
/// and the `read` check.
pub(super) async fn whoami(State(state): State<AppState>, auth: AuthUser) -> AppResult<Response> {
    auth.require(READ)?;
    let emojis = profile_emojis(&state.db, &auth.0).await?;
    Ok(document_response(&actor_json(
        &auth.0,
        &state.config,
        &emojis,
    )))
}

/// `POST /api/ap/upload_media` — the C2S media upload, Pleroma's
/// `upload_media`: the very processing `POST /api/v1/media` runs
/// ([`store_upload`]), answered — as Pleroma does, and the ticket fixes
/// it — `200` with the stored row as an `ActivityStreams` `Document`. The
/// `id` it carries is the media id, which is what a posted `Create` names
/// in its `attachment` to claim the upload.
///
/// # Errors
///
/// [`AppError::Unauthorized`] / [`AppError::Forbidden`] from [`AuthUser`]
/// and the `write:media` check; whatever [`store_upload`] refuses — the
/// 413 over the upload limit, the 422 for a missing, blank or
/// undetectable file — and its storage and database failures.
pub(super) async fn ap_upload_media(
    State(state): State<AppState>,
    auth: AuthUser,
    form: MultipartForm<UploadParams>,
) -> AppResult<Json<Value>> {
    auth.require(WRITE_MEDIA)?;
    let row = store_upload(&state, auth.0.id, form).await?;
    Ok(Json(document_json(&row)))
}

/// The stored media row as its `ActivityStreams` `Document`: Pleroma's
/// object shape — `url` a one-entry `Link` array carrying the file's type
/// and address, `name` the alt text (empty when it carries none), the
/// `mediaType` beside it and the `blurhash` when the analysis computed
/// one — with the `id` a client quotes from a create's attachment: the
/// media id itself, decision 4's hyphenated text.
fn document_json(row: &media_attachment::MediaAttachment) -> Value {
    let mut fields = vec![
        ("type", json!("Document")),
        (
            "url",
            json!([
                { "type": "Link", "mediaType": &row.content_type, "href": &row.url },
            ]),
        ),
        ("name", json!(row.description.clone().unwrap_or_default())),
        ("mediaType", json!(&row.content_type)),
    ];
    if let Some(blurhash) = &row.blurhash {
        fields.push(("blurhash", json!(blurhash)));
    }
    fields.push(("id", json!(row.id)));
    Value::Object(
        fields
            .into_iter()
            .map(|(name, value)| (name.to_owned(), value))
            .collect(),
    )
}

/// The single reference an interaction activity's `object` carries — a
/// bare id string or an embedded document's own — or the shared 404 for
/// an activity that names nothing: there is no object to act on.
pub(super) fn activity_reference(activity: &Value) -> AppResult<String> {
    activity
        .get("object")
        .and_then(ap_json::value_or_id)
        .ok_or(AppError::NotFound)
}

/// The live status a reference names: text that parses as an id reads by
/// primary key, anything else as the object's `uri` — the two forms an
/// `ActivityPub` client may know a status by (`{BASE_URL}/objects/{uuid}`
/// included, which is what a `Like`'s `object` and a note's `inReplyTo`
/// carry). Tombstones and unknown references answer `None`, and the
/// caller turns that into the 404.
pub(super) async fn status_by_reference(
    state: &AppState,
    reference: &str,
) -> AppResult<Option<Status>> {
    match parse_id(reference) {
        Some(id) => status::get_by_id(&state.db, id).await,
        None => status::get_by_uri(&state.db, reference).await,
    }
}

/// The stored, live account an actor reference names — the object of a
/// `Follow` or a `Block` (remote mirrors included; an unseen remote actor
/// is this endpoint's 404, and a deactivated one hides behind it, like
/// [`crate::api::accounts`]'s follow target gate).
pub(super) async fn stored_actor(state: &AppState, reference: &str) -> AppResult<User> {
    user::get_by_ap_id(&state.db, reference)
        .await?
        .filter(|found| found.is_active.get())
        .ok_or(AppError::NotFound)
}

/// The row of a followed or blocked account a relationship names: the
/// pair gate read it live before the write, so its absence means the
/// account row itself is gone underneath the relationship — the 404.
pub(super) async fn user_by_id(state: &AppState, id: Uuid) -> AppResult<User> {
    get_by_id(&state.db, id).await?.ok_or(AppError::NotFound)
}

/// The status an interaction activity points at, as its actor may see it:
/// the same gate [`crate::api::helpers::status_for_viewer_or_404`] is for
/// the Mastodon routes — a live row named by id or object `uri`, visible
/// to the caller, or the one 404 that never reveals a private post.
pub(super) async fn interactable_status(
    state: &AppState,
    auth: &AuthUser,
    activity: &Value,
) -> AppResult<Status> {
    let reference = activity_reference(activity)?;
    let row = status_by_reference(state, &reference)
        .await?
        .ok_or(AppError::NotFound)?;
    status_visibility::require_visible(&state.db, row, Some(&auth.0)).await
}

/// The status a boost lands on: `row` itself, or its original when the
/// client named a reblog row — nobody boosts a boost, the same fold
/// [`crate::api::statuses`]'s reblog route applies.
pub(super) async fn reblog_target(state: &AppState, row: &Status) -> AppResult<Status> {
    match row.reblog_of_id {
        None => Ok(row.clone()),
        Some(original_id) => status::get_by_id(&state.db, original_id)
            .await?
            .ok_or(AppError::NotFound),
    }
}

/// The activity id a relationship row answers in `Location`: its own
/// `ap_id`, which [`follow::follow`](crate::models::follow::follow),
/// [`favourite::favourite`](crate::models::favourite::favourite) and
/// [`block::block`](crate::models::block::block) mint for every local maker (decision 6) — and a
/// freshly minted one for the row that somehow lacks it, so no arm has a
/// `None` to explain away.
pub(super) fn row_activity_id(config: &Config, ap_id: Option<&str>) -> String {
    ap_id.map_or_else(|| new_activity_id(config), str::to_owned)
}

/// A freshly minted local activity id, `{BASE_URL}/activities/{uuid}`
/// (decision 6): the answer's `Location` for the writes that retire a
/// fact instead of leaving a row behind — the `Delete` and the `Undo`s.
pub(super) fn new_activity_id(config: &Config) -> String {
    new_activity_ap_id(&config.base_url)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A media row, `document_json` renders twice over: `url` a one-entry
    /// `Link` array, `name` the alt text, `mediaType` beside it,
    /// `blurhash` only when there is one, and the media `id` for the
    /// create to quote.
    #[test]
    fn the_upload_document_is_the_c2s_attachment_shape() {
        let row = media_row(Some("a gradient"), Some("UBLF5j00Rj"));
        let document = document_json(&row);
        assert_eq!(document["type"], "Document", "the object type");
        assert_eq!(
            document["id"], "01934567-89ab-7cde-f012-3456789abcde",
            "the media id the create names in its attachment"
        );
        assert_eq!(
            document["url"],
            json!([{
                "type": "Link",
                "mediaType": "image/png",
                "href": "https://test.example/media/01934567/cat.png",
            }]),
            "the Link Pleroma's upload object carries"
        );
        assert_eq!(document["name"], "a gradient", "the alt text");
        assert_eq!(document["mediaType"], "image/png", "the file type");
        assert_eq!(document["blurhash"], "UBLF5j00Rj", "the computed hash");

        let bare = media_row(None, None);
        let undecorated = document_json(&bare);
        assert_eq!(undecorated["name"], "", "no alt text is the empty name");
        assert!(
            undecorated.get("blurhash").is_none(),
            "and an unhashed upload carries no blurhash key at all"
        );
    }

    /// The media row `document_json` renders, with only the fields the
    /// `Document` reads set.
    fn media_row(
        description: Option<&str>,
        blurhash: Option<&str>,
    ) -> media_attachment::MediaAttachment {
        media_attachment::MediaAttachment {
            id: Uuid::parse_str("01934567-89ab-7cde-f012-3456789abcde").unwrap(),
            user_id: None,
            status_id: None,
            position: 0,
            storage_key: None,
            remote_url: None,
            url: "https://test.example/media/01934567/cat.png".to_owned(),
            preview_url: None,
            content_type: "image/png".to_owned(),
            description: description.map(str::to_owned),
            blurhash: blurhash.map(str::to_owned),
            width: None,
            height: None,
            preview_width: None,
            preview_height: None,
            duration: None,
            focus_x: None,
            focus_y: None,
            file_size: None,
            sensitive_hint: false,
            inserted_at: chrono::Utc::now(),
            updated_at: chrono::Utc::now(),
        }
    }

    /// The fresh activity id the withdrawals answer in `Location`:
    /// decision 6's local activity address, and `row_activity_id` keeps a
    /// row's own id when it has one.
    #[test]
    fn withdrawal_locations_are_fresh_activity_ids() {
        let config = Config::for_tests();
        let id = new_activity_id(&config);
        let tail = id
            .strip_prefix("https://test.example/activities/")
            .expect("a local activity id under the instance base URL");
        assert!(
            parse_id(tail).is_some(),
            "and the tail is the minted id itself: {id}"
        );
        assert_eq!(
            row_activity_id(&config, Some("https://peer.example/activities/7")),
            "https://peer.example/activities/7",
            "a row that carries an activity id is answered with it"
        );
        let minted = row_activity_id(&config, None);
        assert!(
            minted.starts_with("https://test.example/activities/"),
            "and one that does not falls back to a fresh id: {minted}"
        );
    }
}
