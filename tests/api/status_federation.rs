//! The status lifecycle's outbound federation over HTTP (WBE-176): the
//! activity each of the three writes hands the publisher's queue.
//!
//! `POST /api/v1/statuses` queues the `Create` of the post's note,
//! `PUT /api/v1/statuses/{id}` the `Update` of the re-drafted one and
//! `DELETE /api/v1/statuses/{id}` the `Delete` whose object is the post's
//! `Tombstone` — the documents [`wired_be::services::federation_out`] builds
//! at the create, edit and delete hooks, one `federator.publish` job per
//! write. The endpoints' own behaviour (their gates, bodies, refusals and
//! entity shapes) is `statuses.rs` and `status_edits.rs`; this file is only
//! about what leaves the instance because of them.
//!
//! The queued document is the same pair the object and activity routes
//! serve: `id` `{BASE_URL}/activities/{status id}` for a `Create` (decision
//! 6), a fresh one for the `Update` that re-drafts it, and
//! `{BASE_URL}/activities/delete-{status id}` for the `Delete` that takes it
//! down. The author stands as `actor` and signs the delivery, the note's own
//! `to` and `cc` name the audience, and it is the publisher's follower
//! expansion — run here by the queued job's own handler, as a worker would —
//! that turns those addresses into the `federator.deliver` jobs this file
//! counts. A `local`-visibility post writes its row and queues nothing at
//! all, and an audience of local followers alone delivers nothing: a local
//! timeline is that delivery.
//!
//! [`wired_be::services::federation_out`]: wired_be::services::federation_out

#![cfg(test)]

use axum::{Router, http::StatusCode};
use serde_json::{Value, json};
use sqlx::PgPool;
use uuid::Uuid;
use wired_be::{
    config::TEST_BASE_URL,
    federation::{delivery, publisher},
    jobs::AttemptInfo,
    models::{
        follow::{self, FollowState},
        status::{self, Visibility},
        user::User,
    },
};

use crate::common;

/// The scope all three writes ask.
const WRITE: &str = "write:statuses";
/// The `ActivityStreams` public address a public note carries in `to`.
const PUBLIC: &str = "https://www.w3.org/ns/activitystreams#Public";
/// The instance origin the fixtures post under.
const BASE: &str = TEST_BASE_URL;
/// The first attempt a background handler runs on.
const FIRST_ATTEMPT: AttemptInfo = AttemptInfo {
    attempts: 1,
    max_attempts: 5,
};

/// A local account with the endpoints and keys a registered one carries: the
/// fan-out expands the followers collection stored here, and a signed
/// delivery needs a private key to speak with.
async fn author(pool: &PgPool, nickname: &str) -> User {
    common::create_federating_user_with(pool, nickname, |new| new).await
}

/// Records `follower`'s follow of `target` and asserts it landed accepted —
/// the relationship whose expansion a note's `cc` line names.
async fn followed(pool: &PgPool, follower: &User, target: &User) {
    let row = follow::follow(pool, TEST_BASE_URL, follower, target)
        .await
        .expect("the fixture follow records");
    assert_eq!(
        row.state,
        FollowState::Accepted,
        "an open local account accepts on the spot"
    );
}

/// The queued `federator.publish` activities, in insertion order.
async fn published(pool: &PgPool) -> Vec<Value> {
    sqlx::query_as::<_, (Value,)>(
        "SELECT args->'activity' FROM jobs WHERE kind = $1 ORDER BY inserted_at, id",
    )
    .bind(publisher::KIND_PUBLISH)
    .fetch_all(pool)
    .await
    .expect("the publish jobs read")
    .into_iter()
    .map(|(activity,)| activity)
    .collect()
}

/// The activity of the one `federator.publish` job the queue holds, asserted
/// to be alone — the shape a single-write test compares against.
async fn the_published_activity(pool: &PgPool) -> Value {
    let jobs = published(pool).await;
    assert_eq!(jobs.len(), 1, "the write is announced once");
    jobs[0].clone()
}

/// The activity of the most recently queued `federator.publish` job: the
/// write under test, once a fixture has queued an earlier announcement
/// beside it.
async fn the_latest_activity(pool: &PgPool) -> Value {
    let jobs = published(pool).await;
    let last = jobs.last().expect("a publish job is queued");
    last.clone()
}

/// The inboxes the queued `federator.deliver` jobs name, sorted so a
/// delivery set compares directly.
async fn delivered(pool: &PgPool) -> Vec<String> {
    sqlx::query_as::<_, (String,)>("SELECT args->>'inbox' FROM jobs WHERE kind = $1 ORDER BY 1")
        .bind(delivery::KIND_DELIVER)
        .fetch_all(pool)
        .await
        .expect("the delivery jobs read")
        .into_iter()
        .map(|(inbox,)| inbox)
        .collect()
}

/// The inboxes `activity_id` was queued for, sorted: one announcement's
/// deliveries, apart from those of every other activity in the queue.
async fn delivered_for(pool: &PgPool, activity_id: &str) -> Vec<String> {
    sqlx::query_as::<_, (String,)>(
        "SELECT args->>'inbox' FROM jobs WHERE kind = $1 AND args->>'activity_id' = $2 ORDER BY 1",
    )
    .bind(delivery::KIND_DELIVER)
    .bind(activity_id)
    .fetch_all(pool)
    .await
    .expect("the delivery jobs read")
    .into_iter()
    .map(|(inbox,)| inbox)
    .collect()
}

/// Runs every queued publish through the publisher's own handler, so the
/// follower expansion turns into the delivery jobs a worker would send.
async fn fan_out(pool: &PgPool) {
    let state = common::test_app_state(pool.clone(), common::test_config());
    let rows = sqlx::query_as::<_, (Value,)>(
        "SELECT args FROM jobs WHERE kind = $1 ORDER BY inserted_at, id",
    )
    .bind(publisher::KIND_PUBLISH)
    .fetch_all(pool)
    .await
    .expect("the publish jobs read");
    for (args,) in rows {
        publisher::handle(state.clone(), FIRST_ATTEMPT, args)
            .await
            .expect("the fan-out job completes");
    }
}

/// `POST /api/v1/statuses` with `body`, answered as the created entity.
async fn post(app: &Router, token: &str, body: Value) -> Value {
    let response = common::TestRequest::post("/api/v1/statuses")
        .bearer(token)
        .json(&body)
        .send(app)
        .await;
    common::assert_answer(&response, StatusCode::OK, "the create accepts the body");
    response.json_value()
}

/// The stored row behind an entity id, tombstone included.
async fn stored_row(pool: &PgPool, id: &Value) -> status::Status {
    let parsed = Uuid::parse_str(&entity_id(id)).expect("the entity id is id text");
    status::get_by_id_including_deleted(pool, parsed)
        .await
        .expect("the row reads")
        .expect("the created post is stored")
}

/// The id text of an entity `id` field, as the routes spell it out.
fn entity_id(id: &Value) -> String {
    id.as_str().expect("the entity id is a string").to_owned()
}

/// A public post by a local account with followers on two remote hosts
/// queues the `Create` of its note — the author as actor, the world in `to`
/// and the followers collection in `cc` — and the fan-out delivers it to
/// each host's shared inbox, once.
#[tokio::test]
async fn a_public_post_publishes_its_create_to_every_remote_host() {
    let db = common::test_db().await;
    let pool = db.pool().clone();
    let app = common::test_app(pool.clone());
    let alice = author(&pool, "alice").await;
    let token = common::create_app_and_token(&pool, &alice, WRITE).await;
    let carol = common::create_remote_user(&pool, "carol", "peer.example").await;
    let dave = common::create_remote_user(&pool, "dave", "other.example").await;
    followed(&pool, &carol, &alice).await;
    followed(&pool, &dave, &alice).await;

    let entity = post(&app, &token, json!({"status": "hello from the api"})).await;
    let row = stored_row(&pool, &entity["id"]).await;

    let activity = the_published_activity(&pool).await;
    assert_eq!(activity["type"], "Create");
    assert_eq!(
        activity["id"],
        format!("{BASE}/activities/{}", row.id),
        "the create is addressed by the status' own id (decision 6)"
    );
    assert_eq!(activity["actor"], alice.ap_id, "the author publishes");
    assert_eq!(activity["to"], json!([PUBLIC]));
    assert_eq!(
        activity["cc"],
        json!([format!("{}/followers", alice.ap_id)]),
        "the followers collection is the audience the fan-out expands"
    );
    assert_eq!(activity["object"]["id"], row.uri, "the note is the post");
    assert_eq!(activity["object"]["type"], "Note");
    assert_eq!(
        activity["object"]["content"], "<p>hello from the api</p>",
        "rendered the way the create rendered it"
    );

    fan_out(&pool).await;
    assert_eq!(
        delivered(&pool).await,
        vec!["https://other.example/inbox", "https://peer.example/inbox"],
        "one POST per remote host, at its shared inbox"
    );
}

/// An edit of that post queues the `Update` of the re-drafted note: its own
/// fresh activity id, the reserved object uri and new text inside, the
/// `updated` stamp the row now carries, and the same audience — so the hosts
/// that hold a copy are the hosts that hear about the correction.
#[tokio::test]
async fn an_edit_publishes_the_update_of_the_new_note() {
    let db = common::test_db().await;
    let pool = db.pool().clone();
    let app = common::test_app(pool.clone());
    let alice = author(&pool, "alice").await;
    let token = common::create_app_and_token(&pool, &alice, WRITE).await;
    let carol = common::create_remote_user(&pool, "carol", "peer.example").await;
    followed(&pool, &carol, &alice).await;

    let entity = post(&app, &token, json!({"status": "first draft"})).await;
    let row = stored_row(&pool, &entity["id"]).await;
    let create_id = entity_id(&entity["id"]);

    let response = common::TestRequest::put(&format!("/api/v1/statuses/{create_id}"))
        .bearer(&token)
        .json(&json!({"status": "second draft"}))
        .send(&app)
        .await;
    common::assert_answer(&response, StatusCode::OK, "the edit accepts the body");

    let activities = published(&pool).await;
    assert_eq!(
        activities.len(),
        2,
        "the post is announced, and then re-drafted"
    );
    assert_eq!(activities[0]["type"], "Create");
    let update = &activities[1];
    assert_eq!(update["type"], "Update");
    assert_eq!(update["actor"], alice.ap_id);
    let id = update["id"].as_str().expect("the update carries an id");
    assert!(
        id.starts_with(&format!("{BASE}/activities/")),
        "a fresh activity id of this instance's own: {id}"
    );
    assert_ne!(
        id,
        format!("{BASE}/activities/{create_id}"),
        "and not the create's: an edit is its own activity"
    );
    assert_eq!(
        update["object"]["id"], row.uri,
        "the object is the same reserved uri"
    );
    assert_eq!(
        update["object"]["content"], "<p>second draft</p>",
        "rewritten, because the document is read back after the write"
    );
    assert!(
        update["object"]["updated"].is_string(),
        "and stamped with the moment it changed"
    );
    assert_eq!(
        update["to"],
        json!([PUBLIC]),
        "under the addressing the post has"
    );
    assert_eq!(
        update["cc"],
        json!([format!("{}/followers", alice.ap_id)]),
        "both of its lines"
    );

    fan_out(&pool).await;
    assert_eq!(
        delivered_for(&pool, id).await,
        vec!["https://peer.example/inbox"],
        "the correction reaches the one host that holds a copy"
    );
}

/// Deleting the post queues the `Delete` whose object is its `Tombstone`,
/// under the deterministic id a retry names again, and the withdrawal
/// travels to the followers that received the post.
#[tokio::test]
async fn a_delete_publishes_the_tombstone_of_the_post() {
    let db = common::test_db().await;
    let pool = db.pool().clone();
    let app = common::test_app(pool.clone());
    let alice = author(&pool, "alice").await;
    let token = common::create_app_and_token(&pool, &alice, WRITE).await;
    let carol = common::create_remote_user(&pool, "carol", "peer.example").await;
    followed(&pool, &carol, &alice).await;

    let entity = post(&app, &token, json!({"status": "here today"})).await;
    let row = stored_row(&pool, &entity["id"]).await;
    let id = entity_id(&entity["id"]);

    let response = common::TestRequest::delete(&format!("/api/v1/statuses/{id}"))
        .bearer(&token)
        .send(&app)
        .await;
    common::assert_answer(&response, StatusCode::OK, "the delete answers");

    let activity = the_latest_activity(&pool).await;
    assert_eq!(activity["type"], "Delete");
    let delete_id = entity_id(&activity["id"]);
    assert_eq!(
        activity["id"],
        format!("{BASE}/activities/delete-{id}"),
        "the deterministic withdrawal id the ticket pins"
    );
    assert_eq!(activity["actor"], alice.ap_id);
    assert_eq!(activity["to"], json!([PUBLIC]));
    assert_eq!(
        activity["cc"],
        json!([format!("{}/followers", alice.ap_id)]),
        "the addressing the post was announced under, read from the row it held"
    );
    assert_eq!(
        activity["object"]["id"], row.uri,
        "the object is the reserved uri of the post that went"
    );
    assert_eq!(activity["object"]["type"], "Tombstone");
    assert_eq!(activity["object"]["formerType"], "Note");
    assert!(
        activity["object"]["deleted"].is_string(),
        "and the tombstone states the deletion instant"
    );

    fan_out(&pool).await;
    assert_eq!(
        delivered_for(&pool, &delete_id).await,
        vec!["https://peer.example/inbox"],
        "the host holding the copy is the host told it is gone"
    );
}

/// A `local`-visibility post is written, answered and never announced: the
/// create hook refuses it, so the publish queue stays empty and no inbox is
/// ever named for it.
#[tokio::test]
async fn a_local_visibility_post_publishes_nothing() {
    let db = common::test_db().await;
    let pool = db.pool().clone();
    let app = common::test_app(pool.clone());
    let alice = author(&pool, "alice").await;
    let token = common::create_app_and_token(&pool, &alice, WRITE).await;
    let carol = common::create_remote_user(&pool, "carol", "peer.example").await;
    followed(&pool, &carol, &alice).await;

    let entity = post(
        &app,
        &token,
        json!({"status": "instance only", "visibility": "local"}),
    )
    .await;
    let row = stored_row(&pool, &entity["id"]).await;
    assert_eq!(
        row.visibility,
        Visibility::Local,
        "the row is local-visibility"
    );

    assert!(
        published(&pool).await.is_empty(),
        "a local-visibility post queues no activity"
    );

    let id = entity_id(&entity["id"]);
    let response = common::TestRequest::delete(&format!("/api/v1/statuses/{id}"))
        .bearer(&token)
        .send(&app)
        .await;
    common::assert_answer(&response, StatusCode::OK, "and its delete answers");
    assert!(
        published(&pool).await.is_empty(),
        "taking it down announces nothing either"
    );
}

/// A post whose followers are all local is announced and then delivered
/// nowhere: the publisher expands the followers collection, finds no foreign
/// inbox behind it, and queues no HTTP work at all.
#[tokio::test]
async fn a_local_audience_receives_no_delivery() {
    let db = common::test_db().await;
    let pool = db.pool().clone();
    let app = common::test_app(pool.clone());
    let alice = author(&pool, "alice").await;
    let token = common::create_app_and_token(&pool, &alice, WRITE).await;
    let neighbour = author(&pool, "neighbour").await;
    followed(&pool, &neighbour, &alice).await;

    let entity = post(&app, &token, json!({"status": "for our own eyes"})).await;
    assert_eq!(
        entity["content"], "<p>for our own eyes</p>",
        "the post itself was written as asked"
    );

    let activity = the_published_activity(&pool).await;
    assert_eq!(
        activity["type"], "Create",
        "the post is announced to its audience"
    );

    fan_out(&pool).await;
    assert!(
        delivered(&pool).await.is_empty(),
        "and the fan-out names no inbox: a local follower's timeline is the delivery"
    );
}
