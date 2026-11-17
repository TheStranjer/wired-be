//! End-to-end federation of inbound content (WBE-224 scenario 3 and the chat
//! half of scenario 7): a real, signed peer delivery read back through this
//! instance's public API.
//!
//! Every scenario here runs the round trip the [`remote::MockPeer`] exists
//! for: a mock actor delivers a signed activity to this instance's shared
//! inbox over the in-process router (which authenticates it and mirrors the
//! actor under its loopback host, decision 7), [`common::run_all_jobs`] folds
//! the `federator.incoming` job plus anything it queues (the `Update`,
//! backfill, delivery and notification work), and the result is read back
//! through the client API — the timelines, `GET /api/v1/statuses/{id}`,
//! `accounts/{id}/statuses`, `context`, `polls/{id}`, the reactions and chat
//! endpoints, and `GET /api/v1/notifications` — never off a raw row dump. A
//! delivery that folded into the wrong status, the wrong audience or the
//! wrong place therefore cannot pass.
//!
//! What the scenarios pin, per the ticket:
//!
//! - a remote `Create Note` at each visibility, each read as the mock's actor
//!   mirror under its loopback host: `public` in `timelines/public` and the
//!   author's `accounts/{id}/statuses`, `unlisted` out of the public timeline
//!   but on the profile and in a follower's home, `private` only to an
//!   approved follower, and `direct` only in the recipient's conversations
//!   with nothing leaked to a timeline;
//! - the rich content of one delivered note — an `attachment` (the remote
//!   media shape with `url`/`preview_url`), a `Hashtag` (the status in
//!   `timelines/tag/{name}`), a `Mention` of a local user (a `mention` telling
//!   and visibility), a `sensitive`/`summary` content warning, a `Question`
//!   poll the viewer votes back on, and a custom `Emoji` that carries through
//!   the reaction path;
//! - an `Update` of the same object uri editing the status and leaving the
//!   client's `history`, and a restatement changing nothing further;
//! - a `Delete` taking the note down through the client API and stepping its
//!   parent's counter, with a late `Create` of the same uri ignored and the
//!   author's post count unmoved;
//! - `Like` / `Undo{Like}`, `EmojiReact` / `Undo{EmojiReact}` and `Announce`
//!   / `Undo{Announce}` moving the counters, the `reactions` read and the
//!   public-timeline boost and raising or clearing the matching telling;
//! - a delivered reply backfilling its missing parent over real HTTP and a
//!   parent the peer 404s degrading to the reply standing alone;
//! - a remote ballot marking a local poll's option and telling its author,
//!   idempotent when restated; and
//! - a delivered `Create{ChatMessage}` opening the pair's chat with exactly
//!   one `pleroma:chat_mention`, its sender's `Delete` withdrawing both and a
//!   stranger's leaving them standing.
//!
//! The per-type dispatcher corners belong to `ingest.rs`, `note_edits.rs`,
//! `poll_votes.rs`, `emoji_reactions.rs`, `reblogs.rs`, `favourites.rs`,
//! `notification_generation.rs` and `chat_federation.rs`; this module owns
//! the real-HTTP, real-signature, public-API view of the same folds.
//!
//! [`common::run_all_jobs`]: crate::common::run_all_jobs

#![cfg(test)]

use axum::Router;
use axum::http::StatusCode;
use serde_json::{Value, json};
use sqlx::PgPool;
use wired_be::{
    models::{notification::TYPE_CHAT_MENTION, user::User},
    state::AppState,
};

use crate::{common, remote};

/// The mock's author account, `bob@127.0.0.1:{port}`.
const BOB: &str = "bob";
/// A second mock account, used for a stranger's delivery.
const DAVE: &str = "dave";
/// The scopes every local caller carries: the status and conversation reads,
/// the notification read, the follow and poll-vote writes.
const SCOPES: &str = "read write follow";
/// The `ActivityStreams` public address, as every delivered note names it.
const AS_PUBLIC: &str = remote::PUBLIC;
/// The status write.
const STATUSES: &str = "/api/v1/statuses";
/// The account routes the scenarios read.
const ACCOUNTS: &str = "/api/v1/accounts";
/// The legacy follow write keyed by handle or actor URL.
const FOLLOWS: &str = "/api/v1/follows";
/// The Pleroma emoji-reaction reads' route root.
const REACTIONS: &str = "/api/v1/pleroma/statuses";
/// The Pleroma chat routes' root.
const CHATS: &str = "/api/v1/pleroma/chats";
/// The fixed `published` every delivered note carries: a past instant, so no
/// clock-skew clamp ever moves it and a later `updated` postdates it.
const PUBLISHED: &str = "2024-05-01T10:00:00.000Z";

// --- the fixtures ----------------------------------------------------------------------

/// A local account named `nickname` that can sign its own deliveries, with a
/// [`SCOPES`] bearer token.
async fn local(pool: &PgPool, nickname: &str) -> (User, String) {
    let created = remote::keyed_local_user(pool, nickname).await;
    let token = common::create_app_and_token(pool, &created, SCOPES).await;
    (created, token)
}

/// `GET`s `path` as `token` (anonymous when `None`), asserted 200, answered
/// as a JSON array.
async fn list(app: &Router, token: Option<&str>, path: &str) -> Vec<Value> {
    let request = common::TestRequest::get(path);
    let response = match token {
        Some(found) => request.bearer(found).send(app).await,
        None => request.send(app).await,
    };
    common::assert_answer(&response, StatusCode::OK, &format!("the read of `{path}`"));
    response
        .json_value()
        .as_array()
        .unwrap_or_else(|| panic!("`{path}` should answer a JSON array"))
        .clone()
}

/// `GET`s `path` as `token` (anonymous when `None`), returned for the caller
/// to assert on.
async fn get(app: &Router, token: Option<&str>, path: &str) -> common::TestResponse {
    let request = common::TestRequest::get(path);
    match token {
        Some(found) => request.bearer(found).send(app).await,
        None => request.send(app).await,
    }
}

/// `POST /api/v1/statuses` with `body`, asserted 200, answered as the created
/// entity.
async fn post_status(app: &Router, token: &str, body: Value) -> Value {
    let response = common::TestRequest::post(STATUSES)
        .bearer(token)
        .json(&body)
        .send(app)
        .await;
    common::assert_answer(&response, StatusCode::OK, "the status write");
    response.json_value()
}

/// `GET /api/v1/accounts/lookup` for `acct`, asserted 200, answered as the
/// `Account` entity.
async fn lookup(app: &Router, acct: &str) -> Value {
    get(app, None, &format!("{ACCOUNTS}/lookup?acct={acct}"))
        .await
        .json_value()
}

// --- reading the timelines, context and notifications back ----------------------------

/// `GET /api/v1/timelines/public`, anonymous.
async fn public_timeline(app: &Router) -> Vec<Value> {
    list(app, None, "/api/v1/timelines/public").await
}

/// `GET /api/v1/timelines/home` as `token`.
async fn home_timeline(app: &Router, token: &str) -> Vec<Value> {
    list(app, Some(token), "/api/v1/timelines/home").await
}

/// `GET /api/v1/timelines/tag/{name}`, anonymous.
async fn tag_timeline(app: &Router, name: &str) -> Vec<Value> {
    list(app, None, &format!("/api/v1/timelines/tag/{name}")).await
}

/// `GET /api/v1/accounts/{id}/statuses` as `token` (anonymous when `None`).
async fn account_statuses(app: &Router, token: Option<&str>, account_id: &str) -> Vec<Value> {
    list(app, token, &format!("{ACCOUNTS}/{account_id}/statuses")).await
}

/// `GET /api/v1/notifications` as `token`.
async fn notifications(app: &Router, token: &str) -> Vec<Value> {
    list(app, Some(token), "/api/v1/notifications").await
}

/// `token`'s notifications of `kind`, in delivery order.
async fn told_of(app: &Router, token: &str, kind: &str) -> Vec<Value> {
    notifications(app, token)
        .await
        .into_iter()
        .filter(|notice| notice["type"] == json!(kind))
        .collect()
}

/// `token`'s conversation whose `last_status` is the status with uri `uri`,
/// when the inbox carries one.
async fn conversation_for(app: &Router, token: &str, uri: &str) -> Option<Value> {
    list(app, Some(token), "/api/v1/conversations")
        .await
        .into_iter()
        .find(|entry| entry["last_status"]["uri"] == json!(uri))
}

/// The `uri` text of an entity.
fn uri_of(entity: &Value) -> String {
    entity["uri"]
        .as_str()
        .expect("the entity carries a uri string")
        .to_owned()
}

/// The `id` text of an entity.
fn id_of(entity: &Value) -> String {
    entity["id"]
        .as_str()
        .expect("the entity carries an id string")
        .to_owned()
}

/// Whether `entries` carry a status with object uri `uri`.
fn carries_uri(entries: &[Value], uri: &str) -> bool {
    entries.iter().any(|entry| entry["uri"] == json!(uri))
}

/// The entity in `entries` with object uri `uri`, refusing to be absent.
fn entry_for<'a>(entries: &'a [Value], uri: &str) -> &'a Value {
    entries
        .iter()
        .find(|entry| entry["uri"] == json!(uri))
        .unwrap_or_else(|| panic!("a listing should carry {uri}"))
}

// --- building and delivering the peer's activities -------------------------------------

/// Merges `extra`'s fields over `target`, dropping what they displace.
fn merge(target: &mut Value, extra: Value) {
    let (Some(map), Value::Object(additions)) = (target.as_object_mut(), extra) else {
        panic!("both documents are JSON objects");
    };
    for (key, value) in additions {
        let _previous = map.insert(key, value);
    }
}

/// A `Note`-family object on the peer's origin named `name`, authored by
/// `actor`, whose `content` is `content`, public by default; every field of
/// `extra` — the `to`/`cc` addressing, `tag`, `attachment`, `inReplyTo`,
/// `summary`, `sensitive`, `updated`, or a `Question`'s `oneOf` — merges over
/// those defaults.
fn note_object(
    peer: &remote::MockPeer,
    actor: &str,
    name: &str,
    content: &str,
    extra: Value,
) -> Value {
    let mut object = json!({
        "id": peer.note(name),
        "type": "Note",
        "attributedTo": actor,
        "to": [AS_PUBLIC],
        "cc": [],
        "content": content,
        "published": PUBLISHED,
    });
    merge(&mut object, extra);
    object
}

/// A signed-`POST`-ready activity of `kind` by the peer's actor `actor`,
/// named `name` on the peer's host, whose `object` is `object` (decision 6's
/// ids are what the dispatcher's containment asks of a peer's own activity).
fn activity(peer: &remote::MockPeer, kind: &str, name: &str, actor: &str, object: Value) -> Value {
    let mut envelope = json!({
        "id": peer.activity_id(name),
        "type": kind,
        "actor": actor,
        "object": Value::Null,
    });
    envelope["object"] = object;
    envelope
}

/// A `Create` of a `Note`-family object on the peer named `name`: the same
/// builder [`note_object`] and [`activity`] compose, in the call the visibility
/// and content scenarios deliver.
fn create_note(
    peer: &remote::MockPeer,
    actor: &str,
    name: &str,
    content: &str,
    extra: Value,
) -> Value {
    let object = note_object(peer, actor, name, content, extra);
    activity(peer, "Create", name, actor, object)
}

/// Delivers `activity` to this instance's shared inbox as the peer's actor
/// `nick`, asserts the `202 Accepted` the inbox answers, and drains the fold
/// plus everything it queued.
async fn deliver(
    app: &Router,
    state: &AppState,
    peer: &remote::MockPeer,
    nick: &str,
    activity: &Value,
) {
    let kind = activity["type"].as_str().unwrap_or("activity");
    let response = peer
        .deliver(app, &peer.actor(nick), remote::INBOX, activity)
        .await;
    assert_eq!(
        response.status,
        StatusCode::ACCEPTED,
        "the inbox folds the mock's {kind}"
    );
    let _drained = common::run_all_jobs(state).await;
}

/// Makes the local `alice_token` an accepted follower of the mock's `bob`: the
/// follow resolves and delivers over the wire, and bob's own `Accept` back
/// through the inbox grants it.
async fn follow_bob(
    app: &Router,
    state: &AppState,
    peer: &remote::MockPeer,
    alice_token: &str,
    accept_name: &str,
) {
    let actor = peer.actor(BOB);
    let _target = common::TestRequest::post(FOLLOWS)
        .bearer(alice_token)
        .json(&json!({"uri": actor}))
        .send(app)
        .await;
    let _sent = common::run_all_jobs(state).await;
    let follows = peer.delivered("Follow");
    let follow_id = follows
        .last()
        .expect("alice's Follow reached the peer")
        .activity()["id"]
        .as_str()
        .expect("the Follow carries its id")
        .to_owned();
    let accept = activity(peer, "Accept", accept_name, &actor, json!(follow_id));
    deliver(app, state, peer, BOB, &accept).await;
}

// --- the scenarios ---------------------------------------------------------------------

/// Scenario: a public remote note. It lands on the anonymous public timeline
/// and on the author's own profile listing, each as the mock's `bob` mirror
/// under its loopback host, with the zero counters a mirrored post starts at.
#[tokio::test]
async fn a_public_remote_note_lands_on_the_public_timeline_and_its_profile() {
    let db = common::test_db().await;
    let pool = db.pool().clone();
    let (app, state) = remote::app_and_state(pool.clone());
    let peer = remote::MockPeer::start().await;
    let actor = peer.actor(BOB);

    let uri = peer.note("public-1");
    let create = activity(
        &peer,
        "Create",
        "public-1",
        &actor,
        note_object(
            &peer,
            &actor,
            "public-1",
            "<p>hello from bob</p>",
            json!({}),
        ),
    );
    deliver(&app, &state, &peer, BOB, &create).await;

    let public = public_timeline(&app).await;
    let entry = entry_for(&public, &uri);
    assert_eq!(
        entry["account"]["acct"],
        json!(peer.handle(BOB)),
        "the status reads under its author's loopback handle (decision 7)"
    );
    assert_eq!(
        entry["visibility"],
        json!("public"),
        "the `to: Public` addressing"
    );
    assert_eq!(
        entry["content"],
        json!("<p>hello from bob</p>"),
        "the delivered body"
    );
    assert_eq!(
        entry["reblogs_count"],
        json!(0),
        "a mirror starts uncounted"
    );
    assert_eq!(entry["favourites_count"], json!(0), "on both counters");

    let bob = entry["account"]["id"].as_str().expect("the author id");
    let profile = account_statuses(&app, None, bob).await;
    assert!(
        carries_uri(&profile, &uri),
        "and the author's profile carries it"
    );
}

/// Scenario: an unlisted remote note. The public address in `cc` only keeps it
/// out of the public timeline while a reader who has the link — an anonymous
/// status read, the author's profile, and a follower's home timeline — still
/// reaches it.
#[tokio::test]
async fn an_unlisted_remote_note_stays_off_the_public_timeline() {
    let db = common::test_db().await;
    let pool = db.pool().clone();
    let (app, state) = remote::app_and_state(pool.clone());
    let peer = remote::MockPeer::start().await;
    let (_alice, alice_token) = local(&pool, "ei-unlisted-alice").await;
    follow_bob(&app, &state, &peer, &alice_token, "unlisted-accept").await;
    let actor = peer.actor(BOB);

    let uri = peer.note("unlisted-1");
    let create = activity(
        &peer,
        "Create",
        "unlisted-1",
        &actor,
        note_object(
            &peer,
            &actor,
            "unlisted-1",
            "<p>quietly public</p>",
            json!({"to": [format!("{actor}/followers")], "cc": [AS_PUBLIC]}),
        ),
    );
    deliver(&app, &state, &peer, BOB, &create).await;

    let public = public_timeline(&app).await;
    assert!(
        !carries_uri(&public, &uri),
        "an unlisted post is absent from the public timeline"
    );

    // Reachable to a follower's home, on the author's profile, and to anyone
    // who holds the id directly — that is what separates unlisted from private.
    let home = home_timeline(&app, &alice_token).await;
    assert!(carries_uri(&home, &uri), "a follower's home carries it");
    let entry = entry_for(&home, &uri);
    let id = id_of(entry);
    let bob = entry["account"]["id"].as_str().expect("the author id");
    let profile = account_statuses(&app, None, bob).await;
    assert!(
        carries_uri(&profile, &uri),
        "the author's profile carries it"
    );
    let anon = get(&app, None, &format!("{STATUSES}/{id}")).await;
    common::assert_answer(&anon, StatusCode::OK, "an unlisted post is readable by id");
}

/// Scenario: a followers-only remote note. Addressed to the author's followers
/// collection and to nobody public, it reaches only an approved follower: the
/// anonymous read is the 404 and a follower's home carries it.
#[tokio::test]
async fn a_followers_only_remote_note_reaches_only_an_approved_follower() {
    let db = common::test_db().await;
    let pool = db.pool().clone();
    let (app, state) = remote::app_and_state(pool.clone());
    let peer = remote::MockPeer::start().await;
    let (_alice, alice_token) = local(&pool, "ei-private-alice").await;
    follow_bob(&app, &state, &peer, &alice_token, "private-accept").await;
    let actor = peer.actor(BOB);

    let uri = peer.note("private-1");
    let create = activity(
        &peer,
        "Create",
        "private-1",
        &actor,
        note_object(
            &peer,
            &actor,
            "private-1",
            "<p>for followers</p>",
            json!({"to": [format!("{actor}/followers")]}),
        ),
    );
    deliver(&app, &state, &peer, BOB, &create).await;

    let home = home_timeline(&app, &alice_token).await;
    assert!(
        carries_uri(&home, &uri),
        "the approved follower's home carries it"
    );
    let follower_id = entry_for(&home, &uri)["id"].as_str().expect("a local id");
    let as_follower = get(
        &app,
        Some(&alice_token),
        &format!("{STATUSES}/{follower_id}"),
    )
    .await;
    common::assert_answer(&as_follower, StatusCode::OK, "the follower's status read");
    let anonymous = get(&app, None, &format!("{STATUSES}/{follower_id}")).await;
    assert_eq!(
        anonymous.status,
        StatusCode::NOT_FOUND,
        "a stranger reading the private post by id gets the 404"
    );
}

/// Scenario: a direct remote note. Addressed to the mentioned local user and
/// to nobody public, it appears only in that recipient's conversations, is the
/// 404 to an anonymous read, and never reaches the public timeline.
#[tokio::test]
async fn a_direct_remote_note_reaches_its_recipient_and_no_timeline() {
    let db = common::test_db().await;
    let pool = db.pool().clone();
    let (app, state) = remote::app_and_state(pool.clone());
    let peer = remote::MockPeer::start().await;
    let (alice, alice_token) = local(&pool, "ei-direct-alice").await;
    let actor = peer.actor(BOB);

    let uri = peer.note("direct-1");
    let object = note_object(
        &peer,
        &actor,
        "direct-1",
        &format!(
            "<p>psst <span class=\"h\"><a href=\"{}\">@alice</a></span></p>",
            alice.ap_id
        ),
        json!({
            "to": [alice.ap_id],
            "tag": [{"type": "Mention", "href": alice.ap_id, "name": "@alice"}],
        }),
    );
    deliver(
        &app,
        &state,
        &peer,
        BOB,
        &activity(&peer, "Create", "direct-1", &actor, object),
    )
    .await;

    let public = public_timeline(&app).await;
    assert!(
        !carries_uri(&public, &uri),
        "a direct post leaks to no public timeline"
    );
    let conversation = conversation_for(&app, &alice_token, &uri)
        .await
        .expect("the recipient's conversations carry the DM");
    assert_eq!(
        conversation["last_status"]["account"]["acct"],
        json!(peer.handle(BOB)),
        "read back as the mock's actor mirror"
    );
    let dm_id = id_of(&conversation["last_status"]);
    let as_recipient = get(&app, Some(&alice_token), &format!("{STATUSES}/{dm_id}")).await;
    common::assert_answer(&as_recipient, StatusCode::OK, "the recipient reads the DM");
    let anonymous = get(&app, None, &format!("{STATUSES}/{dm_id}")).await;
    assert_eq!(
        anonymous.status,
        StatusCode::NOT_FOUND,
        "the direct post is the 404 to an anonymous read"
    );
}

/// Scenario: one delivered note's rich content — a `Document` attachment with
/// its remote media shape, a `Hashtag` tag listing the status under
/// `timelines/tag/{name}`, a `Mention` of a local user raising a `mention`
/// telling and visibility, and a `sensitive` content warning rendered as
/// `spoiler_text` over the marked body.
#[tokio::test]
async fn a_rich_remote_note_lands_its_media_hashtag_mention_and_warning() {
    let db = common::test_db().await;
    let pool = db.pool().clone();
    let (app, state) = remote::app_and_state(pool.clone());
    let peer = remote::MockPeer::start().await;
    let (alice, alice_token) = local(&pool, "ei-rich-alice").await;
    let actor = peer.actor(BOB);

    let uri = peer.note("rich-1");
    let create = create_note(
        &peer,
        &actor,
        "rich-1",
        "<p>look, a photo</p>",
        json!({
            "summary": "a content warning",
            "sensitive": true,
            "tag": [
                {"type": "Hashtag", "name": "#Rust", "href": "https://remote.example/tags/rust"},
                {"type": "Mention", "href": alice.ap_id, "name": "@alice"},
            ],
            "attachment": [{
                "type": "Document",
                "mediaType": "image/png",
                "url": "https://remote.example/media/rich.png",
                "name": "a chart",
                "width": 800,
                "height": 600,
            }],
        }),
    );
    deliver(&app, &state, &peer, BOB, &create).await;

    let entry = entry_for(&public_timeline(&app).await, &uri).clone();
    assert_eq!(
        entry["spoiler_text"],
        json!("a content warning"),
        "the CW renders"
    );
    assert_eq!(entry["sensitive"], json!(true), "folded behind the flag");
    let media = entry["media_attachments"]
        .as_array()
        .expect("media is an array");
    assert_remote_media(&media[0]);
    let tags: Vec<&str> = entry["tags"]
        .as_array()
        .expect("tags is an array")
        .iter()
        .map(|tag| tag["name"].as_str().expect("a named tag"))
        .collect();
    assert_eq!(tags, ["rust"], "the hashtag tag renders, lowercased");

    assert!(
        carries_uri(&tag_timeline(&app, "rust").await, &uri),
        "the status shows in the tag listing"
    );
    let told = told_of(&app, &alice_token, "mention").await;
    assert_eq!(told.len(), 1, "the mentioned local user is told once");
    assert_eq!(
        told[0]["status"]["uri"],
        json!(uri),
        "and the telling names the note"
    );
}

/// Asserts `attachment` renders the delivered remote `Document`'s media shape:
/// the document url served at `url`, `preview_url` and `remote_url` alike, with
/// its alt text.
fn assert_remote_media(attachment: &Value) {
    let expected = json!("https://remote.example/media/rich.png");
    assert_eq!(attachment["url"], expected, "the media url");
    assert_eq!(attachment["remote_url"], expected, "and the remote origin");
    assert_eq!(attachment["preview_url"], expected, "with its preview");
    assert_eq!(
        attachment["description"],
        json!("a chart"),
        "and its alt text"
    );
}

/// Scenario: a delivered `Question` poll. Its options and future `endTime`
/// land, the viewer reads them through `GET /api/v1/polls/{id}`, and voting
/// back on the remote poll marks the option the same way a local one would.
#[tokio::test]
async fn a_remote_question_note_lands_a_poll_the_viewer_can_vote_on() {
    let db = common::test_db().await;
    let pool = db.pool().clone();
    let (app, state) = remote::app_and_state(pool.clone());
    let peer = remote::MockPeer::start().await;
    let (_alice, alice_token) = local(&pool, "ei-poll-alice").await;
    let actor = peer.actor(BOB);

    let uri = peer.note("poll-1");
    let object = note_object(
        &peer,
        &actor,
        "poll-1",
        "<p>red or blue?</p>",
        json!({
            "type": "Question",
            "endTime": "2999-01-01T00:00:00Z",
            "oneOf": [
                {"type": "Note", "name": "Red", "replies": {"totalItems": 3}},
                {"type": "Note", "name": "Blue", "replies": {"totalItems": 7}},
            ],
        }),
    );
    deliver(
        &app,
        &state,
        &peer,
        BOB,
        &activity(&peer, "Create", "poll-1", &actor, object),
    )
    .await;

    let public = public_timeline(&app).await;
    let entry = entry_for(&public, &uri);
    let poll_id = id_of(&entry["poll"]);
    let stand = get(
        &app,
        Some(&alice_token),
        &format!("/api/v1/polls/{poll_id}"),
    )
    .await
    .json_value();
    assert_eq!(
        stand["votes_count"],
        json!(10),
        "the reported option totals"
    );
    assert_eq!(
        stand["expired"],
        json!(false),
        "a future endTime leaves it open"
    );
    assert_eq!(
        stand["options"]
            .as_array()
            .expect("options is an array")
            .iter()
            .map(|option| option["votes_count"].clone())
            .collect::<Vec<_>>(),
        vec![json!(3), json!(7)],
        "option by option, in order"
    );

    let vote = common::TestRequest::post(&format!("/api/v1/polls/{poll_id}/votes"))
        .bearer(&alice_token)
        .json(&json!({"choices": [0]}))
        .send(&app)
        .await;
    common::assert_answer(&vote, StatusCode::OK, "voting back on the remote poll");
    assert_eq!(
        vote.json_value()["votes_count"],
        json!(11),
        "the ballot counts"
    );
    assert_eq!(
        vote.json_value()["voted"],
        json!(true),
        "and marks the voter as having voted"
    );
}

/// Scenario: a remote custom emoji through the reaction path. A delivered
/// `EmojiReact` naming a `:shortcode:` with its own `Emoji` tag lands in the
/// reactions read carrying the remote image url, and tells the author.
#[tokio::test]
async fn a_remote_custom_emoji_reaction_carries_its_image_url() {
    let db = common::test_db().await;
    let pool = db.pool().clone();
    let (app, state) = remote::app_and_state(pool.clone());
    let peer = remote::MockPeer::start().await;
    let (alice, alice_token) = local(&pool, "ei-emoji-alice").await;
    let posted = post_status(
        &app,
        &alice_token,
        json!({"status": "react with a custom emoji"}),
    )
    .await;
    let actor = peer.actor(BOB);

    let react = {
        let mut react = activity(
            &peer,
            "EmojiReact",
            "emoji-react",
            &actor,
            json!(uri_of(&posted)),
        );
        merge(
            &mut react,
            json!({
                "content": ":ralph:",
                "to": [alice.ap_id],
                "tag": [{"type": "Emoji", "name": ":ralph:", "icon": {"type": "Image", "url": "https://remote.example/emoji/ralph.png"}}],
            }),
        );
        react
    };
    deliver(&app, &state, &peer, BOB, &react).await;

    let id = id_of(&posted);
    let reactions = list(
        &app,
        Some(&alice_token),
        &format!("{REACTIONS}/{id}/reactions"),
    )
    .await;
    assert_eq!(reactions.len(), 1, "one reaction landed");
    assert_eq!(reactions[0]["name"], json!(":ralph:"), "as the shortcode");
    assert_eq!(
        reactions[0]["url"],
        json!("https://remote.example/emoji/ralph.png"),
        "carrying the delivered emoji's own image"
    );
    let inline = get(&app, Some(&alice_token), &format!("{STATUSES}/{id}"))
        .await
        .json_value();
    assert_eq!(
        inline["pleroma"]["emoji_reactions"][0]["url"],
        json!("https://remote.example/emoji/ralph.png"),
        "and rendering it inline on the status"
    );
    assert_eq!(
        told_of(&app, &alice_token, "pleroma:emoji_reaction")
            .await
            .len(),
        1
    );
}

/// Scenario: an `Update` of a delivered note edits it and leaves the client's
/// `history` with the superseded version, and a later `Update` restating the
/// same version records nothing further.
#[tokio::test]
async fn an_update_of_a_remote_note_edits_it_and_keeps_the_history() {
    let db = common::test_db().await;
    let pool = db.pool().clone();
    let (app, state) = remote::app_and_state(pool.clone());
    let peer = remote::MockPeer::start().await;
    let actor = peer.actor(BOB);

    let uri = peer.note("edit-1");
    let original = create_note(&peer, &actor, "edit-1", "<p>first draft</p>", json!({}));
    deliver(&app, &state, &peer, BOB, &original).await;
    let public = public_timeline(&app).await;
    let id = id_of(entry_for(&public, &uri));

    let first_edit = note_object(
        &peer,
        &actor,
        "edit-1",
        "<p>second draft</p>",
        json!({"updated": "2024-05-01T12:00:00.000Z"}),
    );
    let update1 = activity(&peer, "Update", "edit-1-u1", &actor, first_edit);
    deliver(&app, &state, &peer, BOB, &update1).await;

    let shown = get(&app, None, &format!("{STATUSES}/{id}"))
        .await
        .json_value();
    assert_eq!(
        shown["content"],
        json!("<p>second draft</p>"),
        "the edit rewrote the body"
    );
    assert_ne!(shown["edited_at"], Value::Null, "and stamped the edit");
    let history = list(&app, None, &format!("{STATUSES}/{id}/history")).await;
    assert_eq!(
        history.len(),
        2,
        "the superseded version beside the current one"
    );
    assert_eq!(
        history[0]["content"],
        json!("<p>first draft</p>"),
        "oldest first"
    );

    let restated = note_object(
        &peer,
        &actor,
        "edit-1",
        "<p>second draft again</p>",
        json!({"updated": "2024-05-01T11:00:00.000Z"}),
    );
    let update2 = activity(&peer, "Update", "edit-1-u2", &actor, restated);
    deliver(&app, &state, &peer, BOB, &update2).await;
    let after = get(&app, None, &format!("{STATUSES}/{id}"))
        .await
        .json_value();
    assert_eq!(
        after["content"],
        json!("<p>second draft</p>"),
        "a stale update rewrote nothing"
    );
    assert_eq!(
        list(&app, None, &format!("{STATUSES}/{id}/history"))
            .await
            .len(),
        2,
        "and recorded no third version"
    );
}

/// Scenario: a `Delete` of a delivered reply takes it down and steps its
/// parent's counter back, and a `Create` of the same uri arriving after the
/// delete is ignored — no resurrection, and the author's post count unmoved.
#[tokio::test]
async fn a_delete_of_a_remote_note_takes_it_down_and_ignores_a_late_create() {
    let db = common::test_db().await;
    let pool = db.pool().clone();
    let (app, state) = remote::app_and_state(pool.clone());
    let peer = remote::MockPeer::start().await;
    let actor = peer.actor(BOB);

    let parent_uri = peer.note("del-parent");
    let child_uri = peer.note("del-child");
    let parent = create_note(&peer, &actor, "del-parent", "<p>the parent</p>", json!({}));
    let child = create_note(
        &peer,
        &actor,
        "del-child",
        "<p>the answer</p>",
        json!({"inReplyTo": parent_uri}),
    );
    deliver(&app, &state, &peer, BOB, &parent).await;
    deliver(&app, &state, &peer, BOB, &child).await;

    let public = public_timeline(&app).await;
    let parent_entry = entry_for(&public, &parent_uri);
    let parent_id = id_of(parent_entry);
    assert_eq!(
        parent_entry["replies_count"],
        json!(1),
        "the child counted on its parent"
    );
    let child_id = id_of(entry_for(&public, &child_uri));

    let delete = activity(&peer, "Delete", "del-act", &actor, json!(child_uri));
    deliver(&app, &state, &peer, BOB, &delete).await;
    assert_eq!(
        get(&app, None, &format!("{STATUSES}/{child_id}"))
            .await
            .status,
        StatusCode::NOT_FOUND,
        "the deleted reply is gone from the reads"
    );
    let after_delete = get(&app, None, &format!("{STATUSES}/{parent_id}"))
        .await
        .json_value();
    assert_eq!(
        after_delete["replies_count"],
        json!(0),
        "and its counter stepped back down"
    );
    let count_after_delete = lookup(&app, &peer.handle(BOB)).await["statuses_count"].clone();

    let late = create_note(&peer, &actor, "del-child", "<p>too late</p>", json!({}));
    deliver(&app, &state, &peer, BOB, &late).await;
    assert_eq!(
        get(&app, None, &format!("{STATUSES}/{child_id}"))
            .await
            .status,
        StatusCode::NOT_FOUND,
        "the Create of a deleted uri is ignored — no resurrection"
    );
    let count_after_late = lookup(&app, &peer.handle(BOB)).await["statuses_count"].clone();
    assert_eq!(
        count_after_late, count_after_delete,
        "and the ignored create left the author's post count unchanged"
    );
}

/// Scenario: the favourite and its withdrawal. A delivered `Like` raises the
/// author's `favourite_count` and one telling; the `Undo` takes both back.
#[tokio::test]
async fn a_remote_like_and_its_undo_move_the_count_and_telling() {
    let db = common::test_db().await;
    let pool = db.pool().clone();
    let (app, state) = remote::app_and_state(pool.clone());
    let peer = remote::MockPeer::start().await;
    let (_alice, alice_token) = local(&pool, "ei-like-alice").await;
    let posted = post_status(&app, &alice_token, json!({"status": "favourite me"})).await;
    let id = id_of(&posted);
    let actor = peer.actor(BOB);

    let like = activity(&peer, "Like", "like-1", &actor, json!(uri_of(&posted)));
    deliver(&app, &state, &peer, BOB, &like).await;
    let liked = get(&app, Some(&alice_token), &format!("{STATUSES}/{id}"))
        .await
        .json_value();
    assert_eq!(
        liked["favourites_count"],
        json!(1),
        "the delivered like raised the count"
    );
    let told = told_of(&app, &alice_token, "favourite").await;
    assert_eq!(told.len(), 1, "and raised one telling");
    assert_eq!(
        told[0]["account"]["acct"],
        json!(peer.handle(BOB)),
        "by the mock's actor"
    );

    let like_id = like["id"]
        .as_str()
        .expect("the Like carries its id")
        .to_owned();
    let inner = json!({"id": like_id, "type": "Like", "actor": actor});
    deliver(
        &app,
        &state,
        &peer,
        BOB,
        &activity(&peer, "Undo", "like-undo", &actor, inner),
    )
    .await;
    let unliked = get(&app, Some(&alice_token), &format!("{STATUSES}/{id}"))
        .await
        .json_value();
    assert_eq!(
        unliked["favourites_count"],
        json!(0),
        "the undo took the count back"
    );
    assert!(
        told_of(&app, &alice_token, "favourite").await.is_empty(),
        "and cleared the telling"
    );
}

/// Scenario: the emoji reaction and its withdrawal. A delivered `EmojiReact`
/// lands in the reactions read and the inline entity and tells the author;
/// the `Undo` removes both.
#[tokio::test]
async fn a_remote_reaction_and_its_undo_land_in_the_reactions_read() {
    let db = common::test_db().await;
    let pool = db.pool().clone();
    let (app, state) = remote::app_and_state(pool.clone());
    let peer = remote::MockPeer::start().await;
    let (alice, alice_token) = local(&pool, "ei-react-alice").await;
    let posted = post_status(&app, &alice_token, json!({"status": "react to me"})).await;
    let id = id_of(&posted);
    let actor = peer.actor(BOB);

    let react = {
        let mut react = activity(
            &peer,
            "EmojiReact",
            "react-1",
            &actor,
            json!(uri_of(&posted)),
        );
        merge(&mut react, json!({"content": "🎉", "to": [alice.ap_id]}));
        react
    };
    deliver(&app, &state, &peer, BOB, &react).await;
    let reactions = list(
        &app,
        Some(&alice_token),
        &format!("{REACTIONS}/{id}/reactions"),
    )
    .await;
    assert_eq!(reactions.len(), 1, "one reaction landed");
    assert_eq!(reactions[0]["name"], json!("🎉"), "the delivered emoji");
    assert_eq!(
        told_of(&app, &alice_token, "pleroma:emoji_reaction")
            .await
            .len(),
        1
    );

    let react_id = react["id"]
        .as_str()
        .expect("the EmojiReact carries its id")
        .to_owned();
    let inner = json!({"id": react_id, "type": "EmojiReact", "actor": actor});
    deliver(
        &app,
        &state,
        &peer,
        BOB,
        &activity(&peer, "Undo", "react-undo", &actor, inner),
    )
    .await;
    assert!(
        list(
            &app,
            Some(&alice_token),
            &format!("{REACTIONS}/{id}/reactions")
        )
        .await
        .is_empty(),
        "the undo emptied the reactions read"
    );
    assert!(
        told_of(&app, &alice_token, "pleroma:emoji_reaction")
            .await
            .is_empty(),
        "and cleared the reaction telling"
    );
}

/// Scenario: the boost and its withdrawal. A delivered public `Announce`
/// lands a boost of the author's post on the public timeline and raises the
/// `reblog` telling; the `Undo` takes the boost back out and clears the
/// telling.
#[tokio::test]
async fn a_remote_announce_and_its_undo_boost_the_public_timeline() {
    let db = common::test_db().await;
    let pool = db.pool().clone();
    let (app, state) = remote::app_and_state(pool.clone());
    let peer = remote::MockPeer::start().await;
    let (_alice, alice_token) = local(&pool, "ei-announce-alice").await;
    let posted = post_status(&app, &alice_token, json!({"status": "boost me"})).await;
    let id = id_of(&posted);
    let actor = peer.actor(BOB);

    let announce = {
        let mut announce = activity(&peer, "Announce", "boost-1", &actor, json!(uri_of(&posted)));
        merge(&mut announce, json!({"to": [AS_PUBLIC]}));
        announce
    };
    let announce_id = announce["id"]
        .as_str()
        .expect("the Announce carries its id")
        .to_owned();
    deliver(&app, &state, &peer, BOB, &announce).await;

    assert!(
        carries_uri(&public_timeline(&app).await, &announce_id),
        "the boost appears on the public timeline under the Announce id"
    );
    let boosted = get(&app, Some(&alice_token), &format!("{STATUSES}/{id}"))
        .await
        .json_value();
    assert_eq!(
        boosted["reblogs_count"],
        json!(1),
        "and the original counts the boost"
    );
    assert_eq!(
        told_of(&app, &alice_token, "reblog").await.len(),
        1,
        "the author is told once"
    );

    let inner = json!({"id": announce_id, "type": "Announce", "actor": actor});
    deliver(
        &app,
        &state,
        &peer,
        BOB,
        &activity(&peer, "Undo", "boost-undo", &actor, inner),
    )
    .await;
    assert!(
        !carries_uri(&public_timeline(&app).await, &announce_id),
        "the undo withdrew the boost from the timeline"
    );
    let unboosted = get(&app, Some(&alice_token), &format!("{STATUSES}/{id}"))
        .await
        .json_value();
    assert_eq!(
        unboosted["reblogs_count"],
        json!(0),
        "and stepped the count back"
    );
    assert!(
        told_of(&app, &alice_token, "reblog").await.is_empty(),
        "and cleared the telling"
    );
}

/// Scenario: thread backfill. A delivered reply whose parent this instance has
/// never seen fetches the missing parent from the mock over real HTTP and
/// renders the full `context` with the ancestor present in the right order.
#[tokio::test]
async fn a_delivered_reply_backfills_its_missing_parent_from_the_peer() {
    let db = common::test_db().await;
    let pool = db.pool().clone();
    let (app, state) = remote::app_and_state(pool.clone());
    let peer = remote::MockPeer::start().await;
    let actor = peer.actor(BOB);

    // The mock's own `/objects/parent` is the ancestor: the reply answers it,
    // and the backfill fetch dereferences it over the wire.
    let parent_uri = peer.note("parent");
    let child_uri = peer.note("child");
    let object = note_object(
        &peer,
        &actor,
        "child",
        "<p>answering the parent</p>",
        json!({"inReplyTo": parent_uri, "context": peer.note("thread")}),
    );
    deliver(
        &app,
        &state,
        &peer,
        BOB,
        &activity(&peer, "Create", "child", &actor, object),
    )
    .await;

    let public = public_timeline(&app).await;
    assert!(
        carries_uri(&public, &child_uri),
        "the reply landed even though its parent was unknown on arrival"
    );
    assert!(
        carries_uri(&public, &parent_uri),
        "and the backfill fetched and ingested the parent"
    );
    let child_id = id_of(entry_for(&public, &child_uri));
    let context = get(&app, None, &format!("{STATUSES}/{child_id}/context"))
        .await
        .json_value();
    let ancestors: Vec<&str> = context["ancestors"]
        .as_array()
        .expect("ancestors is an array")
        .iter()
        .map(|entry| entry["uri"].as_str().expect("a uri'd ancestor"))
        .collect();
    assert_eq!(
        ancestors,
        [parent_uri.as_str()],
        "the fetched parent is the sole ancestor"
    );
}

/// Scenario: the degraded backfill. A reply to a parent the mock 404s stands
/// on its own — the thread stays as stored, no ancestor is invented — the way
/// the code degrades today without dropping the delivered reply.
#[tokio::test]
async fn a_reply_to_a_missing_parent_stands_when_the_peer_404s() {
    let db = common::test_db().await;
    let pool = db.pool().clone();
    let (app, state) = remote::app_and_state(pool.clone());
    let peer = remote::MockPeer::start().await;
    let actor = peer.actor(BOB);

    // A path the mock serves nothing at: the backfill's fetch is a 404, which
    // the resolver treats as a thread that legitimately starts here.
    let missing = format!("{}/gone-parent", peer.origin());
    let child_uri = peer.note("orphan");
    let object = note_object(
        &peer,
        &actor,
        "orphan",
        "<p>answering nothing</p>",
        json!({"inReplyTo": missing}),
    );
    deliver(
        &app,
        &state,
        &peer,
        BOB,
        &activity(&peer, "Create", "orphan", &actor, object),
    )
    .await;

    let public = public_timeline(&app).await;
    assert!(
        carries_uri(&public, &child_uri),
        "the reply stands on the timeline despite its parent being gone"
    );
    let child_id = id_of(entry_for(&public, &child_uri));
    let context = get(&app, None, &format!("{STATUSES}/{child_id}/context"))
        .await
        .json_value();
    let ancestors = context["ancestors"]
        .as_array()
        .expect("the context carries an ancestors array");
    assert!(
        ancestors.is_empty(),
        "and the context carries no invented ancestor"
    );
}

/// Scenario: a remote ballot on a local poll. A delivered `Create` of the
/// answer shape `src/federation/handlers/vote.rs` accepts marks the option's
/// count through `GET /api/v1/polls/{id}`, is idempotent when restated, and
/// moves the local poster's `poll` notification.
#[tokio::test]
async fn a_remote_ballot_marks_a_local_poll_and_tells_its_author() {
    let db = common::test_db().await;
    let pool = db.pool().clone();
    let (app, state) = remote::app_and_state(pool.clone());
    let peer = remote::MockPeer::start().await;
    let (_alice, alice_token) = local(&pool, "ei-ballot-alice").await;
    let actor = peer.actor(BOB);

    let posted = post_status(
        &app,
        &alice_token,
        json!({"status": "tea or coffee?", "poll": {"options": ["tea", "coffee"], "expires_in": 3600}}),
    )
    .await;
    let poll_id = id_of(&posted["poll"]);
    let question_uri = uri_of(&posted);

    let vote = vote_create(&peer, &actor, "ballot-1", &question_uri, "tea");
    deliver(&app, &state, &peer, BOB, &vote).await;
    let stand = get(
        &app,
        Some(&alice_token),
        &format!("/api/v1/polls/{poll_id}"),
    )
    .await
    .json_value();
    assert_eq!(
        stand["options"][0]["votes_count"],
        json!(1),
        "the delivered ballot marked the tea option"
    );
    assert_eq!(stand["votes_count"], json!(1), "and the poll's total");
    assert_eq!(
        told_of(&app, &alice_token, "poll").await.len(),
        1,
        "the author is told once"
    );

    deliver(&app, &state, &peer, BOB, &vote).await;
    let restated = get(
        &app,
        Some(&alice_token),
        &format!("/api/v1/polls/{poll_id}"),
    )
    .await
    .json_value();
    assert_eq!(
        restated["votes_count"],
        json!(1),
        "the restated ballot counted nothing further"
    );
}

/// Scenario: an inbound chat message. The mock's `Create{ChatMessage}` opens
/// the pair's chat with exactly one `pleroma:chat_mention`, readable through
/// the chat endpoints; the sender's `Delete` withdraws the message and the
/// telling, and a stranger's `Delete` leaves both standing.
#[tokio::test]
async fn a_delivered_chat_message_lands_the_pair_and_one_telling() {
    let db = common::test_db().await;
    let pool = db.pool().clone();
    let (app, state) = remote::app_and_state(pool.clone());
    let peer = remote::MockPeer::start().await;
    let (alice, alice_token) = local(&pool, "ei-chat-alice").await;
    let actor = peer.actor(BOB);

    let message_uri = peer.note("chat-hi");
    let create = chat_create(&peer, &actor, "chat-hi", &alice.ap_id, "<p>hi alice</p>");
    deliver(&app, &state, &peer, BOB, &create).await;

    let told = told_of(&app, &alice_token, TYPE_CHAT_MENTION).await;
    assert_eq!(told.len(), 1, "exactly one chat_mention for the recipient");
    assert!(
        told[0].get("status").is_none(),
        "and the telling names no status"
    );
    // The recipient's own chat is keyed by the message id the telling carries;
    // it is read back through the chat endpoints, never a row dump.
    let chat_id = told[0]["chat_message"]["chat_id"]
        .as_str()
        .expect("the chat mention names its chat")
        .to_owned();
    let chat = get(&app, Some(&alice_token), &format!("{CHATS}/{chat_id}"))
        .await
        .json_value();
    assert_eq!(chat["unread"], json!(1), "the recipient's side is unread");
    let messages = list(
        &app,
        Some(&alice_token),
        &format!("{CHATS}/{chat_id}/messages"),
    )
    .await;
    assert_eq!(
        messages[0]["content"],
        json!("<p>hi alice</p>"),
        "the delivered text renders in the chat's messages"
    );

    // A stranger's Delete leaves the message and its telling standing.
    let dave = peer.actor(DAVE);
    deliver(
        &app,
        &state,
        &peer,
        DAVE,
        &chat_delete(&peer, &dave, "chat-steal", &message_uri),
    )
    .await;
    assert_eq!(
        told_of(&app, &alice_token, TYPE_CHAT_MENTION).await.len(),
        1,
        "the stranger moved nothing"
    );

    // The sender's own Delete withdraws both.
    deliver(
        &app,
        &state,
        &peer,
        BOB,
        &chat_delete(&peer, &actor, "chat-gone", &message_uri),
    )
    .await;
    assert!(
        told_of(&app, &alice_token, TYPE_CHAT_MENTION)
            .await
            .is_empty(),
        "the telling went with it"
    );
    let after = list(
        &app,
        Some(&alice_token),
        &format!("{CHATS}/{chat_id}/messages"),
    )
    .await;
    assert!(
        after.is_empty(),
        "and the withdrawn message leaves the chat's pages"
    );
}

// --- scenario-local builders ------------------------------------------------------------

/// A `Create` of the answer document `src/federation/handlers/vote.rs` folds:
/// a bodyless `Note` naming option `option` and answering the question at
/// `question_uri`, on the peer's origin named `name`.
fn vote_create(
    peer: &remote::MockPeer,
    actor: &str,
    name: &str,
    question_uri: &str,
    option: &str,
) -> Value {
    let object = json!({
        "id": format!("{}/objects/vote-{}", peer.origin(), name),
        "type": "Note",
        "attributedTo": actor,
        "to": [AS_PUBLIC],
        "inReplyTo": question_uri,
        "name": option,
    });
    peer.activity("Create", name, actor, &object)
}

/// A `Create{ChatMessage}` from `actor` to the local account `recipient_ap`,
/// the object named `name` on the peer, the text `content`.
fn chat_create(
    peer: &remote::MockPeer,
    actor: &str,
    name: &str,
    recipient_ap: &str,
    content: &str,
) -> Value {
    let object = json!({
        "id": peer.note(name),
        "type": "ChatMessage",
        "attributedTo": actor,
        "to": [recipient_ap],
        "content": content,
        "published": "2024-06-01T10:00:00.000Z",
    });
    peer.activity("Create", name, actor, &object)
}

/// A `Delete` by `actor` of the chat message named `message_uri`, its own
/// activity named `name`.
fn chat_delete(peer: &remote::MockPeer, actor: &str, name: &str, message_uri: &str) -> Value {
    let object = json!({"id": message_uri, "type": "ChatMessage"});
    peer.activity("Delete", name, actor, &object)
}
