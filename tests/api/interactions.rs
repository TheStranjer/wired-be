//! Inbound interactions over federation (WBE-177): the dispatcher's own
//! entry point [`handle_incoming`] folding a peer's `Like`, `EmojiReact`
//! and `Announce` — and their `Undo`s — onto this instance's rows
//! (`src/federation/handlers/interactions.rs`).
//!
//! Every test delivers the fixture activity through the real dispatcher —
//! the same job the inbox routes enqueue, run in-process with the signer's
//! stored mirror as its own row — and reads the answer back from the
//! database and from the status and notification endpoints, exactly what a
//! local author would see. What this pins, per the ticket:
//!
//! - a delivered like folds a `favourites` row keyed by the activity id,
//!   the author's counter, and exactly one telling; a restated pair marks
//!   nothing twice, and the `Undo` — by bare id or wrapped inner — takes
//!   row, count and telling back;
//! - a reaction lands with the activity's own `Emoji` tag url, content the
//!   local rules refuse stores nothing, and one emoji's undo removes only
//!   its own row;
//! - an announce is a reblog row whose uri is the activity id, whose
//!   visibility the activity's addressing states, and which folds onto the
//!   original when it names a boost; a restated pair is ignored and the
//!   withdrawn id stays reserved;
//! - a target that is not visible to its actor — a stranger's private or
//!   direct post, a non-public boost — folds nothing, and an `Undo` signed
//!   by another actor withdraws nothing;
//! - an unknown remote object is fetched and ingested before the fold, a
//!   gone answer folds nothing, and a `Like` carrying one emoji is the
//!   Misskey reaction in disguise, while `Dislike` folds nothing at all.
//!
//! [`handle_incoming`]: wired_be::federation::dispatch::handle_incoming

#![cfg(test)]

use axum::extract::{Path, State};
use axum::http::StatusCode;
use axum::routing::get;
use axum::{Json, Router};
use serde_json::{Value, json};
use sqlx::PgPool;
use uuid::Uuid;
use wired_be::{
    config::TEST_BASE_URL,
    federation::dispatch::handle_incoming,
    models::{
        follow,
        notification::{TYPE_EMOJI_REACTION, TYPE_FAVOURITE, TYPE_REBLOG},
        status::{self, NewStatus, Status, Visibility},
        user::User,
    },
    state::AppState,
};

use crate::common;

/// The remote actors the fixtures deliver as, and the public address.
const BOB: &str = "https://remote.example/users/bob";
const MALLORY: &str = "https://remote.example/users/mallory";
const PUBLIC: &str = "https://www.w3.org/ns/activitystreams#Public";

/// The job args the inbox routes write for a delivery signed by `who`.
fn incoming_args(activity: &Value, who: &User) -> Value {
    json!({"activity": activity, "signer_id": who.id.to_string(), "inbox_owner": null})
}

/// Runs one activity through the real dispatcher and fails the test if
/// the job wants a retry or refuses the document. (An ignored activity
/// completes the same: the fold's refusals are outcomes, not errors.)
async fn deliver(state: &AppState, activity: &Value, who: &User) {
    handle_incoming(state.clone(), incoming_args(activity, who))
        .await
        .expect("the interaction fixture delivery completes the job");
}

/// The dispatcher state over the stock test config — whose
/// `HTTP_ALLOW_PRIVATE_HOSTS` lets the resolver's client reach the mocks.
fn delivering_state(pool: &PgPool) -> AppState {
    common::test_app_state(pool.clone(), common::test_config())
}

// --- the delivered activities --------------------------------------------------

/// An interaction of `kind` by `actor` against `object`, ids named by `n`,
/// with the activity fields `extra` merged over the fixture.
fn interaction(kind: &str, actor: &str, object: &Value, n: u32, extra: Value) -> Value {
    let host = actor.split('/').nth(2).expect("the fixture actor is a url");
    let act = json!({
        "id": format!("https://{host}/activities/{}-{n}", kind.to_lowercase()),
        "type": kind,
        "actor": actor,
        "object": object,
    });
    merge(act, extra)
}

/// A wrapped-inner `Undo` of `inner` by `actor`.
fn undo(actor: &str, inner: &Value, n: u32) -> Value {
    let host = actor.split('/').nth(2).expect("the fixture actor is a url");
    json!({
        "id": format!("https://{host}/activities/undo-{n}"),
        "type": "Undo",
        "actor": actor,
        "object": inner,
    })
}

/// The activity id [`interaction`] would mint for this kind, actor and `n`.
fn interaction_id(kind: &str, actor: &str, n: u32) -> String {
    let host = actor.split('/').nth(2).expect("the fixture actor is a url");
    format!("https://{host}/activities/{}-{n}", kind.to_lowercase())
}

/// The fixture status's federation-facing id, what every fold resolves.
fn target_of(post: &Status) -> Value {
    json!(post.uri)
}

/// Merge `extra`'s fields over `document`.
fn merge(document: Value, extra: Value) -> Value {
    let Value::Object(fields) = extra else {
        return document;
    };
    let mut map = document.as_object().expect("the fixture is a map").clone();
    map.extend(fields);
    Value::Object(map)
}

// --- reading the state back -----------------------------------------------------

/// A read token for `who` and an app to spend it on.
struct Viewer {
    app: Router,
    token: String,
}

async fn viewer(pool: &PgPool, who: &User) -> Viewer {
    Viewer {
        app: common::test_app(pool.clone()),
        token: common::create_app_and_token(pool, who, "read").await,
    }
}

impl Viewer {
    /// `GET /api/v1/statuses/:id` as this viewer.
    async fn status(&self, id: Uuid) -> Value {
        let response = common::TestRequest::get(&format!("/api/v1/statuses/{id}"))
            .bearer(&self.token)
            .send(&self.app)
            .await;
        assert_eq!(response.status, StatusCode::OK, "reading the status");
        response.json_value()
    }

    /// `GET /api/v1/notifications` as this viewer.
    async fn notifications(&self) -> Vec<Value> {
        let response = common::TestRequest::get("/api/v1/notifications")
            .bearer(&self.token)
            .send(&self.app)
            .await;
        assert_eq!(response.status, StatusCode::OK, "reading notifications");
        response.json_value().as_array().expect("an array").clone()
    }

    /// This viewer's notifications of `kind`, in delivery order.
    async fn told_of(&self, kind: &str) -> Vec<Value> {
        self.notifications()
            .await
            .into_iter()
            .filter(|n| n["type"] == json!(kind))
            .collect()
    }
}

/// `(reactor ap_id, status uri, activity id)` for every favourite row.
async fn favourite_rows(pool: &PgPool) -> Vec<(String, String, String)> {
    sqlx::query_as::<_, (String, String, String)>(
        "SELECT u.ap_id, s.uri, f.ap_id FROM favourites f \
         JOIN users u ON u.id = f.user_id \
         JOIN statuses s ON s.id = f.status_id ORDER BY f.inserted_at, f.id",
    )
    .fetch_all(pool)
    .await
    .expect("the favourites read")
}

/// `(reactor ap_id, status uri, emoji, url, activity id)` per reaction row.
async fn reaction_rows(pool: &PgPool) -> Vec<(String, String, String, Option<String>, String)> {
    sqlx::query_as::<_, (String, String, String, Option<String>, String)>(
        "SELECT u.ap_id, s.uri, r.emoji, r.url, r.ap_id FROM emoji_reactions r \
         JOIN users u ON u.id = r.user_id \
         JOIN statuses s ON s.id = r.status_id ORDER BY r.inserted_at, r.id",
    )
    .fetch_all(pool)
    .await
    .expect("the reactions read")
}

/// `(uri, visibility, inserted-at day, original uri, booster ap_id)` per
/// live boost row, oldest first.
async fn boost_rows(pool: &PgPool) -> Vec<(String, String, String, String, String)> {
    sqlx::query_as::<_, (String, String, String, String, String)>(
        "SELECT s.uri, s.visibility, to_char(s.inserted_at AT TIME ZONE 'UTC', 'YYYY-MM-DD'), \
         o.uri, u.ap_id \
         FROM statuses s \
         JOIN statuses o ON o.id = s.reblog_of_id \
         JOIN users u ON u.id = s.user_id \
         WHERE s.deleted_at IS NULL ORDER BY s.inserted_at, s.id",
    )
    .fetch_all(pool)
    .await
    .expect("the boosts read")
}

/// Alice and her fresh public `text`: the interaction target of most tests.
async fn alice_post(pool: &PgPool, text: &str) -> (User, Status) {
    let alice = common::create_local_user(pool, "alice").await;
    let posted = common::create_status(pool, &alice, text, Visibility::Public).await;
    (alice, posted)
}

// --- Like -----------------------------------------------------------------------

#[tokio::test]
async fn a_delivered_like_folds_a_favourite_a_count_and_a_telling() {
    let db = common::test_db().await;
    let pool = db.pool().clone();
    let state = delivering_state(&pool);
    let bob = common::create_remote_user(&pool, "bob", "remote.example").await;
    let (alice, posted) = alice_post(&pool, "a cat photo").await;

    deliver(
        &state,
        &interaction("Like", BOB, &target_of(&posted), 1, Value::Null),
        &bob,
    )
    .await;

    let like_id = interaction_id("Like", BOB, 1);
    assert_eq!(
        favourite_rows(&pool).await,
        vec![(BOB.to_owned(), posted.uri.clone(), like_id.clone())],
        "one favourite keyed to the delivered pair and its activity id"
    );
    let seen = viewer(&pool, &alice).await;
    assert_eq!(seen.status(posted.id).await["favourites_count"], json!(1));
    let told = seen.told_of(TYPE_FAVOURITE).await;
    assert_eq!(told.len(), 1, "the author is told exactly once");
    assert_eq!(told[0]["account"]["id"], json!(bob.id.to_string()));
    assert_eq!(told[0]["status"]["id"], json!(posted.id.to_string()));
    assert_eq!(told[0]["status"]["uri"], json!(posted.uri));
}

#[tokio::test]
async fn a_restated_like_folds_one_row_and_tells_once() {
    let db = common::test_db().await;
    let pool = db.pool().clone();
    let state = delivering_state(&pool);
    let bob = common::create_remote_user(&pool, "bob", "remote.example").await;
    let (alice, posted) = alice_post(&pool, "twice over").await;

    // The same pair restated under a fresh activity id marks nothing twice:
    // the pair, not the id, is what the row is keyed on.
    deliver(
        &state,
        &interaction("Like", BOB, &target_of(&posted), 1, Value::Null),
        &bob,
    )
    .await;
    deliver(
        &state,
        &interaction("Like", BOB, &target_of(&posted), 2, Value::Null),
        &bob,
    )
    .await;

    assert_eq!(favourite_rows(&pool).await.len(), 1);
    let seen = viewer(&pool, &alice).await;
    assert_eq!(
        seen.status(posted.id).await["favourites_count"],
        json!(1),
        "the count follows the row, not the deliveries"
    );
    assert_eq!(seen.told_of(TYPE_FAVOURITE).await.len(), 1);
}

#[tokio::test]
async fn an_undo_of_a_like_takes_back_row_count_and_telling() {
    let db = common::test_db().await;
    let pool = db.pool().clone();
    let state = delivering_state(&pool);
    let bob = common::create_remote_user(&pool, "bob", "remote.example").await;
    let (alice, posted) = alice_post(&pool, "here today").await;
    let like_id = interaction_id("Like", BOB, 1);

    deliver(
        &state,
        &interaction("Like", BOB, &target_of(&posted), 1, Value::Null),
        &bob,
    )
    .await;
    // Mastodon peers undo with the id alone, which is enough: the row
    // carries it. A second undo of the empty id completes and refuses.
    deliver(&state, &undo(BOB, &json!(like_id.clone()), 2), &bob).await;
    let seen = viewer(&pool, &alice).await;
    assert!(
        favourite_rows(&pool).await.is_empty(),
        "the undo emptied the table"
    );
    assert_eq!(seen.status(posted.id).await["favourites_count"], json!(0));
    assert!(
        seen.told_of(TYPE_FAVOURITE).await.is_empty(),
        "the telling is gone with the row"
    );

    deliver(&state, &undo(BOB, &json!(like_id), 3), &bob).await;
    assert!(
        favourite_rows(&pool).await.is_empty(),
        "the second undo undoes nothing"
    );

    // A re-delivery after the undo folds again: the withdrawal freed the
    // pair, unlike a tombstoned boost id.
    deliver(
        &state,
        &interaction("Like", BOB, &target_of(&posted), 4, Value::Null),
        &bob,
    )
    .await;
    assert_eq!(favourite_rows(&pool).await.len(), 1);
    assert_eq!(seen.status(posted.id).await["favourites_count"], json!(1));
}

#[tokio::test]
async fn an_undo_carrying_the_inner_like_takes_back_the_same_row() {
    let db = common::test_db().await;
    let pool = db.pool().clone();
    let state = delivering_state(&pool);
    let bob = common::create_remote_user(&pool, "bob", "remote.example").await;
    let (alice, posted) = alice_post(&pool, "nested undo").await;

    let like = interaction("Like", BOB, &target_of(&posted), 1, Value::Null);
    deliver(&state, &like, &bob).await;
    deliver(&state, &undo(BOB, &like, 2), &bob).await;

    assert!(
        favourite_rows(&pool).await.is_empty(),
        "the embedded inner is unwrapped and routed by its type"
    );
    assert_eq!(
        viewer(&pool, &alice).await.status(posted.id).await["favourites_count"],
        json!(0)
    );
}

#[tokio::test]
async fn an_undo_signed_by_another_actor_withdraws_nothing() {
    let db = common::test_db().await;
    let pool = db.pool().clone();
    let state = delivering_state(&pool);
    let bob = common::create_remote_user(&pool, "bob", "remote.example").await;
    let mallory = common::create_remote_user(&pool, "mallory", "remote.example").await;
    let (_alice, posted) = alice_post(&pool, "not yours to undo").await;

    deliver(
        &state,
        &interaction("Like", BOB, &target_of(&posted), 1, Value::Null),
        &bob,
    )
    .await;

    // Mallory undoes bob's like by its id: the row is keyed per actor, so
    // bob's favourite stands.
    let like_id = interaction_id("Like", BOB, 1);
    deliver(&state, &undo(MALLORY, &json!(like_id.clone()), 2), &mallory).await;
    // ...and wrapped in an inner naming bob: refused at the dispatcher's
    // own gate before the handler ever runs.
    deliver(
        &state,
        &undo(
            MALLORY,
            &interaction("Like", BOB, &target_of(&posted), 3, Value::Null),
            4,
        ),
        &mallory,
    )
    .await;
    assert_eq!(
        favourite_rows(&pool).await,
        vec![(BOB.to_owned(), posted.uri.clone(), like_id)],
        "bob's like survives both attempts"
    );
}

// --- EmojiReact ------------------------------------------------------------------

#[tokio::test]
async fn a_delivered_unicode_reaction_lands_with_its_telling_and_roll_up() {
    let db = common::test_db().await;
    let pool = db.pool().clone();
    let state = delivering_state(&pool);
    let bob = common::create_remote_user(&pool, "bob", "remote.example").await;
    let (alice, posted) = alice_post(&pool, "himalayan cat").await;

    deliver(
        &state,
        &interaction(
            "EmojiReact",
            BOB,
            &target_of(&posted),
            1,
            json!({"content": "😺"}),
        ),
        &bob,
    )
    .await;

    assert_eq!(
        reaction_rows(&pool).await,
        vec![(
            BOB.to_owned(),
            posted.uri.clone(),
            "😺".to_owned(),
            None,
            interaction_id("EmojiReact", BOB, 1),
        )],
        "one bare-emoji row, no url, keyed to the activity"
    );
    let seen = viewer(&pool, &alice).await;
    let status = seen.status(posted.id).await;
    assert_eq!(status["pleroma"]["emoji_reactions"][0]["count"], json!(1));
    assert_eq!(status["pleroma"]["emoji_reactions"][0]["name"], json!("😺"));
    let told = seen.told_of(TYPE_EMOJI_REACTION).await;
    assert_eq!(told.len(), 1, "the author is told once");
    assert_eq!(told[0]["account"]["id"], json!(bob.id.to_string()));
    assert_eq!(told[0]["emoji"], json!("😺"), "and told with what");
}

#[tokio::test]
async fn a_shortcode_reaction_needs_its_emoji_tag_and_bad_content_stores_nothing() {
    let db = common::test_db().await;
    let pool = db.pool().clone();
    let state = delivering_state(&pool);
    let bob = common::create_remote_user(&pool, "bob", "remote.example").await;
    let (alice, posted) = alice_post(&pool, "custom emoji please").await;

    // Refused content: a bare shortcode, two emojis, a shortcode whose tag
    // names another one, and an icon url no http(s) fetch could serve.
    for (n, extra) in [
        (1, json!({"content": ":blobcat:"})),
        (2, json!({"content": "😺😹"})),
        (
            3,
            json!({"content": ":blobcat:", "tag": [{"type": "Emoji", "name": ":other:", "icon": {"url": "https://remote.example/e.png"}}]}),
        ),
        (
            4,
            json!({"content": ":blobcat:", "tag": [{"type": "Emoji", "name": ":blobcat:", "icon": {"url": "ftp://remote.example/e.png"}}]}),
        ),
    ] {
        deliver(
            &state,
            &interaction("EmojiReact", BOB, &target_of(&posted), n, extra),
            &bob,
        )
        .await;
    }
    assert!(
        reaction_rows(&pool).await.is_empty(),
        "content the local rules refuse stores nothing"
    );

    let tagged = json!({
        "content": ":blobcat:",
        "tag": [{"type": "Emoji", "name": ":blobcat:", "icon": {"type": "Image", "mediaType": "image/png", "url": "https://remote.example/emoji/blobcat.png"}}],
    });
    deliver(
        &state,
        &interaction("EmojiReact", BOB, &target_of(&posted), 5, tagged),
        &bob,
    )
    .await;
    assert_eq!(
        reaction_rows(&pool).await,
        vec![(
            BOB.to_owned(),
            posted.uri.clone(),
            ":blobcat:".to_owned(),
            Some("https://remote.example/emoji/blobcat.png".to_owned()),
            interaction_id("EmojiReact", BOB, 5),
        )],
        "the activity's own emoji url is stored with the shortcode"
    );
    let status = viewer(&pool, &alice).await.status(posted.id).await;
    assert_eq!(
        status["pleroma"]["emoji_reactions"][0]["url"],
        json!("https://remote.example/emoji/blobcat.png"),
        "and the status json hands it to clients"
    );
}

#[tokio::test]
async fn an_undo_takes_back_one_emojis_reaction_and_its_telling() {
    let db = common::test_db().await;
    let pool = db.pool().clone();
    let state = delivering_state(&pool);
    let bob = common::create_remote_user(&pool, "bob", "remote.example").await;
    let mallory = common::create_remote_user(&pool, "mallory", "remote.example").await;
    let (alice, posted) = alice_post(&pool, "react away").await;

    // Mallory parties and cats, bob parties: the undo of mallory's party
    // leaves the other two standing.
    for (actor, n, emoji) in [(BOB, 1, "🎉"), (MALLORY, 2, "🎉"), (MALLORY, 3, "😺")] {
        let who = if n == 1 { &bob } else { &mallory };
        deliver(
            &state,
            &interaction(
                "EmojiReact",
                actor,
                &target_of(&posted),
                n,
                json!({"content": emoji}),
            ),
            who,
        )
        .await;
    }
    assert_eq!(reaction_rows(&pool).await.len(), 3);

    let party_id = interaction_id("EmojiReact", MALLORY, 2);
    deliver(&state, &undo(MALLORY, &json!(party_id), 4), &mallory).await;

    let rows = reaction_rows(&pool).await;
    assert_eq!(rows.len(), 2);
    assert_eq!((rows[0].0.as_str(), rows[0].2.as_str()), (BOB, "🎉"));
    assert_eq!((rows[1].0.as_str(), rows[1].2.as_str()), (MALLORY, "😺"));
    let seen = viewer(&pool, &alice).await;
    let told = seen.told_of(TYPE_EMOJI_REACTION).await;
    assert_eq!(
        told.iter()
            .filter(|t| t["account"]["id"] == json!(mallory.id.to_string())
                && t["emoji"] == json!("🎉"))
            .count(),
        0,
        "mallory's withdrawn party is untold: {told:?}"
    );
    assert_eq!(told.len(), 2, "bob's party and mallory's cat still tell");
    let status = seen.status(posted.id).await;
    assert_eq!(
        status["pleroma"]["emoji_reactions"],
        json!([
            {"name": "🎉", "count": 1, "me": false, "url": null, "account_ids": [bob.id.to_string()]},
            {"name": "😺", "count": 1, "me": false, "url": null, "account_ids": [mallory.id.to_string()]},
        ]),
        "one party (bob's), one cat (mallory's), in first-reaction order"
    );
}

// --- Announce ---------------------------------------------------------------------

#[tokio::test]
async fn a_delivered_announce_boosts_the_status_under_its_activity_id() {
    let db = common::test_db().await;
    let pool = db.pool().clone();
    let state = delivering_state(&pool);
    let mallory = common::create_remote_user(&pool, "mallory", "remote.example").await;
    let (alice, posted) = alice_post(&pool, "boost me").await;

    deliver(
        &state,
        &interaction(
            "Announce",
            MALLORY,
            &target_of(&posted),
            1,
            json!({"to": [PUBLIC], "published": "2026-11-01T00:00:00Z"}),
        ),
        &mallory,
    )
    .await;

    assert_eq!(
        boost_rows(&pool).await,
        vec![(
            interaction_id("Announce", MALLORY, 1),
            "public".to_owned(),
            "2026-11-01".to_owned(),
            posted.uri.clone(),
            MALLORY.to_owned(),
        )],
        "a reblog row IS the activity: its uri, its date, its public addressing"
    );
    let seen = viewer(&pool, &alice).await;
    assert_eq!(seen.status(posted.id).await["reblogs_count"], json!(1));
    let told = seen.told_of(TYPE_REBLOG).await;
    assert_eq!(told.len(), 1, "the author is told once");
    assert_eq!(told[0]["account"]["id"], json!(mallory.id.to_string()));
    assert_eq!(told[0]["status"]["id"], json!(posted.id.to_string()));
}

#[tokio::test]
async fn a_boost_of_a_boost_and_a_cc_addressed_boost_fold_onto_the_original() {
    let db = common::test_db().await;
    let pool = db.pool().clone();
    let state = delivering_state(&pool);
    let bob = common::create_remote_user(&pool, "bob", "remote.example").await;
    let mallory = common::create_remote_user(&pool, "mallory", "remote.example").await;
    let (_alice, posted) = alice_post(&pool, "boost chain").await;

    // mallory boosts, addressed so the public address sits in `cc`: the
    // row parses the same way a note would — unlisted.
    let first = interaction(
        "Announce",
        MALLORY,
        &target_of(&posted),
        1,
        json!({"to": [format!("{}followers", MALLORY)], "cc": [PUBLIC]}),
    );
    deliver(&state, &first, &mallory).await;
    // bob boosts the boost: the pair is folded to the original, and bob's
    // public addressing gives the public row.
    deliver(
        &state,
        &interaction(
            "Announce",
            BOB,
            &json!(first["id"]),
            2,
            json!({"to": [PUBLIC]}),
        ),
        &bob,
    )
    .await;

    let rows = boost_rows(&pool).await;
    assert_eq!(rows.len(), 2);
    assert_eq!(
        (rows[0].0.as_str(), rows[0].1.as_str()),
        (first["id"].as_str().expect("an id"), "unlisted"),
        "cc'd boost lands unlisted"
    );
    assert_eq!(rows[1].1, "public");
    assert_eq!(rows[0].3, posted.uri, "both name the original");
    assert_eq!(rows[1].3, posted.uri);
}

#[tokio::test]
async fn a_restated_boost_folds_nothing_and_a_withdrawn_ids_restatement_folds_nothing() {
    let db = common::test_db().await;
    let pool = db.pool().clone();
    let state = delivering_state(&pool);
    let mallory = common::create_remote_user(&pool, "mallory", "remote.example").await;
    let (alice, posted) = alice_post(&pool, "boosted once").await;
    let announce = |n| {
        interaction(
            "Announce",
            MALLORY,
            &target_of(&posted),
            n,
            json!({"to": [PUBLIC]}),
        )
    };

    deliver(&state, &announce(1), &mallory).await;
    // The same id redelivered, and a fresh id for the same pair: both
    // restate a standing fact.
    deliver(&state, &announce(1), &mallory).await;
    deliver(&state, &announce(2), &mallory).await;
    assert_eq!(boost_rows(&pool).await.len(), 1);
    assert_eq!(
        viewer(&pool, &alice).await.status(posted.id).await["reblogs_count"],
        json!(1)
    );

    // The undo tombstones the row; its uri stays reserved, so the id that
    // announced once never announces again — only a fresh one does.
    deliver(
        &state,
        &undo(MALLORY, &json!(interaction_id("Announce", MALLORY, 1)), 3),
        &mallory,
    )
    .await;
    assert!(
        boost_rows(&pool).await.is_empty(),
        "the live boost is gone with the undo"
    );
    let seen = viewer(&pool, &alice).await;
    assert_eq!(seen.status(posted.id).await["reblogs_count"], json!(0));
    assert!(
        seen.told_of(TYPE_REBLOG).await.is_empty(),
        "the telling is gone with the boost"
    );

    deliver(&state, &announce(1), &mallory).await;
    assert_eq!(
        boost_rows(&pool).await.len(),
        0,
        "the reserved id restates nothing"
    );
    deliver(&state, &announce(4), &mallory).await;
    assert_eq!(boost_rows(&pool).await.len(), 1, "a fresh id boosts again");
}

#[tokio::test]
async fn a_boost_of_a_hidden_status_folds_nothing() {
    let db = common::test_db().await;
    let pool = db.pool().clone();
    let state = delivering_state(&pool);
    let mallory = common::create_remote_user(&pool, "mallory", "remote.example").await;
    let alice = common::create_local_user(&pool, "alice").await;
    let followers_only = common::create_status(&pool, &alice, "private", Visibility::Private).await;
    let just_bob = status::insert(
        &pool,
        TEST_BASE_URL,
        &NewStatus {
            visibility: Visibility::Direct,
            ..NewStatus::local(&alice, "direct")
        },
    )
    .await
    .expect("the direct fixture inserts");

    for (n, post) in [(1, &followers_only), (2, &just_bob)] {
        deliver(
            &state,
            &interaction(
                "Announce",
                MALLORY,
                &target_of(post),
                n,
                json!({"to": [PUBLIC]}),
            ),
            &mallory,
        )
        .await;
    }
    assert!(
        boost_rows(&pool).await.is_empty(),
        "private and direct post nothing to the boost fold"
    );
}

// --- the target's visibility to its actor ----------------------------------------

#[tokio::test]
async fn a_private_post_folds_a_like_only_for_an_accepted_follower() {
    let db = common::test_db().await;
    let pool = db.pool().clone();
    let state = delivering_state(&pool);
    let bob = common::create_remote_user(&pool, "bob", "remote.example").await;
    let mallory = common::create_remote_user(&pool, "mallory", "remote.example").await;
    let alice = common::create_local_user(&pool, "alice").await;
    let secret = common::create_status(&pool, &alice, "followers only", Visibility::Private).await;

    deliver(
        &state,
        &interaction("Like", MALLORY, &target_of(&secret), 1, Value::Null),
        &mallory,
    )
    .await;
    assert!(
        favourite_rows(&pool).await.is_empty(),
        "a stranger cannot like what it cannot see"
    );

    // The shared model write accepts bob, and then bob's like folds.
    let _accepted = follow::follow(&pool, TEST_BASE_URL, &bob, &alice)
        .await
        .expect("bob's follow of open alice is granted");
    deliver(
        &state,
        &interaction("Like", BOB, &target_of(&secret), 2, Value::Null),
        &bob,
    )
    .await;
    assert_eq!(
        favourite_rows(&pool).await,
        vec![(
            BOB.to_owned(),
            secret.uri.clone(),
            interaction_id("Like", BOB, 2)
        )],
        "the accepted follower's like folds onto the private status"
    );
}

#[tokio::test]
async fn a_direct_post_folds_for_a_mentioned_actor_only() {
    let db = common::test_db().await;
    let pool = db.pool().clone();
    let state = delivering_state(&pool);
    let bob = common::create_remote_user(&pool, "bob", "remote.example").await;
    let mallory = common::create_remote_user(&pool, "mallory", "remote.example").await;
    let alice = common::create_local_user(&pool, "alice").await;
    let note = status::insert(
        &pool,
        TEST_BASE_URL,
        &NewStatus {
            visibility: Visibility::Direct,
            mentioned_user_ids: vec![bob.id],
            ..NewStatus::local(&alice, "psst, bob")
        },
    )
    .await
    .expect("the direct fixture inserts");

    deliver(
        &state,
        &interaction(
            "EmojiReact",
            MALLORY,
            &target_of(&note),
            1,
            json!({"content": "👀"}),
        ),
        &mallory,
    )
    .await;
    assert!(
        reaction_rows(&pool).await.is_empty(),
        "a stranger's peek stores nothing"
    );
    deliver(
        &state,
        &interaction(
            "EmojiReact",
            BOB,
            &target_of(&note),
            2,
            json!({"content": "👀"}),
        ),
        &bob,
    )
    .await;
    assert_eq!(
        reaction_rows(&pool).await.len(),
        1,
        "the mentioned actor reacts"
    );
}

// --- the fetch path and the lookalikes ---------------------------------------------

#[tokio::test]
async fn an_unknown_remote_object_is_fetched_and_then_folded() {
    let db = common::test_db().await;
    let pool = db.pool().clone();
    let state = delivering_state(&pool);
    let mallory = common::create_remote_user(&pool, "mallory", "remote.example").await;
    let (base, _peer) = spawn_peer().await;
    let remote_note = json!(format!("{base}/objects/n1"));

    // The Like's object is a name this instance has never heard of: it is
    // fetched, ingested, and folded in one delivery.
    deliver(
        &state,
        &interaction("Like", MALLORY, &remote_note, 1, Value::Null),
        &mallory,
    )
    .await;
    let ingested = sqlx::query_scalar::<_, String>("SELECT uri FROM statuses WHERE uri = $1")
        .bind(remote_note.as_str().expect("an id"))
        .fetch_optional(&pool)
        .await
        .expect("the statuses read");
    assert_eq!(
        ingested.as_deref(),
        remote_note.as_str(),
        "the fetched note is stored"
    );
    let row = favourite_rows(&pool).await.pop().expect("one favourite");
    assert_eq!(
        (row.0.as_str(), row.1.as_str()),
        (MALLORY, remote_note.as_str().expect("an id"))
    );

    // The second fold resolves from the store now: the announce names the
    // same ingested row, and its undo takes it back.
    deliver(
        &state,
        &interaction(
            "Announce",
            MALLORY,
            &remote_note,
            2,
            json!({"to": [PUBLIC]}),
        ),
        &mallory,
    )
    .await;
    let boost = boost_rows(&pool).await.pop().expect("the boost");
    assert_eq!(boost.3, remote_note.as_str().expect("an id"));

    // A gone object is nothing to retry and nothing to fold.
    deliver(
        &state,
        &interaction(
            "Like",
            MALLORY,
            &json!(format!("{base}/missing")),
            3,
            Value::Null,
        ),
        &mallory,
    )
    .await;
    assert_eq!(
        favourite_rows(&pool).await.len(),
        1,
        "the 404 folded nothing"
    );
}

#[tokio::test]
async fn a_like_carrying_an_emoji_is_a_reaction_and_resemblances_fold_nothing() {
    let db = common::test_db().await;
    let pool = db.pool().clone();
    let state = delivering_state(&pool);
    let bob = common::create_remote_user(&pool, "bob", "remote.example").await;
    let (alice, posted) = alice_post(&pool, "misskey favourite").await;

    // The favourite-with-emoji shape: a `Like` whose content names one
    // emoji is a reaction, not a favourite.
    let misskey = interaction(
        "Like",
        BOB,
        &target_of(&posted),
        1,
        json!({"content": "❤️", "tag": []}),
    );
    deliver(&state, &misskey, &bob).await;
    assert!(
        favourite_rows(&pool).await.is_empty(),
        "a favourite-with-emoji is no favourite"
    );
    assert_eq!(
        reaction_rows(&pool).await,
        vec![(
            BOB.to_owned(),
            posted.uri.clone(),
            "❤️".to_owned(),
            None,
            interaction_id("Like", BOB, 1),
        )],
        "the reaction holds the Like's own activity id"
    );

    // A `Like` whose text names no emoji is refused outright — neither a
    // favourite nor a reaction — and `Dislike` never reaches a handler.
    for (kind, actor, n, extra) in [
        ("Like", BOB, 2, json!({"content": "<p>great post!</p>"})),
        ("Dislike", BOB, 3, Value::Null),
        ("Like", BOB, 4, json!({"content": "   "})),
    ] {
        let act = json!({
            "id": interaction_id(kind, actor, n),
            "type": kind,
            "actor": actor,
            "object": target_of(&posted),
        });
        deliver(&state, &merge(act, extra), &bob).await;
    }
    assert_eq!(
        reaction_rows(&pool).await.len(),
        1,
        "text that names nothing usable folds no reaction"
    );
    let rows = favourite_rows(&pool).await;
    assert_eq!(
        rows.len(),
        1,
        "the blank content liked plainly, nothing else did"
    );
    assert_eq!(rows[0].2, interaction_id("Like", BOB, 4));
    assert_eq!(
        viewer(&pool, &alice).await.status(posted.id).await["favourites_count"],
        json!(1)
    );
}

// --- the mock peer ------------------------------------------------------------------

/// The mock peer of the fetch test: alice's note at `/objects/n1`, its
/// author at `/users/nobody`, and a 404 at `/missing`. Serves on a loopback
/// port; the stock test state's client is allowed to reach it.
async fn spawn_peer() -> (String, tokio::task::JoinHandle<()>) {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
        .await
        .expect("the loopback peer binds");
    let base = format!("http://{}", listener.local_addr().expect("an addr"));
    let app = Router::new()
        .route("/objects/{name}", get(serve_object))
        .route("/users/{name}", get(serve_actor))
        .route("/missing", get(|| async { StatusCode::NOT_FOUND }))
        .with_state(base.clone());
    let server = tokio::spawn(async move {
        axum::serve(listener, app).await.expect("the peer serves");
    });
    (base, server)
}

/// A self-naming remote actor document: enough for [`fetch_actor`]-style
/// ingest (the fixture's public key is inert — ingest stores, it does not
/// verify).
///
/// [`fetch_actor`]: wired_be::federation::fetch_actor
async fn serve_actor(State(base): State<String>, Path(name): Path<String>) -> Json<Value> {
    let id = format!("{base}/users/{name}");
    Json(json!({
        "type": "Person",
        "id": id,
        "preferredUsername": name,
        "inbox": format!("{id}/inbox"),
        "followers": format!("{id}/followers"),
        "publicKey": {"id": format!("{id}#main-key"), "owner": id, "publicKeyPem": "PEM"},
    }))
}

/// The note the interaction names: public, authored by `nobody` at this
/// same base.
async fn serve_object(State(base): State<String>, Path(name): Path<String>) -> Json<Value> {
    Json(json!({
        "id": format!("{base}/objects/{name}"),
        "type": "Note",
        "attributedTo": format!("{base}/users/nobody"),
        "to": [PUBLIC],
        "content": "<p>a note from the peer</p>",
        "published": "2026-11-02T00:00:00Z",
    }))
}
