//! The three inbox `POST`s over HTTP: a valid signed delivery to each
//! route answers 202 with an empty body and queues exactly one
//! `federator.incoming` job per activity id, redeliveries included; an
//! unknown, remote or deactivated owner is the 404, an activity without
//! the required fields the 400, an unsigned delivery the extractor's 401,
//! a `FEDERATION_ENABLED`-off instance the 403 on every route, and an
//! `mrf` reject list (or a missing `accept` entry) refuses the signer's
//! host with its own 403.

use axum::{Router, http::StatusCode};
use chrono::Utc;
use serde_json::{Value, json};
use sqlx::PgPool;
use uuid::Uuid;
use wired_be::{
    config::Toggle,
    federation::{
        dispatch::KIND_INCOMING,
        signature::{SigningKey, sign_request},
    },
    jobs::QUEUE_FEDERATOR_INCOMING,
    models::{
        job::Job,
        user::{Flag, NewUser, User},
    },
    services::{instance_actor::ensure_instance_actor, runtime_config},
};

use super::support::{BASE, HOST};
use crate::common;

/// The signer of the inbox tests' deliveries: the instance actor, a stored
/// local row carrying the key pair [`ensure_instance_actor`] generated, so
/// the `SignedActivity` extractor resolves the `keyId` from the database
/// and no mock peer is needed. Every delivered activity names its `ap_id`
/// as `actor` — the containment the extractor insists on.
#[derive(Debug)]
struct Signer {
    /// `users.id` of the signing actor.
    id: Uuid,
    /// Its actor id, the `actor` every test activity carries.
    ap_id: String,
    /// The `keyId`: actor id plus Mastodon's key fragment.
    key_id: String,
    /// The private key the deliveries are signed with.
    private_pem: String,
}

/// Provisions the instance actor and hands it over as a delivery signer.
async fn instance_signer(pool: &PgPool) -> Signer {
    let config = common::test_config();
    let actor = ensure_instance_actor(pool, &config)
        .await
        .expect("the instance actor is provisioned");
    Signer {
        id: actor.id,
        key_id: format!("{}#main-key", actor.ap_id),
        ap_id: actor.ap_id,
        private_pem: actor
            .private_key
            .expect("the provisioned actor holds its private key"),
    }
}

/// The `Follow` of alice carrying activity id `id`.
fn follow(signer: &Signer, id: &str) -> Value {
    json!({
        "@context": "https://www.w3.org/ns/activitystreams",
        "id": id,
        "type": "Follow",
        "actor": signer.ap_id,
        "object": format!("{BASE}/users/alice"),
    })
}

/// `POST`s `activity` to `path` the way a peer delivers: the exact bytes
/// serialized here are digested, signed and sent, and the `host`, `date`,
/// `digest` and `signature` headers are what [`sign_request`] produced for
/// that path as request target.
async fn deliver(
    app: &Router,
    signer: &Signer,
    path: &str,
    activity: &Value,
) -> common::TestResponse {
    let wire = sign_request(
        "POST",
        &format!("{BASE}{path}"),
        Some(&serde_json::to_vec(activity).expect("the test activity serializes")),
        &SigningKey {
            key_id: &signer.key_id,
            private_pem: &signer.private_pem,
        },
        Utc::now(),
    )
    .expect("signing the test delivery");
    common::TestRequest::post(path)
        .json(activity)
        .header("host", &wire.host)
        .header("date", &wire.date)
        .header("digest", wire.digest.as_deref().expect("a body digest"))
        .header("signature", &wire.signature)
        .send(app)
        .await
}

/// Every [`KIND_INCOMING`] job the database holds, oldest first.
async fn incoming_jobs(pool: &PgPool) -> Vec<Job> {
    sqlx::query_as::<_, Job>("SELECT * FROM jobs WHERE kind = $1 ORDER BY id")
        .bind(KIND_INCOMING)
        .fetch_all(pool)
        .await
        .expect("the jobs table reads back")
}

/// The three inbox routes with the `inbox_owner` each queues: the local
/// account's id for the personal inbox, null for the shared ones.
fn inbox_routes(alice: &User) -> [(&'static str, Option<Uuid>); 3] {
    [
        ("/inbox", None),
        ("/users/alice/inbox", Some(alice.id)),
        ("/internal/fetch/inbox", None),
    ]
}

/// A valid signed delivery to each of the three inbox routes answers 202
/// with an empty body and queues exactly one `federator.incoming` job
/// carrying the activity, the signer and the route's owner.
#[tokio::test]
async fn each_inbox_route_queues_one_job_per_delivery() {
    let db = common::test_db().await;
    let pool = db.pool().clone();
    let alice = common::create_local_user(&pool, "alice").await;
    let signer = instance_signer(&pool).await;
    let app = common::test_app(pool.clone());

    let mut activities = Vec::new();
    for (n, (path, _owner)) in inbox_routes(&alice).into_iter().enumerate() {
        let activity = follow(&signer, &format!("https://peer.example/activities/{n}"));
        let response = deliver(&app, &signer, path, &activity).await;
        assert_eq!(
            response.status,
            StatusCode::ACCEPTED,
            "{path} accepts a signed delivery"
        );
        assert_eq!(response.text(), "", "{path} answers with an empty body");
        activities.push(activity);
    }

    let jobs = incoming_jobs(&pool).await;
    assert_eq!(jobs.len(), 3, "one job per route, no extras");
    for (n, (job, (_path, owner))) in jobs.iter().zip(inbox_routes(&alice)).enumerate() {
        assert_eq!(job.queue, QUEUE_FEDERATOR_INCOMING, "the incoming queue");
        assert_eq!(job.kind, KIND_INCOMING, "the incoming kind");
        assert_eq!(job.max_attempts, 5, "the ticket's attempt budget");
        assert_eq!(
            job.unique_key.as_deref(),
            Some(format!("incoming:https://peer.example/activities/{n}").as_str()),
            "the dedup key names the activity"
        );
        assert_eq!(
            job.args["signer_id"],
            signer.id.to_string(),
            "the authenticated signer is recorded"
        );
        assert_eq!(
            job.args["inbox_owner"],
            owner.map_or(Value::Null, |id| Value::from(id.to_string())),
            "the owner is the route's, or null for a shared inbox"
        );
        assert_eq!(
            job.args["activity"], activities[n],
            "the activity rides along"
        );
    }
}

/// The same activity delivered twice is queued once: both deliveries are
/// accepted, the dedup key leaves exactly one job.
#[tokio::test]
async fn a_redelivered_activity_is_queued_once() {
    let db = common::test_db().await;
    let pool = db.pool().clone();
    let _alice = common::create_local_user(&pool, "alice").await;
    let signer = instance_signer(&pool).await;
    let app = common::test_app(pool.clone());
    let activity = follow(&signer, "https://peer.example/activities/only-once");

    let first = deliver(&app, &signer, "/inbox", &activity).await;
    let second = deliver(&app, &signer, "/inbox", &activity).await;

    assert_eq!(first.status, StatusCode::ACCEPTED, "the first delivery");
    assert_eq!(
        second.status,
        StatusCode::ACCEPTED,
        "the redelivery is accepted too"
    );
    let jobs = incoming_jobs(&pool).await;
    assert_eq!(jobs.len(), 1, "but queued once: {jobs:?}");
    assert_eq!(
        jobs[0].unique_key.as_deref(),
        Some("incoming:https://peer.example/activities/only-once"),
        "the dedup key is the activity's id"
    );
}

/// A personal inbox exists only for a live local account: an unknown
/// nickname, a remote mirror's handle and a deactivated account's name all
/// answer the plain 404, and queue nothing.
#[tokio::test]
async fn an_inbox_of_a_non_local_or_gone_user_is_not_found() {
    let db = common::test_db().await;
    let pool = db.pool().clone();
    let bob = common::create_remote_user(&pool, "bob", "remote.example").await;
    let ex = common::create_local_user_with(&pool, "ex", |new| NewUser {
        is_active: Flag::new(false),
        ..new
    })
    .await;
    let signer = instance_signer(&pool).await;
    let app = common::test_app(pool.clone());

    for path in [
        "/users/nobody/inbox",
        &format!("/users/{}/inbox", bob.nickname),
        &format!("/users/{}/inbox", ex.nickname),
    ] {
        let response = deliver(
            &app,
            &signer,
            path,
            &follow(&signer, "https://peer.example/activities/x"),
        )
        .await;
        assert_eq!(
            response.status,
            StatusCode::NOT_FOUND,
            "{path} names no live local account"
        );
        assert_eq!(
            response.json_value(),
            json!({"error": "Not found"}),
            "{path} keeps the fallback body"
        );
    }
    assert!(
        incoming_jobs(&pool).await.is_empty(),
        "a refused owner queues no job"
    );
}

/// An authenticated activity must carry a string `id` and a string `type`
/// (the extractor already vouched for the `actor`): either one missing is
/// the 400 `{"error":"Invalid activity"}`, and nothing is queued.
#[tokio::test]
async fn an_activity_without_id_or_type_is_invalid() {
    let db = common::test_db().await;
    let pool = db.pool().clone();
    let _alice = common::create_local_user(&pool, "alice").await;
    let signer = instance_signer(&pool).await;
    let app = common::test_app(pool.clone());

    let no_type = json!({
        "@context": "https://www.w3.org/ns/activitystreams",
        "id": "https://peer.example/activities/no-type",
        "actor": signer.ap_id,
        "object": format!("{BASE}/users/alice"),
    });
    let no_id = json!({
        "@context": "https://www.w3.org/ns/activitystreams",
        "type": "Create",
        "actor": signer.ap_id,
        "object": format!("{BASE}/users/alice"),
    });
    for (name, activity) in [("type", &no_type), ("id", &no_id)] {
        let response = deliver(&app, &signer, "/inbox", activity).await;
        assert_eq!(
            response.status,
            StatusCode::BAD_REQUEST,
            "an activity without a {name} is refused"
        );
        assert_eq!(
            response.json_value(),
            json!({"error": "Invalid activity"}),
            "the 400 body names the problem"
        );
    }
    assert!(
        incoming_jobs(&pool).await.is_empty(),
        "an invalid activity queues no job"
    );
}

/// An unsigned delivery never reaches a handler: the extractor answers 401
/// on all three routes, and nothing is queued.
#[tokio::test]
async fn an_unsigned_delivery_is_not_authenticated() {
    let db = common::test_db().await;
    let pool = db.pool().clone();
    let _alice = common::create_local_user(&pool, "alice").await;
    let signer = instance_signer(&pool).await;
    let app = common::test_app(pool.clone());
    let activity = follow(&signer, "https://peer.example/activities/unsigned");

    for path in ["/inbox", "/users/alice/inbox", "/internal/fetch/inbox"] {
        let response = common::TestRequest::post(path)
            .json(&activity)
            .send(&app)
            .await;
        assert_eq!(
            response.status,
            StatusCode::UNAUTHORIZED,
            "{path} demands a signature"
        );
        assert_eq!(
            response.json_value(),
            json!({"error": "Missing signature"}),
            "{path} says what is missing"
        );
    }
    assert!(
        incoming_jobs(&pool).await.is_empty(),
        "an unsigned delivery queues no job"
    );
}

/// With `FEDERATION_ENABLED=false` every inbox route answers 403
/// `{"error":"Federation is disabled"}` — unsigned deliveries included,
/// because the gate runs before authentication — and queues nothing.
#[tokio::test]
async fn a_disabled_federation_refuses_every_inbox_post() {
    let db = common::test_db().await;
    let pool = db.pool().clone();
    let _alice = common::create_local_user(&pool, "alice").await;
    let signer = instance_signer(&pool).await;
    let mut config = common::test_config();
    config.federation_enabled = Toggle::new(false);
    let app = common::test_app_with_config(pool.clone(), config);

    for path in ["/inbox", "/users/alice/inbox", "/internal/fetch/inbox"] {
        let signed_response = deliver(
            &app,
            &signer,
            path,
            &follow(&signer, "https://peer.example/activities/off"),
        )
        .await;
        assert_eq!(
            signed_response.status,
            StatusCode::FORBIDDEN,
            "{path} is shut while federation is disabled"
        );
        assert_eq!(
            signed_response.json_value(),
            json!({"error": "Federation is disabled"}),
            "{path} says why"
        );

        let unsigned_response = common::TestRequest::post(path)
            .json(&follow(
                &signer,
                "https://peer.example/activities/off-unsigned",
            ))
            .send(&app)
            .await;
        assert_eq!(
            unsigned_response.status,
            StatusCode::FORBIDDEN,
            "{path} refuses unsigned deliveries at the same gate"
        );
    }
    assert!(
        incoming_jobs(&pool).await.is_empty(),
        "a disabled instance queues no activity at all"
    );
}

/// The `mrf` domain lists gate the inbox the way the instance's own policy
/// table does: a delivery signed by an actor whose host `simple` rejects —
/// or one its `accept` allowlist leaves out — is refused with 403
/// `{"error":"Rejected"}` and queues nothing, while the same lists behind a
/// policy that is not enabled let it through.
#[tokio::test]
async fn a_delivery_from_an_mrf_rejected_host_is_refused() {
    let db = common::test_db().await;
    let pool = db.pool().clone();
    let _alice = common::create_local_user(&pool, "alice").await;
    let signer = instance_signer(&pool).await;

    for lists in [
        json!({"reject": [[HOST, "bots"]]}),
        json!({"accept": [["other.example", "the only peer"]]}),
    ] {
        set_mrf(&pool, &json!({"policies": ["simple"], "simple": lists})).await;
        let app = common::test_app(pool.clone());
        let response = deliver(
            &app,
            &signer,
            "/inbox",
            &follow(&signer, "https://peer.example/activities/blocked"),
        )
        .await;
        assert_eq!(
            response.status,
            StatusCode::FORBIDDEN,
            "the lists refuse the signer's host: {lists}"
        );
        assert_eq!(
            response.json_value(),
            json!({"error": "Rejected"}),
            "the same body the policy table gives"
        );
    }
    assert!(
        incoming_jobs(&pool).await.is_empty(),
        "a refused delivery queues no job"
    );

    set_mrf(
        &pool,
        &json!({"policies": ["noop"], "simple": {"reject": [HOST]}}),
    )
    .await;
    let app = common::test_app(pool.clone());
    let allowed = deliver(
        &app,
        &signer,
        "/inbox",
        &follow(&signer, "https://peer.example/activities/allowed"),
    )
    .await;
    assert_eq!(
        allowed.status,
        StatusCode::ACCEPTED,
        "lists behind a policy that is not enabled govern nothing"
    );
    assert_eq!(incoming_jobs(&pool).await.len(), 1, "and the job is queued");
}

/// Writes the `:mrf` config entry: the pipeline reads it (uncached: each
/// phase of the test builds a fresh state) as its policy chain.
async fn set_mrf(pool: &PgPool, setting: &Value) {
    runtime_config::write(pool, ":pleroma", ":mrf", setting)
        .await
        .expect("the mrf entry writes");
}

/// A signed `Create` whose `content` (and a key) carries U+0000 is accepted
/// with a 202, not a 500: the queued job holds the document with the NUL
/// removed, since `PostgreSQL` cannot store it.
#[tokio::test]
async fn a_nul_in_a_signed_activity_is_stripped_not_a_server_error() {
    let db = common::test_db().await;
    let pool = db.pool().clone();
    let _alice = common::create_local_user(&pool, "alice").await;
    let signer = instance_signer(&pool).await;
    let app = common::test_app(pool.clone());
    let activity = json!({
        "@context": "https://www.w3.org/ns/activitystreams",
        "id": "https://peer.example/activities/nul",
        "type": "Create",
        "actor": signer.ap_id,
        "object": {
            "id": "https://peer.example/notes/nul",
            "type": "Note",
            "content": "nul here \u{0} end",
            "summary\u{0}": "cw\u{0}",
        },
    });

    let response = deliver(&app, &signer, "/inbox", &activity).await;

    assert_eq!(response.status, StatusCode::ACCEPTED, "never a 500");
    let jobs = incoming_jobs(&pool).await;
    assert_eq!(jobs.len(), 1, "queued once");
    assert_eq!(
        job_object(&jobs[0]),
        &json!({
            "id": "https://peer.example/notes/nul",
            "type": "Note",
            "content": "nul here  end",
            "summary": "cw",
        }),
        "the stored object has no NUL"
    );
}

/// The `object` of the activity a job carries.
fn job_object(job: &Job) -> &Value {
    &job.args["activity"]["object"]
}
