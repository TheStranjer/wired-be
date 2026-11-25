//! The `preview` arm of `POST /api/v1/statuses`: [`preview_status`] runs
//! the same `prepared_new_status` step a create runs and answers the
//! entity a create would have, writing nothing.
//!
//! The in-memory row (`preview_row`) carries a freshly minted id,
//! `now()` and zero counters, and its fabricated tags (`preview_tags`)
//! exist only for the render.

use chrono::Utc;
use tracing::debug;

use super::{CreateStatusParams, Prepared, prepared_new_status};
use crate::{
    entities::{
        account::JsonBool,
        status::{Status, StatusSource},
    },
    error::AppResult,
    models::{
        hashtag::Hashtag,
        oauth_app::OauthApp,
        status::{self, NewStatus},
        user::{self, User},
    },
    services::{
        account_emojis::AccountEmojis,
        render_status::{load_emojis, row_texts},
    },
    state::AppState,
    util::id::{new_id, object_ap_id},
};

/// Builds the [`Status`] entity `params` describes **without writing
/// anything**.
///
/// The very pipeline [`crate::services::create_status::create_status`]
/// runs before its write — the checks, the reply's parent and its
/// visibility cap, the conversation participation, the quote target, the
/// render with its mentions resolved and its tags extracted — and an entity
/// whose `id` is a freshly minted `UUIDv7` (decision 4), whose `created_at`
/// is now and whose counters are zero — Pleroma's `preview` parameter,
/// whose `ActivityPub.create(changes, true)` answers the draft activity and
/// persists none of it. A preview of a reply therefore answers the
/// parent-capped visibility, lists the inherited mentions and is a 404 for
/// a parent the poster cannot see, exactly as the create would.
///
/// Nothing touches `statuses`, `status_mentions`, the hashtag tables or
/// the poll tables, no counter moves, no key is claimed and
/// `after_status_created` never fires. A body carrying a poll is checked
/// the way a post's is and its entity answers `poll: null` — the question
/// is previewed, never stored. The reads the preview does perform are the
/// create's own: the mention lookup — network fetch of an unknown remote
/// account included, since the rendered content is only honest if the
/// mention links are — and the reply parent and quote target lookups. A
/// body naming a `quote_id` has its target gated like a create's, but the
/// in-memory row quotes nothing and no `quotes_count` moves. One
/// `custom_emojis` read fills `emojis` when the text holds a `:shortcode:`.
///
/// # Errors
///
/// Those of [`crate::services::create_status::create_status`]'s
/// preparation: every refusal the submitted body is owed — its 422s, the
/// 404 of an unseen parent, participation or quote target — arrives
/// unchanged, so a preview that would not post is refused exactly as the
/// post would be.
pub async fn preview_status(
    state: &AppState,
    user: &User,
    app: Option<&OauthApp>,
    params: &CreateStatusParams,
) -> AppResult<Status> {
    let Prepared { new, mentioned, .. } = prepared_new_status(state, user, app, params).await?;
    let reply_author = match new.in_reply_to_account_id {
        Some(id) => user::get_by_id(&state.db, id).await?,
        None => None,
    };
    let tags = preview_tags(&new.hashtags);
    let row = preview_row(&state.config.base_url, &new);
    let wanted = [row_texts(&row, user, &[])];
    let emojis = load_emojis(&state.db, &wanted).await?;
    debug!(
        status = %row.id,
        user = %user.id,
        "status previewed"
    );
    let author_emojis = AccountEmojis::load_one(&state.db, user).await?;
    let mut preview = Status::from_source(
        StatusSource {
            row: &row,
            author: user,
            reply_author: reply_author.as_ref(),
            media: &[],
            mentions: &mentioned,
            tags: &tags,
            emojis: emojis.get(&row.id).map_or(&[], Vec::as_slice),
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
    );
    preview.account = author_emojis.fill(preview.account, user, &state.config);
    Ok(preview)
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

/// The in-memory row a preview renders: a fresh id with the `uri` and `url`
/// [`status::insert`] would derive from it (decision 6), every column the
/// prepared `new` decided — its reply links, its thread (the new id when it
/// names none, as the insert takes), its visibility and language after the
/// reply rules — zero counters and `now()` for both timestamps. It is the
/// row a create of this body would have stored — except that it will never
/// be stored, and that it quotes nothing: `quote_of_id` stays empty because
/// the entity would render a quote the preview never fetched.
fn preview_row(base_url: &str, new: &NewStatus) -> status::Status {
    let id = new_id();
    let now = Utc::now();
    status::Status {
        id,
        uri: object_ap_id(base_url, id),
        url: Some(format!("{base_url}/notice/{id}")),
        user_id: new.user_id,
        content: new.content.clone(),
        raw_text: new.raw_text.clone(),
        content_type: new.content_type.clone(),
        spoiler_text: new.spoiler_text.clone(),
        sensitive: new.sensitive,
        visibility: new.visibility,
        language: new.language.clone(),
        in_reply_to_id: new.in_reply_to_id,
        in_reply_to_account_id: new.in_reply_to_account_id,
        in_reply_to_ap_id: new.in_reply_to_ap_id.clone(),
        conversation_id: new.conversation_id.unwrap_or(id),
        ap_context: new.ap_context.clone(),
        direct_conversation_id: new.direct_conversation_id,
        reblog_of_id: None,
        quote_of_id: None,
        quote_policy: None,
        local: new.local,
        application_name: new.application_name.clone(),
        application_website: new.application_website.clone(),
        replies_count: 0,
        reblogs_count: 0,
        favourites_count: 0,
        quotes_count: 0,
        edited_at: None,
        deleted_at: None,
        expires_at: new.expires_at,
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
                BASE, key_rows, live_ids, mention_ids, missed, params, plain_post, poll_rows,
                refused, replying, state, user, wire,
            },
        },
        util::entities::{hashtag_html, hashtag_link_html, mention_html},
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
                hashtag_link_html(BASE, "rust", "Rust"),
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
        let counted = user::get_by_id(pool, alice.id)
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

    /// A preview of a reply runs the create's reply rules: under a
    /// `private` parent a body naming no `visibility` answers `private`,
    /// and the entity carries the reply links, with nothing written.
    #[tokio::test]
    async fn a_reply_preview_takes_the_parent_capped_visibility() {
        let db = crate::test_db::test_db().await.expect("test database slot");
        let pool = db.pool();
        let app_state = state(pool);
        let alice = user(pool, "replier").await;
        let parent = status::insert(
            pool,
            BASE,
            &NewStatus {
                visibility: status::Visibility::Private,
                ..NewStatus::local(&alice, "for my followers")
            },
        )
        .await
        .expect("the private parent inserts");

        let entity = preview_status(&app_state, &alice, None, &replying("an answer", parent.id))
            .await
            .expect("the reply previews");

        let value = serde_json::to_value(&entity).expect("the entity serializes");
        assert_eq!(
            value["visibility"], "private",
            "the default scope is capped by the parent, as a create would"
        );
        assert_eq!(value["in_reply_to_id"], parent.id.to_string());
        assert_eq!(value["in_reply_to_account_id"], alice.id.to_string());
        assert_eq!(
            live_ids(pool).await,
            vec![parent.id],
            "the preview wrote nothing beside the parent"
        );
    }

    /// A reply preview lists the parent's author in `mentions` and names
    /// them in `in_reply_to_account_acct`, as the stored reply would.
    #[tokio::test]
    async fn a_reply_preview_lists_the_parent_author_in_mentions() {
        let db = crate::test_db::test_db().await.expect("test database slot");
        let pool = db.pool();
        let app_state = state(pool);
        let alice = user(pool, "answerer").await;
        let bob = user(pool, "bob").await;
        let parent = plain_post(pool, &bob, "question").await;

        let entity = preview_status(&app_state, &alice, None, &replying("answer", parent.id))
            .await
            .expect("the reply previews");

        let value = serde_json::to_value(&entity).expect("the entity serializes");
        assert_eq!(
            value["mentions"],
            json!([{ "id": bob.id.to_string(), "username": "bob",
                     "url": bob.ap_id, "acct": "bob" }]),
            "the parent's author is auto-mentioned"
        );
        assert_eq!(value["pleroma"]["in_reply_to_account_acct"], "bob");
        assert_eq!(
            value["content"], "<p>answer</p>",
            "the text itself is not modified"
        );
        assert_eq!(
            live_ids(pool).await,
            vec![parent.id],
            "and nothing was stored"
        );
        assert!(
            mention_ids(pool, parent.id).await.is_empty(),
            "no mention link was written"
        );
    }

    /// A preview naming a parent the poster cannot see is the create's 404,
    /// the same for an unknown id.
    #[tokio::test]
    async fn a_reply_preview_to_an_unseen_parent_is_not_found() {
        let db = crate::test_db::test_db().await.expect("test database slot");
        let pool = db.pool();
        let app_state = state(pool);
        let alice = user(pool, "outsider").await;
        let bob = user(pool, "bob").await;
        let hidden = status::insert(
            pool,
            BASE,
            &NewStatus {
                visibility: status::Visibility::Private,
                ..NewStatus::local(&bob, "followers only")
            },
        )
        .await
        .expect("the private parent inserts");

        for parent_id in [hidden.id, new_id()] {
            let err = preview_status(&app_state, &alice, None, &replying("peek", parent_id))
                .await
                .expect_err("the parent is not visible to the poster");
            missed(err).await;
        }
        assert_eq!(
            live_ids(pool).await,
            vec![hidden.id],
            "the refused previews wrote nothing"
        );
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
        let counted = user::get_by_id(pool, alice.id)
            .await
            .unwrap()
            .expect("the author exists");
        assert_eq!(counted.statuses_count, 0, "and no count moved");
    }
}
