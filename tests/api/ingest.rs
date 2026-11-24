//! Remote-note ingestion over HTTP (WBE-167): a federated `Create` folded
//! by [`handle_incoming`] and the status it leaves behind, observed
//! through the client API.
//!
//! Every test delivers a fixture `Create` activity (Mastodon and Pleroma
//! flavours, `tests/fixtures/ap/create_*.json`) through the real
//! dispatcher — the same job the inbox routes enqueue, run in-process with
//! the signer's stored mirror as its own row — and then reads the result
//! the way a client does. What this pins, per the ticket:
//!
//! - a new public note appears in `GET /api/v1/timelines/public` as a
//!   remote status with its attachments, tags and mentions, and with the
//!   zero boost and favourite counters the ingest starts every mirrored
//!   post at;
//! - a duplicate `Create` is idempotent — both the ledger's swallow of the
//!   same activity id and the ingest's `Existing` answer for the same
//!   object `uri` keep exactly one row;
//! - a reply to a local parent links to it, bumps its `replies_count` and
//!   notifies its author with a `mention`;
//! - a `Create` of a tombstoned `uri` is ignored, a deactivated author's
//!   post is ignored, and a mention whose actor no lookup can settle drops
//!   the mention without dropping the post;
//! - `ACCEPT_UNSOLICITED_PUBLIC=false` drops a public post from an
//!   unfollowed author, and the exemptions — a mention of a local user, a
//!   local follow — let the solicited ones through;
//! - the Pleroma flavour lands `unlisted`, with its
//!   `ostatus:conversation` kept as the status' context, and two `direct`
//!   orphans of one context share a conversation and stay out of the
//!   public timeline.

#![cfg(test)]

use axum::{Router, http::StatusCode};
use serde_json::{Map, Value, json};
use sqlx::PgPool;
use uuid::Uuid;
use wired_be::{
    config::Config,
    models::{
        follow,
        status::{self, Status},
        user::User,
    },
    services::runtime_config,
    state::AppState,
};

use crate::common::{self, deliver, delivering_state};

/// The Mastodon-flavour `Create`: a public note mentioning `carol`,
/// tagging `#Rust` and `:ralph:`, with a `Document` pair attached.
const MASTODON: &str = include_str!("../fixtures/ap/create_mastodon.json");
/// The Pleroma-flavour `Create`: an `as:Public`-in-`cc` note with a
/// `contentMap`, a `Link` url, a bare-object `ostatus:conversation` and a
/// single-object `tag`.
const PLEROMA: &str = include_str!("../fixtures/ap/create_pleroma.json");
/// The object uri the Mastodon fixture names.
const MASTODON_URI: &str = "https://remote.example/users/bob/statuses/01MASTOFIX";
/// The object uri the Pleroma fixture names.
const PLEROMA_URI: &str = "https://remote.example/objects/PLEROMAFIX";
/// The scopes the fixtures mint tokens with: the status reads, the
/// notification read and the follow write.
const SCOPES: &str = "read write follow";

/// A new public note from a remote author lands in the public timeline as
/// a remote status carrying everything its object described.
#[tokio::test]
async fn a_mastodon_create_lands_in_the_public_timeline() {
    let db = common::test_db().await;
    let pool = db.pool().clone();
    let app = common::test_app(pool.clone());
    let state = delivering_state(&pool);
    let signer = bob(&pool).await;
    let _carol = common::create_remote_user(&pool, "carol", "remote.example").await;

    deliver(&state, &fixture(MASTODON), &signer).await;

    let entry = public_entry(&app, MASTODON_URI).await;
    assert_eq!(
        entry["account"]["acct"], "bob@remote.example",
        "the status reads under its author's remote handle (decision 7)"
    );
    assert_eq!(entry["visibility"], "public", "the `to: Public` addressing");
    assert_eq!(
        entry["created_at"], "2024-05-01T10:00:00.000Z",
        "the object's published time is the status' creation time"
    );
    assert_eq!(
        entry["url"], "https://remote.example/@bob/01MASTOFIX",
        "and its human url"
    );
    assert_eq!(
        entry["reblogs_count"], 0,
        "counters are learned from activities, never from the object's own claims"
    );
    assert_eq!(entry["favourites_count"], 0, "both of them");
    let tags: Vec<&str> = entry["tags"]
        .as_array()
        .expect("tags is an array")
        .iter()
        .filter_map(|tag| tag["name"].as_str())
        .collect();
    assert_eq!(tags, ["rust"], "the Hashtag tag is stored and rendered");
    let mentions: Vec<&str> = entry["mentions"]
        .as_array()
        .expect("mentions is an array")
        .iter()
        .filter_map(|mention| mention["acct"].as_str())
        .collect();
    assert_eq!(
        mentions,
        ["carol@remote.example"],
        "the stored mention resolves against `users`"
    );
    let media = entry["media_attachments"]
        .as_array()
        .expect("media_attachments is an array");
    assert_attachment_entities(media);

    let emoji_rows = sqlx::query_scalar::<_, i64>(
        "SELECT count(*) FROM custom_emojis \
          WHERE shortcode = 'ralph' AND domain = 'remote.example'",
    )
    .fetch_one(&pool)
    .await
    .expect("the emoji count reads");
    assert_eq!(
        emoji_rows, 1,
        "the Emoji tag upserted its domain-scoped row"
    );

    let row = stored(&pool, MASTODON_URI).await;
    assert_eq!(
        mention_count(&pool, row.id).await,
        1,
        "and exactly one account is linked as mentioned"
    );
}

/// The Mastodon fixture's two attachment documents, as the entity renders
/// them: both present in document order, the first carrying every stated
/// column, the second typed from its content type.
fn assert_attachment_entities(media: &[Value]) {
    assert_eq!(media.len(), 2, "both documents land, in document order");
    assert_eq!(
        media[0]["url"], "https://remote.example/media/fix-a.png",
        "the remote url is served as the attachment url"
    );
    assert_eq!(
        media[0]["remote_url"], "https://remote.example/media/fix-a.png",
        "and named as the media's origin"
    );
    assert_eq!(media[0]["description"], "A chart", "with its alt text");
    assert_eq!(
        media[0]["meta"]["original"]["width"], 1280,
        "and its stated dimensions"
    );
    assert_eq!(media[0]["meta"]["original"]["height"], 720, "both");
    assert_eq!(
        media[0]["blurhash"], "LEHV6nWB2yk8pyo0adR*.7kCMdnj",
        "and its placeholder"
    );
    assert_eq!(
        media[1]["type"], "video",
        "the video document types as video"
    );
}

/// The same object twice — through the same activity id (the ledger's
/// dedupe) and through a different one (the ingest's `uri` dedupe) — is
/// stored once.
#[tokio::test]
async fn a_duplicate_create_is_idempotent() {
    let db = common::test_db().await;
    let pool = db.pool().clone();
    let app = common::test_app(pool.clone());
    let state = delivering_state(&pool);
    let signer = bob(&pool).await;

    deliver(&state, &fixture(MASTODON), &signer).await;
    deliver(&state, &fixture(MASTODON), &signer).await;
    deliver(&state, &mastodon_renamed_activity("redelivered"), &signer).await;

    assert_eq!(
        rows_for_uri(&pool, MASTODON_URI).await,
        1,
        "three deliveries, one row: the uri stays reserved"
    );
    let carried = public_timeline(&app)
        .await
        .into_iter()
        .filter(|entry| entry["uri"] == json!(MASTODON_URI))
        .count();
    assert_eq!(carried, 1, "and the timeline carries it once");
    let author_count =
        sqlx::query_scalar::<_, i32>("SELECT statuses_count FROM users WHERE ap_id = $1")
            .bind("https://remote.example/users/bob")
            .fetch_one(&pool)
            .await
            .expect("the author count reads");
    assert_eq!(author_count, 1, "and the author's post count moved once");
}

/// A remote reply to a local status links to it, bumps its reply counter,
/// and tells its author — the notification the create path's hook lands
/// for remote posters exactly as for local ones.
#[tokio::test]
async fn a_reply_to_a_local_parent_links_notifies_and_counts() {
    let db = common::test_db().await;
    let pool = db.pool().clone();
    let app = common::test_app(pool.clone());
    let state = delivering_state(&pool);
    let signer = bob(&pool).await;
    let alice = common::create_local_user(&pool, "alice").await;
    let alice_token = common::create_app_and_token(&pool, &alice, SCOPES).await;
    let parent = common::create_status(
        &pool,
        &alice,
        "a local post to answer",
        status::Visibility::Public,
    )
    .await;

    let mut activity = mastodon_named("reply-to-alice");
    object_field_mut(&mut activity, "inReplyTo", &parent.uri);

    deliver(&state, &activity, &signer).await;

    let child = stored(
        &pool,
        "https://remote.example/users/bob/statuses/reply-to-alice",
    )
    .await;
    assert_eq!(
        child.in_reply_to_id,
        Some(parent.id),
        "the stored parent is linked by id"
    );
    assert_eq!(
        child.in_reply_to_account_id,
        Some(alice.id),
        "and the local author is known as the answered account"
    );
    assert_eq!(
        child.in_reply_to_ap_id, None,
        "a linked parent leaves no dangling reference"
    );
    assert_eq!(
        child.conversation_id, parent.conversation_id,
        "the reply joins the parent's conversation"
    );

    let parent_entity = common::TestRequest::get(&format!("/api/v1/statuses/{}", parent.id))
        .bearer(&alice_token)
        .send(&app)
        .await;
    common::assert_answer(&parent_entity, StatusCode::OK, "the parent read");
    assert_eq!(
        parent_entity.json_value()["replies_count"],
        1,
        "and the parent counts the answer"
    );

    let response = common::TestRequest::get("/api/v1/notifications")
        .bearer(&alice_token)
        .send(&app)
        .await;
    common::assert_answer(&response, StatusCode::OK, "the mention inbox");
    let notices = response.json_value();
    let mentions: Vec<&Value> = notices
        .as_array()
        .expect("the listing is an array")
        .iter()
        .filter(|notice| notice["type"] == json!("mention"))
        .collect();
    assert_eq!(mentions.len(), 1, "the replied-to author is told once");
    assert_eq!(
        mentions[0]["account"]["acct"], "bob@remote.example",
        "by the remote replier"
    );
    assert_eq!(
        mentions[0]["status"]["id"],
        json!(child.id.to_string()),
        "and the telling names the new status"
    );
}

/// A `Create` for an already stored but deleted `uri` is ignored: a
/// deleted remote post is never re-imported.
#[tokio::test]
async fn a_create_of_a_tombstoned_uri_is_ignored() {
    let db = common::test_db().await;
    let pool = db.pool().clone();
    let app = common::test_app(pool.clone());
    let state = delivering_state(&pool);
    let signer = bob(&pool).await;

    deliver(&state, &fixture(MASTODON), &signer).await;
    let row = stored(&pool, MASTODON_URI).await;
    let _tombstone = status::soft_delete(&pool, row.id)
        .await
        .expect("the delete tombstones");

    deliver(&state, &mastodon_renamed_activity("undead"), &signer).await;

    assert_eq!(
        rows_for_uri(&pool, MASTODON_URI).await,
        1,
        "the re-delivery wrote nothing beside the tombstone"
    );
    let after = stored(&pool, MASTODON_URI).await;
    assert!(
        after.deleted_at.is_some(),
        "and the row is still the tombstone it was"
    );
    let still_there = public_timeline(&app)
        .await
        .into_iter()
        .any(|entry| entry["uri"] == json!(MASTODON_URI));
    assert!(!still_there, "a tombstoned post stays out of the timeline");
}

/// A deactivated author's post is not ingested.
#[tokio::test]
async fn a_deactivated_author_is_ignored() {
    let db = common::test_db().await;
    let pool = db.pool().clone();
    let state = delivering_state(&pool);
    let signer = bob(&pool).await;
    let _deactivated = sqlx::query("UPDATE users SET is_active = false WHERE id = $1")
        .bind(signer.id)
        .execute(&pool)
        .await
        .expect("the deactivation writes");

    deliver(&state, &fixture(MASTODON), &signer).await;

    assert_eq!(
        rows_for_uri(&pool, MASTODON_URI).await,
        0,
        "a retired mirror brings no post in"
    );
}

/// A mention whose actor no lookup can settle drops the mention, not the
/// post.
#[tokio::test]
async fn an_unresolved_mention_is_dropped() {
    let db = common::test_db().await;
    let pool = db.pool().clone();
    let app = common::test_app(pool.clone());
    let state = delivering_state(&pool);
    let signer = bob(&pool).await;

    deliver(&state, &fixture(MASTODON), &signer).await;

    let row = stored(&pool, MASTODON_URI).await;
    assert_eq!(
        mention_count(&pool, row.id).await,
        0,
        "carol is not stored and cannot be fetched — the mention drops"
    );
    assert_eq!(
        sqlx::query_scalar::<_, i64>("SELECT count(*) FROM users WHERE ap_id = $1")
            .bind("https://remote.example/users/carol")
            .fetch_one(&pool)
            .await
            .expect("the user count reads"),
        0,
        "and the failed lookup stored no mirror"
    );
    let entry = public_entry(&app, MASTODON_URI).await;
    assert_eq!(
        entry["mentions"]
            .as_array()
            .expect("mentions is an array")
            .len(),
        0,
        "the post stands, with no mentions"
    );
}

/// `ACCEPT_UNSOLICITED_PUBLIC=false`: an unfollowed author's public post
/// is dropped, and the exemptions — a mention of a local user and a local
/// follow — let one through each.
#[tokio::test]
async fn the_unsolicited_public_setting_gates_new_authors() {
    let db = common::test_db().await;
    let pool = db.pool().clone();
    let app = common::test_app(pool.clone());
    let state = gated_state(&pool);
    let signer = bob(&pool).await;
    let alice = common::create_local_user(&pool, "alice").await;

    deliver(&state, &mastodon_named("unsolicited"), &signer).await;
    assert_eq!(
        rows_for_uri(
            &pool,
            "https://remote.example/users/bob/statuses/unsolicited"
        )
        .await,
        0,
        "off, an unfollowed author's public post is dropped"
    );

    let mut inviting = mastodon_named("mentions-alice");
    mention_local(&mut inviting, &alice);
    deliver(&state, &inviting, &signer).await;
    assert_eq!(
        rows_for_uri(
            &pool,
            "https://remote.example/users/bob/statuses/mentions-alice"
        )
        .await,
        1,
        "a post that mentions a local user passes anyway"
    );

    let open = follow::follow(&pool, &state.config.base_url, &alice, &signer)
        .await
        .expect("the follow writes");
    let _accepted = follow::accept(&pool, open.id)
        .await
        .expect("the follow is accepted");
    deliver(&state, &mastodon_named("solicited"), &signer).await;
    assert_eq!(
        rows_for_uri(&pool, "https://remote.example/users/bob/statuses/solicited").await,
        1,
        "and once somebody here follows, so does the plain post"
    );

    let visible: Vec<String> = public_timeline(&app)
        .await
        .into_iter()
        .filter_map(|entry| entry["uri"].as_str().map(str::to_owned))
        .collect();
    assert!(
        !visible.contains(&"https://remote.example/users/bob/statuses/unsolicited".to_owned()),
        "the dropped post never appears: {visible:?}"
    );
}

/// The Pleroma flavour: `as:Public` in `cc` reads unlisted (which keeps it
/// out of the public timeline), the `contentMap` body and `Link` url are
/// kept, and the `ostatus:conversation` reference is stored as the
/// status' context.
#[tokio::test]
async fn a_pleroma_create_lands_unlisted_with_its_context() {
    let db = common::test_db().await;
    let pool = db.pool().clone();
    let app = common::test_app(pool.clone());
    let state = delivering_state(&pool);
    let signer = bob(&pool).await;
    let _carol = common::create_remote_user(&pool, "carol", "remote.example").await;

    deliver(&state, &fixture(PLEROMA), &signer).await;

    let row = stored(&pool, PLEROMA_URI).await;
    assert_eq!(
        row.visibility,
        status::Visibility::Unlisted,
        "public in cc only is unlisted"
    );
    assert_eq!(
        row.ap_context.as_deref(),
        Some("https://remote.example/contexts/PLEROMAFIX"),
        "the ostatus conversation href is the stored context"
    );
    assert_eq!(
        row.url.as_deref(),
        Some("https://remote.example/notice/PLEROMAFIX"),
        "the Link url wins over the id"
    );
    assert!(
        row.content.contains("A Pleroma-flavoured note"),
        "the contentMap body is what was stored: {}",
        row.content
    );
    assert_eq!(
        mention_count(&pool, row.id).await,
        1,
        "the single-object tag resolves"
    );
    let unlisted_shown = public_timeline(&app)
        .await
        .into_iter()
        .any(|entry| entry["uri"] == json!(PLEROMA_URI));
    assert!(
        !unlisted_shown,
        "an unlisted post stays out of the public timeline"
    );
}

/// The [MRF chain](wired_be::federation::mrf) runs inside the dispatcher,
/// so what a policy rewrites is what gets folded: a `Create` from a host on
/// `followers_only` lands private, the same post from a host on
/// `media_removal` lands with no attachments, and from a host on `reject`
/// it lands nowhere at all.
#[tokio::test]
async fn the_simple_lists_rewrite_what_the_dispatcher_folds() {
    let db = common::test_db().await;
    let pool = db.pool().clone();
    let signer = bob(&pool).await;
    let _carol = common::create_remote_user(&pool, "carol", "remote.example").await;

    set_simple(&pool, &json!({"followers_only": ["remote.example"]})).await;
    deliver(
        &delivering_state(&pool),
        &mastodon_named("mrf-private"),
        &signer,
    )
    .await;
    let private = stored(&pool, &status_uri("mrf-private")).await;
    assert_eq!(
        private.visibility,
        status::Visibility::Private,
        "the note reaches this instance followers-only"
    );

    set_simple(&pool, &json!({"media_removal": ["remote.example"]})).await;
    deliver(
        &delivering_state(&pool),
        &mastodon_named("mrf-nomedia"),
        &signer,
    )
    .await;
    let stripped = stored(&pool, &status_uri("mrf-nomedia")).await;
    assert_eq!(
        media_count(&pool, stripped.id).await,
        0,
        "the fixture's two documents were stripped before the fold"
    );

    set_simple(&pool, &json!({"reject": [["remote.example", "bots"]]})).await;
    deliver(
        &delivering_state(&pool),
        &mastodon_named("mrf-rejected"),
        &signer,
    )
    .await;
    assert_eq!(
        rows_for_uri(&pool, &status_uri("mrf-rejected")).await,
        0,
        "a rejected host's post is never stored"
    );
}

/// The object uri `mastodon_named` gives a post renamed under `n`.
fn status_uri(n: &str) -> String {
    format!("https://remote.example/users/bob/statuses/{n}")
}

/// How many `media_attachments` rows the status `status_id` carries.
async fn media_count(pool: &PgPool, status_id: Uuid) -> i64 {
    sqlx::query_scalar::<_, i64>("SELECT count(*) FROM media_attachments WHERE status_id = $1")
        .bind(status_id)
        .fetch_one(pool)
        .await
        .expect("the media count reads")
}

/// Enables the `simple` policy with the domain lists `lists` configures.
async fn set_simple(pool: &PgPool, lists: &Value) {
    runtime_config::write(
        pool,
        ":pleroma",
        ":mrf",
        &json!({"policies": ["simple"], "simple": lists}),
    )
    .await
    .expect("the mrf entry writes");
}

/// Two `direct` statuses answering the same unstored parent under one
/// context join one conversation, and neither reaches the public
/// timeline.
#[tokio::test]
async fn direct_messages_group_by_context() {
    let db = common::test_db().await;
    let pool = db.pool().clone();
    let app = common::test_app(pool.clone());
    let state = delivering_state(&pool);
    let signer = bob(&pool).await;
    let alice = common::create_local_user(&pool, "alice").await;

    deliver(&state, &direct_create(&alice, "dm-1"), &signer).await;
    deliver(&state, &direct_create(&alice, "dm-2"), &signer).await;

    let one = stored(&pool, "https://remote.example/objects/dm-1").await;
    let two = stored(&pool, "https://remote.example/objects/dm-2").await;
    assert_eq!(
        one.visibility,
        status::Visibility::Direct,
        "addressed to the mentioned account only"
    );
    assert_eq!(two.visibility, status::Visibility::Direct, "both of them");
    assert_eq!(
        one.conversation_id, one.id,
        "the first status of the context is its root"
    );
    assert_eq!(
        two.conversation_id, one.conversation_id,
        "the second joins the conversation the first started under the shared context"
    );
    assert_eq!(
        one.in_reply_to_ap_id.as_deref(),
        Some("https://remote.example/objects/never-delivered"),
        "the unstored parent stays an ap_id reference"
    );
    let dm_shown = public_timeline(&app).await.into_iter().any(|entry| {
        entry["uri"] == json!("https://remote.example/objects/dm-1")
            || entry["uri"] == json!("https://remote.example/objects/dm-2")
    });
    assert!(!dm_shown, "a direct post reaches no public timeline");
}

/// Reads a fixture string as JSON; a broken fixture fails the test.
fn fixture(text: &str) -> Value {
    serde_json::from_str(text).expect("the fixture is valid JSON")
}

/// The Mastodon fixture with every remote id (activity, object, url,
/// context) renamed under `n`, so two deliveries of "the same" post in
/// one database never collide unless a test says they should.
fn mastodon_named(n: &str) -> Value {
    let mut activity = fixture(MASTODON);
    set_field(
        &mut activity,
        "id",
        &format!("https://remote.example/activities/{n}"),
    );
    object_field_mut(
        &mut activity,
        "id",
        &format!("https://remote.example/users/bob/statuses/{n}"),
    );
    object_field_mut(
        &mut activity,
        "url",
        &format!("https://remote.example/@bob/{n}"),
    );
    object_field_mut(
        &mut activity,
        "context",
        &format!("https://remote.example/contexts/{n}"),
    );
    activity
}

/// The Mastodon fixture with its activity id replaced, the object (and so
/// its dedupe `uri`) untouched: the same post announced twice.
fn mastodon_renamed_activity(n: &str) -> Value {
    let mut activity = fixture(MASTODON);
    set_field(
        &mut activity,
        "id",
        &format!("https://remote.example/activities/{n}"),
    );
    activity
}

/// Sets one top-level string field of an activity.
fn set_field(activity: &mut Value, key: &str, value: &str) {
    let Some(top) = activity.as_object_mut() else {
        panic!("the fixture is a JSON object");
    };
    put(top, key, json!(value));
}

/// Sets one string field on the activity's embedded object.
fn object_field_mut(activity: &mut Value, key: &str, value: &str) {
    let Some(object) = activity.get_mut("object").and_then(Value::as_object_mut) else {
        panic!("the fixture embeds an object");
    };
    put(object, key, json!(value));
}

/// Sets one field of a JSON object, dropping whatever stood there.
fn put(object: &mut Map<String, Value>, key: &str, value: Value) {
    let previous = object.insert(key.to_owned(), value);
    drop(previous);
}

/// Replaces the embedded object's `tag` with a mention of the local
/// `alice` beside the fixture's unresolvable `carol`.
fn mention_local(activity: &mut Value, alice: &User) {
    let Some(object) = activity.get_mut("object").and_then(Value::as_object_mut) else {
        panic!("the fixture embeds an object");
    };
    put(
        object,
        "tag",
        json!([
            { "type": "Mention", "href": alice.ap_id, "name": "@alice" },
            { "type": "Mention", "href": "https://remote.example/users/carol", "name": "@carol" },
        ]),
    );
}

/// A `Create` of a `direct` note to `alice`, answering a never-delivered
/// post under the shared dm-thread context, its object id named by `n`.
fn direct_create(alice: &User, n: &str) -> Value {
    json!({
        "id": format!("https://remote.example/activities/{n}"),
        "type": "Create",
        "actor": "https://remote.example/users/bob",
        "to": [alice.ap_id],
        "object": {
            "id": format!("https://remote.example/objects/{n}"),
            "type": "Note",
            "attributedTo": "https://remote.example/users/bob",
            "to": [alice.ap_id],
            "content": format!("<p>direct {n}</p>"),
            "published": "2024-05-03T10:00:00.000Z",
            "inReplyTo": "https://remote.example/objects/never-delivered",
            "context": "https://remote.example/contexts/dm-thread",
        }
    })
}

/// The dispatcher state with `ACCEPT_UNSOLICITED_PUBLIC=false`.
fn gated_state(pool: &PgPool) -> AppState {
    let config = Config::from_lookup(|key| match key {
        wired_be::config::DATABASE_URL_VAR => Some(wired_be::config::TEST_DATABASE_URL.to_owned()),
        wired_be::config::BASE_URL_VAR => Some(wired_be::config::TEST_BASE_URL.to_owned()),
        wired_be::config::ACCEPT_UNSOLICITED_PUBLIC_VAR => Some("false".to_owned()),
        _ => None,
    })
    .expect("the gated test config is valid");
    common::test_app_state(pool.clone(), config)
}

/// The remote author the fixtures deliver as: `bob@remote.example`, the
/// host every fixture id names.
async fn bob(pool: &PgPool) -> User {
    common::create_remote_user(pool, "bob", "remote.example").await
}

/// The stored row under object uri `uri`, tombstone included.
async fn stored(pool: &PgPool, uri: &str) -> Status {
    status::get_by_uri(pool, uri)
        .await
        .expect("the uri read")
        .expect("the status is stored")
}

/// How many `statuses` rows carry object uri `uri`.
async fn rows_for_uri(pool: &PgPool, uri: &str) -> i64 {
    sqlx::query_scalar::<_, i64>("SELECT count(*) FROM statuses WHERE uri = $1")
        .bind(uri)
        .fetch_one(pool)
        .await
        .expect("the row count reads")
}

/// How many `status_mentions` links the status `status_id` carries.
async fn mention_count(pool: &PgPool, status_id: Uuid) -> i64 {
    sqlx::query_scalar::<_, i64>("SELECT count(*) FROM status_mentions WHERE status_id = $1")
        .bind(status_id)
        .fetch_one(pool)
        .await
        .expect("the mention count reads")
}

/// `GET /api/v1/timelines/public`, anonymous, as JSON entities.
async fn public_timeline(app: &Router) -> Vec<Value> {
    let response = common::TestRequest::get("/api/v1/timelines/public")
        .send(app)
        .await;
    common::assert_answer(&response, StatusCode::OK, "the anonymous public timeline");
    response
        .json_value()
        .as_array()
        .expect("the timeline is an array")
        .clone()
}

/// The entity of the status with object uri `uri` from the public
/// timeline, refusing to be absent.
async fn public_entry(app: &Router, uri: &str) -> Value {
    public_timeline(app)
        .await
        .into_iter()
        .find(|entry| entry["uri"] == json!(uri))
        .unwrap_or_else(|| panic!("the public timeline should carry {uri}"))
}
