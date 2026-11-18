//! End-to-end federation of outbound content (WBE-224 scenario 4): signed,
//! correctly addressed deliveries, dedupe and retries over real HTTP.
//!
//! Every scenario builds a real world through this instance's public API —
//! a local author with keys, a mock actor that followed her through the
//! inbox path — performs the local write over HTTP, drains the queues with
//! [`common::run_all_jobs`] and reads the facts off the [`remote::MockPeer`]'s
//! captured deliveries: the inbox path the bytes arrived at, the `Signature`
//! / `Digest` / `Host` / `Date` headers they arrived with, the verification
//! of that signature against the local actor's own published key, and the
//! activity JSON's `to` / `cc` / `actor` / `object` lines. Addressing is
//! asserted positively from the capture, never off job counts alone;
//! nothing waits on a clock — a retry scenario backdates the job row and
//! drains again rather than sleeping.
//!
//! What the scenarios pin:
//!
//! - the `Create`, `Update` and `Delete` of a status, the `Announce` of a
//!   boost and its `Undo`, the `Like` of a favourite and its `Undo`, and
//!   the `EmojiReact` of a reaction and its `Undo` — each a signed delivery
//!   with the visibility-specific `to`/`cc` split for public, unlisted,
//!   followers-only (`private`) and `direct` posts, and with `bto`/`bcc`
//!   never on the wire;
//! - a `local`-visibility post, edit and delete that deliver nothing at all,
//!   and a direct post that reaches only the mentioned remote's personal
//!   inbox — no shared-inbox copy, no follower fan-out;
//! - shared-inbox dedupe: two followers on one host cost one delivery per
//!   activity, keyed `deliver:{activity id}:{inbox}`, and a restated
//!   reaction while its delivery is still live never sends twice;
//! - the profile `Update` behind `PATCH /api/v1/accounts/update_credentials`
//!   with the freshly rendered actor document (display name, note, avatar,
//!   profile fields), and its per-account debounce key;
//! - the `Move` and moved-document `Update` behind
//!   `POST /api/v1/accounts/move`, the forwarded `Flag` behind
//!   `POST /api/v1/accounts/{id}/flag` spoken by the instance actor and
//!   filed into the admin queue, and the `Add` / `Remove` behind a
//!   list-membership write, `target` the list's curated inbox and the
//!   member's personal inbox the only recipient;
//! - a `500` retried on the queue's own backoff and a `503` at its
//!   `Retry-After` moment, both resending the exact same signed bytes; a
//!   `410 Gone` discarded for good, its host marked unreachable so later
//!   posts skip the address entirely; and a peer that answers `401` like a
//!   follower whose key no longer verifies — refused once, retried, then
//!   discarded, the valid signature it was sent with still in the capture.
//!
//! The per-endpoint entity rules belong to the area files; the queue-level
//! fan-out computations to `status_federation.rs` and the handshake round
//! trips to `e2e_federation_follow.rs`; this module owns the content bytes
//! that actually left the process.
//!
//! [`common::run_all_jobs`]: crate::common::run_all_jobs

#![cfg(test)]

use std::time::Duration;

use axum::{Router, body::Bytes, http::StatusCode};
use serde_json::{Value, json};
use sqlx::PgPool;
use tempfile::TempDir;
use wired_be::{
    config::TEST_BASE_URL,
    federation::delivery::KIND_DELIVER,
    models::{
        job::Job,
        user::{Flag, NewUser, User, get_by_ap_id},
    },
    services::instance_actor::ensure_instance_actor,
    state::AppState,
    util::password::hash_password,
};

use crate::{common, remote};

/// The mock's follower account, `bob@127.0.0.1:{port}`.
const BOB: &str = "bob";
/// The mock's second account, `dave@127.0.0.1:{port}`.
const DAVE: &str = "dave";
/// The `ActivityStreams` public address, as the note builder spells it.
const AS_PUBLIC: &str = "https://www.w3.org/ns/activitystreams#Public";
/// The scopes every local caller carries: `read write` covers each sub-scope
/// the posts, interactions, lists and reports in these scenarios ask for.
const SCOPES: &str = "read write";
/// The media type a delivery carries and asks for.
const ACTIVITY_JSON: &str = "application/activity+json";
/// The percent-encoded `🎉` of the reaction routes' path segment.
const PARTY: &str = "%F0%9F%8E%89";
/// A `PNG`: the magic bytes the profile-image detector names. A profile
/// image is never decoded, so the header is the whole fixture.
const PNG: &[u8] = b"\x89PNG\r\n\x1a\n\x00\x00\x00\rIHDR\x00\x00\x00\x01";
/// The status write.
const STATUSES: &str = "/api/v1/statuses";
/// The account routes the scenarios read and write.
const ACCOUNTS: &str = "/api/v1/accounts";
/// The legacy follow write keyed by handle or actor URL.
const FOLLOWS: &str = "/api/v1/follows";
/// The `backoff(1)` step the queue applies after a first failed attempt.
const FIRST_BACKOFF_SECS: i64 = 15;

// --- the fixtures ----------------------------------------------------------------------

/// A local account named `nickname` that can sign its own deliveries, with a
/// bearer token granting [`SCOPES`].
async fn local(pool: &PgPool, nickname: &str) -> (User, String) {
    let created = remote::keyed_local_user(pool, nickname).await;
    let token = common::create_app_and_token(pool, &created, SCOPES).await;
    (created, token)
}

/// A key-carrying local account that can also authorise the move write: the
/// same fixture with a password hash on the row.
async fn keyed_with_password(pool: &PgPool, nickname: &str, password: &str) -> User {
    let hash = hash_password(password).expect("the fixture password hashes");
    remote::keyed_local_user_with(pool, nickname, |user| NewUser {
        password_hash: Some(hash),
        ..user
    })
    .await
}

/// This instance's own actor: the signer of a forwarded `Flag`.
async fn instance_actor(state: &AppState) -> User {
    ensure_instance_actor(&state.db, &common::test_config())
        .await
        .expect("the instance actor stands")
}

/// The followers-collection address a delivered document names for `actor`.
fn followers_of(actor: &User) -> String {
    format!("{}/followers", actor.ap_id)
}

/// The mock's recorded path of the peer account `nick`'s personal inbox.
fn personal(peer: &remote::MockPeer, nick: &str) -> String {
    peer.inbox_path(&peer.actor(nick))
}

// --- driving the peer and the queues -----------------------------------------------------

/// Delivers the mock's signed `Follow` of `target` by the peer account
/// `nick` through this instance's shared inbox and drains the fold plus the
/// `Accept` answer it queues.
async fn peer_follows(
    app: &Router,
    state: &AppState,
    peer: &remote::MockPeer,
    nick: &str,
    target: &User,
) {
    let name = format!("{nick}-follows-{}", target.nickname);
    let follow = peer.follow_of(&name, &peer.actor(nick), &target.ap_id);
    let folded = peer
        .deliver(app, &peer.actor(nick), remote::INBOX, &follow)
        .await;
    assert_eq!(
        folded.status,
        StatusCode::ACCEPTED,
        "the inbox folds the mock's Follow"
    );
    let _drained = common::run_all_jobs(state).await;
}

/// The answer of `POST /api/v1/statuses` to `body`, asserted 200.
async fn post(app: &Router, token: &str, body: Value) -> Value {
    let response = common::TestRequest::post(STATUSES)
        .bearer(token)
        .json(&body)
        .send(app)
        .await;
    common::assert_answer(&response, StatusCode::OK, "the status write");
    response.json_value()
}

/// `POST /api/v1/statuses` with `text` at `visibility` (`None` for the
/// default), answered as the created entity.
async fn post_text(app: &Router, token: &str, text: &str, visibility: Option<&str>) -> Value {
    let mut body = json!({"status": text});
    if let Some(scope) = visibility {
        body["visibility"] = json!(scope);
    }
    post(app, token, body).await
}

/// The answer of `PUT /api/v1/statuses/{id}` editing to `status`, asserted 200.
async fn edit(app: &Router, token: &str, id: &str, status: &str) -> Value {
    let response = common::TestRequest::put(&format!("{STATUSES}/{id}"))
        .bearer(token)
        .json(&json!({"status": status}))
        .send(app)
        .await;
    common::assert_answer(&response, StatusCode::OK, "the edit write");
    response.json_value()
}

/// `DELETE /api/v1/statuses/{id}`, asserted 200.
async fn destroy(app: &Router, token: &str, id: &str) {
    let response = common::TestRequest::delete(&format!("{STATUSES}/{id}"))
        .bearer(token)
        .send(app)
        .await;
    common::assert_answer(&response, StatusCode::OK, "the delete write");
}

/// `POST /api/v1/statuses/{id}/{verb}` (`reblog`, `unreblog`, `favourite`,
/// `unfavourite`), optionally with a JSON `body`; asserted 200.
async fn status_action(
    app: &Router,
    token: &str,
    id: &str,
    verb: &str,
    body: Option<Value>,
) -> Value {
    let request = common::TestRequest::post(&format!("{STATUSES}/{id}/{verb}")).bearer(token);
    let request = match body {
        Some(found) => request.json(&found),
        None => request,
    };
    let response = request.send(app).await;
    common::assert_answer(&response, StatusCode::OK, &format!("the {verb} write"));
    response.json_value()
}

/// The action `verb` (`PUT` or `DELETE`) of Pleroma's reaction route for
/// `emoji` on status `id`, asserted 200.
async fn reaction_write(app: &Router, token: &str, id: &str, verb: &str, emoji: &str) {
    let path = format!("/api/v1/pleroma/statuses/{id}/reactions/{emoji}");
    let request = match verb {
        "DELETE" => common::TestRequest::delete(&path),
        _ => common::TestRequest::put(&path),
    };
    let response = request.bearer(token).send(app).await;
    common::assert_answer(&response, StatusCode::OK, &format!("the reaction {verb}"));
}

/// `POST /api/v1/follows` of `uri`, asserted 200 and answered with the
/// target's `Account` entity.
async fn follow_by_uri(app: &Router, token: &str, uri: &str) -> Value {
    let response = common::TestRequest::post(FOLLOWS)
        .bearer(token)
        .json(&json!({"uri": uri}))
        .send(app)
        .await;
    common::assert_answer(&response, StatusCode::OK, "the follow write");
    response.json_value()
}

/// `GET` of `path` as `token`, asserted 200 and parsed.
async fn get_json(app: &Router, token: &str, path: &str) -> Value {
    let response = common::TestRequest::get(path).bearer(token).send(app).await;
    common::assert_answer(&response, StatusCode::OK, &format!("the read of `{path}`"));
    response.json_value()
}

/// `GET /api/v1/accounts/lookup` for `acct`, asserted 200 and parsed.
async fn lookup(app: &Router, acct: &str) -> Value {
    let response = common::TestRequest::get(&format!("{ACCOUNTS}/lookup?acct={acct}"))
        .send(app)
        .await;
    common::assert_answer(
        &response,
        StatusCode::OK,
        &format!("the lookup of `{acct}`"),
    );
    response.json_value()
}

// --- reading the delivery queue -----------------------------------------------------------

/// The one `federator.deliver` job queued for `activity_id`: the row whose
/// `state`, `attempts` and `last_error` a retry scenario reads.
///
/// # Panics
///
/// If no delivery job names that activity, or the read fails.
async fn delivery_job(pool: &PgPool, activity_id: &str) -> Job {
    sqlx::query_as::<_, Job>("SELECT * FROM jobs WHERE kind = $1 AND args->>'activity_id' = $2")
        .bind(KIND_DELIVER)
        .bind(activity_id)
        .fetch_one(pool)
        .await
        .expect("the delivery job reads")
}

/// The whole seconds `job` sits out before its next attempt, measured
/// against the statement that scheduled it: `run_at - updated_at` is the
/// delay the queue wrote, whatever the clock now says.
///
/// # Panics
///
/// If the read fails.
async fn seconds_until_due(pool: &PgPool, job: &Job) -> i64 {
    sqlx::query_scalar::<_, i64>(
        "SELECT EXTRACT(EPOCH FROM (run_at - updated_at))::bigint FROM jobs WHERE id = $1",
    )
    .bind(job.id)
    .fetch_one(pool)
    .await
    .expect("the due-difference read runs")
}

/// Makes every scheduled `federator.deliver` retry due at once: the test's
/// stand-in for time passing, without waiting for any of it.
async fn make_retries_due(pool: &PgPool) {
    let _hit = sqlx::query("UPDATE jobs SET run_at = now() - interval '1 minute' WHERE kind = $1")
        .bind(KIND_DELIVER)
        .execute(pool)
        .await
        .expect("the backdating runs");
}

/// How many `federator.publish` jobs the queue holds under `unique_key`,
/// whatever their state: the debounce assertions count rows, not attempts.
///
/// # Panics
///
/// If the count read fails.
async fn publishes_with_key(pool: &PgPool, unique_key: &str) -> i64 {
    sqlx::query_scalar::<_, i64>(
        "SELECT count(*) FROM jobs WHERE kind = 'federator.publish' AND unique_key = $1",
    )
    .bind(unique_key)
    .fetch_one(pool)
    .await
    .expect("the publish count reads")
}

// --- asserting the peer's capture ---------------------------------------------------------

/// What one captured delivery is expected to look like: where it arrived,
/// what it carries and whose key signed it.
#[derive(Debug)]
struct Expected<'a> {
    /// The request path the mock recorded.
    path: &'a str,
    /// The `type` of the delivered activity.
    kind: &'a str,
    /// The local actor the document names and whose published key must
    /// authenticate the bytes.
    actor: &'a User,
    /// The pinned `to` line, when the scenario fixes it.
    to: Option<Value>,
    /// The pinned `cc` line, when the scenario fixes it.
    cc: Option<Value>,
}

/// One row of the visibility matrix: where the note's kind arrives and the
/// two addressing lines it must carry, for every lifecycle activity.
#[derive(Debug)]
struct NoteCase {
    /// The inbox path the mock records for this visibility.
    path: String,
    /// The expected `to` line.
    to: Value,
    /// The expected `cc` line.
    cc: Value,
}

/// The four delivered visibilities' cases in the order
/// [`post_all_visibilities`] posts them: `public` talks to the world with
/// the followers in `cc`, `unlisted` inverts the pair, `private` reaches
/// followers alone, and `direct` the mentioned remote's own inbox.
fn note_cases(bob_actor: &str, followers: &str, bob_personal: &str) -> [NoteCase; 4] {
    [
        NoteCase {
            path: remote::INBOX.to_owned(),
            to: json!([AS_PUBLIC]),
            cc: json!([followers]),
        },
        NoteCase {
            path: remote::INBOX.to_owned(),
            to: json!([followers]),
            cc: json!([AS_PUBLIC]),
        },
        NoteCase {
            path: remote::INBOX.to_owned(),
            to: json!([followers]),
            cc: json!([]),
        },
        NoteCase {
            path: bob_personal.to_owned(),
            to: json!([bob_actor]),
            cc: json!([]),
        },
    ]
}

/// `assert_delivered` shaped from a [`NoteCase`]: the same `kind` and
/// addressing the case pins.
fn expected_case<'a>(case: &'a NoteCase, kind: &'a str, actor: &'a User) -> Expected<'a> {
    Expected {
        path: &case.path,
        kind,
        actor,
        to: Some(case.to.clone()),
        cc: Some(case.cc.clone()),
    }
}

/// Asserts `delivery` is this instance's own signed `POST` of the activity
/// [`Expected`] describes: one that arrived at the mock's `path` with the
/// four auth headers plus the media-type pair, whose `Digest` describes the
/// bytes as received, whose `Signature` names the actor's `#main-key` and
/// verifies against the key that actor publishes, and whose document carries
/// the pinned addressing with the blind-recipient lines never on the wire.
fn assert_delivered(delivery: &remote::Delivery, expected: &Expected<'_>, mock_host: &str) {
    let body = delivery.activity();
    assert_eq!(delivery.path(), expected.path, "the arrival path");
    assert_eq!(body["type"], json!(expected.kind), "the activity type");
    assert_eq!(body["actor"], json!(expected.actor.ap_id), "the maker");
    let id = body["id"].as_str().expect("the activity carries its id");
    assert!(
        id.starts_with(&format!("{TEST_BASE_URL}/")),
        "an id on this instance's own origin: {id}"
    );
    assert_eq!(
        delivery.key_id().as_deref(),
        Some(format!("{}#main-key", expected.actor.ap_id)).as_deref(),
        "the signer is the actor the activity names"
    );
    assert!(
        delivery.digest_matches(),
        "the Digest describes the bytes that arrived"
    );
    let pem = expected
        .actor
        .public_key
        .as_deref()
        .expect("the fixture actor publishes its key");
    assert!(
        delivery.signature_verifies(pem),
        "and the Signature verifies against that key"
    );
    assert_eq!(
        delivery.header("content-type"),
        Some(ACTIVITY_JSON),
        "the body is labelled an activity"
    );
    assert_eq!(
        delivery.header("accept"),
        Some(ACTIVITY_JSON),
        "and the answer asked for in the same media type"
    );
    assert_eq!(delivery.header("host"), Some(mock_host), "the Host");
    let date = delivery.header("date").expect("a Date header is sent");
    assert!(date.ends_with(" GMT"), "a signed RFC1123 Date: {date}");
    assert!(
        delivery
            .header("digest")
            .is_some_and(|value| value.starts_with("SHA-256=")),
        "a SHA-256 Digest"
    );
    assert!(body.get("bto").is_none(), "no blind `to` on the wire");
    assert!(body.get("bcc").is_none(), "and no blind `cc`");
    if let Some(to) = &expected.to {
        assert_eq!(&body["to"], to, "the delivered `to` line");
    }
    if let Some(cc) = &expected.cc {
        assert_eq!(&body["cc"], cc, "the delivered `cc` line");
    }
}

/// Asserts the entity carries each `(key, value)` pair of `expected`
/// verbatim: one named fact per line.
fn assert_flags(entity: &Value, expected: &[(&str, Value)]) {
    for (key, value) in expected {
        assert_eq!(
            entity.get(*key).cloned().unwrap_or(Value::Null),
            *value,
            "the entity's `{key}` reads as expected"
        );
    }
}

/// The single recorded delivery whose activity is a `kind` naming `uri` as
/// its object (embedded or by reference): the one copy of one document a
/// scenario asserts its addressing against.
///
/// # Panics
///
/// If `uri` was delivered by that `kind` more than once, or not at all.
fn delivered_for<'a>(posts: &'a [remote::Delivery], kind: &str, uri: &str) -> &'a remote::Delivery {
    let found: Vec<&remote::Delivery> = posts
        .iter()
        .filter(|post| {
            post.activity_type() == Some(kind) && object_reference(post.activity()) == Some(uri)
        })
        .collect();
    assert_eq!(
        found.len(),
        1,
        "exactly one {kind} of `{uri}` was delivered"
    );
    found[0]
}

/// How many recorded deliveries carry `path` and a `kind` activity naming
/// `uri`: the count an addressing scenario compares against its table.
fn count_delivered(posts: &[remote::Delivery], path: &str, kind: &str, uri: &str) -> usize {
    posts
        .iter()
        .filter(|post| {
            post.path() == path
                && post.activity_type() == Some(kind)
                && object_reference(post.activity()) == Some(uri)
        })
        .count()
}

/// How many recorded deliveries arrived at `path`: the positive silence an
/// addressing scenario reads off the capture.
fn count_at(posts: &[remote::Delivery], path: &str) -> usize {
    posts.iter().filter(|post| post.path() == path).count()
}

/// How many recorded deliveries carry `kind`.
fn count_of(posts: &[remote::Delivery], kind: &str) -> usize {
    posts
        .iter()
        .filter(|post| post.activity_type() == Some(kind))
        .count()
}

/// The `object` of a delivered activity as a uri: the embedded document's
/// own `id`, or the plain string an `Announce` / `Like` names it with.
fn object_reference(activity: &Value) -> Option<&str> {
    activity.get("object").and_then(|object| {
        object
            .as_str()
            .or_else(|| object.get("id").and_then(Value::as_str))
    })
}

/// The id text of an entity's `id` field.
fn id_of(entity: &Value) -> String {
    entity["id"]
        .as_str()
        .expect("the entity id is a string")
        .to_owned()
}

/// The `uri` of an entity — for a boost, the id its `Announce` federates
/// under.
fn uri_of(entity: &Value) -> String {
    entity["uri"]
        .as_str()
        .expect("the entity uri is a string")
        .to_owned()
}

/// The deterministic `Create` id of a posted status entity (decision 6).
fn create_id_of(entity: &Value) -> String {
    format!("{TEST_BASE_URL}/activities/{}", id_of(entity))
}
// --- the scenarios ------------------------------------------------------------------------

/// Asserts the `Create` of the lifecycle test: decision 6's deterministic
/// id, the author's note embedded, public addressing on both lines.
fn assert_lifecycle_create(
    posts: &[remote::Delivery],
    alice: &User,
    host: &str,
    uri: &str,
    create_id: &str,
) {
    let create = delivered_for(posts, "Create", uri);
    assert_delivered(
        create,
        &expected_case(&public_case(alice), "Create", alice),
        host,
    );
    assert_flags(create.activity(), &[("id", json!(create_id))]);
    assert!(
        create.activity()["published"].is_string(),
        "the Create stamps its publication"
    );
    assert_flags(
        &create.activity()["object"],
        &[
            ("id", json!(uri)),
            ("type", json!("Note")),
            ("attributedTo", json!(alice.ap_id)),
            ("to", json!([AS_PUBLIC])),
        ],
    );
}

/// Asserts the `Update` of the lifecycle test: a fresh activity id of this
/// instance's own — never the Create restated — the same reserved uri, the
/// rewritten content and the edit stamp.
fn assert_lifecycle_update(
    posts: &[remote::Delivery],
    alice: &User,
    host: &str,
    uri: &str,
    create_id: &str,
) {
    let update = delivered_for(posts, "Update", uri);
    assert_delivered(
        update,
        &expected_case(&public_case(alice), "Update", alice),
        host,
    );
    let update_id = update.activity()["id"]
        .as_str()
        .expect("the Update carries its id");
    assert_ne!(
        update_id, create_id,
        "an edit is its own activity, not the Create restated"
    );
    assert!(
        update_id.starts_with(&format!("{TEST_BASE_URL}/activities/")),
        "under a fresh id of this instance's own: {update_id}"
    );
    assert_flags(
        &update.activity()["object"],
        &[
            ("id", json!(uri)),
            ("content", json!("<p>second draft</p>")),
        ],
    );
    assert!(
        update.activity()["object"]["updated"].is_string(),
        "and the re-drafted note carries its edit stamp"
    );
}

/// Asserts the `Delete` of the lifecycle test: the deterministic
/// `delete-{status id}` id and the `Tombstone` the object route now serves.
fn assert_lifecycle_delete(
    posts: &[remote::Delivery],
    alice: &User,
    host: &str,
    uri: &str,
    status_id: &str,
) {
    let delete = delivered_for(posts, "Delete", uri);
    assert_delivered(
        delete,
        &expected_case(&public_case(alice), "Delete", alice),
        host,
    );
    assert_flags(
        delete.activity(),
        &[(
            "id",
            json!(format!("{TEST_BASE_URL}/activities/delete-{status_id}")),
        )],
    );
    assert_flags(
        &delete.activity()["object"],
        &[
            ("id", json!(uri)),
            ("type", json!("Tombstone")),
            ("formerType", json!("Note")),
        ],
    );
}

/// The public note's case: `to` the world, `cc` the followers collection,
/// the mock's shared inbox the arrival path.
fn public_case(alice: &User) -> NoteCase {
    NoteCase {
        path: remote::INBOX.to_owned(),
        to: json!([AS_PUBLIC]),
        cc: json!([followers_of(alice)]),
    }
}

/// Scenario: the public status lifecycle. The `Create`, the `Update` of the
/// re-drafted note and the `Delete` of the `Tombstone` each arrive as one
/// signed `POST` at the follower's shared inbox, under decision 6's ids and
/// the same public addressing throughout.
#[tokio::test]
async fn a_public_status_lifecycle_travels_signed_to_the_follower_inbox() {
    let db = common::test_db().await;
    let pool = db.pool().clone();
    let (app, state) = remote::app_and_state(pool.clone());
    let peer = remote::MockPeer::start().await;
    let (alice, alice_token) = local(&pool, "eo-alice").await;
    peer_follows(&app, &state, &peer, BOB, &alice).await;
    let host = peer.host();

    let posted = post(&app, &alice_token, json!({"status": "first draft"})).await;
    let id = id_of(&posted);
    let uri = uri_of(&posted);
    let create_id = create_id_of(&posted);
    let _create_delivered = common::run_all_jobs(&state).await;
    let _edited = edit(&app, &alice_token, &id, "second draft").await;
    let _update_delivered = common::run_all_jobs(&state).await;
    destroy(&app, &alice_token, &id).await;
    let _delete_delivered = common::run_all_jobs(&state).await;

    let posts = peer.received();
    assert_lifecycle_create(&posts, &alice, &host, &uri, &create_id);
    assert_lifecycle_update(&posts, &alice, &host, &uri, &create_id);
    assert_lifecycle_delete(&posts, &alice, &host, &uri, &id);
    assert_eq!(
        posts.len(),
        4,
        "the Accept that answered the follow, and the lifecycle — nothing else"
    );
    assert_eq!(posts[0].activity_type(), Some("Accept"));
    assert_eq!(posts[0].path(), personal(&peer, BOB));
}

/// Scenario: the visibility matrix over the post and the edit. Each of
/// `public`, `unlisted`, `private` and `direct` travels with its own `to`
/// and `cc` split to the inbox its audience expands to, and `local` never
/// arrives: the mock holds four notes up and four re-drafts, no fifth.
#[tokio::test]
async fn every_visibility_shapes_and_sends_the_note_its_own_way() {
    let db = common::test_db().await;
    let pool = db.pool().clone();
    let (app, state) = remote::app_and_state(pool.clone());
    let peer = remote::MockPeer::start().await;
    let (alice, alice_token) = local(&pool, "eo-matrix-alice").await;
    peer_follows(&app, &state, &peer, BOB, &alice).await;
    let host = peer.host();
    let cases = note_cases(
        &peer.actor(BOB),
        &followers_of(&alice),
        &personal(&peer, BOB),
    );

    let specs = [
        ("to the world", None),
        ("off the timeline", Some("unlisted")),
        ("for followers", Some("private")),
        ("psst", Some("direct")),
        ("instance only", Some("local")),
    ];
    let mut posted = Vec::new();
    for (text, visibility) in specs {
        let body = if visibility == Some("direct") {
            json!({"status": format!("{text} @{}", peer.handle(BOB)), "visibility": "direct"})
        } else {
            json!({"status": text, "visibility": visibility.unwrap_or("public")})
        };
        posted.push(post(&app, &alice_token, body).await);
    }
    // The direct note must keep its mention when edited: the re-drafted
    // document is addressed from the live row's audience, and an edited DM
    // that names nobody travels nowhere.
    let drafts = [
        "edited world",
        "edited timeline",
        "edited followers",
        &format!("edited psst @{}", peer.handle(BOB)),
        "edited inside",
    ];
    for (entity, text) in posted.iter().zip(drafts) {
        let id = id_of(entity);
        let _edited = edit(&app, &alice_token, &id, text).await;
    }
    let _drained = common::run_all_jobs(&state).await;

    let posts = peer.received();
    for (index, case) in cases.iter().enumerate() {
        let uri = uri_of(&posted[index]);
        for kind in ["Create", "Update"] {
            assert_delivered(
                delivered_for(&posts, kind, &uri),
                &expected_case(case, kind, &alice),
                &host,
            );
        }
    }
    assert_eq!(
        count_delivered(&posts, remote::INBOX, "Create", &uri_of(&posted[4])),
        0,
        "the `local` post delivered nothing"
    );
    assert_eq!(
        count_of(&posts, "Create") + count_of(&posts, "Update"),
        8,
        "four notes up, four re-drafts, no fifth"
    );
}

/// Scenario: the withdrawal of every visibility repeats its audience. The
/// `Delete` of each post lands on exactly the path its `Create` arrived at,
/// with the same `to` and `cc` — the direct one's comes down at the
/// personal inbox alone.
#[tokio::test]
async fn each_delete_comes_down_exactly_where_its_post_went_up() {
    let db = common::test_db().await;
    let pool = db.pool().clone();
    let (app, state) = remote::app_and_state(pool.clone());
    let peer = remote::MockPeer::start().await;
    let (alice, alice_token) = local(&pool, "eo-down-alice").await;
    peer_follows(&app, &state, &peer, BOB, &alice).await;
    let host = peer.host();
    let cases = note_cases(
        &peer.actor(BOB),
        &followers_of(&alice),
        &personal(&peer, BOB),
    );

    let mut posted = Vec::new();
    for (text, visibility) in [
        ("public down", None),
        ("unlisted down", Some("unlisted")),
        ("private down", Some("private")),
        ("direct down", Some("direct")),
    ] {
        let body = if visibility == Some("direct") {
            json!({"status": format!("{text} @{}", peer.handle(BOB)), "visibility": "direct"})
        } else {
            json!({"status": text, "visibility": visibility.unwrap_or("public")})
        };
        posted.push(post(&app, &alice_token, body).await);
    }
    let _up = common::run_all_jobs(&state).await;
    for entity in &posted {
        destroy(&app, &alice_token, &id_of(entity)).await;
    }
    let _down = common::run_all_jobs(&state).await;

    let posts = peer.received();
    for (case, entity) in cases.iter().zip(&posted) {
        let uri = uri_of(entity);
        assert_eq!(
            count_delivered(&posts, &case.path, "Delete", &uri),
            1,
            "the withdrawal of `{uri}` arrived once, at `{}`",
            case.path
        );
        assert_delivered(
            delivered_for(&posts, "Delete", &uri),
            &expected_case(case, "Delete", &alice),
            &host,
        );
    }
    assert_eq!(count_of(&posts, "Create"), 4, "one up for each down");
}

/// Scenario: `local` visibility keeps everything at home. The post, its edit
/// and its delete write their rows, change what the client reads, and leave
/// the mock with nothing but the `Accept` that answered its own follow.
#[tokio::test]
async fn a_local_only_status_leaves_nothing_at_all() {
    let db = common::test_db().await;
    let pool = db.pool().clone();
    let (app, state) = remote::app_and_state(pool.clone());
    let peer = remote::MockPeer::start().await;
    let (alice, alice_token) = local(&pool, "eo-home-alice").await;
    peer_follows(&app, &state, &peer, BOB, &alice).await;

    let posted = post_text(&app, &alice_token, "stays inside", Some("local")).await;
    let id = id_of(&posted);
    let _edited = edit(&app, &alice_token, &id, "still inside").await;
    let _first = common::run_all_jobs(&state).await;
    destroy(&app, &alice_token, &id).await;
    let _second = common::run_all_jobs(&state).await;

    assert_eq!(
        peer.delivery_count(),
        1,
        "nothing of the local post, its edit or its delete left the process"
    );
    assert_eq!(peer.received()[0].activity_type(), Some("Accept"));
}

/// Scenario: a direct post is for the mentionee alone. It reaches `dave`'s
/// personal inbox even though the mock publishes a shared endpoint for
/// every actor — the publisher puts a directly-addressed recipient at the
/// inbox that names them — and the follower `bob` never hears of it: no
/// shared-inbox copy and no follower fan-out.
#[tokio::test]
async fn a_direct_post_reaches_the_mentioned_inbox_alone() {
    let db = common::test_db().await;
    let pool = db.pool().clone();
    let (app, state) = remote::app_and_state(pool.clone());
    let peer = remote::MockPeer::start().await;
    let (alice, alice_token) = local(&pool, "eo-dm-alice").await;
    peer_follows(&app, &state, &peer, BOB, &alice).await;

    let text = format!("dave, between us @{}", peer.handle(DAVE));
    let posted = post_text(&app, &alice_token, &text, Some("direct")).await;
    let _drained = common::run_all_jobs(&state).await;

    let posts = peer.received();
    let uri = uri_of(&posted);
    assert_delivered(
        delivered_for(&posts, "Create", &uri),
        &Expected {
            path: &personal(&peer, DAVE),
            kind: "Create",
            actor: &alice,
            to: Some(json!([peer.actor(DAVE)])),
            cc: Some(json!([])),
        },
        &peer.host(),
    );
    assert_eq!(
        count_delivered(&posts, remote::INBOX, "Create", &uri),
        0,
        "the shared inbox holds no copy of a direct"
    );
    assert_eq!(
        count_delivered(&posts, &personal(&peer, BOB), "Create", &uri),
        0,
        "the follower who was not mentioned holds none either"
    );
    assert_eq!(
        posts.len(),
        2,
        "bob's Accept and this DM: the direct fans out to nobody else"
    );
}

/// Scenario: the boost and its withdrawal. A public `Announce` names the
/// world in `to` and the followers with the original author in `cc`, and
/// travels under the reblog row's own `uri`; the `Undo` repeats the
/// addressing and embeds that very document.
#[tokio::test]
async fn a_boost_and_its_withdrawal_travel_with_the_original_author_in_cc() {
    let db = common::test_db().await;
    let pool = db.pool().clone();
    let (app, state) = remote::app_and_state(pool.clone());
    let peer = remote::MockPeer::start().await;
    let (alice, alice_token) = local(&pool, "eo-boost-alice").await;
    let (carol, carol_token) = local(&pool, "eo-boost-carol").await;
    peer_follows(&app, &state, &peer, BOB, &alice).await;

    let original = post(&app, &carol_token, json!({"status": "boost me"})).await;
    let boost = status_action(&app, &alice_token, &id_of(&original), "reblog", None).await;
    let quiet_source = post(&app, &carol_token, json!({"status": "quietly boost me"})).await;
    let _quiet = status_action(
        &app,
        &alice_token,
        &id_of(&quiet_source),
        "reblog",
        Some(json!({"visibility": "unlisted"})),
    )
    .await;
    let _drained = common::run_all_jobs(&state).await;

    let posts = peer.received();
    let with_author = json!([followers_of(&alice), carol.ap_id]);
    let announce = delivered_for(&posts, "Announce", &uri_of(&original));
    assert_delivered(
        announce,
        &Expected {
            path: remote::INBOX,
            kind: "Announce",
            actor: &alice,
            to: Some(json!([AS_PUBLIC])),
            cc: Some(with_author.clone()),
        },
        &peer.host(),
    );
    assert_eq!(
        announce.activity()["id"],
        json!(uri_of(&boost)),
        "the Announce travels under the reblog row's own uri (decision 6)"
    );
    let quiet_announce = delivered_for(&posts, "Announce", &uri_of(&quiet_source));
    assert_eq!(
        quiet_announce.activity()["to"],
        json!([followers_of(&alice)]),
        "an unlisted boost names the followers collection in `to`"
    );
    assert_eq!(
        quiet_announce.activity()["cc"],
        with_author,
        "and still carries the followers with the original author in `cc`"
    );

    let _withdrawn = status_action(&app, &alice_token, &id_of(&boost), "unreblog", None).await;
    let _withdrawal_delivered = common::run_all_jobs(&state).await;
    let undos = peer.delivered("Undo");
    assert_eq!(undos.len(), 1, "the withdrawal is announced once");
    assert_delivered(
        &undos[0],
        &Expected {
            path: remote::INBOX,
            kind: "Undo",
            actor: &alice,
            to: Some(json!([AS_PUBLIC])),
            cc: Some(with_author),
        },
        &peer.host(),
    );
    assert_flags(
        &undos[0].activity()["object"],
        &[
            ("id", json!(uri_of(&boost))),
            ("type", json!("Announce")),
            ("actor", json!(alice.ap_id)),
        ],
    );
}

/// Scenario: the favourite and its withdrawal. A `Like` of a public post
/// names the acted-on author in `to` and the followers with the public
/// address in `cc`, under the favourite row's stored activity id; the
/// unfavourite's `Undo` repeats those lines and embeds that very `Like`. A
/// `Like` of an unlisted post stays with its (local) author: nothing leaves
/// the process for it.
#[tokio::test]
async fn a_favourite_and_its_withdrawal_name_the_author_and_the_followers() {
    let db = common::test_db().await;
    let pool = db.pool().clone();
    let (app, state) = remote::app_and_state(pool.clone());
    let peer = remote::MockPeer::start().await;
    let (alice, alice_token) = local(&pool, "eo-like-alice").await;
    let (carol, carol_token) = local(&pool, "eo-like-carol").await;
    peer_follows(&app, &state, &peer, BOB, &alice).await;

    let open = post(&app, &carol_token, json!({"status": "favourite me"})).await;
    let _liked = status_action(&app, &alice_token, &id_of(&open), "favourite", None).await;
    let _drained = common::run_all_jobs(&state).await;

    let posts = peer.received();
    let author_to = json!([carol.ap_id]);
    let crowd_cc = json!([followers_of(&alice), AS_PUBLIC]);
    let like = delivered_for(&posts, "Like", &uri_of(&open));
    assert_delivered(
        like,
        &Expected {
            path: remote::INBOX,
            kind: "Like",
            actor: &alice,
            to: Some(author_to.clone()),
            cc: Some(crowd_cc.clone()),
        },
        &peer.host(),
    );
    let like_id = like.activity()["id"].as_str().expect("the Like id");
    assert!(
        like_id.starts_with(&format!("{TEST_BASE_URL}/activities/")),
        "the favourite row's stored activity id: {like_id}"
    );

    let hidden = post(
        &app,
        &carol_token,
        json!({"status": "quietly yours", "visibility": "unlisted"}),
    )
    .await;
    let _quiet_like = status_action(&app, &alice_token, &id_of(&hidden), "favourite", None).await;
    let _drained_again = common::run_all_jobs(&state).await;
    assert_eq!(
        peer.received().len(),
        posts.len(),
        "a like of an unlisted post with a local author announces nothing new"
    );

    let _unliked = status_action(&app, &alice_token, &id_of(&open), "unfavourite", None).await;
    let _drained_thrice = common::run_all_jobs(&state).await;
    let undos = peer.delivered("Undo");
    assert_eq!(undos.len(), 1, "the unfavourite is announced once");
    assert_delivered(
        &undos[0],
        &Expected {
            path: remote::INBOX,
            kind: "Undo",
            actor: &alice,
            to: Some(author_to),
            cc: Some(crowd_cc),
        },
        &peer.host(),
    );
    assert_flags(
        &undos[0].activity()["object"],
        &[("id", json!(like_id)), ("type", json!("Like"))],
    );
}

/// Scenario: the emoji reaction and its withdrawal. An `EmojiReact` shares
/// the `Like`'s author-in-`to`, followers-in-`cc` shape but never the public
/// address, carries the emoji as `content` and the acted-on uri as its
/// object; the `Undo` repeats the lines and embeds the reaction's stored id.
#[tokio::test]
async fn an_emoji_reaction_and_its_withdrawal_federate_without_the_public_address() {
    let db = common::test_db().await;
    let pool = db.pool().clone();
    let (app, state) = remote::app_and_state(pool.clone());
    let peer = remote::MockPeer::start().await;
    let (alice, alice_token) = local(&pool, "eo-react-alice").await;
    let (carol, carol_token) = local(&pool, "eo-react-carol").await;
    peer_follows(&app, &state, &peer, BOB, &alice).await;

    let open = post(&app, &carol_token, json!({"status": "react to me"})).await;
    reaction_write(&app, &alice_token, &id_of(&open), "PUT", PARTY).await;
    let _drained = common::run_all_jobs(&state).await;

    let posts = peer.received();
    let author_to = json!([carol.ap_id]);
    let followers_cc = json!([followers_of(&alice)]);
    let react = delivered_for(&posts, "EmojiReact", &uri_of(&open));
    assert_delivered(
        react,
        &Expected {
            path: remote::INBOX,
            kind: "EmojiReact",
            actor: &alice,
            to: Some(author_to.clone()),
            cc: Some(followers_cc.clone()),
        },
        &peer.host(),
    );
    assert_eq!(
        react.activity()["content"],
        "🎉",
        "the emoji is the activity's own content"
    );
    let react_id = react.activity()["id"].as_str().expect("the stored id");

    reaction_write(&app, &alice_token, &id_of(&open), "DELETE", PARTY).await;
    let _drained_again = common::run_all_jobs(&state).await;
    let undos = peer.delivered("Undo");
    assert_eq!(undos.len(), 1, "the withdrawal is announced once");
    assert_delivered(
        &undos[0],
        &Expected {
            path: remote::INBOX,
            kind: "Undo",
            actor: &alice,
            to: Some(author_to),
            cc: Some(followers_cc),
        },
        &peer.host(),
    );
    assert_flags(
        &undos[0].activity()["object"],
        &[
            ("id", json!(react_id)),
            ("type", json!("EmojiReact")),
            ("content", json!("🎉")),
        ],
    );
}

/// Scenario: shared-inbox dedupe. `bob` and `dave` — two followers on one
/// instance — cost one `Create` between them, because the publisher expands
/// the followers collection through the shared endpoint and dedupes the
/// candidate set by URL; the delivery row carries the key
/// `deliver:{activity id}:{inbox}` that swallows a second queueing of the
/// same pair while it is live. The per-actor inboxes stay reachable, and a
/// reaction restated before its delivery has run never sends twice.
#[tokio::test]
async fn followers_sharing_one_inbox_cost_one_delivery_per_activity() {
    let db = common::test_db().await;
    let pool = db.pool().clone();
    let (app, state) = remote::app_and_state(pool.clone());
    let peer = remote::MockPeer::start().await;
    let (alice, alice_token) = local(&pool, "eo-dedupe-alice").await;
    peer_follows(&app, &state, &peer, BOB, &alice).await;
    peer_follows(&app, &state, &peer, DAVE, &alice).await;

    let posted = post(&app, &alice_token, json!({"status": "two followers"})).await;
    let _drained = common::run_all_jobs(&state).await;

    let posts = peer.received();
    let uri = uri_of(&posted);
    assert_eq!(
        count_delivered(&posts, remote::INBOX, "Create", &uri),
        1,
        "two followers, one shared inbox, one delivery"
    );
    let create_id = create_id_of(&posted);
    let job = delivery_job(&pool, &create_id).await;
    assert_eq!(
        job.unique_key.as_deref(),
        Some(format!("deliver:{create_id}:{}", peer.shared_inbox()).as_str()),
        "the dedup key names exactly this activity-inbox pair"
    );
    assert_eq!(job.state, "completed", "and it delivered once, cleanly");
    assert!(
        posts.iter().any(|p| p.path() == personal(&peer, BOB)),
        "the per-actor inbox path stayed reachable throughout"
    );

    let restated = post(&app, &alice_token, json!({"status": "restated fact"})).await;
    reaction_write(&app, &alice_token, &id_of(&restated), "PUT", PARTY).await;
    reaction_write(&app, &alice_token, &id_of(&restated), "PUT", PARTY).await;
    let _drained_again = common::run_all_jobs(&state).await;
    assert_eq!(
        peer.delivered("EmojiReact").len(),
        1,
        "the repeat write queues a second fan-out, the live delivery's key swallows it"
    );
}

/// One `PATCH /api/v1/accounts/update_credentials` multipart write with the
/// given text parts and an optional image file, asserted 200 and answered
/// as the `Account` entity.
async fn profile_patch(app: &Router, token: &str, parts: &[common::MultipartPart]) -> Value {
    let response = common::TestRequest::patch("/api/v1/accounts/update_credentials")
        .bearer(token)
        .multipart("wired-e2e-outbound-profile", parts)
        .send(app)
        .await;
    common::assert_answer(&response, StatusCode::OK, "the profile write");
    response.json_value()
}

/// Scenario: the profile `Update`. A `PATCH /api/v1/accounts/update_credentials`
/// puts the freshly rendered actor document on the wire to the follower's
/// shared inbox — display name, bio, profile fields and avatar as the row
/// now holds them, `to` the public and the followers collection in `cc` —
/// and a burst of later edits collapses into the one activity the
/// per-account `profile-update:{id}` key keeps queued.
#[tokio::test]
async fn a_profile_update_federates_the_fresh_actor_document() {
    let db = common::test_db().await;
    let pool = db.pool().clone();
    let uploads = TempDir::new().expect("temp dir for uploads");
    let config = common::test_config_with_uploads_dir(uploads.path());
    let state = common::test_app_state(pool.clone(), config);
    let app = wired_be::app(state.clone());
    let peer = remote::MockPeer::start().await;
    let (alice, alice_token) = local(&pool, "eo-profile-alice").await;
    peer_follows(&app, &state, &peer, BOB, &alice).await;

    let entity = profile_patch(
        &app,
        &alice_token,
        &[
            common::MultipartPart::text("display_name", "Alice Wonder"),
            common::MultipartPart::text("note", "bio the peers should read"),
            common::MultipartPart::text("fields_attributes[0][name]", "Site"),
            common::MultipartPart::text("fields_attributes[0][value]", "wired.example"),
            common::MultipartPart::file("avatar", "me.png", "image/png", Bytes::from_static(PNG)),
        ],
    )
    .await;
    let _drained = common::run_all_jobs(&state).await;

    let first_updates = peer.delivered("Update");
    assert_eq!(
        first_updates.len(),
        1,
        "the profile write is announced once"
    );
    assert_delivered(
        &first_updates[0],
        &Expected {
            path: remote::INBOX,
            kind: "Update",
            actor: &alice,
            to: Some(json!([AS_PUBLIC])),
            cc: Some(json!([followers_of(&alice)])),
        },
        &peer.host(),
    );
    let document = &first_updates[0].activity()["object"];
    assert_flags(
        document,
        &[
            ("id", json!(alice.ap_id)),
            ("type", json!("Person")),
            ("name", json!("Alice Wonder")),
            ("summary", json!("<p>bio the peers should read</p>")),
            ("icon", json!({ "type": "Image", "url": entity["avatar"] })),
        ],
    );
    assert_eq!(
        document["attachment"][0]["name"],
        json!("Site"),
        "the profile field travels as a property"
    );

    let burst = [common::MultipartPart::text("note", "second bio")];
    let _second = profile_patch(&app, &alice_token, &burst).await;
    let _third = profile_patch(
        &app,
        &alice_token,
        &[common::MultipartPart::text("display_name", "Alice Twice")],
    )
    .await;
    let _drained_again = common::run_all_jobs(&state).await;
    let updates = peer.delivered("Update");
    assert_eq!(updates.len(), 2, "the burst rides out as one Update");
    assert_eq!(
        publishes_with_key(&pool, &format!("profile-update:{}", alice.id)).await,
        2,
        "two live windows, two keyed publish jobs: the swallowed edit queued none"
    );
    assert_flags(
        &updates[1].activity()["object"],
        &[
            ("name", json!("Alice Wonder")),
            ("summary", json!("<p>second bio</p>")),
        ],
    );
}

/// Scenario: `POST /api/v1/accounts/move` announces the move over the wire.
/// The origin's remote follower receives the `Move` — `object` the origin,
/// `target` the destination, addressed to the followers collection alone —
/// and the profile `Update` of the re-read actor carrying the new `movedTo`.
#[tokio::test]
async fn a_move_announces_the_new_home_to_the_origin_followers() {
    let db = common::test_db().await;
    let pool = db.pool().clone();
    let (app, state) = remote::app_and_state(pool.clone());
    let peer = remote::MockPeer::start().await;
    let alice = keyed_with_password(&pool, "eo-move-alice", "moving-password").await;
    let alice_token = common::create_app_and_token(&pool, &alice, SCOPES).await;
    let carol = keyed_with_password(&pool, "eo-move-carol", "waiting-password").await;
    let carol_token = common::create_app_and_token(&pool, &carol, SCOPES).await;
    peer_follows(&app, &state, &peer, BOB, &alice).await;

    let aliased = common::TestRequest::put("/api/pleroma/aliases")
        .bearer(&carol_token)
        .json(&json!({"alias": alice.ap_id}))
        .send(&app)
        .await;
    common::assert_answer(&aliased, StatusCode::OK, "the destination lists its alias");
    let moved = common::TestRequest::post(&format!("{ACCOUNTS}/move"))
        .bearer(&alice_token)
        .json(&json!({"to_account": carol.id.to_string(), "password": "moving-password"}))
        .send(&app)
        .await;
    common::assert_answer(&moved, StatusCode::OK, "the move write");
    assert_eq!(
        moved.json_value()["id"],
        json!(carol.id.to_string()),
        "the answer is the caller's relationship to the destination"
    );
    let _drained = common::run_all_jobs(&state).await;

    let movement = peer.delivered("Move");
    assert_eq!(movement.len(), 1, "one Move to the follower's shared inbox");
    assert_delivered(
        &movement[0],
        &Expected {
            path: remote::INBOX,
            kind: "Move",
            actor: &alice,
            to: Some(json!([followers_of(&alice)])),
            cc: Some(json!([])),
        },
        &peer.host(),
    );
    assert_flags(
        movement[0].activity(),
        &[
            ("object", json!(alice.ap_id)),
            ("target", json!(carol.ap_id)),
        ],
    );
    let updates = peer.delivered("Update");
    assert_eq!(updates.len(), 1, "and the re-read actor follows the move");
    assert_eq!(
        updates[0].activity()["object"]["movedTo"],
        json!(carol.ap_id),
        "the delivered document grows the gone-to pointer"
    );
    assert_eq!(
        peer.received().len(),
        3,
        "the Accept, the Move and the Update: nothing else left"
    );
}

/// Scenario: the forwarded `Flag`. `POST /api/v1/accounts/{id}/flag` files
/// the report and asks the target's instance to handle it: the INSTANCE
/// actor speaks a `Flag` whose `object` names the reported account and
/// whose `content` is the reporter's comment, delivered to the target's
/// personal inbox alone — no fan-out beyond it — and the filing stands in
/// the admin queue. The same write against a local account answers the
/// entity and sends nothing: those moderators are our own.
#[tokio::test]
async fn a_flag_reaches_the_remote_inbox_and_the_admin_queue() {
    let db = common::test_db().await;
    let pool = db.pool().clone();
    let (app, state) = remote::app_and_state(pool.clone());
    let peer = remote::MockPeer::start().await;
    let (alice, alice_token) = local(&pool, "eo-flag-alice").await;
    peer_follows(&app, &state, &peer, BOB, &alice).await;
    let bob = lookup(&app, &peer.handle(BOB)).await;
    let bob_id = bob["id"].as_str().expect("the mirror id");

    let filed = common::TestRequest::post(&format!("{ACCOUNTS}/{bob_id}/flag"))
        .bearer(&alice_token)
        .json(&json!({"comment": "harassment, please review", "category": "spam"}))
        .send(&app)
        .await;
    common::assert_answer(&filed, StatusCode::OK, "the flag write");
    assert_flags(
        &filed.json_value(),
        &[
            ("forwarded", json!(true)),
            ("comment", json!("harassment, please review")),
        ],
    );
    let _drained = common::run_all_jobs(&state).await;

    let instance = instance_actor(&state).await;
    let flags = peer.delivered("Flag");
    assert_eq!(flags.len(), 1, "the target's instance is told once");
    assert_delivered(
        &flags[0],
        &Expected {
            path: &personal(&peer, BOB),
            kind: "Flag",
            actor: &instance,
            to: Some(json!([peer.actor(BOB)])),
            cc: Some(json!([])),
        },
        &peer.host(),
    );
    assert_flags(
        flags[0].activity(),
        &[
            ("object", json!([peer.actor(BOB)])),
            ("content", json!("harassment, please review")),
        ],
    );

    let admin = common::create_local_user_with(&pool, "eo-flag-admin", |user| NewUser {
        is_admin: Flag::new(true),
        ..user
    })
    .await;
    let admin_token = common::create_app_and_token(&pool, &admin, "admin:read:reports").await;
    let queue = get_json(&app, &admin_token, "/api/v1/pleroma/admin/reports").await;
    assert_eq!(
        queue["total"],
        json!(1),
        "the filing stands in the admin queue"
    );
    assert_eq!(
        queue["reports"][0]["account"]["id"],
        json!(bob_id),
        "naming the reported account with the reporter's comment"
    );

    let before = peer.delivery_count();
    let carol = remote::keyed_local_user(&pool, "eo-flag-carol").await;
    let carol_id = lookup(&app, &format!("{}@test.example", carol.nickname)).await;
    let local_path = format!("{ACCOUNTS}/{}/flag", id_of(&carol_id));
    let local_filed = common::TestRequest::post(&local_path)
        .bearer(&alice_token)
        .json(&json!({"comment": "our own moderator problem"}))
        .send(&app)
        .await;
    common::assert_answer(
        &local_filed,
        StatusCode::OK,
        "flagging a local account files",
    );
    let _drained_again = common::run_all_jobs(&state).await;
    assert_eq!(
        peer.delivery_count(),
        before,
        "and a local target's moderators hear nothing on the wire"
    );
}

/// Accepts `follower`'s pending follow of the mock account `nick`: drains
/// the outbound `Follow`, reads its id off the capture and rides the peer's
/// `Accept` back through the inbox, so the pair stands accepted.
async fn accept_outgoing_follow(
    app: &Router,
    state: &AppState,
    peer: &remote::MockPeer,
    nick: &str,
    name: &str,
) {
    let follows = peer.delivered("Follow");
    let follow_id = follows[0].activity()["id"]
        .as_str()
        .expect("the Follow carries its id")
        .to_owned();
    let accept = peer.activity("Accept", name, &peer.actor(nick), &json!(&follow_id));
    let folded = peer
        .deliver(app, &peer.actor(nick), remote::INBOX, &accept)
        .await;
    assert_eq!(
        folded.status,
        StatusCode::ACCEPTED,
        "the inbox folds the peer's Accept"
    );
    let _drained = common::run_all_jobs(state).await;
}

/// Scenario: list membership. Adding the remote member announces an `Add` —
/// `actor` the list owner, `object` the member, `target` the list's curated
/// inbox — delivered to the member's personal inbox alone; removing them
/// announces the matching `Remove`. Nothing of it touches the shared inbox.
#[tokio::test]
async fn list_membership_adds_and_removes_reach_the_member_inbox() {
    let db = common::test_db().await;
    let pool = db.pool().clone();
    let (app, state) = remote::app_and_state(pool.clone());
    let peer = remote::MockPeer::start().await;
    let (alice, alice_token) = local(&pool, "eo-list-alice").await;
    let target = follow_by_uri(&app, &alice_token, &peer.handle(BOB)).await;
    let bob_id = id_of(&target);
    let _sent = common::run_all_jobs(&state).await;
    accept_outgoing_follow(&app, &state, &peer, BOB, "eo-list-accept").await;

    let list = common::TestRequest::post("/api/v1/lists")
        .bearer(&alice_token)
        .json(&json!({"title": "picks"}))
        .send(&app)
        .await;
    common::assert_answer(&list, StatusCode::OK, "the list create");
    let list_id = list.json_value()["id"]
        .as_str()
        .expect("the list id")
        .to_owned();

    let added = common::TestRequest::post(&format!("/api/v1/lists/{list_id}/accounts"))
        .bearer(&alice_token)
        .json(&json!({"account_ids": [bob_id]}))
        .send(&app)
        .await;
    common::assert_answer(&added, StatusCode::OK, "the member add");
    let _drained = common::run_all_jobs(&state).await;

    let curated = format!("{TEST_BASE_URL}/api/v1/lists/{list_id}/inbox");
    let adds = peer.delivered("Add");
    assert_eq!(adds.len(), 1, "the join is announced once");
    assert_list_membership(&adds[0], &alice, &peer, "Add", &curated);

    let removed = common::TestRequest::delete(&format!(
        "/api/v1/lists/{list_id}/accounts?account_ids%5B%5D={bob_id}"
    ))
    .bearer(&alice_token)
    .send(&app)
    .await;
    common::assert_answer(&removed, StatusCode::OK, "the member remove");
    let _drained_again = common::run_all_jobs(&state).await;

    let withdrawals = peer.delivered("Remove");
    assert_eq!(withdrawals.len(), 1, "the leaving is announced once");
    assert_list_membership(&withdrawals[0], &alice, &peer, "Remove", &curated);
    assert_eq!(
        count_at(&peer.received(), remote::INBOX),
        0,
        "the shared inbox holds no membership news"
    );
}

/// Asserts `delivery` is one list-membership activity of `kind` (`Add` or
/// `Remove`): signed by the list owner `alice`, naming the mock's `bob` as
/// object and only addressee, and `target`ing the list's curated inbox.
fn assert_list_membership(
    delivery: &remote::Delivery,
    alice: &User,
    peer: &remote::MockPeer,
    kind: &str,
    curated: &str,
) {
    assert_delivered(
        delivery,
        &Expected {
            path: &personal(peer, BOB),
            kind,
            actor: alice,
            to: Some(json!([peer.actor(BOB)])),
            cc: Some(json!([])),
        },
        &peer.host(),
    );
    assert_eq!(
        delivery.activity()["object"],
        json!(peer.actor(BOB)),
        "the member is the object of their own membership news"
    );
    assert_eq!(
        delivery.activity()["target"],
        json!(curated),
        "the activity points back at the list's curated inbox"
    );
}

/// Asserts one read of the retried delivery row: its state, the attempts
/// spent, and `answer` in the `last_error` of its most recent failure —
/// which [`complete`] leaves on the row even once the job finishes.
///
/// [`complete`]: wired_be::models::job::complete
async fn assert_job_after(
    pool: &PgPool,
    activity_id: &str,
    state_want: &str,
    attempts_want: i32,
    answer: &str,
) -> Job {
    let job = delivery_job(pool, activity_id).await;
    assert_eq!(job.state, state_want, "the delivery row's state");
    assert_eq!(job.attempts, attempts_want, "attempts spent so far");
    assert!(
        job.last_error
            .as_deref()
            .is_some_and(|err| err.contains(answer)),
        "the error names the peer's answer ({answer}): {:?}",
        job.last_error
    );
    job
}

/// Scenario: the retry ladder. The mock's inbox answers the `Create` once
/// with `500` and once with `503 Retry-After: 3600`: the first failure puts
/// the job back on the queue's own fifteen-second backoff — due later, and
/// untouched by a drain that runs now — the second schedules the peer's own
/// moment instead, and each later attempt resends the byte-identical signed
/// body. Nothing sleeps: the attempts advance by backdating the job row.
#[tokio::test]
async fn a_failing_inbox_retries_the_same_signed_bytes_until_it_answers() {
    let db = common::test_db().await;
    let pool = db.pool().clone();
    let (app, state) = remote::app_and_state(pool.clone());
    let peer = remote::MockPeer::start().await;
    let (alice, alice_token) = local(&pool, "eo-retry-alice").await;
    peer_follows(&app, &state, &peer, BOB, &alice).await;

    let posted = post(&app, &alice_token, json!({"status": "ride it out"})).await;
    let create_id = create_id_of(&posted);
    peer.script_inbox(StatusCode::INTERNAL_SERVER_ERROR, None, 1);
    let _first_pass = common::run_all_jobs(&state).await;

    let stalled = assert_job_after(&pool, &create_id, "retryable", 1, "500").await;
    assert_eq!(
        seconds_until_due(&pool, &stalled).await,
        FIRST_BACKOFF_SECS,
        "due again at the queue's own backoff step"
    );
    let idle = common::run_all_jobs(&state).await;
    assert_eq!(idle, 0, "the not-yet-due delivery is not claimed early");

    make_retries_due(&pool).await;
    peer.script_inbox(
        StatusCode::SERVICE_UNAVAILABLE,
        Some(Duration::from_secs(3600)),
        1,
    );
    let _second_pass = common::run_all_jobs(&state).await;
    let throttled = assert_job_after(&pool, &create_id, "retryable", 2, "503").await;
    assert_eq!(
        seconds_until_due(&pool, &throttled).await,
        3600,
        "the peer's Retry-After beat the queue's own backoff"
    );
    assert!(
        throttled
            .last_error
            .as_deref()
            .is_some_and(|err| err.contains("3600")),
        "the error names the peer's own delay: {:?}",
        throttled.last_error
    );

    make_retries_due(&pool).await;
    let _third_pass = common::run_all_jobs(&state).await;
    let finished = assert_job_after(&pool, &create_id, "completed", 3, "503").await;
    assert_eq!(finished.state, "completed", "the ladder ends in a success");

    assert_signed_resends(&peer, &alice, 3);
}

/// Asserts the mock recorded `want` deliveries of the one `Create`, each
/// byte-identical to the first and each freshly signed by `alice`'s key.
fn assert_signed_resends(peer: &remote::MockPeer, alice: &User, want: usize) {
    let creates = peer.delivered("Create");
    assert_eq!(creates.len(), want, "each try arrived at the mock");
    let pem = alice.public_key.as_deref().expect("the fixture key");
    for delivery in &creates {
        assert_eq!(delivery.body(), creates[0].body(), "the same body, resent");
        assert!(
            delivery.digest_matches() && delivery.signature_verifies(pem),
            "every attempt went out freshly signed over the same bytes"
        );
        assert_eq!(
            delivery.key_id().as_deref(),
            Some(format!("{}#main-key", alice.ap_id)).as_deref(),
            "the same key names every attempt"
        );
    }
}

/// Scenario: the closed address. Once the mock answers `410 Gone`, the
/// delivery is discarded — not retried, never re-claimed — the peer's own
/// word marks its host unreachable in the ledger the admin surface reads,
/// and every later post skips the address: the fan-out computes its
/// candidates, finds the host marked, and queues nothing.
#[tokio::test]
async fn a_gone_inbox_drops_the_address_and_later_posts_skip_it() {
    let db = common::test_db().await;
    let pool = db.pool().clone();
    let (app, state) = remote::app_and_state(pool.clone());
    let peer = remote::MockPeer::start().await;
    let (alice, alice_token) = local(&pool, "eo-gone-alice").await;
    peer_follows(&app, &state, &peer, BOB, &alice).await;

    let _fine = post(&app, &alice_token, json!({"status": "delivered fine"})).await;
    let _drained = common::run_all_jobs(&state).await;
    assert_eq!(
        peer.delivered("Create").len(),
        1,
        "one delivery lands first"
    );

    peer.script_inbox_permanently(StatusCode::GONE, None);
    let doomed = post(&app, &alice_token, json!({"status": "nowhere to go"})).await;
    let _drained_again = common::run_all_jobs(&state).await;
    let ended = assert_job_after(&pool, &create_id_of(&doomed), "discarded", 1, "410").await;
    assert!(
        ended.run_at <= chrono::Utc::now(),
        "a discard never reschedules"
    );

    make_retries_due(&pool).await;
    let _reclaim = common::run_all_jobs(&state).await;
    assert_eq!(
        peer.delivered("Create").len(),
        2,
        "the discarded job is never re-claimed: it tried once"
    );

    let status = common::TestRequest::get("/api/v1/pleroma/federation_status")
        .send(&app)
        .await;
    common::assert_answer(&status, StatusCode::OK, "the reachability ledger reads");
    assert!(
        status.json_value()["unreachable"]
            .get("127.0.0.1")
            .is_some(),
        "the 410 marked the peer's host unreachable: {}",
        status.text()
    );

    let _skipped = post(&app, &alice_token, json!({"status": "never leaves"})).await;
    let _quiet = common::run_all_jobs(&state).await;
    assert_eq!(
        peer.delivered("Create").len(),
        2,
        "and later posts skip the dropped address — nothing new arrives"
    );
}

/// Scenario: the signature the peer cannot verify. The mock rotates the key
/// its actor documents publish and its inbox thereafter answers `401`, the
/// way a follower that can no longer check our signature does: the delivery
/// treats the first refusal as a rotation worth retrying, and the second is
/// a real refusal it gives up on. The stored mirror keeps the key it was
/// fetched with — the delivery path never refetches a follower — and every
/// attempt's capture shows the valid signature this instance sent it under.
#[tokio::test]
async fn a_refused_signature_retries_once_and_the_capture_keeps_it() {
    let db = common::test_db().await;
    let pool = db.pool().clone();
    let (app, state) = remote::app_and_state(pool.clone());
    let peer = remote::MockPeer::start().await;
    let (alice, alice_token) = local(&pool, "eo-rotate-alice").await;
    peer_follows(&app, &state, &peer, BOB, &alice).await;
    let mirror = get_by_ap_id(&pool, &peer.actor(BOB))
        .await
        .expect("the mirror reads")
        .expect("the follow mirrored the peer actor");
    let stored_pem = mirror.public_key.clone().expect("the mirror holds a key");

    peer.rotate_keys();
    assert_ne!(
        stored_pem,
        peer.served_public_key(),
        "the peer now publishes a different key than this instance holds"
    );
    peer.script_inbox_permanently(StatusCode::UNAUTHORIZED, None);

    let posted = post(&app, &alice_token, json!({"status": "who verifies me"})).await;
    let create_id = create_id_of(&posted);
    let _first_pass = common::run_all_jobs(&state).await;
    let retried = assert_job_after(&pool, &create_id, "retryable", 1, "401").await;
    assert_eq!(
        retried.attempts, 1,
        "the first 401 is given its refresh window"
    );

    make_retries_due(&pool).await;
    let _second_pass = common::run_all_jobs(&state).await;
    let dropped = assert_job_after(&pool, &create_id, "discarded", 2, "401").await;
    assert_eq!(dropped.max_attempts, 7, "the budget was never the issue");

    let unchanged = get_by_ap_id(&pool, &peer.actor(BOB))
        .await
        .expect("the mirror reads")
        .expect("the mirror stands");
    assert_eq!(
        unchanged.public_key.as_deref(),
        Some(stored_pem.as_str()),
        "the delivery path never refetched the follower's document"
    );
    assert_signed_resends(&peer, &alice, 2);
}
