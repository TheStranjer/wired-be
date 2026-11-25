//! Integration tests for the activity reads: `GET /activities/{uuid}` and
//! `GET /users/{nickname}/statuses/{uuid}/activity` (`src/api/activitypub.rs`,
//! the counterparts of Pleroma's `ActivityPubController.activity/2` and
//! `notice_activity`).
//!
//! `/activities/{uuid}` keys on the activity row's own `UUIDv7` — the id
//! decision 6 mints into `{BASE_URL}/activities/{uuid}` — and answers the
//! `Like`, `EmojiReact`, `Follow` or `Block` behind a relation row, or the
//! `Create`/`Announce` of a status row. A relation document is served only
//! to a signed fetch whose reader is one of the activity's two parties;
//! everyone else — unsigned, unrelated — gets the plain 404. A
//! `Create`/`Announce` follows its status' visibility exactly as the object
//! core does, and a soft-deleted status resolves to no arm at all: 404,
//! never a `Tombstone`. `/users/{nickname}/statuses/{uuid}/activity` wraps
//! the object core's `Note` in its `Create`, the nickname having to name
//! the live local author. Neither route has an HTML branch: an `Accept`
//! naming no `ActivityPub` media type, an unknown or unparsable id, and
//! every refused visibility are the 404 `{"error":"Not found"}`; every
//! answer carries `Vary: Accept`, and `AUTHORIZED_FETCH` gates the reads
//! like it gates the actor and object documents.
//!
//! The fixtures are driven by the real model writes —
//! [`wired_be::models::favourite::favourite`],
//! [`wired_be::models::emoji_reaction::react`],
//! [`wired_be::models::follow::follow`] and [`wired_be::models::block::block`]
//! — so each test fetches exactly the row whose id that write minted. The
//! document rendering itself is covered where it lives, in
//! `src/federation/activities.rs` and `src/federation/note.rs`; what these
//! tests own is the route: the negotiation, the gates, the status codes and
//! the ids end to end.

#![cfg(test)]

use axum::{Router, http::StatusCode};
use chrono::Utc;
use serde_json::{Value, json};
use sqlx::PgPool;
use uuid::Uuid;
use wired_be::{
    config::{Config, Toggle},
    federation::{
        publisher,
        signature::{SignedHeaders, SigningKey, sign_request},
    },
    models::{
        block, emoji_reaction, favourite, follow,
        status::{self, NewStatus, Status, Visibility},
        user::User,
    },
    services::instance_actor::ensure_instance_actor,
    util::time::mastodon_time,
};

use crate::common;

/// The media type of an `ActivityPub` document, charset included.
const ACTIVITY_JSON: &str = "application/activity+json; charset=utf-8";
/// The instance origin of [`common::test_config`].
const BASE: &str = "https://test.example";
/// The `ActivityStreams` public collection a `Like`/`EmojiReact` addresses.
const AS_PUBLIC: &str = "https://www.w3.org/ns/activitystreams#Public";
/// The `ActivityStreams` vocabulary, first entry of every document's
/// `@context`.
const AS_VOCABULARY: &str = "https://www.w3.org/ns/activitystreams";
/// This instance's JSON-LD schema, second entry of every `@context`.
const SCHEMA_URL: &str = "https://test.example/schemas/litepub-0.1.jsonld";
/// The refusal body the `AUTHORIZED_FETCH` gate answers with.
const REFUSAL: &str = "This instance requires authenticated fetches";
/// The plain 404 body every miss on these routes answers with.
const NOT_FOUND: &str = "Not found";

/// `GET`s `path` as an `ActivityPub` client.
async fn fetch_ap(app: &Router, path: &str) -> common::TestResponse {
    common::TestRequest::get(path)
        .header("accept", "application/activity+json")
        .send(app)
        .await
}

/// `GET`s `path` as an `ActivityPub` client whose request `signed` proves.
async fn fetch_signed(app: &Router, path: &str, signed: &SignedHeaders) -> common::TestResponse {
    common::TestRequest::get(path)
        .header("accept", "application/activity+json")
        .header("host", &signed.host)
        .header("date", &signed.date)
        .header("signature", &signed.signature)
        .send(app)
        .await
}

/// Asserts a served document's headers and hands its JSON back.
async fn served(app: &Router, path: &str, signed: Option<&SignedHeaders>) -> Value {
    let response = match signed {
        Some(proof) => fetch_signed(app, path, proof).await,
        None => fetch_ap(app, path).await,
    };
    assert_eq!(response.status, StatusCode::OK, "GET {path} is served");
    assert_eq!(
        response.header("content-type"),
        Some(ACTIVITY_JSON),
        "the document is ActivityPub JSON"
    );
    assert_eq!(response.header("vary"), Some("Accept"), "GET {path}");
    response.json_value()
}

/// The `{BASE_URL}/activities/{uuid}` path of the row `id`.
fn activity_path(id: Uuid) -> String {
    format!("/activities/{id}")
}

/// Signs a `GET` of `https://test.example{path}` as actor `signer`, with
/// `signer`'s stored key pair, the way a peer's authorized fetch would.
fn signed_get_as(signer: &common::Signer, path: &str) -> SignedHeaders {
    let key_id = format!("{}#main-key", signer.ap_id);
    sign_request(
        "GET",
        &format!("{BASE}{path}"),
        None,
        &SigningKey {
            key_id: &key_id,
            private_pem: &signer.private_pem,
        },
        Utc::now(),
    )
    .expect("signing the test fetch")
}

/// The signer of the `AUTHORIZED_FETCH` test: the instance actor, a stored
/// row with the key pair [`ensure_instance_actor`] generated.
#[derive(Debug)]
struct InstanceSigner {
    /// Its actor id.
    ap_id: String,
    /// The private key the fetches are signed with.
    private_pem: String,
}

/// Provisions the instance actor and hands it over as a fetch signer.
async fn instance_signer(pool: &PgPool) -> InstanceSigner {
    let actor = ensure_instance_actor(pool, &common::test_config())
        .await
        .expect("the instance actor is provisioned");
    let private_pem = common::actor_of(pool, &actor)
        .await
        .private_key
        .expect("the provisioned actor holds its private key");
    InstanceSigner {
        ap_id: actor.ap_id,
        private_pem,
    }
}

/// Signs a `GET` of `https://test.example{path}` as [`InstanceSigner`].
fn signed_get_as_instance(signer: &InstanceSigner, path: &str) -> SignedHeaders {
    let key_id = format!("{}#main-key", signer.ap_id);
    sign_request(
        "GET",
        &format!("{BASE}{path}"),
        None,
        &SigningKey {
            key_id: &key_id,
            private_pem: &signer.private_pem,
        },
        Utc::now(),
    )
    .expect("signing the test fetch")
}

/// `follower` follows `target` through [`wired_be::models::follow`], with
/// the accepted state the visibility model reads.
///
/// # Panics
///
/// If the follow write fails or is not granted on the spot.
async fn accepted_follow(pool: &PgPool, follower: &User, target: &User) {
    let opened = follow::follow(pool, BASE, follower, target)
        .await
        .expect("the fixture follow is written");
    assert_eq!(
        opened.state,
        follow::FollowState::Accepted,
        "the follow of `{}` is granted",
        target.nickname
    );
}

/// A `remote` mirror of a peer's status: stored with the remote's own
/// `uri` and `local` clear, the shape [`wired_be::models::status::insert`]
/// writes for an activity this instance received rather than posted.
async fn remote_status(pool: &PgPool, author: &User, id: &str) -> Status {
    status::insert(
        pool,
        BASE,
        &NewStatus {
            uri: Some(format!("https://remote.example/objects/{id}")),
            user_id: author.id,
            content: "a mirrored post".to_owned(),
            raw_text: "a mirrored post".to_owned(),
            ..NewStatus::default()
        },
    )
    .await
    .expect("the remote status fixture inserts")
}

/// Asserts `response` is the plain 404 the ticket prescribes —
/// `{"error":"Not found"}` with `Vary: Accept` — for the case `who` names.
fn assert_not_found(response: &common::TestResponse, who: &str) {
    assert_eq!(response.status, StatusCode::NOT_FOUND, "{who}");
    assert_eq!(
        response.json_value(),
        json!({"error": NOT_FOUND}),
        "the 404 body for {who}"
    );
    assert_eq!(
        response.header("vary"),
        Some("Accept"),
        "the 404 for {who} varies on Accept"
    );
}

/// The `Like` behind a favourite row: served to the favouriter and the
/// liked status' author over a signature, and to nobody else. Its `id` is
/// the row's own `{BASE_URL}/activities/{uuid}`.
#[tokio::test]
async fn a_favourite_is_served_as_its_like_to_its_pair() {
    let db = common::test_db().await;
    let pool = db.pool().clone();
    let alice = common::create_signer(&pool, "alice").await;
    let bob = common::create_signer(&pool, "bob").await;
    let mallory = common::create_signer(&pool, "mallory").await;
    let note = common::create_status(&pool, &alice, "like me", Visibility::Public).await;
    let liked = favourite::favourite(&pool, BASE, bob.id, note.id)
        .await
        .expect("the favourite write runs")
        .expect("this pair had not liked yet");
    let app = common::test_app(pool);
    let path = activity_path(liked.id);

    assert_not_found(&fetch_ap(&app, &path).await, "an unsigned read of a Like");
    assert_not_found(
        &fetch_signed(&app, &path, &signed_get_as(&mallory, &path)).await,
        "an unrelated signer's read of a Like",
    );

    let document = served(&app, &path, Some(&signed_get_as(&bob, &path))).await;
    assert_eq!(
        document,
        json!({
            "@context": [AS_VOCABULARY, SCHEMA_URL, { "@language": "und" }],
            "id": format!("{BASE}/activities/{}", liked.id),
            "type": "Like",
            "actor": bob.ap_id,
            "object": note.uri,
            "published": mastodon_time(&liked.inserted_at),
            "to": [AS_PUBLIC],
            "cc": [],
        }),
        "the whole Like, its id the row's own activity url"
    );
    let as_author = served(&app, &path, Some(&signed_get_as(&alice, &path))).await;
    assert_eq!(
        as_author["id"],
        format!("{BASE}/activities/{}", liked.id),
        "and the liked status' author reaches the same document"
    );
}

/// The `EmojiReact` behind a reaction row: the unicode variant names its
/// emoji in `content` and carries no `tag`; the custom one also carries the
/// note-shaped `Emoji` tag. Both are served only to the reacting pair.
#[tokio::test]
async fn a_reaction_is_served_as_its_emoji_react() {
    let db = common::test_db().await;
    let pool = db.pool().clone();
    let alice = common::create_signer(&pool, "alice").await;
    let bob = common::create_signer(&pool, "bob").await;
    let note = common::create_status(&pool, &alice, "react to me", Visibility::Public).await;
    let unicode = emoji_reaction::react(
        &pool,
        BASE,
        &emoji_reaction::NewReaction {
            user_id: bob.id,
            status_id: note.id,
            emoji: "🎉",
            url: None,
        },
    )
    .await
    .expect("the unicode reaction writes");
    let custom = emoji_reaction::react(
        &pool,
        BASE,
        &emoji_reaction::NewReaction {
            user_id: bob.id,
            status_id: note.id,
            emoji: ":railroad_track:",
            url: Some("https://test.example/emoji/railroad_track.png"),
        },
    )
    .await
    .expect("the custom reaction writes");
    let app = common::test_app(pool);

    assert_not_found(
        &fetch_ap(&app, &activity_path(unicode.id)).await,
        "an unsigned read of an EmojiReact",
    );
    let signed = signed_get_as(&bob, &activity_path(unicode.id));
    let document = served(&app, &activity_path(unicode.id), Some(&signed)).await;
    assert_eq!(
        document,
        json!({
            "@context": [AS_VOCABULARY, SCHEMA_URL, { "@language": "und" }],
            "id": format!("{BASE}/activities/{}", unicode.id),
            "type": "EmojiReact",
            "actor": bob.ap_id,
            "object": note.uri,
            "content": "🎉",
            "published": mastodon_time(&unicode.inserted_at),
            "to": [AS_PUBLIC],
            "cc": [],
        }),
        "a unicode reaction says which emoji and carries no tag"
    );
    assert!(document.get("tag").is_none(), "no tag for a unicode one");

    let custom_signed = signed_get_as(&bob, &activity_path(custom.id));
    let custom_document = served(&app, &activity_path(custom.id), Some(&custom_signed)).await;
    assert_eq!(
        custom_document["type"], "EmojiReact",
        "the custom reaction type"
    );
    assert_eq!(
        custom_document["id"],
        format!("{BASE}/activities/{}", custom.id),
        "its own row's activity url"
    );
    assert_eq!(
        custom_document["content"], ":railroad_track:",
        "the shortcode"
    );
    assert_eq!(
        custom_document["tag"],
        json!([{
            "type": "Emoji",
            "id": "https://test.example/emoji/railroad_track.png",
            "name": ":railroad_track:",
            "icon": {
                "type": "Image",
                "url": "https://test.example/emoji/railroad_track.png",
            },
        }]),
        "and only the custom one carries the Emoji tag"
    );
}

/// The `Follow` behind a follow row: both parties reach it over a
/// signature, and an unrelated signer and an anonymous read do not.
#[tokio::test]
async fn a_follow_is_served_to_follower_and_followee_alone() {
    let db = common::test_db().await;
    let pool = db.pool().clone();
    let alice = common::create_signer(&pool, "alice").await;
    let bob = common::create_signer(&pool, "bob").await;
    let mallory = common::create_signer(&pool, "mallory").await;
    let opened = follow::follow(&pool, BASE, &bob, &alice)
        .await
        .expect("the follow write runs");
    let app = common::test_app(pool);
    let path = activity_path(opened.id);

    assert_not_found(&fetch_ap(&app, &path).await, "an unsigned read of a Follow");
    assert_not_found(
        &fetch_signed(&app, &path, &signed_get_as(&mallory, &path)).await,
        "an unrelated signer's read of a Follow",
    );

    let document = served(&app, &path, Some(&signed_get_as(&bob, &path))).await;
    assert_eq!(
        document,
        json!({
            "@context": [AS_VOCABULARY, SCHEMA_URL, { "@language": "und" }],
            "id": format!("{BASE}/activities/{}", opened.id),
            "type": "Follow",
            "actor": bob.ap_id,
            "object": alice.ap_id,
            "published": mastodon_time(&opened.inserted_at),
            "to": [alice.ap_id],
            "cc": [],
        }),
        "the whole Follow, addressed to the followee alone"
    );
    let as_followee = served(&app, &path, Some(&signed_get_as(&alice, &path))).await;
    assert_eq!(
        as_followee["type"], "Follow",
        "the followee reaches the same document"
    );
}

/// The `Block` behind a block row: the blocker and the blocked account
/// reach it over a signature; everyone else gets the 404.
#[tokio::test]
async fn a_block_is_served_to_blocker_and_blocked_alone() {
    let db = common::test_db().await;
    let pool = db.pool().clone();
    let alice = common::create_signer(&pool, "alice").await;
    let carol = common::create_signer(&pool, "carol").await;
    let mallory = common::create_signer(&pool, "mallory").await;
    let blocked = block::block(&pool, BASE, &alice, &carol)
        .await
        .expect("the block write runs");
    let app = common::test_app(pool);
    let path = activity_path(blocked.id);

    assert_not_found(&fetch_ap(&app, &path).await, "an unsigned read of a Block");
    assert_not_found(
        &fetch_signed(&app, &path, &signed_get_as(&mallory, &path)).await,
        "an unrelated signer's read of a Block",
    );

    let document = served(&app, &path, Some(&signed_get_as(&alice, &path))).await;
    assert_eq!(
        document,
        json!({
            "@context": [AS_VOCABULARY, SCHEMA_URL, { "@language": "und" }],
            "id": format!("{BASE}/activities/{}", blocked.id),
            "type": "Block",
            "actor": alice.ap_id,
            "object": carol.ap_id,
            "published": mastodon_time(&blocked.inserted_at),
            "to": [carol.ap_id],
            "cc": [],
        }),
        "the whole Block, addressed to the blocked account alone"
    );
    let as_blocked = served(&app, &path, Some(&signed_get_as(&carol, &path))).await;
    assert_eq!(as_blocked["id"], document["id"], "and the blocked too");
}

/// A live status' `Create`, reached by the status' own id: the object
/// core's note document wrapped, with the activity id the row's uuid
/// names. `local` visibility is 404 exactly as on the object read.
#[tokio::test]
async fn a_status_create_is_served_by_its_own_id() {
    let db = common::test_db().await;
    let pool = db.pool().clone();
    let alice = common::create_local_user(&pool, "alice").await;
    let note = common::create_status(&pool, &alice, "hello fediverse", Visibility::Public).await;
    let kept = common::create_status(&pool, &alice, "stay at home", Visibility::Local).await;
    let app = common::test_app(pool);

    let document = served(&app, &activity_path(note.id), None).await;
    assert_eq!(document["type"], "Create", "a Create activity");
    assert_eq!(
        document["id"],
        format!("{BASE}/activities/{}", note.id),
        "the status row's own activity url"
    );
    assert_eq!(document["actor"], alice.ap_id, "the author acts");
    assert_eq!(document["published"], mastodon_time(&note.inserted_at));
    assert_eq!(document["object"]["id"], note.uri, "the embedded note");
    assert_eq!(document["object"]["content"], "<p>hello fediverse</p>");
    assert_eq!(document["to"], json!([AS_PUBLIC]), "a public Create");

    assert_not_found(
        &fetch_ap(&app, &activity_path(kept.id)).await,
        "a local-visibility status' Create",
    );
}

/// A reblog's `Announce`, reached by the reblog row's own id: its `uri` is
/// that activity url (decision 6), and the boost travels with the reblog's
/// visibility — a public one open, a private one only to a signed reader
/// the visibility model admits.
#[tokio::test]
async fn a_reblogs_announce_is_gated_by_its_visibility() {
    let db = common::test_db().await;
    let pool = db.pool().clone();
    let alice = common::create_local_user(&pool, "alice").await;
    let bob = common::create_local_user(&pool, "bob").await;
    let dave = common::create_local_user(&pool, "dave").await;
    let carol = common::create_signer(&pool, "carol").await;
    let mallory = common::create_signer(&pool, "mallory").await;
    accepted_follow(&pool, &carol, &dave).await;
    let note = common::create_status(&pool, &bob, "boost me", Visibility::Public).await;
    let (reblog, _wrote_public) =
        status::create_reblog(&pool, BASE, &alice, &note, Visibility::Public)
            .await
            .expect("the public reblog writes");
    // A second original: reblogging is idempotent per (user, original)
    // pair, so dave's private boost must ride a different post.
    let other = common::create_status(&pool, &bob, "boost me too", Visibility::Public).await;
    let (secret, _wrote_secret) =
        status::create_reblog(&pool, BASE, &dave, &other, Visibility::Private)
            .await
            .expect("the private reblog writes");
    let app = common::test_app(pool);

    let document = served(&app, &activity_path(reblog.id), None).await;
    assert_eq!(document["type"], "Announce", "the reblog row's activity");
    assert_eq!(
        document["id"],
        format!("{BASE}/activities/{}", reblog.id),
        "the reblog row's own activity url, which its uri already spells"
    );
    assert_eq!(document["id"], reblog.uri, "and the two agree");
    assert_eq!(document["actor"], alice.ap_id, "the reblogger acts");
    assert_eq!(document["object"], note.uri, "the boosted original");
    assert_eq!(document["published"], mastodon_time(&reblog.inserted_at));

    let path = activity_path(secret.id);
    assert_not_found(
        &fetch_ap(&app, &path).await,
        "an anonymous read of a private Announce",
    );
    assert_not_found(
        &fetch_signed(&app, &path, &signed_get_as(&mallory, &path)).await,
        "an unaffiliated signer's read of a private Announce",
    );
    let follower = signed_get_as(&carol, &path);
    let secret_document = served(&app, &path, Some(&follower)).await;
    assert_eq!(
        secret_document["id"],
        format!("{BASE}/activities/{}", secret.id)
    );
    assert_eq!(
        secret_document["type"], "Announce",
        "a follower of the reblogger reaches the private boost"
    );
    assert_eq!(
        secret_document["object"], other.uri,
        "its own boosted original"
    );
}

/// The `Announce` a boost queued for delivery and the one `/activities/{id}`
/// serves a signed re-fetch are one document: the same `to` and `cc`, for a
/// public and an unlisted boost alike.
#[tokio::test]
async fn a_boosts_served_announce_matches_the_delivered_one() {
    let db = common::test_db().await;
    let pool = db.pool().clone();
    let alice = common::create_local_user(&pool, "alice").await;
    let bob = common::create_local_user(&pool, "bob").await;
    let carol = common::create_signer(&pool, "carol").await;
    let token = common::create_app_and_token(&pool, &alice, "write:statuses").await;
    let app = common::test_app(pool.clone());

    for (visibility, body) in [
        (Visibility::Public, json!({})),
        (Visibility::Unlisted, json!({"visibility": "unlisted"})),
    ] {
        let original = common::create_status(&pool, &bob, "boost me", Visibility::Public).await;
        let response =
            common::TestRequest::post(&format!("/api/v1/statuses/{}/reblog", original.id))
                .bearer(&token)
                .json(&body)
                .send(&app)
                .await;
        common::assert_answer(&response, StatusCode::OK, "the boost is accepted");

        let delivered = sqlx::query_as::<_, (Value,)>(
            "SELECT args->'activity' FROM jobs WHERE kind = $1 \
             AND args->'activity'->>'object' = $2",
        )
        .bind(publisher::KIND_PUBLISH)
        .bind(&original.uri)
        .fetch_one(&pool)
        .await
        .expect("the boost queued exactly one publish job")
        .0;
        assert_eq!(
            delivered["type"], "Announce",
            "{visibility:?}: the delivery"
        );
        let id = delivered["id"].as_str().expect("the announce has an id");
        let path = id.strip_prefix(BASE).expect("the id is under this origin");

        let signed = signed_get_as(&carol, path);
        let refetched = served(&app, path, Some(&signed)).await;
        assert_eq!(
            refetched["to"], delivered["to"],
            "{visibility:?}: to agrees"
        );
        assert_eq!(
            refetched["cc"], delivered["cc"],
            "{visibility:?}: cc agrees"
        );
        assert_eq!(
            refetched, delivered,
            "{visibility:?}: the whole document agrees"
        );
    }
}

/// A tombstoned status or reblog names no activity at all: its id is the
/// plain 404 on `/activities/{uuid}` — never the object route's 410 with a
/// `Tombstone` document.
#[tokio::test]
async fn tombstoned_statuses_are_404_never_a_tombstone_document() {
    let db = common::test_db().await;
    let pool = db.pool().clone();
    let alice = common::create_local_user(&pool, "alice").await;
    let bob = common::create_local_user(&pool, "bob").await;
    let note = common::create_status(&pool, &alice, "here and gone", Visibility::Public).await;
    let (reblog, _written) = status::create_reblog(&pool, BASE, &bob, &note, Visibility::Public)
        .await
        .expect("the reblog fixture writes");
    let _post_gone = status::soft_delete(&pool, note.id)
        .await
        .expect("the post's soft delete runs");
    let _boost_gone = status::soft_delete(&pool, reblog.id)
        .await
        .expect("the reblog's soft delete runs");
    let app = common::test_app(pool);

    assert_not_found(
        &fetch_ap(&app, &activity_path(note.id)).await,
        "the Create of a tombstoned status",
    );
    assert_not_found(
        &fetch_ap(&app, &activity_path(reblog.id)).await,
        "the Announce of a tombstoned reblog",
    );
}

/// The 404 arms both activity routes share: a uuid no row carries, a path
/// segment that is no uuid, a bare JSON ask and a browser's `text/html` —
/// neither route has an HTML branch — are all `{"error":"Not found"}` with
/// `Vary: Accept`.
#[tokio::test]
async fn the_activity_route_misses_are_the_plain_404() {
    let db = common::test_db().await;
    let pool = db.pool().clone();
    let alice = common::create_local_user(&pool, "alice").await;
    let bob = common::create_local_user(&pool, "bob").await;
    let note = common::create_status(&pool, &alice, "only real here", Visibility::Public).await;
    let mirror = remote_status(&pool, &bob, "mirror-1").await;
    let app = common::test_app(pool);

    assert_not_found(
        &fetch_ap(&app, &activity_path(Uuid::now_v7())).await,
        "a uuid no activity row carries",
    );
    assert_not_found(
        &fetch_ap(&app, "/activities/not-a-uuid").await,
        "a path segment that is no uuid",
    );
    assert_not_found(
        &fetch_ap(&app, &activity_path(mirror.id)).await,
        "a mirrored status' row id, which this instance does not own",
    );
    assert_not_found(
        &common::TestRequest::get(&activity_path(note.id))
            .header("accept", "application/json")
            .send(&app)
            .await,
        "a bare JSON ask on the activity route",
    );
    assert_not_found(
        &common::TestRequest::get(&activity_path(note.id))
            .header("accept", "text/html")
            .send(&app)
            .await,
        "a browser's read of an activity document",
    );
    let notice_path = format!("/users/alice/statuses/{}/activity", Uuid::now_v7());
    assert_not_found(
        &fetch_ap(&app, &notice_path).await,
        "a notice activity for an unknown status",
    );
}

/// With `AUTHORIZED_FETCH` on the activity reads demand the signature the
/// actor and object routes already demand: the unsigned read of a public
/// `Create` is the 401 refusal, a signed one is served.
#[tokio::test]
async fn the_authorized_fetch_gate_gates_the_activity_reads() {
    let db = common::test_db().await;
    let pool = db.pool().clone();
    let mut config = common::test_config();
    config.fetch.authorized_fetch = Toggle::new(true);
    let alice = common::create_local_user(&pool, "alice").await;
    let note = common::create_status(&pool, &alice, "gated activity", Visibility::Public).await;
    let app = common::test_app_with_config(pool.clone(), config);
    let path = activity_path(note.id);

    let unsigned = fetch_ap(&app, &path).await;
    assert_eq!(unsigned.status, StatusCode::UNAUTHORIZED, "the gate is on");
    assert_eq!(
        unsigned.json_value()["error"],
        REFUSAL,
        "with the gate's refusal wording"
    );
    assert_eq!(
        unsigned.header("vary"),
        Some("Accept"),
        "even the 401 varies on Accept"
    );

    let actor = instance_signer(&pool).await;
    let signed = signed_get_as_instance(&actor, &path);
    let document = served(&app, &path, Some(&signed)).await;
    assert_eq!(document["type"], "Create", "a signed read unlocks it");
    assert_eq!(document["id"], format!("{BASE}/activities/{}", note.id));
}

/// `GET /users/{nickname}/statuses/{uuid}/activity` answers the very
/// document `/activities/{uuid}` resolves for the same row, and the plain
/// 404 for everything the object core refuses on this path: a nickname
/// that is not the author, a reblog row, a tombstone, a remote mirror, a
/// `local` status and an unsigned read of a private one.
#[tokio::test]
async fn the_notice_activity_route_wraps_the_note_in_its_create() {
    let db = common::test_db().await;
    let pool = db.pool().clone();
    let alice = common::create_signer(&pool, "alice").await;
    let carol = common::create_signer(&pool, "carol").await;
    // A second live local account: the mismatch case must name one that
    // exists, not merely a nickname nobody answers to.
    let bob = common::create_local_user(&pool, "bob").await;
    let note = common::create_status(&pool, &alice, "notice my create", Visibility::Public).await;
    let secret = common::create_status(&pool, &alice, "followers boost", Visibility::Private).await;
    accepted_follow(&pool, &carol, &alice).await;
    let app = common::test_app(pool);
    let path = format!("/users/alice/statuses/{}/activity", note.id);

    let document = served(&app, &path, None).await;
    assert_eq!(document["type"], "Create", "the status' Create activity");
    assert_eq!(
        document["id"],
        format!("{BASE}/activities/{}", note.id),
        "the row's own activity url"
    );
    assert_eq!(document["object"]["id"], note.uri, "the embedded note");
    let same = served(&app, &activity_path(note.id), None).await;
    assert_eq!(document, same, "both routes answer the same document");

    assert_not_found(
        &fetch_ap(
            &app,
            &format!("/users/{}/statuses/{}/activity", bob.nickname, note.id),
        )
        .await,
        "a mismatched nickname",
    );
    assert_not_found(
        &fetch_ap(
            &app,
            &format!("/users/alice/statuses/{}/activity", secret.id),
        )
        .await,
        "an unsigned read of a private status",
    );
    let follower_path = format!("/users/alice/statuses/{}/activity", secret.id);
    let follower = signed_get_as(&carol, &follower_path);
    let follower_document = served(&app, &follower_path, Some(&follower)).await;
    assert_eq!(
        follower_document["id"],
        format!("{BASE}/activities/{}", secret.id),
        "a signed admitted reader reaches the private status' Create"
    );
}

/// The remaining notice-activity 404s: a reblog row (that is an `Announce`,
/// served under `/activities/{uuid}`), a soft-deleted status (404 JSON
/// here, where `/objects/{uuid}` would answer a `Tombstone`), a remote
/// mirror, a `local` status, an unparsable id and a wrong `Accept`.
#[tokio::test]
async fn the_notice_activity_route_refuses_every_other_row() {
    let db = common::test_db().await;
    let pool = db.pool().clone();
    let alice = common::create_local_user(&pool, "alice").await;
    let bob = common::create_local_user(&pool, "bob").await;
    let note = common::create_status(&pool, &alice, "original", Visibility::Public).await;
    let (reblog, _written) = status::create_reblog(&pool, BASE, &bob, &note, Visibility::Public)
        .await
        .expect("the reblog fixture writes");
    let doomed = common::create_status(&pool, &alice, "doomed", Visibility::Public).await;
    let _gone = status::soft_delete(&pool, doomed.id)
        .await
        .expect("the soft delete runs");
    let mirror = remote_status(&pool, &bob, "mirror-2").await;
    let kept = common::create_status(&pool, &alice, "instance only", Visibility::Local).await;
    let app = common::test_app(pool);

    assert_not_found(
        &fetch_ap(&app, &format!("/users/bob/statuses/{}/activity", reblog.id)).await,
        "a reblog row, which is an Announce not a Create",
    );
    assert_not_found(
        &fetch_ap(
            &app,
            &format!("/users/alice/statuses/{}/activity", doomed.id),
        )
        .await,
        "a soft-deleted status, which is 404 JSON and never a Tombstone",
    );
    assert_not_found(
        &fetch_ap(&app, &format!("/users/bob/statuses/{}/activity", mirror.id)).await,
        "a mirrored status' row",
    );
    assert_not_found(
        &fetch_ap(&app, &format!("/users/alice/statuses/{}/activity", kept.id)).await,
        "a local-visibility status",
    );
    assert_not_found(
        &fetch_ap(&app, "/users/alice/statuses/not-a-uuid/activity").await,
        "a path segment that is no uuid",
    );
    assert_not_found(
        &common::TestRequest::get(&format!("/users/alice/statuses/{}/activity", note.id))
            .header("accept", "text/html")
            .send(&app)
            .await,
        "a browser's read of a notice activity",
    );
}

/// The instance's own test configuration keeps the origin every assertion
/// above names, and [`common::create_local_user`] mints actor ids on it.
#[test]
fn the_fixture_origin_is_the_test_origin() {
    let config = Config::for_tests();
    assert_eq!(config.base_url, BASE, "the fixture origin");
    assert_eq!(
        config.frontend_base_url, config.base_url,
        "and no separate frontend to redirect to — which these routes \
         would not use anyway, having no HTML branch"
    );
}
