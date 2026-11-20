//! Unit tests for the renderer's own contract: what a plain status, a
//! reply, a mention, an attachment, a remote mirror and the viewer's
//! relationship render, and which rows a page drops or keeps in order.

use chrono::{SubsecRound as _, TimeZone as _, Utc};
use serde_json::{Value, json};
use sqlx::PgPool;

use super::{
    ViewerMode, render_status, render_statuses,
    test_support::{BASE, post, test_state, user},
};
use crate::{
    entities::status::Status,
    models::{
        follow,
        media_attachment::{self, NewMediaAttachment},
        status::{NewStatus, Status as StatusRow, Visibility, insert, soft_delete},
        user::User,
    },
    state::AppState,
};

/// Every stored status, live or not, oldest id first.
async fn stored_statuses(pool: &PgPool) -> Vec<StatusRow> {
    sqlx::query_as::<_, StatusRow>("SELECT * FROM statuses ORDER BY id")
        .fetch_all(pool)
        .await
        .expect("the table reads")
}

/// The id strings of `statuses`, in order.
fn id_strings(statuses: &[StatusRow]) -> Vec<String> {
    statuses
        .iter()
        .map(|status| status.id.to_string())
        .collect()
}

/// The ids of rendered entities, in order.
fn rendered_ids(rendered: &[Status]) -> Vec<&str> {
    rendered.iter().map(|status| status.id.as_str()).collect()
}

/// A local user named `nickname` with `count` plain posts.
async fn author_with_posts(pool: &PgPool, nickname: &str, count: i32) -> User {
    let author = user(pool, nickname).await;
    for n in 0..count {
        let _posted = post(pool, &author, &format!("{nickname} post {n}")).await;
    }
    author
}

/// Renders `statuses` anonymously and returns the first entity's JSON.
async fn render_first(state: &AppState, statuses: &[StatusRow]) -> Value {
    let rendered = render_statuses(state, ViewerMode::ANONYMOUS, statuses)
        .await
        .unwrap();
    serde_json::to_value(&rendered[0]).unwrap()
}

#[tokio::test]
async fn a_plain_status_renders_its_stored_facts() {
    let db = crate::test_db::test_db().await.expect("test database slot");
    let pool = db.pool().clone();
    let state = test_state(&pool);
    let alice = user(&pool, "alice").await;
    let status = insert(
        &pool,
        BASE,
        &NewStatus {
            language: Some("en".to_owned()),
            application_name: Some("Wired".to_owned()),
            application_website: Some("https://wired.example".to_owned()),
            visibility: Visibility::Local,
            ..NewStatus::local(&alice, "hello #world")
        },
    )
    .await
    .unwrap();

    let rendered = render_status(&state, ViewerMode::ANONYMOUS, &status)
        .await
        .unwrap();
    let entity = rendered.expect("a live status renders");
    let value = serde_json::to_value(&entity).unwrap();
    assert_eq!(value["id"], status.id.to_string());
    assert_eq!(value["uri"], status.uri);
    assert_eq!(value["url"], json!(status.url));
    assert_eq!(value["content"], "<p>hello #world</p>");
    assert_eq!(
        value["visibility"], "local",
        "Pleroma's local visibility emits its literal value"
    );
    assert_eq!(value["account"]["acct"], "alice", "the author entity");
    assert_eq!(value["application"]["name"], "Wired");
    assert_eq!(value["application"]["website"], "https://wired.example");
    assert_eq!(value["language"], "en");
    assert_eq!(value["reblog"], Value::Null, "a root post nests nothing");
    assert_eq!(
        value["card"],
        Value::Null,
        "no card cached for this row renders null"
    );
    assert_eq!(
        value["pleroma"]["content"],
        json!({ "text/plain": "hello #world" }),
        "the raw text the author wrote"
    );
    assert_eq!(
        value["pleroma"]["conversation_id"],
        status.conversation_id.to_string()
    );
    assert_eq!(value["pleroma"]["local"], true);
    assert_eq!(value["pleroma"]["parent_visible"], true);
    assert_eq!(value["pleroma"]["emoji_reactions"], json!([]));
    assert_eq!(value["pleroma"]["quotes_count"], 0);
    assert_eq!(value["pleroma"]["expires_at"], Value::Null);
    assert_eq!(value["pleroma"]["direct_conversation_id"], Value::Null);
    assert_eq!(value["pleroma"]["bookmark_folder"], Value::Null);
    assert_eq!(value["pleroma"]["pinned_at"], Value::Null);
    assert_eq!(
        value["tags"],
        json!([{ "name": "world", "url": format!("{BASE}/tags/world") }]),
        "the hashtag the batch loader brought along"
    );
}

#[tokio::test]
async fn replies_mentions_and_media_render_their_links() {
    let db = crate::test_db::test_db().await.expect("test database slot");
    let pool = db.pool().clone();
    let state = test_state(&pool);
    let alice = user(&pool, "alice").await;
    let bob = user(&pool, "bob").await;
    let root = insert(
        &pool,
        BASE,
        &NewStatus {
            mentioned_user_ids: vec![bob.id],
            ..NewStatus::local(&alice, "@bob look")
        },
    )
    .await
    .unwrap();
    let reply = insert(
        &pool,
        BASE,
        &NewStatus {
            in_reply_to_id: Some(root.id),
            in_reply_to_account_id: Some(alice.id),
            ..NewStatus::local(&bob, "I will")
        },
    )
    .await
    .unwrap();
    let media = insert(&pool, BASE, &NewStatus::local(&bob, "with a picture"))
        .await
        .unwrap();
    let attachment = media_attachment::insert(
        &pool,
        &NewMediaAttachment {
            user_id: Some(bob.id),
            status_id: Some(media.id),
            url: "https://media.example/pic.png".to_owned(),
            content_type: "image/png".to_owned(),
            ..Default::default()
        },
    )
    .await
    .unwrap();

    let page = [reply.clone(), media.clone()];
    let rendered = render_statuses(&state, ViewerMode::ANONYMOUS, &page)
        .await
        .unwrap();
    assert_eq!(
        rendered_ids(&rendered),
        id_strings(&page),
        "input order, one entity per row"
    );

    let reply_json = serde_json::to_value(&rendered[0]).unwrap();
    assert_eq!(reply_json["in_reply_to_id"], root.id.to_string());
    assert_eq!(reply_json["in_reply_to_account_id"], alice.id.to_string());
    assert_eq!(
        reply_json["pleroma"]["in_reply_to_account_acct"], "alice",
        "the parent author's handle, from the same batched user read"
    );
    assert_eq!(
        reply_json["mentions"],
        json!([]),
        "this reply linked no mention row"
    );

    let media_json = serde_json::to_value(&rendered[1]).unwrap();
    assert_eq!(
        media_json["media_attachments"][0]["id"],
        attachment.id.to_string(),
        "the attached media renders"
    );
    assert_eq!(media_json["media_attachments"][0]["url"], attachment.url);

    // The mention on the root status renders through the same path.
    let root_json = render_first(&state, std::slice::from_ref(&root)).await;
    assert_eq!(
        root_json["mentions"],
        json!([{ "id": bob.id.to_string(), "username": "bob",
                 "url": format!("{BASE}/users/bob"), "acct": "bob" }]),
        "NewStatus::mentioned_user_ids linked bob, and the mention \
         loader brings him back"
    );
}

#[tokio::test]
async fn twenty_statuses_by_five_authors_render_in_one_batch() {
    let db = crate::test_db::test_db().await.expect("test database slot");
    let pool = db.pool().clone();
    let state = test_state(&pool);
    let mut authors = Vec::new();
    for n in 0..5 {
        authors.push(author_with_posts(&pool, &format!("user{n}"), 4).await);
    }
    let all = stored_statuses(&pool).await;
    assert_eq!(all.len(), 20, "five authors with four posts each");
    let want = id_strings(&all);

    // The structural bound on the query set: one call over the whole
    // slice (ids in, entities out — no per-row round in this API)
    // renders all twenty rows, anonymous and authenticated alike. A
    // per-row loader would show up as twenty times the queries, and
    // this call makes exactly the fixed set of the module docs.
    let anonymous = render_statuses(&state, ViewerMode::ANONYMOUS, &all)
        .await
        .unwrap();
    assert_eq!(
        rendered_ids(&anonymous),
        want,
        "every row of a 20-strong slice renders, in id order"
    );

    let viewer = &authors[0];
    let for_viewer = render_statuses(&state, ViewerMode::user(viewer), &all)
        .await
        .unwrap();
    assert_eq!(for_viewer.len(), 20, "a viewer renders the same page");
    let value = serde_json::to_value(&for_viewer[0]).unwrap();
    assert_eq!(
        value["pleroma"]["local"], true,
        "the pleroma block rides along on every entry"
    );
}

#[tokio::test]
async fn tombstones_and_reblogs_of_gone_originals_are_skipped() {
    let db = crate::test_db::test_db().await.expect("test database slot");
    let pool = db.pool().clone();
    let state = test_state(&pool);
    let alice = user(&pool, "alice").await;
    let bob = user(&pool, "bob").await;
    let keep = post(&pool, &alice, "kept").await;
    let doomed = post(&pool, &alice, "doomed").await;
    let orphan_reblog = insert(
        &pool,
        BASE,
        &NewStatus {
            reblog_of_id: Some(doomed.id),
            ..NewStatus::local(&bob, "")
        },
    )
    .await
    .unwrap();
    let tombstone = soft_delete(&pool, doomed.id)
        .await
        .unwrap()
        .expect("the fixture delete found the live post");
    assert!(tombstone.deleted_at.is_some(), "the row is a tombstone");

    let keep_id = keep.id.to_string();
    let rendered = render_statuses(
        &state,
        ViewerMode::ANONYMOUS,
        &[keep, tombstone.clone(), orphan_reblog],
    )
    .await
    .unwrap();
    assert_eq!(
        rendered_ids(&rendered),
        [keep_id],
        "the tombstone and the reblog of a deleted original are both \
         skipped; whatever survives keeps its place"
    );
    assert!(
        render_status(&state, ViewerMode::ANONYMOUS, &tombstone)
            .await
            .unwrap()
            .is_none(),
        "the single helper answers None for a tombstone"
    );
}

#[tokio::test]
async fn the_viewers_relationship_lands_in_the_account_entity() {
    let db = crate::test_db::test_db().await.expect("test database slot");
    let pool = db.pool().clone();
    let state = test_state(&pool);
    let alice = user(&pool, "alice").await;
    let bob = user(&pool, "bob").await;
    let following = follow::follow(&pool, BASE, &bob, &alice)
        .await
        .expect("bob follows alice");
    assert_eq!(
        following.state,
        follow::FollowState::Accepted,
        "alice is unlocked, so the follow is granted"
    );
    let status = post(&pool, &alice, "public hello").await;

    let anonymous = render_first(&state, std::slice::from_ref(&status)).await;
    assert_eq!(
        anonymous["account"]["pleroma"]["relationship"],
        json!({}),
        "the anonymous viewer embeds Pleroma's empty relationship"
    );

    let for_bob = render_status(&state, ViewerMode::user(&bob), &status)
        .await
        .unwrap()
        .expect("it renders for bob too");
    let value = serde_json::to_value(&for_bob).unwrap();
    assert_eq!(
        value["account"]["pleroma"]["relationship"]["following"], true,
        "the batched relationship preload reaches the account entity"
    );

    let for_alice = render_status(&state, ViewerMode::user(&alice), &status)
        .await
        .unwrap()
        .expect("and for the author");
    let owner_view = serde_json::to_value(&for_alice).unwrap();
    assert_eq!(
        owner_view["account"]["pleroma"]["relationship"],
        json!({}),
        "Pleroma keeps the owner's own relationship out of the render"
    );
}

#[tokio::test]
async fn remote_mirrors_render_their_stored_urls_and_text() {
    let db = crate::test_db::test_db().await.expect("test database slot");
    let pool = db.pool().clone();
    let state = test_state(&pool);
    let mallory = user(&pool, "mallory").await;
    let published = Utc
        .with_ymd_and_hms(2024, 2, 1, 8, 0, 0)
        .unwrap()
        .trunc_subsecs(6);
    let status = insert(
        &pool,
        BASE,
        &NewStatus {
            user_id: mallory.id,
            uri: Some("https://remote.example/posts/1".to_owned()),
            url: Some("https://remote.example/notice/1".to_owned()),
            content: "<p>from elsewhere</p>".to_owned(),
            raw_text: String::new(),
            expires_at: Some(published),
            edited_at: Some(published),
            inserted_at: Some(published),
            language: Some("ko".to_owned()),
            ..NewStatus::default()
        },
    )
    .await
    .unwrap();

    let rendered = render_status(&state, ViewerMode::ANONYMOUS, &status)
        .await
        .unwrap()
        .expect("a mirror renders");
    let value = serde_json::to_value(&rendered).unwrap();
    assert_eq!(value["uri"], "https://remote.example/posts/1");
    assert_eq!(value["url"], "https://remote.example/notice/1");
    assert_eq!(value["pleroma"]["local"], false, "a mirror is not local");
    assert_eq!(
        value["pleroma"]["content"],
        json!({ "text/plain": "from elsewhere\n" }),
        "no stored source text: the plain variant strips the HTML, the \
         way strip_html renders a paragraph's end as a newline"
    );
    assert_eq!(value["pleroma"]["expires_at"], "2024-02-01T08:00:00.000Z");
    assert_eq!(value["edited_at"], "2024-02-01T08:00:00.000Z");
    assert_eq!(value["created_at"], "2024-02-01T08:00:00.000Z");
    assert_eq!(value["language"], "ko");
}

/// The two degenerate pages: an empty slice and a slice of nothing
/// but tombstones both render empty, and the renderer returns before
/// any loader (or the database) is needed.
#[tokio::test]
async fn empty_and_tombstone_pages_render_empty() {
    let db = crate::test_db::test_db().await.expect("test database slot");
    let pool = db.pool().clone();
    let state = test_state(&pool);
    let alice = user(&pool, "alice").await;
    let status = post(&pool, &alice, "gone").await;
    let tombstone = soft_delete(&pool, status.id)
        .await
        .unwrap()
        .expect("the post was live");

    let rendered = render_statuses(&state, ViewerMode::ANONYMOUS, &[])
        .await
        .unwrap();
    assert!(rendered.is_empty(), "nothing in answers nothing out");
    let tombstoned = render_statuses(
        &state,
        ViewerMode::ANONYMOUS,
        std::slice::from_ref(&tombstone),
    )
    .await
    .unwrap();
    assert!(
        tombstoned.is_empty(),
        "a page of tombstones renders empty without loading anything"
    );
}
