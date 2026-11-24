//! The loader's own tests: the shortcode scan and the database-backed
//! [`super::load_note_context`] reads over the fixture users, statuses,
//! media, emojis and polls they insert.

use chrono::Duration as TimeDelta;
use serde_json::json;

use super::*;
use crate::{
    federation::note::{note_json, test_support::*},
    models::{
        custom_emoji::{NewCustomEmoji, upsert as upsert_emoji},
        media_attachment::{NewMediaAttachment, insert as insert_media},
        poll::{PollParams, create as create_poll},
        status::{NewStatus, insert as insert_status, soft_delete},
        user::{NewUser, insert as insert_user},
    },
    test_fixtures::local_user as create_local,
    util::{id::new_id, time::mastodon_time},
};

#[test]
fn shortcode_scan_reads_complete_references_only() {
    assert_eq!(
        shortcodes_in("a :one: b :two:"),
        vec!["one".to_owned(), "two".to_owned()],
        "two references in running text"
    );
    assert_eq!(
        shortcodes_in(":a::b:"),
        vec!["a".to_owned(), "b".to_owned()],
        "adjacent references split on the second colon"
    );
    assert_eq!(
        shortcodes_in("x :blob-cat: :blobfox+1:"),
        vec!["blob-cat".to_owned(), "blobfox+1".to_owned()],
        "dashes and pluses belong to a shortcode"
    );
    assert!(shortcodes_in("no codes here").is_empty(), "no colons");
    assert!(
        shortcodes_in(":unclosed").is_empty(),
        "an open candidate drops"
    );
    assert!(shortcodes_in("::").is_empty(), "an empty code is no code");
    assert!(
        shortcodes_in(":a b:").is_empty(),
        "spaces break a candidate"
    );
    assert_eq!(
        shortcodes_in("text:colon and :ok:"),
        vec!["ok".to_owned()],
        "a bare inner colon does not start what never closes right"
    );
}

/// A mirror of `nickname` on `host`, for mention fixtures.
async fn create_remote(pool: &PgPool, nickname: &str, host: &str) -> User {
    let ap_id = format!("https://{host}/users/{nickname}");
    insert_user(
        pool,
        &NewUser {
            nickname: format!("{nickname}@{host}"),
            ap_id,
            domain: Some(host.to_owned()),
            ..NewUser::default()
        },
    )
    .await
    .expect("fixture remote user inserts")
}

/// A live local post of `author` built from `new`, already inserted.
async fn post(pool: &PgPool, new: &NewStatus) -> Status {
    insert_status(pool, BASE, new)
        .await
        .expect("fixture status inserts")
}

/// The mention list's actor ids, the shape every loader assertion
/// compares.
fn mention_ap_ids(ctx: &NoteContext) -> Vec<&str> {
    ctx.mentions
        .iter()
        .map(|person| person.ap_id.as_str())
        .collect()
}

#[tokio::test]
async fn load_note_context_reads_a_bare_status_with_no_satellites() {
    let db = crate::test_db::test_db().await.expect("test database slot");
    let pool = db.pool().clone();
    let alice = create_local(&pool, "plainalice").await;
    let posted = post(&pool, &NewStatus::local(&alice, "plain hello")).await;
    let config = Config::for_tests();

    let ctx = load_note_context(&pool, &config, posted.id)
        .await
        .expect("the note context loads");
    assert_eq!(ctx.status.id, posted.id, "the row itself");
    assert_eq!(ctx.author.id, alice.id, "its author");
    assert_eq!(ctx.config.base_url, BASE, "the config rides along");
    assert!(
        ctx.reply_parent_uri.is_none()
            && ctx.mentions.is_empty()
            && ctx.hashtags.is_empty()
            && ctx.attachments.is_empty()
            && ctx.emojis.is_empty()
            && ctx.poll.is_none()
            && ctx.quote_uri.is_none(),
        "a plain post carries nothing else"
    );
    let document = note_json(&ctx).expect("and the loaded context renders");
    assert_eq!(document["id"], posted.uri, "the object id is the uri");
    assert_eq!(document["attributedTo"], alice.ap_id, "the author acts");
}

#[tokio::test]
async fn load_note_context_assembles_the_reply_tags_and_quote() {
    let db = crate::test_db::test_db().await.expect("test database slot");
    let pool = db.pool().clone();
    let alice = create_local(&pool, "loadingalice").await;
    let bob = create_local(&pool, "loadingbob").await;
    let carol = create_remote(&pool, "loadingcarol", "remote.example").await;
    let parent = post(&pool, &NewStatus::local(&bob, "the parent")).await;
    let enabled = upsert_emoji(
        &pool,
        &NewCustomEmoji::local("railroad_track", &format!("{BASE}/emoji/railroad.png")),
    )
    .await
    .expect("enabled emoji row");
    assert_eq!(
        enabled.shortcode, "railroad_track",
        "the enabled row is stored"
    );
    let hidden = upsert_emoji(
        &pool,
        &NewCustomEmoji {
            disabled: true,
            ..NewCustomEmoji::local("hidden_thing", &format!("{BASE}/emoji/hidden.png"))
        },
    )
    .await
    .expect("disabled emoji row");
    assert!(hidden.disabled, "and it is stored disabled");
    let posted = post(
        &pool,
        &NewStatus {
            in_reply_to_id: Some(parent.id),
            in_reply_to_account_id: Some(bob.id),
            mentioned_user_ids: vec![bob.id, carol.id],
            hashtags: vec!["fediverse".to_owned()],
            quote_of_id: Some(parent.id),
            raw_text: "hi :railroad_track: :hidden_thing: #fediverse".to_owned(),
            content: "<p>hi :railroad_track: :hidden_thing: #fediverse</p>".to_owned(),
            ..NewStatus::local(&alice, "answer")
        },
    )
    .await;
    let config = Config::for_tests();

    let ctx = load_note_context(&pool, &config, posted.id)
        .await
        .expect("the full context loads");
    assert_eq!(
        ctx.reply_parent_uri.as_deref(),
        Some(parent.uri.as_str()),
        "the parent resolves to its object uri"
    );
    assert_eq!(
        mention_ap_ids(&ctx),
        vec![bob.ap_id.as_str(), carol.ap_id.as_str()],
        "mentions keep link order and the parent author is not doubled"
    );
    assert_eq!(ctx.hashtags, vec!["fediverse".to_owned()], "the tag name");
    assert_eq!(ctx.emojis.len(), 1, "only the enabled emoji row survives");
    assert_eq!(ctx.emojis[0].shortcode, "railroad_track", "…by name");
    assert_eq!(
        ctx.quote_uri.as_deref(),
        Some(parent.uri.as_str()),
        "the quote resolves to its object uri"
    );

    let document = note_json(&ctx).expect("the assembled note renders");
    assert_eq!(document["inReplyTo"], json!(parent.uri), "the parent line");
    assert_eq!(document["quoteUrl"], json!(parent.uri), "the quote line");
    assert_eq!(
        document["tag"],
        json!([
            {"type": "Hashtag", "href": format!("{BASE}/tags/fediverse"), "name": "#fediverse"},
            {"type": "Mention", "href": bob.ap_id, "name": "@loadingbob"},
            {"type": "Mention", "href": carol.ap_id, "name": "@loadingcarol@remote.example"},
            {"type": "Emoji", "id": format!("{BASE}/emoji/railroad.png"), "name": ":railroad_track:",
             "icon": {"type": "Image", "url": format!("{BASE}/emoji/railroad.png")}},
        ]),
        "the full tag list, in render order"
    );
}

#[tokio::test]
async fn load_note_context_loads_media_and_the_expired_poll() {
    let db = crate::test_db::test_db().await.expect("test database slot");
    let pool = db.pool().clone();
    let alice = create_local(&pool, "pollalice").await;
    let media = insert_media(
        &pool,
        &NewMediaAttachment {
            user_id: Some(alice.id),
            url: format!("{BASE}/media/loading/cat.png"),
            content_type: "image/png".to_owned(),
            description: Some("a cat".to_owned()),
            ..NewMediaAttachment::default()
        },
    )
    .await
    .expect("fixture media uploads");
    let posted = post(
        &pool,
        &NewStatus {
            media_ids: vec![media.id],
            ..NewStatus::local(&alice, "vote now")
        },
    )
    .await;
    let mut conn = pool.acquire().await.expect("slot connection");
    let created = create_poll(
        &mut conn,
        posted.id,
        &PollParams {
            options: vec!["yes".to_owned(), "no".to_owned()],
            expires_at: Some(Utc::now() - TimeDelta::hours(1)),
            multiple: true,
            ..PollParams::default()
        },
    )
    .await
    .expect("fixture poll creates");
    assert_eq!(created.status_id, posted.id, "the poll names its status");
    drop(conn);
    let config = Config::for_tests();

    let ctx = load_note_context(&pool, &config, posted.id)
        .await
        .expect("the media-and-poll context loads");
    assert_eq!(ctx.attachments.len(), 1, "the claimed media");
    assert_eq!(ctx.attachments[0].id, media.id, "and it is the row");
    let poll_data = ctx.poll.clone().expect("the poll loaded");
    assert!(poll_data.multiple, "the multiple flag round trips");
    assert!(poll_data.closed, "an expired deadline reads as closed");
    assert_eq!(poll_data.voters_count, 0, "a fresh poll has no voters");
    assert_eq!(poll_data.options.len(), 2, "both options came along");

    let document = note_json(&ctx).expect("the assembled note renders");
    assert_eq!(document["type"], "Question", "the poll makes it a question");
    assert!(
        document.get("anyOf").is_some() && document.get("oneOf").is_none(),
        "multiple choice renders anyOf, never oneOf"
    );
    assert_eq!(
        document["attachment"],
        json!([
            {"type": "Image", "mediaType": "image/png", "url": format!("{BASE}/media/loading/cat.png"),
             "name": "a cat"},
        ]),
        "the media as an Image attachment"
    );
    assert_eq!(
        document["endTime"],
        json!(mastodon_time(&poll_data.expires_at.expect("a deadline"))),
        "the rendered end time is the stored one"
    );
}

#[tokio::test]
async fn load_note_context_loads_emojis_used_only_in_the_cw_or_a_poll_option() {
    let db = crate::test_db::test_db().await.expect("test database slot");
    let pool = db.pool().clone();
    let alice = create_local(&pool, "cwemojialice").await;
    let stored = upsert_emoji(
        &pool,
        &NewCustomEmoji::local("wiredcat", &format!("{BASE}/emoji/wiredcat.png")),
    )
    .await
    .expect("emoji row");
    assert_eq!(stored.shortcode, "wiredcat", "the emoji row is stored");
    let config = Config::for_tests();

    let with_cw = post(
        &pool,
        &NewStatus {
            spoiler_text: "CW with :wiredcat: emoji".to_owned(),
            ..NewStatus::local(&alice, "body without emoji")
        },
    )
    .await;
    let ctx = load_note_context(&pool, &config, with_cw.id)
        .await
        .expect("the CW context loads");
    assert_eq!(ctx.emojis.len(), 1, "the CW-only emoji loads");
    assert_eq!(ctx.emojis[0].shortcode, "wiredcat", "…by name");
    let document = note_json(&ctx).expect("the CW note renders");
    assert_eq!(
        document["tag"].as_array().map(Vec::len),
        Some(1),
        "and it is tagged"
    );

    let with_poll = post(&pool, &NewStatus::local(&alice, "poll emoji test")).await;
    let mut conn = pool.acquire().await.expect("slot connection");
    let created = create_poll(
        &mut conn,
        with_poll.id,
        &PollParams {
            options: vec![":wiredcat: yes".to_owned(), "no".to_owned()],
            expires_at: Some(Utc::now() + TimeDelta::hours(1)),
            ..PollParams::default()
        },
    )
    .await
    .expect("fixture poll creates");
    assert_eq!(created.status_id, with_poll.id, "the poll names its status");
    drop(conn);
    let poll_ctx = load_note_context(&pool, &config, with_poll.id)
        .await
        .expect("the poll context loads");
    assert_eq!(poll_ctx.emojis.len(), 1, "the option-only emoji loads");
    let poll_document = note_json(&poll_ctx).expect("the poll note renders");
    assert_eq!(
        poll_document["tag"].as_array().map(Vec::len),
        Some(1),
        "and it is tagged"
    );
}

#[test]
fn used_shortcodes_cover_the_cw_and_poll_options_once_each() {
    let mut ctx = note_context(status::Visibility::Public);
    ctx.status.raw_text = ":a: body".to_owned();
    ctx.status.spoiler_text = ":b: :a:".to_owned();
    let poll_data = PollData {
        multiple: false,
        expires_at: None,
        closed: false,
        voters_count: 0,
        options: vec![PollOptionData {
            title: ":c: :b:".to_owned(),
            votes_count: 0,
        }],
    };
    assert_eq!(
        used_shortcodes(&ctx.status, Some(&poll_data)),
        vec!["a".to_owned(), "b".to_owned(), "c".to_owned()],
        "first-appearance order, deduplicated across all texts"
    );
    assert_eq!(
        used_shortcodes(&ctx.status, None),
        vec!["a".to_owned(), "b".to_owned()],
        "no poll, no option codes"
    );
}

#[tokio::test]
async fn load_note_context_adds_a_silent_reply_s_parent_author() {
    let db = crate::test_db::test_db().await.expect("test database slot");
    let pool = db.pool().clone();
    let alice = create_local(&pool, "quietalice").await;
    let bob = create_local(&pool, "quietbob").await;
    let parent = post(&pool, &NewStatus::local(&bob, "answer me")).await;
    let posted = post(
        &pool,
        &NewStatus {
            in_reply_to_id: Some(parent.id),
            in_reply_to_account_id: Some(bob.id),
            ..NewStatus::local(&alice, "okay")
        },
    )
    .await;
    let config = Config::for_tests();

    let ctx = load_note_context(&pool, &config, posted.id)
        .await
        .expect("the reply context loads");
    assert_eq!(
        mention_ap_ids(&ctx),
        vec![bob.ap_id.as_str()],
        "the parent author joins the addressing even unmentioned"
    );
    let document = note_json(&ctx).expect("and renders");
    assert_eq!(
        document["cc"],
        json!([format!("{}/followers", alice.ap_id), bob.ap_id]),
        "the public note cc-ed the parent author as well as the followers"
    );
}

#[tokio::test]
async fn load_note_context_resolves_stored_and_unstored_parents_alike() {
    let db = crate::test_db::test_db().await.expect("test database slot");
    let pool = db.pool().clone();
    let alice = create_local(&pool, "eitheralice").await;
    let posted = post(
        &pool,
        &NewStatus {
            in_reply_to_ap_id: Some(PARENT_URI.to_owned()),
            ..NewStatus::local(&alice, "remote answer")
        },
    )
    .await;
    let config = Config::for_tests();
    let stored_ctx = load_note_context(&pool, &config, posted.id)
        .await
        .expect("the unresolved-parent reply loads");
    assert_eq!(
        stored_ctx.reply_parent_uri.as_deref(),
        Some(PARENT_URI),
        "a parent this instance never stored rides on as its ap_id"
    );

    let bob = create_local(&pool, "eitherbob").await;
    let parent = post(&pool, &NewStatus::local(&bob, "a deleted parent")).await;
    let reply = post(
        &pool,
        &NewStatus {
            in_reply_to_id: Some(parent.id),
            in_reply_to_account_id: Some(bob.id),
            ..NewStatus::local(&alice, "still answering")
        },
    )
    .await;
    let tombstone = soft_delete(&pool, parent.id)
        .await
        .expect("the parent is soft-deleted")
        .expect("and was live");
    assert_eq!(tombstone.id, parent.id, "the parent is a tombstone now");
    let tombstoned_ctx = load_note_context(&pool, &config, reply.id)
        .await
        .expect("a reply to a deleted parent still loads");
    assert_eq!(
        tombstoned_ctx.reply_parent_uri.as_deref(),
        Some(parent.uri.as_str()),
        "and still names the tombstone's reserved uri"
    );
}

#[tokio::test]
async fn load_note_context_refuses_unknown_and_deleted_statuses() {
    let db = crate::test_db::test_db().await.expect("test database slot");
    let pool = db.pool().clone();
    let alice = create_local(&pool, "gonealice").await;
    let config = Config::for_tests();

    let unknown_err = load_note_context(&pool, &config, new_id())
        .await
        .expect_err("an unknown id loads nothing");
    assert!(
        matches!(unknown_err, AppError::NotFound),
        "the unknown id is a 404"
    );

    let posted = post(&pool, &NewStatus::local(&alice, "delete me")).await;
    let tombstone = soft_delete(&pool, posted.id)
        .await
        .expect("the status is soft-deleted")
        .expect("and was live");
    assert_eq!(tombstone.id, posted.id, "the row is a tombstone now");
    let deleted_err = load_note_context(&pool, &config, posted.id)
        .await
        .expect_err("a tombstone loads no document");
    assert!(
        matches!(deleted_err, AppError::NotFound),
        "and a deleted status is the same 404"
    );
}
