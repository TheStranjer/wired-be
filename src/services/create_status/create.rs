//! [`create_status`]: the database-facing half of the endpoint.
//!
//! The shared [`RenderedFields`] type lives here too: it is what the create's
//! render step hands the write. `create_status` prepares the row through
//! `prepared_new_status`, inserts it and the body's poll on one
//! transaction, and answers the author's view of the fresh post through
//! `created_entity`; the file's own `quote_target` and `quoted_original`
//! resolve a submitted `quote_id` to the live, viewable original before
//! anything is written. The themed children [`super::create_render`],
//! [`super::create_media`] and [`super::create_poll`] hold the create
//! path's tests for rendering, attachments and stored questions.
//!
//! [`create_status`]: crate::services::create_status::create_status

use sqlx::PgConnection;
use tracing::debug;
use uuid::Uuid;

use super::{
    CheckedPoll, CreateStatusParams, QUOTE_NOT_ALLOWED, after_status_created,
    idempotency::create_status_with_key, quote_consent::require_consent,
};
use crate::{
    entities::status::Status,
    error::{AppError, AppResult},
    models::{
        oauth_app::OauthApp,
        poll::{self, PollParams},
        status::{self, Visibility},
        status_visibility,
        user::User,
    },
    services::render_status::{ViewerMode, render_status},
    state::AppState,
    util::id::parse_id,
};

/// What reading a status text came to: the HTML to store, and the two
/// entity lists that go with it in the order the text carries them.
#[derive(Debug)]
pub struct RenderedFields {
    /// The rendered, linkified HTML for the `content` column.
    pub content: String,
    /// The accounts the text @-mentioned, in mention order.
    pub mentioned_user_ids: Vec<Uuid>,
    /// The hashtags the text carries, as [`crate::util::entities::written_hashtags`]
    /// spells them (the author's casing).
    pub hashtags: Vec<String>,
}

/// Creates the status `params` describes as a local post by `user` and
/// answers its [`Status`] entity as the author sees it.
///
/// Nothing is written before [`crate::services::create_status::check_body`]
/// has accepted the body: it
/// supplies the author's own two defaults — the row's
/// [`default_scope`](User::default_scope) and
/// [`sensitive_default`](User::sensitive_default), Pleroma reads the same
/// pair off the user — and [`create_status`] does the rest: the quote
/// target (the private `quote_target` step in the shared
/// `prepared_new_status` below, resolved before anything is written), the
/// render and the entity lists (the [`crate::services::create_status::render_status_fields`]
/// step below),
/// the stored row with its mention and hashtag links, its claimed
/// attachments and the author's moved counters, the body's poll stored
/// beside it when it carried one (the `store_poll` step below) — and the
/// quoted original's `quotes_count`, stepped by [`status::insert`] along
/// with the row — and the render for the author. `app` — the client
/// that issued the presenting
/// token, `None` when the caller names none — is recorded as the post's
/// `application` pair, Pleroma's `put_application`.
///
/// The body's `media_ids` ride the stored row: [`status::insert`] claims
/// exactly the poster's own uploads no status has taken yet, on the insert's
/// own transaction, each at its position in the submitted order, and one id
/// the poster may not claim refuses the whole write (422 `Media not found`,
/// the media model's gate — see [`crate::models::media_attachment`]).
///
/// When the body names an `in_reply_to_id`, the shared `prepared_new_status`
/// step resolves the parent before anything renders: an unanswerable parent
/// is the shared 404, and an answerable one lends the row its thread, author
/// link, capped audience, fallback language and auto-mentions (Pleroma's
/// `ActivityDraft` reply arms), with the parent's own `replies_count`
/// stepped by [`status::insert`].
///
/// # Errors
///
/// [`AppError::Unprocessable`] for the body's own refusals, through
/// [`crate::services::create_status::check_body`] — attachments included, both the
/// [`crate::config::Config::max_media_attachments`] cap and the write's `Media not found`
/// gate — and for a quote target that is visible but not quotable, through
/// the private `quote_target` step; [`AppError::NotFound`] for an
/// `in_reply_to_id` that names no status the poster may see and likewise for
/// a `quote_id` naming a malformed, unknown, deleted or unseen status,
/// through `quote_target`; [`AppError::Internal`] for the one row this
/// function wrote that the renderer refuses to answer (unreachable for a
/// fresh local post, which is neither a tombstone nor a reblog). Any
/// database failure arrives through `?` as [`AppError::Database`].
pub async fn create_status(
    state: &AppState,
    user: &User,
    app: Option<&OauthApp>,
    params: CreateStatusParams,
) -> AppResult<Status> {
    create_status_with_key(state, user, app, params, None).await
}

/// Stores the checked body's poll beside its freshly inserted status row,
/// on the create's open transaction: [`poll::create`] with the whole
/// [`CheckedPoll`] reshaped into [`PollParams`]. A body that carried no
/// poll writes nothing here, so a plain create is the same write it always
/// was; and a create that never commits — an idempotency-key race lost
/// after this point — leaves no stray poll behind, because the rollback
/// takes the poll and its options down with the status: a stored poll can
/// only ever belong to a status that was committed beside it.
///
/// # Errors
///
/// Any database failure of the poll or option insert arrives through `?`
/// as [`AppError::Database`].
pub(super) async fn store_poll(
    conn: &mut PgConnection,
    status_id: Uuid,
    poll: Option<&CheckedPoll>,
) -> AppResult<()> {
    let Some(poll) = poll else {
        return Ok(());
    };
    poll::create(
        conn,
        status_id,
        &PollParams {
            options: poll.options.clone(),
            expires_at: poll.expires_at,
            multiple: poll.multiple,
            hide_totals: poll.hide_totals,
            ..PollParams::default()
        },
    )
    .await
    .map(drop)
}

/// The shared tail of both create arms: the fresh row's log line, the
/// [`statuses.expire`](crate::jobs::statuses::KIND_EXPIRE) job a row carrying
/// an expiry schedules for itself (see [`schedule_expiration`]), its
/// [`Status`] entity as `user` — the author — sees it, and the
/// [`after_status_created`] hook every consumer of a new post hangs off.
///
/// [`schedule_expiration`]: crate::jobs::statuses::schedule_expiration
///
/// # Errors
///
/// [`AppError::Internal`] for the one row this module wrote that the
/// renderer refuses to answer (unreachable for a fresh local post, which is
/// neither a tombstone nor a reblog); any database failure behind the render
/// arrives through `?` as [`AppError::Database`].
pub(super) async fn created_entity(
    state: &AppState,
    user: &User,
    row: status::Status,
) -> AppResult<Status> {
    debug!(
        status = %row.id,
        user = %user.id,
        visibility = %row.visibility,
        "status created"
    );
    crate::jobs::statuses::schedule_expiration(state, &row).await;
    // The hook runs first: it records the `quote_requests` row of a quote of
    // a remote post, and the entity reads that row to render `pending`.
    after_status_created(state, &row).await;
    let entity = render_status(state, ViewerMode::user(user), &row)
        .await?
        // Unreachable for what was written a moment ago — live, local and no
        // reblog, so the renderer has nothing to drop it for — and a 500
        // rather than a panic if it ever happens.
        .ok_or_else(|| AppError::Internal(format!("the status {} did not render", row.id)))?;
    Ok(entity)
}

/// The original a body's `quote_id` names: `None` when the body named no
/// target (absent, `null` or the exact empty string, as Pleroma ignores it —
/// posting without a quote is untouched), the
/// quotable original's row once the target has been read and checked; its
/// id is the `quote_of_id` to store.
///
/// The checks run in the order the ticket pins them, and every miss on the
/// target is decision 8's shared 404 so a probe learns nothing:
/// [`parse_id`] refuses text that is not an id, [`status::get_by_id`] hides
/// unknown rows and tombstones alike, a reblog row is walked through to the
/// original it boosts — the quote is always of the post underneath, and that
/// is the id stored — and [`status_visibility::can_view`] hides what the
/// poster may not see. Past the gate, only a [`Visibility::Public`] or
/// [`Visibility::Unlisted`] original may be quoted, its own author excepted:
/// whoever may see their own `private`, `direct` or `local` post may quote
/// it, and anybody quoting somebody else's gets the 422 wording of
/// [`QUOTE_NOT_ALLOWED`]. Last, the quoted author's own policy is asked
/// (see [`require_consent`]): a target whose `quote_approval` is `denied`
/// for the poster is the 422 Mastodon answers it with.
///
/// # Errors
///
/// [`AppError::NotFound`] for a malformed, unknown, deleted or unseen
/// target, [`AppError::Unprocessable`] with [`QUOTE_NOT_ALLOWED`] for a
/// visible but unquotable one, and with `QUOTE_DENIED` for one whose author
/// refuses the poster. Any database failure arrives through `?` as
/// [`AppError::Database`].
pub(super) async fn quote_target(
    state: &AppState,
    user: &User,
    quote_id: Option<&str>,
) -> AppResult<Option<status::Status>> {
    let Some(text) = quote_id.filter(|text| !text.is_empty()) else {
        return Ok(None);
    };
    let id = parse_id(text).ok_or(AppError::NotFound)?;
    let row = status::get_by_id(&state.db, id)
        .await?
        .ok_or(AppError::NotFound)?;
    let original = quoted_original(state, row).await?;
    let target = status_visibility::require_visible(&state.db, original, Some(user)).await?;
    if !matches!(target.visibility, Visibility::Public | Visibility::Unlisted)
        && target.user_id != user.id
    {
        return Err(AppError::Unprocessable(QUOTE_NOT_ALLOWED.to_owned()));
    }
    require_consent(state, user, &target).await?;
    Ok(Some(target))
}

/// Follows `row`'s reblog links through to the original a quote of it names:
/// reblogs are rows too (decision 6), a client that names a boost means the
/// post underneath it, and a broken or tombstoned link is the same 404 any
/// other miss on the target is. Chains longer than one link do not occur in
/// stored data, but the loop walks one to its end all the same.
async fn quoted_original(state: &AppState, row: status::Status) -> AppResult<status::Status> {
    let mut current = row;
    while let Some(original_id) = current.reblog_of_id {
        current = status::get_by_id(&state.db, original_id)
            .await?
            .ok_or(AppError::NotFound)?;
    }
    Ok(current)
}

#[cfg(test)]
mod tests {
    use chrono::Utc;
    use serde_json::{Value, json};

    use super::*;
    use crate::services::create_status::{
        EMPTY_STATUS, INVALID_CONTENT_TYPE, INVALID_VISIBILITY,
        test_support::{BASE, created, posted, refused, state, stored, user, wire},
    };
    #[tokio::test]
    async fn a_plain_post_stores_every_column_the_body_decided() {
        let db = crate::test_db::test_db().await.expect("test database slot");
        let pool = db.pool();
        let app_state = state(pool);
        let alice = user(pool, "alice").await;

        let entity = created(
            &app_state,
            &alice,
            CreateStatusParams {
                spoiler_text: Some("cats".to_owned()),
                sensitive: Some(true),
                visibility: Some("local".to_owned()),
                language: Some("eo".to_owned()),
                content_type: Some("text/markdown".to_owned()),
                ..posted("  hello **world**  ")
            },
        )
        .await;
        let row = stored(pool, &entity).await;

        assert_eq!(row.user_id, alice.id, "the author is the row's");
        assert!(row.local, "a posted status is a local one");
        assert_eq!(row.uri, format!("{BASE}/objects/{}", row.id));
        assert_eq!(
            row.url,
            Some(format!("{BASE}/notice/{}", row.id)),
            "and the human URL derives from the same id"
        );
        assert_eq!(
            row.raw_text, "hello **world**",
            "the source text is the trimmed one"
        );
        assert_eq!(
            row.content, "<p>hello <strong>world</strong></p>",
            "the content is the format rendered"
        );
        assert_eq!(row.content_type, "text/markdown");
        assert_eq!(row.spoiler_text, "cats", "the warning is stored");
        assert!(row.sensitive, "so is the flag");
        assert_eq!(row.visibility, Visibility::Local, "and the scope");
        assert_eq!(row.language.as_deref(), Some("eo"), "and the tag");
        assert_eq!(row.conversation_id, row.id, "a root post is its own thread");
        assert!(
            row.application_name.is_none() && row.application_website.is_none(),
            "no application is named, so the block stays empty"
        );

        let value = serde_json::to_value(&entity).expect("the entity serializes");
        assert_eq!(value["id"], row.id.to_string(), "the entity is the row");
        assert_eq!(value["uri"], row.uri, "with its object id");
        assert_eq!(value["url"], json!(row.url), "and its human URL");
        assert_eq!(value["content"], row.content, "and the render");
        assert_eq!(value["visibility"], "local", "as stored, verbatim");
        assert_eq!(value["sensitive"], true);
        assert_eq!(value["spoiler_text"], "cats");
        assert_eq!(value["language"], "eo");
        assert_eq!(value["application"], Value::Null);
        assert_eq!(value["account"]["acct"], "alice", "rendered as the author");
        assert_eq!(
            value["pleroma"]["content"],
            json!({ "text/plain": "hello **world**" }),
            "the source text answers in the Pleroma block"
        );

        let counted = crate::models::user::get_by_id(pool, alice.id)
            .await
            .unwrap()
            .expect("the author exists");
        assert_eq!(
            counted.statuses_count, 1,
            "the author counts the post they made"
        );
    }

    #[tokio::test]
    async fn the_application_of_the_token_is_recorded_and_rendered() {
        let db = crate::test_db::test_db().await.expect("test database slot");
        let pool = db.pool();
        let app_state = state(pool);
        let alice = user(pool, "client-author").await;
        let app = OauthApp {
            id: Uuid::now_v7(),
            client_name: "Wired Client".to_owned(),
            redirect_uris: "https://client.example/callback".to_owned(),
            scopes: "write:statuses".to_owned(),
            website: Some("https://client.example".to_owned()),
            client_id: "client-id".to_owned(),
            client_secret: "client-secret".to_owned(),
            trusted: false,
            user_id: Some(alice.id),
            inserted_at: Utc::now(),
            updated_at: Utc::now(),
        };

        let entity = create_status(&app_state, &alice, Some(&app), posted("from a client"))
            .await
            .expect("the post creates");
        let row = stored(pool, &entity).await;
        assert_eq!(
            row.application_name.as_deref(),
            Some("Wired Client"),
            "the client name is stored"
        );
        assert_eq!(
            row.application_website.as_deref(),
            Some("https://client.example"),
            "and its website"
        );

        let value = serde_json::to_value(&entity).expect("the entity serializes");
        assert_eq!(
            value["application"],
            json!({ "name": "Wired Client", "website": "https://client.example" }),
            "both answer as the entity's application block"
        );

        let nameless = created(&app_state, &alice, posted("no client at all")).await;
        let bare = serde_json::to_value(&nameless).expect("the entity serializes");
        assert_eq!(
            bare["application"],
            Value::Null,
            "a post with no named client renders no block"
        );
    }

    #[tokio::test]
    async fn a_refused_body_writes_nothing() {
        let db = crate::test_db::test_db().await.expect("test database slot");
        let pool = db.pool();
        let app_state = state(pool);
        let alice = user(pool, "refused-author").await;

        let err = create_status(&app_state, &alice, None, CreateStatusParams::default())
            .await
            .expect_err("an absent text is refused");
        refused(&wire(err).await, EMPTY_STATUS);
        refused(
            &wire(
                create_status(
                    &app_state,
                    &alice,
                    None,
                    CreateStatusParams {
                        visibility: Some("sideways".to_owned()),
                        ..posted("hi")
                    },
                )
                .await
                .expect_err("an unknown scope is refused"),
            )
            .await,
            INVALID_VISIBILITY,
        );
        refused(
            &wire(
                create_status(
                    &app_state,
                    &alice,
                    None,
                    CreateStatusParams {
                        content_type: Some("text/rtf".to_owned()),
                        ..posted("hi")
                    },
                )
                .await
                .expect_err("an unknown format is refused"),
            )
            .await,
            INVALID_CONTENT_TYPE,
        );

        let rows: (i64,) = sqlx::query_as("SELECT count(*) FROM statuses")
            .fetch_one(pool)
            .await
            .unwrap();
        assert_eq!(rows.0, 0, "not one request above wrote a row");
        let counted = crate::models::user::get_by_id(pool, alice.id)
            .await
            .unwrap()
            .expect("the author exists");
        assert_eq!(counted.statuses_count, 0, "and no count moved");
    }
}
