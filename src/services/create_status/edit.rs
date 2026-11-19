//! The edit arm, `PUT /api/v1/statuses/{id}`.
//!
//! [`EditStatusParams`] as the client wrote it, `merged_body` over the
//! stored row, and [`edit_status`] — which pushes the merge through the
//! very same [`check_body`] and [`render_status_fields`] a creation runs,
//! so an edit renders a post exactly the way posting the same text would.
//!
//! [`check_body`]: crate::services::create_status::check_body
//! [`render_status_fields`]: crate::services::create_status::render_status_fields

use serde::Deserialize;
use tracing::debug;

use super::{CreateStatusParams, render_status_fields, validate::check_body};
use crate::{
    entities::status::Status,
    error::AppResult,
    models::{status, status_edit, user::User},
    services::render_status::{ViewerMode, render_status},
    state::AppState,
    util::id::parse_id,
};

/// The body of `PUT /api/v1/statuses/{id}`, as the client wrote it.
///
/// The five text parameters mirror [`CreateStatusParams`]'s without
/// `visibility` — an edit never moves a post to a different audience
/// (Pleroma's `make_update_data` carries the stored visibility forward) —
/// and every one is optional: only the parameters the body presents change
/// the post ([`edit_status`]). `media_ids` is decision 10's id-array —
/// JSON's `["a","b"]` or the form's `media_ids[]=a&media_ids[]=b`, both
/// read as a list of id text — and `sensitive` the same
/// [`flex`] boolean the create body accepts.
///
/// [`flex`]: crate::extract::flex
#[derive(Debug, Default, Deserialize)]
pub struct EditStatusParams {
    /// The new post text; trimmed and capped the way creation caps it.
    #[serde(default)]
    status: Option<String>,
    /// The new content warning, stored verbatim; at most
    /// `MAX_SPOILER_CHARS` characters.
    #[serde(default)]
    spoiler_text: Option<String>,
    /// Whether the post starts folded.
    #[serde(default, deserialize_with = "crate::extract::flex::opt_bool")]
    sensitive: Option<bool>,
    /// The new content language tag; empty clears it.
    #[serde(default)]
    language: Option<String>,
    /// The `MIME` type [`status`](Self::status) was written in.
    #[serde(default)]
    content_type: Option<String>,
    /// The post's whole new attachment set, as id text; ids that name no
    /// attachment the author may attach drop out — see [`edit_status`].
    #[serde(default)]
    media_ids: Option<Vec<String>>,
}

/// The edit body merged over the stored row: every [`CreateStatusParams`]
/// field carrying the submitted value where the body presented one and the
/// row's own value where it did not. The merged body is what [`check_body`]
/// reads, so the create path's checks run on the post as it will stand —
/// and a field nobody edited can never fail a check it already passed.
fn merged_body(row: &status::Status, params: &EditStatusParams) -> CreateStatusParams {
    CreateStatusParams {
        status: kept(params.status.clone(), || row.raw_text.clone()),
        spoiler_text: kept(params.spoiler_text.clone(), || row.spoiler_text.clone()),
        sensitive: Some(params.sensitive.unwrap_or(row.sensitive)),
        visibility: Some(row.visibility.as_str().to_owned()),
        language: params.language.clone().or_else(|| row.language.clone()),
        content_type: kept(params.content_type.clone(), || row.content_type.clone()),
        // An edit is never a preview, never a schedule and never replays:
        // the create's decision parameters have no part in this merge. An
        // edit never moves the expiry either — `expires_in` is a creation
        // parameter, the stored `expires_at` rides along untouched
        // (Pleroma's `make_update_data` carries it forward the same way)
        // — and never its parent: an edit rewrites what was said in the
        // thread it stands in, and `in_reply_to_id` is not an edit
        // parameter. Neither is a poll: a post's question is fixed when it
        // is asked, and neither is a quote: `quote_id` is a creation
        // parameter, the stored `quote_of_id` rides along untouched. The
        // edit's own `media_ids` ride [`status_edit::EditParams`] through
        // [`crate::models::status_edit::update_status`]'s replacement arm,
        // which drops what the author may not attach instead of refusing
        // it, so the merge carries none and [`check_body`] never sees the
        // create's attachment rules.
        quote_id: None,
        preview: None,
        expires_in: None,
        in_reply_to_id: None,
        in_reply_to_conversation_id: None,
        poll: None,
        media_ids: None,
        scheduled_at: None,
    }
}

/// One optional edit parameter against the value the stored row keeps in
/// its place: the submitted one wins outright — even the empty spellings
/// that mean "clear this field" — and only its absence asks for `stored`.
fn kept<T, F: FnOnce() -> T>(submitted: Option<T>, stored: F) -> Option<T> {
    submitted.or_else(|| Some(stored()))
}

/// Edits the status `row` as `params` describes and answers the [`Status`]
/// entity as `user` — its author, which is the endpoint's precondition for
/// reaching this function at all — sees the post after the write.
///
/// `merged_body` resolves which parameters the body presented,
/// [`check_body`] refuses the merge exactly as it refuses a creation
/// (422 `Cannot post an empty status without attachments`,
/// `The status is over the character limit`, the spoiler and language
/// length wordings, `Invalid content_type`), and [`render_status_fields`]
/// renders the merged text into the content, mention list and tag list
/// creation would have written for it. The whole new state then travels to
/// [`status_edit::update_status`], whose one transaction records the
/// superseded version (and the `edited_at` stamp) only when content,
/// warning, sensitive flag or media actually changed, and replaces the
/// post's mentions, hashtags and media.
///
/// Of the edit's own `media_ids`, only the ids the author may attach to
/// this post survive to the write: rows nobody owns, rows another status
/// claims and text that is no id at all name nothing and drop out, as
/// Pleroma's `CommonAPI.Utils.attachments_from_ids` rejects its nils.
///
/// `None` is the row a delete took between the endpoint's gate and this
/// write; the endpoint answers that its shared 404.
///
/// # Errors
///
/// [`crate::error::AppError::Unprocessable`] for the merged body's refusals, through
/// [`crate::services::create_status::check_body`]; the database failures of the write and of the render
/// arrive through `?` as [`crate::error::AppError::Database`].
pub async fn edit_status(
    state: &AppState,
    user: &User,
    row: &status::Status,
    params: EditStatusParams,
) -> AppResult<Option<Status>> {
    let merged = merged_body(row, &params);
    let checked = check_body(
        &state.config,
        row.visibility.as_str(),
        row.sensitive,
        &merged,
    )?;
    let fields = render_status_fields(state, checked.text, checked.content_type).await?;
    let media_ids = params
        .media_ids
        .map(|ids| ids.iter().filter_map(|id| parse_id(id)).collect());
    let edit = status_edit::EditParams {
        content: fields.content,
        raw_text: checked.text.to_owned(),
        content_type: checked.content_type.to_owned(),
        spoiler_text: checked.spoiler_text.to_owned(),
        sensitive: checked.sensitive,
        language: checked.language.map(str::to_owned),
        mentioned_user_ids: fields.mentioned_user_ids,
        hashtags: fields.hashtags,
        media_ids,
    };
    let Some(updated) = status_edit::update_status(&state.db, row.id, edit).await? else {
        return Ok(None);
    };
    debug!(
        status = %updated.id,
        user = %user.id,
        edited = updated.edited_at.is_some(),
        "status edited"
    );
    render_status(state, ViewerMode::user(user), &updated).await
}

#[cfg(test)]
mod tests {
    use serde_json::{Value, json};
    use uuid::Uuid;

    use super::*;
    use crate::{
        config::Config,
        models::status::Visibility,
        services::create_status::{
            EMPTY_STATUS, INVALID_CONTENT_TYPE, MAX_SPOILER_CHARS,
            test_support::{created, posted, state, user},
        },
    };
    /// An edit body as `ApiBody` hands it over, from the JSON shape the
    /// extractor builds for either accepted content type.
    fn body(value: Value) -> EditStatusParams {
        crate::extract::into_params::<EditStatusParams>(value)
            .expect("every body in these tests fits the edit parameter type")
    }

    #[test]
    fn every_edit_parameter_is_optional() {
        let empty = body(json!({}));
        assert_eq!(empty.status, None, "no text means the text stays");
        assert_eq!(empty.spoiler_text, None, "as does the warning");
        assert_eq!(empty.sensitive, None, "the flag");
        assert_eq!(empty.language, None, "the tag");
        assert_eq!(empty.content_type, None, "the type");
        assert_eq!(empty.media_ids, None, "and nothing is said about media");

        let full = body(json!({
            "status": "new words",
            "spoiler_text": "careful",
            "sensitive": "on",
            "language": "eo",
            "content_type": "text/markdown",
            "media_ids": ["a", "b"],
        }));
        assert_eq!(full.status.as_deref(), Some("new words"));
        assert_eq!(full.spoiler_text.as_deref(), Some("careful"));
        assert_eq!(full.sensitive, Some(true), "the form spellings ride along");
        assert_eq!(full.language.as_deref(), Some("eo"));
        assert_eq!(full.content_type.as_deref(), Some("text/markdown"));
        assert_eq!(
            full.media_ids,
            Some(vec!["a".to_owned(), "b".to_owned()]),
            "decision 10's array arrives as id text either way"
        );
    }

    #[tokio::test]
    async fn the_merge_carries_the_submitted_field_and_the_stored_rest() {
        let db = crate::test_db::test_db().await.expect("test database slot");
        let pool = db.pool();
        let app_state = state(pool);
        let alice = user(pool, "merger").await;
        let entity = created(
            &app_state,
            &alice,
            CreateStatusParams {
                spoiler_text: Some("kept warning".to_owned()),
                sensitive: Some(true),
                visibility: Some("private".to_owned()),
                language: Some("eo".to_owned()),
                content_type: Some("text/markdown".to_owned()),
                ..posted("the stored text")
            },
        )
        .await;
        let id = Uuid::parse_str(&entity.id).expect("the entity carries its id");
        let row = status::get_by_id(pool, id)
            .await
            .unwrap()
            .expect("the fixture post is stored");

        let merged = merged_body(&row, &body(json!({"status": "new words"})));
        assert_eq!(
            merged.status.as_deref(),
            Some("new words"),
            "the submitted field wins"
        );
        assert_eq!(
            merged.spoiler_text.as_deref(),
            Some("kept warning"),
            "and every field the body left out arrives as the row has it"
        );
        assert_eq!(merged.sensitive, Some(true));
        assert_eq!(merged.visibility.as_deref(), Some("private"));
        assert_eq!(merged.language.as_deref(), Some("eo"));
        assert_eq!(merged.content_type.as_deref(), Some("text/markdown"));

        let checked = check_body(&Config::for_tests(), "public", false, &merged)
            .expect("a merge of a stored post is always checkable");
        assert_eq!(checked.text, "new words", "the new text goes through");
        assert_eq!(checked.visibility, Visibility::Private, "the scope stays");
        assert!(checked.sensitive, "so does the flag");
    }

    #[tokio::test]
    async fn an_empty_language_submitted_clears_where_absence_keeps() {
        let db = crate::test_db::test_db().await.expect("test database slot");
        let pool = db.pool();
        let app_state = state(pool);
        let alice = user(pool, "untagger").await;
        let entity = created(
            &app_state,
            &alice,
            CreateStatusParams {
                language: Some("de".to_owned()),
                ..posted("hallo")
            },
        )
        .await;
        let id = Uuid::parse_str(&entity.id).expect("the entity carries its id");
        let row = status::get_by_id(pool, id)
            .await
            .unwrap()
            .expect("the fixture post is stored");

        let kept = merged_body(&row, &body(json!({"status": "hi again"})));
        assert_eq!(kept.language.as_deref(), Some("de"), "absent keeps the tag");
        let cleared = merged_body(&row, &body(json!({"language": ""})));
        assert_eq!(
            cleared.language.as_deref(),
            Some(""),
            "a submitted empty tag merges as the empty string, which \
             `check_body` reads as no tag at all"
        );
        assert_eq!(
            check_body(&Config::for_tests(), "public", false, &cleared)
                .expect("clearing is always checkable")
                .language,
            None
        );
    }

    #[tokio::test]
    async fn editing_through_the_service_supersedes_a_version_and_rewrites_the_row() {
        let db = crate::test_db::test_db().await.expect("test database slot");
        let pool = db.pool();
        let app_state = state(pool);
        let alice = user(pool, "editor-service").await;
        let entity = created(&app_state, &alice, posted("first version #keep")).await;
        let id = Uuid::parse_str(&entity.id).expect("the entity carries its id");
        let row = status::get_by_id(pool, id)
            .await
            .unwrap()
            .expect("the fixture post is stored");

        let edited = edit_status(
            &app_state,
            &alice,
            &row,
            body(json!({"status": "second version"})),
        )
        .await
        .expect("the edit writes")
        .expect("the status is live");
        assert_eq!(edited.content, "<p>second version</p>", "the new render");
        assert!(
            edited.edited_at.is_some(),
            "a superseded version stamps the edit"
        );

        let stored = status::get_by_id(pool, id)
            .await
            .unwrap()
            .expect("the edited status is stored");
        assert_eq!(stored.raw_text, "second version");
        assert_eq!(
            stored.spoiler_text, "",
            "the fields nobody sent are the fields nobody moved"
        );
        let history = status_edit::list_for_status(pool, id)
            .await
            .expect("the revisions read");
        assert_eq!(history.len(), 1, "the first version is on record");
        assert_eq!(history[0].raw_text, "first version #keep");
        assert_eq!(
            history[0].content, entity.content,
            "the snapshot carries the old render, linkified and all — not \
             just the old source"
        );
    }

    #[tokio::test]
    async fn a_rejected_edit_writes_nothing() {
        let db = crate::test_db::test_db().await.expect("test database slot");
        let pool = db.pool();
        let app_state = state(pool);
        let alice = user(pool, "refused-editor").await;
        let entity = created(&app_state, &alice, posted("untouched")).await;
        let id = Uuid::parse_str(&entity.id).expect("the entity carries its id");
        let row = status::get_by_id(pool, id)
            .await
            .unwrap()
            .expect("the fixture post is stored");

        let long = format!(
            "Validation failed: Spoiler text is too long (maximum is {MAX_SPOILER_CHARS} characters)"
        );
        for (value, message) in [
            (json!({"status": "   "}), EMPTY_STATUS),
            (json!({"content_type": "text/rtf"}), INVALID_CONTENT_TYPE),
            (
                json!({"spoiler_text": "x".repeat(MAX_SPOILER_CHARS + 1)}),
                long.as_str(),
            ),
        ] {
            let err = edit_status(&app_state, &alice, &row, body(value))
                .await
                .expect_err("the body above is refused before anything is written");
            assert_eq!(err.to_string(), message, "the refusal wording");
        }

        let history = status_edit::list_for_status(pool, id)
            .await
            .expect("the revisions read");
        assert!(history.is_empty(), "no refused edit recorded a revision");
        let stored = status::get_by_id(pool, id)
            .await
            .unwrap()
            .expect("the fixture post is stored");
        assert_eq!(stored.content, "<p>untouched</p>", "or moved the row");
        assert!(stored.edited_at.is_none(), "or stamped it either");
    }
}
