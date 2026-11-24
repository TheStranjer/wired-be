//! The `preview` arm of `POST /api/v1/statuses`: [`preview_status`] runs
//! the whole check-and-render pipeline of a create and answers the entity
//! a create would have, writing nothing.
//!
//! The in-memory row (`preview_row`) carries a freshly minted id,
//! `now()` and zero counters, and its fabricated tags (`preview_tags`)
//! exist only for the render.

use chrono::Utc;
use tracing::debug;

use super::{
    CreateStatusParams, RenderedFields,
    build::rendered_fields,
    validate::{Checked, check_body},
};
use crate::{
    entities::{
        account::JsonBool,
        status::{Status, StatusSource},
    },
    error::AppResult,
    models::{hashtag::Hashtag, oauth_app::OauthApp, status, user::User},
    services::mentions::resolve_mentions,
    state::AppState,
    util::id::{new_id, object_ap_id},
};

/// Builds the [`Status`] entity `params` describes **without writing
/// anything**.
///
/// Every check [`crate::services::create_status::create_status`] runs, the same render with its mentions
/// resolved and its tags extracted, and an entity whose `id` is a freshly
/// minted `UUIDv7` (decision 4), whose `created_at` is now and whose
/// counters are zero — Pleroma's `preview` parameter, whose
/// `ActivityPub.create(changes, true)` answers the draft activity and
/// persists none of it.
///
/// Nothing touches `statuses`, `status_mentions`, the hashtag tables or
/// the poll tables, no counter moves, no key is claimed and
/// `after_status_created` never fires. A body carrying a poll is checked
/// the way a post's is and its entity answers `poll: null` — the question
/// is previewed, never stored. The one read the preview does perform is
/// the mention lookup —
/// [`crate::services::create_status::render_status_fields`]'s own, network fetch of
/// an unknown remote account included, since the rendered content is only
/// honest if the mention links are. A body naming a `quote_id` previews
/// the same way: the in-memory row quotes nothing, the original is not
/// gated, and no `quotes_count` moves.
///
/// # Errors
///
/// Those of [`crate::services::create_status::create_status`]'s checks and render: every refusal
/// [`check_body`] owes the submitted body arrives unchanged, so a preview
/// that would not post is refused exactly as the post would be.
pub async fn preview_status(
    state: &AppState,
    user: &User,
    app: Option<&OauthApp>,
    params: &CreateStatusParams,
) -> AppResult<Status> {
    let checked = check_body(
        &state.config,
        &user.default_scope,
        user.sensitive_default.get(),
        params,
    )?;
    let found = resolve_mentions(state, checked.text).await?;
    let fields = rendered_fields(state, checked.text, checked.content_type, &found);
    let mentioned: Vec<User> = found.into_iter().map(|(_, account)| account).collect();
    let tags = preview_tags(&fields.hashtags);
    let row = preview_row(state, user, app, &checked, &fields);
    debug!(
        status = %row.id,
        user = %user.id,
        "status previewed"
    );
    Ok(Status::from_source(
        StatusSource {
            row: &row,
            author: user,
            reply_author: None,
            media: &[],
            mentions: &mentioned,
            tags: &tags,
            reactions: &[],
            card: None,
            favourited: false,
            reblogged: JsonBool(false),
            bookmarked: false,
            bookmark_folder: None,
            pinned_at: None,
            muted: JsonBool(false),
            direct_participation: None,
            quote: None,
            reblog: None,
            poll: None,
            parent_visible: JsonBool(true),
        },
        Some(user),
        None,
        &state.config,
    ))
}

/// The `tags` entries a preview renders: one per usable name
/// [`extract_hashtags`] folded, deduplicated and in the name order
/// [`crate::models::hashtag::for_statuses`] reads stored tags back in. The
/// rows are fabricated — no `hashtags` row is written for a preview — and
/// only the name the entity renders off them is meaningful.
fn preview_tags(names: &[String]) -> Vec<Hashtag> {
    let created_at = Utc::now();
    let mut unique: Vec<String> = names.to_vec();
    unique.sort_unstable();
    unique.dedup();
    unique
        .into_iter()
        .map(|name| Hashtag {
            id: new_id(),
            name,
            inserted_at: created_at,
        })
        .collect()
}

/// The in-memory row a preview renders: a fresh id with the `uri`, `url` and
/// `conversation_id` [`status::insert`] would derive from it (decision 6),
/// every column the checked body decided, zero counters, no links to any
/// other status and `now()` for both timestamps. It is the row a create of
/// this body would have stored — except that it will never be stored.
fn preview_row(
    state: &AppState,
    user: &User,
    app: Option<&OauthApp>,
    checked: &Checked<'_>,
    fields: &RenderedFields,
) -> status::Status {
    let id = new_id();
    let now = Utc::now();
    let base = state.config.base_url.as_str();
    status::Status {
        id,
        uri: object_ap_id(base, id),
        url: Some(format!("{base}/notice/{id}")),
        user_id: user.id,
        content: fields.content.clone(),
        raw_text: checked.text.to_owned(),
        content_type: checked.content_type.to_owned(),
        spoiler_text: checked.spoiler_text.to_owned(),
        sensitive: checked.sensitive,
        visibility: checked.visibility,
        language: checked.language.map(str::to_owned),
        in_reply_to_id: None,
        in_reply_to_account_id: None,
        in_reply_to_ap_id: None,
        conversation_id: id,
        ap_context: None,
        direct_conversation_id: None,
        reblog_of_id: None,
        quote_of_id: None,
        local: true,
        application_name: app.map(|found| found.client_name.clone()),
        application_website: app.and_then(|found| found.website.clone()),
        replies_count: 0,
        reblogs_count: 0,
        favourites_count: 0,
        quotes_count: 0,
        edited_at: None,
        deleted_at: None,
        expires_at: checked.expires_at,
        inserted_at: now,
        updated_at: now,
    }
}

#[cfg(test)]
mod tests {
    use serde_json::{Value, json};
    use uuid::Uuid;

    use super::*;
    use crate::{
        services::create_status::{
            BAD_MEDIA_ID, EMPTY_STATUS, INVALID_CONTENT_TYPE, INVALID_VISIBILITY,
            MAX_SPOILER_CHARS, TOO_MANY_ATTACHMENTS,
            test_support::{
                BASE, key_rows, live_ids, params, poll_rows, refused, state, user, wire,
            },
        },
        util::entities::{hashtag_html, mention_html},
    };
    #[tokio::test]
    async fn a_preview_builds_the_whole_entity() {
        let db = crate::test_db::test_db().await.expect("test database slot");
        let pool = db.pool();
        let app_state = state(pool);
        let alice = user(pool, "looker").await;
        let bob = user(pool, "bob").await;

        let entity = preview_status(
            &app_state,
            &alice,
            None,
            &params(json!({
                "status": "hi @bob and @nobody, see #Rust #rust",
                "preview": true,
            })),
        )
        .await
        .expect("the preview builds");
        let value = serde_json::to_value(&entity).expect("the entity serializes");
        let id = Uuid::parse_str(&entity.id).expect("the preview carries its id");

        assert!(!id.is_nil(), "a freshly minted id");
        assert_eq!(value["id"], id.to_string());
        assert_eq!(
            value["uri"],
            format!("{BASE}/objects/{id}"),
            "the urls a create of this body would derive"
        );
        assert_eq!(value["url"], json!(format!("{BASE}/notice/{id}")));
        assert_eq!(
            value["content"],
            format!(
                "<p>hi {} and @nobody, see {} {}</p>",
                mention_html(&bob.ap_id, "bob"),
                hashtag_html(BASE, "rust"),
                hashtag_html(BASE, "rust"),
            ),
            "the same render the create stores, mentions and tags linkified"
        );
        assert_eq!(
            value["mentions"],
            json!([{ "id": bob.id.to_string(), "username": "bob",
                     "url": bob.ap_id, "acct": "bob" }]),
            "the mentioned account is resolved into the entity"
        );
        assert_eq!(
            value["tags"],
            json!([{ "name": "rust", "url": format!("{BASE}/tags/rust") }]),
            "and the tag once, folded, as the stored render answers it"
        );
        let age = (Utc::now() - entity.created_at).num_seconds();
        assert!((0..120).contains(&age), "created now: {age}s ago");
        assert_eq!(value["reblogs_count"], 0);
        assert_eq!(value["favourites_count"], 0);
        assert_eq!(value["replies_count"], 0);
        assert_eq!(value["pleroma"]["quotes_count"], 0);
        assert_eq!(value["account"]["acct"], "looker", "rendered as its author");
        assert_eq!(
            value["pleroma"]["content"],
            json!({ "text/plain": "hi @bob and @nobody, see #Rust #rust" }),
            "the source text beside the render"
        );
    }

    #[tokio::test]
    async fn a_preview_writes_no_row_anywhere() {
        let db = crate::test_db::test_db().await.expect("test database slot");
        let pool = db.pool();
        let app_state = state(pool);
        let alice = user(pool, "look-but-dont").await;
        let bob = user(pool, "bob").await;

        let entity = preview_status(
            &app_state,
            &alice,
            None,
            &params(json!({
                "status": "hi @bob, see #Rust",
                "preview": true,
            })),
        )
        .await
        .expect("the preview builds");
        let id = Uuid::parse_str(&entity.id).expect("the preview carries its id");

        assert!(live_ids(pool).await.is_empty(), "no status was written");
        assert_eq!(key_rows(pool).await, 0, "no key was claimed");
        let mentions: (i64,) = sqlx::query_as("SELECT count(*) FROM status_mentions")
            .fetch_one(pool)
            .await
            .expect("the mention count reads");
        let tags: (i64,) = sqlx::query_as("SELECT count(*) FROM hashtags")
            .fetch_one(pool)
            .await
            .expect("the tag count reads");
        let tag_links: (i64,) = sqlx::query_as("SELECT count(*) FROM status_hashtags")
            .fetch_one(pool)
            .await
            .expect("the tag link count reads");
        assert_eq!(
            (mentions.0, tags.0, tag_links.0),
            (0, 0, 0),
            "no mention link, no tag row and no tag link were written — \
             the mentions and tags the entity answers are resolved, not stored"
        );
        let counted = crate::models::user::get_by_id(pool, alice.id)
            .await
            .unwrap()
            .expect("the author exists");
        assert_eq!(counted.statuses_count, 0, "no count moved");
        assert!(
            status::get_by_id(pool, id).await.unwrap().is_none(),
            "and the previewed id names no row"
        );
        let value = serde_json::to_value(&entity).expect("the entity serializes");
        assert_eq!(
            value["mentions"],
            json!([{ "id": bob.id.to_string(), "username": "bob",
                     "url": bob.ap_id, "acct": "bob" }]),
            "the entity resolves the mention although nothing links it"
        );
    }

    #[tokio::test]
    async fn two_previews_of_one_body_are_two_fresh_ids() {
        let db = crate::test_db::test_db().await.expect("test database slot");
        let pool = db.pool();
        let app_state = state(pool);
        let alice = user(pool, "twice-seen").await;
        let asked = params(json!({"status": "same words", "preview": "true"}));

        let first = preview_status(&app_state, &alice, None, &asked)
            .await
            .expect("the first preview builds");
        let second = preview_status(&app_state, &alice, None, &asked)
            .await
            .expect("the second preview builds");

        assert_ne!(first.id, second.id, "every preview mints its own id");
        assert_eq!(first.content, second.content, "and the same render");
        assert!(live_ids(pool).await.is_empty(), "and neither wrote");
    }

    #[tokio::test]
    async fn a_preview_refuses_what_a_creation_refuses() {
        let db = crate::test_db::test_db().await.expect("test database slot");
        let pool = db.pool();
        let app_state = state(pool);
        let alice = user(pool, "wishful").await;

        for (value, message) in [
            (json!({"status": "  ", "preview": true}), EMPTY_STATUS),
            (
                json!({"status": "hi", "visibility": "sideways", "preview": true}),
                INVALID_VISIBILITY,
            ),
            (
                json!({"status": "hi", "content_type": "text/rtf", "preview": true}),
                INVALID_CONTENT_TYPE,
            ),
            (
                json!({"status": "hi", "media_ids": ["not-an-id"], "preview": true}),
                BAD_MEDIA_ID,
            ),
            (
                json!({ "status": "hi",
                        "media_ids": ["", "", "", "", ""],
                        "preview": true, }),
                TOO_MANY_ATTACHMENTS,
            ),
            (
                json!({
                    "status": "hi",
                    "spoiler_text": "x".repeat(MAX_SPOILER_CHARS + 1),
                    "preview": true,
                }),
                "Validation failed: Spoiler text is too long (maximum is 200 characters)",
            ),
        ] {
            let err = preview_status(&app_state, &alice, None, &params(value))
                .await
                .expect_err("the body above is refused like any post");
            refused(&wire(err).await, message);
        }
        assert!(
            live_ids(pool).await.is_empty(),
            "not one refused preview wrote a row"
        );
        assert_eq!(key_rows(pool).await, 0, "or claimed a key");
    }

    /// A preview whose body carries a poll runs the whole check and render
    /// and stores nothing: the entity answers `poll: null`, the `polls`
    /// and `poll_options` tables stay as empty as the `statuses` one, and
    /// the poll still stood in for the empty text the way a post's would.
    #[tokio::test]
    async fn a_previewed_poll_answers_null_and_is_stored_nowhere() {
        let db = crate::test_db::test_db().await.expect("test database slot");
        let pool = db.pool();
        let app_state = state(pool);
        let alice = user(pool, "poll-looker").await;

        let entity = preview_status(
            &app_state,
            &alice,
            None,
            &params(json!({
                "status": "   ",
                "preview": true,
                "poll": {
                    "options": ["a", "b"],
                    "expires_in": 3600,
                    "multiple": true,
                },
            })),
        )
        .await
        .expect("the poll stands in for the empty text, previewed or not");

        let value = serde_json::to_value(&entity).expect("the entity serializes");
        assert_eq!(
            value["poll"],
            Value::Null,
            "a preview has stored nothing to render"
        );
        assert!(live_ids(pool).await.is_empty(), "no status row");
        assert_eq!(poll_rows(pool).await, 0, "no polls row");
        let options: (i64,) = sqlx::query_as("SELECT count(*) FROM poll_options")
            .fetch_one(pool)
            .await
            .expect("the option count reads");
        assert_eq!(options.0, 0, "and no option rows either");
        assert_eq!(key_rows(pool).await, 0, "and no key was claimed");
        let counted = crate::models::user::get_by_id(pool, alice.id)
            .await
            .unwrap()
            .expect("the author exists");
        assert_eq!(counted.statuses_count, 0, "and no count moved");
    }
}
