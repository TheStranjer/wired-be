//! The moderators' notes: creating one, its checks and deleting one.

use axum::{
    extract::{Path, State},
    http::StatusCode,
};
use serde::Deserialize;
use tracing::debug;

use super::{AREA_REPORTS, CLASS_WRITE, MAX_NOTE_LENGTH, NOTE_BLANK, listing::report_by_id};
use crate::{
    auth::admin::StaffUser,
    error::{AppError, AppResult},
    extract::ApiBody,
    models::{moderation_log, report, user},
    state::AppState,
    util::id::parse_id,
};

/// The body of `POST /api/v1/pleroma/admin/reports/{id}/notes`: Pleroma's
/// `notes_create_request`, the moderator's text.
#[derive(Debug, Deserialize)]
pub(super) struct NoteBody {
    /// The note to write; a body without it is the extractor's 422.
    content: String,
}

/// `POST /api/v1/pleroma/admin/reports/{id}/notes` — Pleroma's
/// `notes_create`: write the caller's note on the report, log the
/// `report_note` entry, answer `204`.
///
/// # Errors
///
/// [`AppError::Unauthorized`] / [`AppError::Forbidden`] from [`StaffUser`]
/// and its `admin:write` gate; [`AppError::RouteNotFound`] for an id that
/// is no id or names no report; [`AppError::Unprocessable`] for a blank
/// or over-long `content`; [`AppError::Database`] from the writes.
pub(super) async fn add_note(
    State(state): State<AppState>,
    staff: StaffUser,
    Path(raw): Path<String>,
    ApiBody(body): ApiBody<NoteBody>,
) -> AppResult<StatusCode> {
    staff.require(CLASS_WRITE, AREA_REPORTS)?;
    let row = report_by_id(&state.db, &raw).await?;
    validate_note(&body.content)?;
    let note = report::create_note(&state.db, row.id, staff.0.id, &body.content).await?;
    let target = user::get_by_id(&state.db, row.target_id).await?;
    let logged =
        moderation_log::report_note(&state.db, &staff.0, row.id, &body.content, target.as_ref())
            .await?;
    debug!(
        log = %logged.id,
        report = %row.id,
        note = %note.id,
        "admin report note added"
    );
    Ok(StatusCode::NO_CONTENT)
}

/// The note body's checks the ticket pins: an empty text is blank, and
/// more than [`MAX_NOTE_LENGTH`] characters is over the cap — both the
/// `422` Mastodon's changeset wording states.
///
/// # Errors
///
/// [`AppError::Unprocessable`] as described.
fn validate_note(content: &str) -> AppResult<()> {
    if content.is_empty() {
        return Err(AppError::Unprocessable(NOTE_BLANK.to_owned()));
    }
    if content.chars().count() > MAX_NOTE_LENGTH {
        return Err(AppError::Unprocessable(format!(
            "Validation failed: Content is too long (maximum is {MAX_NOTE_LENGTH} characters)"
        )));
    }
    Ok(())
}

/// `DELETE /api/v1/pleroma/admin/reports/{report_id}/notes/{id}` —
/// Pleroma's `notes_delete`: remove the report's note, log the
/// `report_note_delete` entry with its text, answer `204`.
///
/// # Errors
///
/// [`AppError::Unauthorized`] / [`AppError::Forbidden`] from [`StaffUser`]
/// and its `admin:write` gate; [`AppError::RouteNotFound`] for a
/// `report_id` that is no id or names no report, and for a note `id`
/// that is no id or names no note *of that report*;
/// [`AppError::Database`] from the writes.
pub(super) async fn delete_note(
    State(state): State<AppState>,
    staff: StaffUser,
    Path((report_raw, note_raw)): Path<(String, String)>,
) -> AppResult<StatusCode> {
    staff.require(CLASS_WRITE, AREA_REPORTS)?;
    let row = report_by_id(&state.db, &report_raw).await?;
    let Some(note_id) = parse_id(&note_raw) else {
        return Err(AppError::RouteNotFound);
    };
    let Some(note) = report::delete_note(&state.db, row.id, note_id).await? else {
        return Err(AppError::RouteNotFound);
    };
    let target = user::get_by_id(&state.db, row.target_id).await?;
    let logged = moderation_log::report_note_delete(
        &state.db,
        &staff.0,
        row.id,
        &note.content,
        target.as_ref(),
    )
    .await?;
    debug!(
        log = %logged.id,
        report = %row.id,
        note = %note.id,
        "admin report note deleted"
    );
    Ok(StatusCode::NO_CONTENT)
}

#[cfg(test)]
mod tests {
    use serde_json::json;

    use super::*;

    /// The note body's two refusals and its boundary: blank is blank, the
    /// cap counts characters, and it is inclusive.
    #[test]
    fn the_note_body_validates_against_the_cap() {
        let blank = validate_note("").expect_err("a blank note is refused");
        assert_eq!(blank.to_string(), NOTE_BLANK, "the blank wording");
        let edge = "x".repeat(MAX_NOTE_LENGTH);
        validate_note(&edge).expect("the cap itself is allowed");
        let over =
            validate_note(&format!("{edge}!")).expect_err("one character past the cap is not");
        assert_eq!(
            over.to_string(),
            "Validation failed: Content is too long (maximum is 5000 characters)",
            "the wording names the cap"
        );
        let multibyte = "é".repeat(MAX_NOTE_LENGTH);
        validate_note(&multibyte).expect("characters, not bytes");
    }

    /// The note body demands its `content`.
    #[test]
    fn the_note_body_requires_content() {
        let missing: Result<NoteBody, _> = serde_json::from_value(json!({}));
        assert!(missing.is_err(), "`content` is required");
        let body: NoteBody =
            serde_json::from_value(json!({"content": "looked"})).expect("the shape parses");
        assert_eq!(body.content, "looked", "and arrives verbatim");
    }
}
