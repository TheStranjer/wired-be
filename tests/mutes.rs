//! Integration tests for the mutes endpoints (the mutes ticket):
//! `POST /api/v1/accounts/{id}/mute` stores the pair with Mastodon's two
//! parameters and answers the caller's relationship with `muting: true` and
//! `muting_notifications` as the request left them; `…/unmute` lifts the mute
//! idempotently; muting yourself is the 422 the ticket pins and any other
//! account miss the shared 404; a mute whose `duration` has run out is absent
//! from the flags and from the listing before its row is even swept; re-muting
//! a pair rewrites its options and keeps its row. `GET /api/v1/mutes` answers
//! the caller's muted accounts as `Account` entities, newest mute first,
//! keyset-paginated over the *mute row* id with Pleroma's `Link` header and
//! Mastodon's 80-row cap, each entry embedding the caller's relationship and
//! carrying the extra top-level `mute_expires_at` key — a Mastodon time, or
//! `null` for an indefinite mute. The `mutes.expire` job deletes the lapsed
//! rows the reads already ignore. Every route half of the auth and scope
//! pipelines is pinned: the listing with `read:mutes`, the two writes with
//! `write:mutes | follow`.

#![cfg(test)]

// `pub` because Clippy's nursery `redundant_pub_crate` forbids `pub(crate)`
// items inside a private module, while `unreachable_pub` forbids `pub` ones:
// only a `pub` module makes the harness helpers lint-clean.
pub mod common;

use axum::{Router, http::StatusCode};
use chrono::{DateTime, Utc};
use serde_json::{Value, json};
use sqlx::PgPool;
use uuid::Uuid;
use wired_be::{
    jobs::{AttemptInfo, mutes::handle_expire},
    models::{
        mute::{self, MuteOptions},
        user::User,
    },
    util::time::mastodon_time,
};

/// The instance origin the fixtures live on (`Config::for_tests`).
const BASE: &str = "https://test.example";
/// The muted-listing path, as the `Link` header links back to it.
const MUTES: &str = "/api/v1/mutes";
/// The 422 body `POST …/mute` answers for the caller's own id: the wording
/// this endpoint's ticket pins.
const SELF_MUTE: &str = r#"{"error":"Can not mute yourself"}"#;
/// The 401 body `AuthUser` renders when no token was presented at all.
const NO_TOKEN: &str = r#"{"error":"Invalid credentials"}"#;
/// The 401 body for a token that does not resolve.
const INVALID_TOKEN: &str = r#"{"error":"The access token is invalid"}"#;
/// The 403 body an app-only token gets from the user-requiring extractor.
const USER_REQUIRED: &str = r#"{"error":"This API requires an authenticated user"}"#;
/// The 403 body of a write whose token grants neither accepted scope;
/// `require_any` joins the alternatives with the `|` operator.
const NO_WRITE_SCOPE: &str = r#"{"error":"Insufficient permissions: write:mutes | follow."}"#;
/// The 403 body of a listing whose token lacks `read:mutes`.
const NO_READ_SCOPE: &str = r#"{"error":"Insufficient permissions: read:mutes."}"#;
/// The 404 body every account route shares.
const NOT_FOUND: &str = r#"{"error":"Record not found"}"#;
/// The 400 body a malformed page cursor gets from the extractor.
const INVALID_ID: &str = r#"{"error":"Invalid id"}"#;
/// The prefix of the 422 body a parameter of the wrong shape gets from the
/// extractor (decision 10), whatever the detail serde added.
const INVALID_PARAMS: &str = r#"{"error":"Invalid parameters: "#;
/// The two write actions, so the auth and scope loops drive both.
const ACTIONS: [&str; 2] = ["mute", "unmute"];
/// The attempt info a test hands the sweep handler: the first pass of a job
/// with an ordinary budget, which is all [`handle_expire`] looks at.
const FIRST_ATTEMPT: AttemptInfo = AttemptInfo {
    attempts: 1,
    max_attempts: 5,
};

/// One fixture mute: the `mutes` row id the listing keys on, next to the muted
/// account's id as its entity shows it and the expiry as `mute_expires_at`
/// renders it (`None` for an indefinite mute).
type Listed = (Uuid, String, Option<String>);

/// The relationship JSON for `target` with nothing recorded about them: every
/// flag at the documented default — `showing_reblogs` true, `note` empty, the
/// rest false.
fn relationship_default(target: &Uuid) -> Value {
    json!({
        "id": target.to_string(),
        "following": false,
        "requested": false,
        "endorsed": false,
        "followed_by": false,
        "blocking": false,
        "blocked_by": false,
        "muting": false,
        "muting_notifications": false,
        "notifying": false,
        "showing_reblogs": true,
        "domain_blocking": false,
        "note": "",
        "subscribing": false,
    })
}

/// [`relationship_default`] with the flags in `computed` overwritten: the
/// tests name exactly what the write changed.
fn relationship(target: &Uuid, computed: &Value) -> Value {
    let mut value = relationship_default(target);
    let base = value.as_object_mut().expect("the default is an object");
    let overrides = computed.as_object().expect("the flags are a JSON object");
    for (key, flag) in overrides {
        let replaced = base.insert(key.clone(), flag.clone());
        assert!(
            replaced.is_some(),
            "`{key}` overrides a documented default, not a typo"
        );
    }
    value
}

/// A request for one of the ticket's three routes: `GET` the listing, `POST`
/// either action path.
fn request(path: &str) -> common::TestRequest {
    if path == MUTES {
        common::TestRequest::get(path)
    } else {
        common::TestRequest::post(path)
    }
}

/// A local user named `nickname` plus a bearer token granting `scopes`.
async fn fixture(pool: &PgPool, nickname: &str, scopes: &str) -> (User, String) {
    let created = common::create_local_user(pool, nickname).await;
    let token = common::create_app_and_token(pool, &created, scopes).await;
    (created, token)
}

/// A `GET` of `path` with `token` (`None` anonymous).
async fn get(app: &Router, token: Option<&str>, path: &str) -> common::TestResponse {
    match token {
        Some(found) => common::TestRequest::get(path).bearer(found).send(app).await,
        None => common::TestRequest::get(path).send(app).await,
    }
}

/// The mute action route of account `target`, `mute` or `unmute`.
fn action_path(target: &Uuid, action: &str) -> String {
    format!("/api/v1/accounts/{target}/{action}")
}

/// The same route with `id` as the raw path segment, so a test can feed
/// something that is not an id.
fn raw_action_path(id: &str, action: &str) -> String {
    format!("/api/v1/accounts/{id}/{action}")
}

/// Mutes `target` as `token` with no parameters and answers the relationship
/// the ticket pins for it — `muting` and `muting_notifications` both true: the
/// fixture every later test starts from.
async fn mute_account(app: &Router, token: &str, target: &Uuid) {
    let response = common::TestRequest::post(&action_path(target, "mute"))
        .bearer(token)
        .send(app)
        .await;
    assert_eq!(response.status, StatusCode::OK, "the mute is written");
    assert_eq!(
        response.json_value(),
        relationship(
            target,
            &json!({ "muting": true, "muting_notifications": true })
        ),
        "the answer states the stored fact with both of its flags"
    );
}

/// Mutes `target` for `muter` straight through the model, with `options`: the
/// fixture a test needs in a state the endpoints cannot write in one call (or
/// needs before it checks what a call changes).
async fn seed_mute(pool: &PgPool, muter: &User, target: &User, options: MuteOptions) {
    let row = mute::mute(pool, muter, target, options)
        .await
        .expect("the fixture mute stores");
    assert_eq!(row.muted_id, target.id, "the row is the pair asked for");
}

/// Moves the pair's `expires_at` to `seconds` from now (negative for the past)
/// in one statement: the state a timed mute reaches when its duration runs out,
/// which no endpoint writes and every read has to honour.
async fn set_expiry(pool: &PgPool, muter: &User, target: &User, seconds: i64) {
    let sql = "UPDATE mutes SET expires_at = now() + make_interval(secs => $3)
                WHERE muter_id = $1 AND muted_id = $2";
    let moved = sqlx::query(sql)
        .bind(muter.id)
        .bind(target.id)
        .bind(seconds)
        .execute(pool)
        .await
        .expect("the expiry moves");
    assert_eq!(moved.rows_affected(), 1, "the pair's row was there to move");
}

/// The `id` of every entity of a listing response, in response order.
fn listed_ids(body: &Value) -> Vec<String> {
    body.as_array()
        .expect("the listing is an array")
        .iter()
        .map(|account| account["id"].as_str().expect("an account id").to_owned())
        .collect()
}

/// The muted ids of `stored`, in order.
fn muted_ids(stored: &[Listed]) -> Vec<String> {
    stored.iter().map(|(_, muted, _)| muted.clone()).collect()
}

/// One `mutes` row as the fixture reads it back: the row's own id, the muted
/// account's id, and the expiry as the database stored it.
type MuteRow = (Uuid, Uuid, Option<DateTime<Utc>>);

/// The stored rows of `muter`'s mutes, newest first — read from storage so
/// same-millisecond `UUIDv7`s cannot make an expectation depend on insert
/// timing (decision 4) — with the expiry rendered in the same format the
/// listing sends it in.
async fn stored_mutes(pool: &PgPool, muter: &User) -> Vec<Listed> {
    let sql = "SELECT id, muted_id, expires_at FROM mutes WHERE muter_id = $1 ORDER BY id DESC";
    let rows: Vec<MuteRow> = sqlx::query_as::<_, MuteRow>(sql)
        .bind(muter.id)
        .fetch_all(pool)
        .await
        .expect("the stored rows read");
    rows.into_iter()
        .map(|(id, muted, expires_at)| {
            (
                id,
                muted.to_string(),
                expires_at.map(|at| mastodon_time(&at)),
            )
        })
        .collect()
}

/// The number of rows in the table.
async fn mute_count(pool: &PgPool) -> i64 {
    let row: (i64,) = sqlx::query_as("SELECT count(*) FROM mutes")
        .fetch_one(pool)
        .await
        .unwrap();
    row.0
}

/// The `Link` header Pleroma's `add_link_headers` writes for a page of the
/// muted listing: `next` walks to the oldest mute row, `prev` back to the
/// newest.
fn expected_link(oldest: &Uuid, newest: &Uuid) -> String {
    format!(
        "<{BASE}{MUTES}?max_id={oldest}>; rel=\"next\", \
         <{BASE}{MUTES}?min_id={newest}>; rel=\"prev\""
    )
}

/// The relationship `token`'s account holds to `target`, as the batch endpoint
/// answers it — how a test pins the flags from the other end of a pair.
async fn relationship_of(app: &Router, token: &str, target: &Uuid) -> Value {
    let response = get(
        app,
        Some(token),
        &format!("/api/v1/accounts/relationships?id%5B%5D={target}"),
    )
    .await;
    assert_eq!(response.status, StatusCode::OK, "the batch answers");
    assert_eq!(
        response.json_value().as_array().expect("an array").len(),
        1,
        "one relationship for the one requested id"
    );
    response.json_value()[0].clone()
}

/// Muting answers the caller's relationship with `muting` and
/// `muting_notifications` both true (Mastodon's default for the
/// `notifications` parameter), stores the pair as one indefinite row with
/// `hide_notifications` set, and says nothing about the other end of the pair.
#[sqlx::test(migrator = "wired_be::MIGRATOR")]
async fn muting_lights_both_flags_from_the_caller_s_end(pool: PgPool) {
    let (alice, token) = fixture(&pool, "alice", "read write").await;
    let (bob, bob_token) = fixture(&pool, "bob", "read").await;
    let app = common::test_app(pool.clone());

    let response = common::TestRequest::post(&action_path(&bob.id, "mute"))
        .bearer(&token)
        .send(&app)
        .await;
    assert_eq!(response.status, StatusCode::OK, "the mute is written");
    assert_eq!(
        response.header("content-type"),
        Some("application/json"),
        "and JSON"
    );
    assert_eq!(
        response.json_value(),
        relationship(
            &bob.id,
            &json!({ "muting": true, "muting_notifications": true })
        ),
        "both mutes flags ride the answer"
    );

    let stored = stored_mutes(&pool, &alice).await;
    assert_eq!(stored.len(), 1, "one row for the pair");
    assert_eq!(stored[0].1, bob.id.to_string(), "and it names bob");
    assert_eq!(
        stored[0].2, None,
        "with no duration asked for, the mute is indefinite"
    );
    assert!(
        mute::is_muting(&pool, alice.id, bob.id).await.unwrap(),
        "the model agrees"
    );
    assert_eq!(
        relationship_of(&app, &bob_token, &alice.id).await,
        relationship(&alice.id, &json!({})),
        "being muted lights nothing on bob's side of the pair"
    );
}

/// `notifications=false` mutes the timeline only: `muting` stays true,
/// `muting_notifications` is false, and the row stores the flag as the caller
/// set it. The parameter arrives in a JSON body, a form body and the query
/// string alike (decision 10).
#[sqlx::test(migrator = "wired_be::MIGRATOR")]
async fn the_notifications_parameter_says_which_flags_light(pool: PgPool) {
    let (alice, token) = fixture(&pool, "alice", "write:mutes").await;
    let app = common::test_app(pool.clone());

    for (n, nickname) in ["in_json", "in_form", "in_query"].iter().enumerate() {
        let muted = common::create_local_user(&pool, nickname).await;
        let path = action_path(&muted.id, "mute");
        let request = match n {
            0 => common::TestRequest::post(&path).json(&json!({ "notifications": false })),
            1 => common::TestRequest::post(&path).form(&[("notifications", "false")]),
            _ => common::TestRequest::post(&format!("{path}?notifications=0")),
        };
        let response = request.bearer(&token).send(&app).await;
        assert_eq!(
            response.status,
            StatusCode::OK,
            "`{nickname}` spells a mute that works"
        );
        assert_eq!(
            response.json_value(),
            relationship(
                &muted.id,
                &json!({ "muting": true, "muting_notifications": false })
            ),
            "a timeline-only mute lights `muting` alone"
        );
    }

    assert_eq!(
        mute::muted_ids(&pool, alice.id).await.unwrap().len(),
        3,
        "three pairs, each spelled differently on the wire"
    );
    assert!(
        notification_ids(&pool, alice.id).await.is_empty(),
        "and none of them hides notifications"
    );
    assert_eq!(mute_count(&pool).await, 3, "and three rows");
}

/// The ids whose notifications `muter` hides: the shorter sibling of a
/// read-back assertion that says "nothing" without spelling out a `Vec<Uuid>`.
async fn notification_ids(pool: &PgPool, muter: Uuid) -> Vec<Uuid> {
    mute::notification_muted_ids(pool, muter)
        .await
        .expect("the read works")
}

/// `duration` becomes an `expires_at` that many seconds ahead, and `duration=0`
/// is Mastodon's spelling of an indefinite mute.
#[sqlx::test(migrator = "wired_be::MIGRATOR")]
async fn the_duration_parameter_stores_an_expiry(pool: PgPool) {
    let (alice, token) = fixture(&pool, "alice", "follow").await;
    let timed = common::create_local_user(&pool, "timed").await;
    let forever = common::create_local_user(&pool, "forever").await;
    let app = common::test_app(pool.clone());

    let response = common::TestRequest::post(&action_path(&timed.id, "mute"))
        .bearer(&token)
        .json(&json!({ "duration": 3_600 }))
        .send(&app)
        .await;
    assert_eq!(response.status, StatusCode::OK, "the timed mute is written");
    assert_eq!(
        response.json_value(),
        relationship(
            &timed.id,
            &json!({ "muting": true, "muting_notifications": true })
        ),
        "and the answer carries both flags"
    );

    let expiry: Option<DateTime<Utc>> =
        sqlx::query_scalar("SELECT expires_at FROM mutes WHERE muted_id = $1")
            .bind(timed.id)
            .fetch_one(&pool)
            .await
            .expect("the expiry reads");
    let expiry = expiry.expect("a timed mute stores an expiry");
    let ahead = expiry.signed_duration_since(Utc::now()).num_seconds();
    assert!(
        (3_590..=3_600).contains(&ahead),
        "the row's expiry is the hour the caller asked for, got {ahead}s"
    );

    let zero = common::TestRequest::post(&action_path(&forever.id, "mute"))
        .bearer(&token)
        .form(&[("duration", "0")])
        .send(&app)
        .await;
    assert_eq!(zero.status, StatusCode::OK, "`duration=0` is written");
    assert_eq!(
        stored_mutes(&pool, &alice)
            .await
            .into_iter()
            .filter(|(_, muted, _)| *muted == forever.id.to_string())
            .map(|(_, _, expires_at)| expires_at)
            .collect::<Vec<Option<String>>>(),
        vec![None],
        "and `duration=0` means no expiry at all"
    );
}

/// A mute whose `expires_at` lies in the past is treated as absent: the flags
/// fall back to their defaults and the listing drops the entry, while the row
/// is still on the books for the sweep to take.
#[sqlx::test(migrator = "wired_be::MIGRATOR")]
async fn a_lapsed_mute_is_absent_from_flags_and_listing(pool: PgPool) {
    let (alice, token) = fixture(&pool, "alice", "read write:mutes").await;
    let bob = common::create_local_user(&pool, "bob").await;
    let carol = common::create_local_user(&pool, "carol").await;
    seed_mute(
        &pool,
        &alice,
        &bob,
        MuteOptions {
            notifications: true,
            expires_in: Some(3_600),
        },
    )
    .await;
    seed_mute(&pool, &alice, &carol, MuteOptions::default()).await;
    let app = common::test_app(pool.clone());

    set_expiry(&pool, &alice, &bob, -60).await;

    assert_eq!(
        relationship_of(&app, &token, &bob.id).await,
        relationship(&bob.id, &json!({})),
        "the lapsed mute lights neither flag"
    );
    assert!(
        !mute::is_muting(&pool, alice.id, bob.id).await.unwrap(),
        "and the model treats it as lifted"
    );
    let listed = get(&app, Some(&token), MUTES).await;
    assert_eq!(
        listed_ids(&listed.json_value()),
        vec![carol.id.to_string()],
        "the listing holds only the live mute"
    );
    assert_eq!(mute_count(&pool).await, 2, "both rows are still stored");
}

/// Re-muting a pair is one row whose options the second write rewrote: the same
/// id (so the listing's cursor does not move), the new `notifications` flag and
/// a duration counted from the latest write.
#[sqlx::test(migrator = "wired_be::MIGRATOR")]
async fn remuting_replaces_the_options_of_the_same_row(pool: PgPool) {
    let (alice, token) = fixture(&pool, "alice", "read write").await;
    let bob = common::create_local_user(&pool, "bob").await;
    let app = common::test_app(pool.clone());

    let first = common::TestRequest::post(&action_path(&bob.id, "mute"))
        .bearer(&token)
        .json(&json!({ "notifications": false }))
        .send(&app)
        .await;
    assert_eq!(first.status, StatusCode::OK, "the first mute is written");
    let stored = stored_mutes(&pool, &alice).await;
    assert_eq!(stored.len(), 1, "one row");
    assert!(
        notification_ids(&pool, alice.id).await.is_empty(),
        "with notifications left visible"
    );

    let again = common::TestRequest::post(&action_path(&bob.id, "mute"))
        .bearer(&token)
        .form(&[("notifications", "true"), ("duration", "600")])
        .send(&app)
        .await;
    assert_eq!(again.status, StatusCode::OK, "the repeat mute is written");
    assert_eq!(
        again.json_value(),
        relationship(
            &bob.id,
            &json!({ "muting": true, "muting_notifications": true })
        ),
        "and now both flags light"
    );

    let after = stored_mutes(&pool, &alice).await;
    assert_eq!(after.len(), 1, "still one row for the pair");
    assert_eq!(after[0].0, stored[0].0, "and it kept its id");
    assert!(
        after[0].2.is_some(),
        "with the duration the second write asked for: {:?}",
        after[0].2
    );

    let cleared = common::TestRequest::post(&action_path(&bob.id, "mute"))
        .bearer(&token)
        .send(&app)
        .await;
    assert_eq!(
        cleared.status,
        StatusCode::OK,
        "muting with no parameters at all is a rewrite too"
    );
    assert_eq!(
        stored_mutes(&pool, &alice).await[0].2,
        None,
        "and the absent duration clears the stored expiry"
    );
    assert_eq!(mute_count(&pool).await, 1, "one row throughout");
}

/// Unmuting answers the relationship back at its defaults, is idempotent, and
/// takes the row with it; the pair's follows are nobody's business here.
#[sqlx::test(migrator = "wired_be::MIGRATOR")]
async fn unmuting_lifts_the_mute_without_asking_for_one(pool: PgPool) {
    let (_, token) = fixture(&pool, "alice", "read follow").await;
    let bob = common::create_local_user(&pool, "bob").await;
    let app = common::test_app(pool.clone());
    mute_account(&app, &token, &bob.id).await;

    let lifted = common::TestRequest::post(&action_path(&bob.id, "unmute"))
        .bearer(&token)
        .send(&app)
        .await;
    assert_eq!(lifted.status, StatusCode::OK, "the unmute answers");
    assert_eq!(
        lifted.json_value(),
        relationship(&bob.id, &json!({})),
        "every flag back at its default"
    );
    assert_eq!(mute_count(&pool).await, 0, "and the row is gone");

    let again = common::TestRequest::post(&action_path(&bob.id, "unmute"))
        .bearer(&token)
        .send(&app)
        .await;
    assert_eq!(again.status, StatusCode::OK, "lifting twice is no error");
    assert_eq!(
        again.json_value(),
        relationship(&bob.id, &json!({})),
        "and the same unchanged answer"
    );
    assert_eq!(mute_count(&pool).await, 0, "writes nothing");

    let listed = get(&app, Some(&token), MUTES).await;
    assert_eq!(listed.json_value(), json!([]), "and the listing is empty");
}

/// Muting yourself is the 422 the ticket pins, `unmute` is not a path to it,
/// and an unknown or malformed id is the shared 404 on both writes.
#[sqlx::test(migrator = "wired_be::MIGRATOR")]
async fn a_self_mute_is_422_and_any_other_miss_is_404(pool: PgPool) {
    let (alice, token) = fixture(&pool, "alice", "write:mutes").await;
    let app = common::test_app(pool.clone());

    let yourself = common::TestRequest::post(&action_path(&alice.id, "mute"))
        .bearer(&token)
        .send(&app)
        .await;
    assert_eq!(
        yourself.status,
        StatusCode::UNPROCESSABLE_ENTITY,
        "the caller's own id is the 422"
    );
    assert_eq!(yourself.text(), SELF_MUTE, "the ticket's wording");
    assert_eq!(
        stored_mutes(&pool, &alice).await.len(),
        0,
        "and the refusal stored nothing"
    );

    let unknown = Uuid::now_v7().to_string();
    for id in [unknown, "not-an-id".to_owned()] {
        for action in ACTIONS {
            let response = common::TestRequest::post(&raw_action_path(&id, action))
                .bearer(&token)
                .send(&app)
                .await;
            assert_eq!(
                response.status,
                StatusCode::NOT_FOUND,
                "a `{action}` of `{id}` is the shared 404"
            );
            assert_eq!(response.text(), NOT_FOUND, "the 404 body");
        }
    }
}

/// A parameter that is not the value the endpoint asks for is the extractor's
/// 422 (decision 10), on either of the two optional keys.
#[sqlx::test(migrator = "wired_be::MIGRATOR")]
async fn a_malformed_parameter_is_422(pool: PgPool) {
    let alice = common::create_local_user(&pool, "alice").await;
    let bob = common::create_local_user(&pool, "bob").await;
    let token = common::create_app_and_token(&pool, &alice, "write:mutes").await;
    let app = common::test_app(pool.clone());

    for (key, value) in [("notifications", "maybe"), ("duration", "soon")] {
        let response = common::TestRequest::post(&action_path(&bob.id, "mute"))
            .bearer(&token)
            .form(&[(key, value)])
            .send(&app)
            .await;
        assert_eq!(
            response.status,
            StatusCode::UNPROCESSABLE_ENTITY,
            "`{key}={value}` is refused"
        );
        assert!(
            response.text().starts_with(INVALID_PARAMS),
            "the extractor names what did not parse: {}",
            response.text()
        );
        assert_eq!(mute_count(&pool).await, 0, "and nothing was stored");
    }
}

/// `GET /api/v1/mutes` answers the caller's muted accounts as `Account`
/// entities, newest mute first, each embedding the caller's relationship and
/// carrying `mute_expires_at` — the mute's expiry as a Mastodon time, `null`
/// for an indefinite one — under a `Link` header keyed on the mute row ids; an
/// account with nothing muted gets the empty array and no header, and another
/// account's mutes are nobody else's list.
#[sqlx::test(migrator = "wired_be::MIGRATOR")]
async fn the_listing_answers_accounts_with_their_expiry(pool: PgPool) {
    let (alice, token) = fixture(&pool, "alice", "read write").await;
    let bob = common::create_local_user(&pool, "bob").await;
    let carol = common::create_local_user(&pool, "carol").await;
    let app = common::test_app(pool.clone());

    let empty = get(&app, Some(&token), MUTES).await;
    assert_eq!(empty.status, StatusCode::OK, "an empty listing is a 200");
    assert_eq!(empty.json_value(), json!([]), "the empty array");
    assert_eq!(empty.header("link"), None, "with nothing to link from");

    mute_account(&app, &token, &bob.id).await;
    let response = common::TestRequest::post(&action_path(&carol.id, "mute"))
        .bearer(&token)
        .json(&json!({ "duration": 3_600, "notifications": false }))
        .send(&app)
        .await;
    assert_eq!(response.status, StatusCode::OK, "the timed mute is written");

    let stored = stored_mutes(&pool, &alice).await;
    assert_eq!(stored.len(), 2, "two fixture mutes");
    assert_eq!(
        stored[0].1,
        carol.id.to_string(),
        "the timed mute is the newer row"
    );

    let listed = get(&app, Some(&token), MUTES).await;
    assert_eq!(
        listed_ids(&listed.json_value()),
        muted_ids(&stored),
        "the muted accounts, newest mute first"
    );
    assert_eq!(
        listed.header("link"),
        Some(expected_link(&stored[1].0, &stored[0].0).as_str()),
        "the Link cursors are the mute row ids, not the accounts"
    );

    let timed = &listed.json_value()[0];
    assert_eq!(
        timed["id"],
        json!(stored[0].1),
        "an entry is a rendered `Account`, keyed on the muted account's id"
    );
    assert!(
        timed["username"].is_string(),
        "with the account's own fields beside it"
    );
    assert_eq!(
        timed["mute_expires_at"],
        json!(
            stored[0]
                .2
                .clone()
                .expect("the timed fixture has an expiry")
        ),
        "and the extra key carries the mute's expiry"
    );
    assert_eq!(
        timed["pleroma"]["relationship"]["muting"],
        json!(true),
        "each entry embeds the caller's relationship"
    );
    assert_eq!(
        timed["pleroma"]["relationship"]["muting_notifications"],
        json!(false),
        "including the option the write set"
    );

    let indefinite = &listed.json_value()[1];
    assert!(
        indefinite
            .as_object()
            .expect("an object")
            .contains_key("mute_expires_at"),
        "the key is on every entry of this listing"
    );
    assert_eq!(
        indefinite["mute_expires_at"],
        Value::Null,
        "and an indefinite mute sends it as null"
    );

    let (_, erin_token) = fixture(&pool, "erin", "read").await;
    let theirs = get(&app, Some(&erin_token), MUTES).await;
    assert_eq!(
        theirs.json_value(),
        json!([]),
        "another account's listing holds none of alice's mutes"
    );
}

/// The listing's page walk: `limit` at a time, `max_id` continuing where a page
/// ended, `min_id` answered newest-first again, the ends of the walk carrying no
/// header, and a malformed cursor the extractor's 400.
#[sqlx::test(migrator = "wired_be::MIGRATOR")]
async fn the_listing_pages_by_mute_row_id(pool: PgPool) {
    let (alice, token) = fixture(&pool, "alice", "read write:mutes").await;
    let app = common::test_app(pool.clone());
    for nickname in ["bob", "carol", "dave", "erin"] {
        let muted = common::create_local_user(&pool, nickname).await;
        mute_account(&app, &token, &muted.id).await;
    }
    let stored = stored_mutes(&pool, &alice).await;
    assert_eq!(stored.len(), 4, "four fixture mutes");

    let first = get(&app, Some(&token), &format!("{MUTES}?limit=2")).await;
    assert_eq!(
        listed_ids(&first.json_value()),
        vec![stored[0].1.clone(), stored[1].1.clone()],
        "two newest first"
    );
    assert_eq!(
        first.header("link"),
        Some(expected_link(&stored[1].0, &stored[0].0).as_str()),
        "the cursors are the page's own ends"
    );

    let older = get(
        &app,
        Some(&token),
        &format!("{MUTES}?limit=2&max_id={}", stored[1].0),
    )
    .await;
    assert_eq!(
        listed_ids(&older.json_value()),
        vec![stored[2].1.clone(), stored[3].1.clone()],
        "max_id continues where the page ended"
    );

    let newer = get(
        &app,
        Some(&token),
        &format!("{MUTES}?min_id={}", stored[2].0),
    )
    .await;
    assert_eq!(
        listed_ids(&newer.json_value()),
        vec![stored[0].1.clone(), stored[1].1.clone()],
        "min_id answers the newest-first page just after the cursor"
    );

    let polled = get(
        &app,
        Some(&token),
        &format!("{MUTES}?since_id={}", stored[0].0),
    )
    .await;
    assert_eq!(
        polled.json_value(),
        json!([]),
        "nothing is newer than the head"
    );
    assert_eq!(
        polled.header("link"),
        None,
        "and an empty page links nowhere"
    );

    let past = get(
        &app,
        Some(&token),
        &format!("{MUTES}?max_id={}", stored[3].0),
    )
    .await;
    assert_eq!(past.status, StatusCode::OK, "walking off the end is a 200");
    assert_eq!(past.json_value(), json!([]), "with the empty array");
    assert_eq!(past.header("link"), None, "and no header");

    let malformed = get(&app, Some(&token), &format!("{MUTES}?max_id=nope")).await;
    assert_eq!(
        malformed.status,
        StatusCode::BAD_REQUEST,
        "a bad cursor is Pleroma's 400"
    );
    assert_eq!(malformed.text(), INVALID_ID);
}

/// The listing's cap is Mastodon's 80, not the global 40: a page of 41 mutes
/// comes back whole, while Pleroma's 20 stays the default when no `limit` is
/// asked for.
#[sqlx::test(migrator = "wired_be::MIGRATOR")]
async fn the_listing_pages_up_to_eighty_rows(pool: PgPool) {
    let alice = common::create_local_user(&pool, "alice").await;
    for n in 0..41 {
        let muted = common::create_local_user(&pool, &format!("muted{n}")).await;
        seed_mute(&pool, &alice, &muted, MuteOptions::default()).await;
    }
    let stored = stored_mutes(&pool, &alice).await;
    assert_eq!(stored.len(), 41, "and they are all on the books");
    let app = common::test_app(pool.clone());
    let token = common::create_app_and_token(&pool, &alice, "read:mutes").await;

    let default_page = get(&app, Some(&token), MUTES).await;
    assert_eq!(
        listed_ids(&default_page.json_value()).len(),
        20,
        "Pleroma's default page of twenty"
    );

    let big = get(&app, Some(&token), &format!("{MUTES}?limit=41")).await;
    assert_eq!(
        listed_ids(&big.json_value()).len(),
        41,
        "a page past the global 40 cap gets through: the listing's cap is 80"
    );
    assert_eq!(
        big.header("link"),
        Some(expected_link(&stored[40].0, &stored[0].0).as_str()),
        "and the whole page is one page, linked at both ends"
    );
}

/// All three routes demand a user token: no token is the 401, an invalid one
/// the 401, and an app-only credential the 403.
#[sqlx::test(migrator = "wired_be::MIGRATOR")]
async fn the_routes_require_a_user_token(pool: PgPool) {
    let bob = common::create_local_user(&pool, "bob").await;
    let app_only = common::create_app_only_token(&pool, "read write follow").await;
    let app = common::test_app(pool);

    for path in [
        MUTES.to_owned(),
        action_path(&bob.id, "mute"),
        action_path(&bob.id, "unmute"),
    ] {
        let anonymous = request(&path).send(&app).await;
        assert_eq!(
            anonymous.status,
            StatusCode::UNAUTHORIZED,
            "no token cannot use `{path}`"
        );
        assert_eq!(anonymous.text(), NO_TOKEN, "AuthUser's no-token body");

        let invalid = request(&path).bearer("bogus-token").send(&app).await;
        assert_eq!(
            invalid.status,
            StatusCode::UNAUTHORIZED,
            "an invalid token is not treated as anonymous (`{path}`)"
        );
        assert_eq!(invalid.text(), INVALID_TOKEN, "AuthUser's invalid body");

        let machine = request(&path).bearer(&app_only).send(&app).await;
        assert_eq!(
            machine.status,
            StatusCode::FORBIDDEN,
            "an app-only token is not a user (`{path}`)"
        );
        assert_eq!(machine.text(), USER_REQUIRED, "the user-required body");
    }
}

/// The listing's scope gate is exactly `read:mutes`: broad `read` and the scope
/// itself list; `follow` (the writes' alternative, not this read side), `write`
/// and an unrelated sub-scope answer the insufficient-permissions 403.
#[sqlx::test(migrator = "wired_be::MIGRATOR")]
async fn the_listing_requires_the_read_mutes_scope(pool: PgPool) {
    let app = common::test_app(pool.clone());
    let alice = common::create_local_user(&pool, "alice").await;

    for scopes in ["read", "read:mutes"] {
        let token = common::create_app_and_token(&pool, &alice, scopes).await;
        let response = get(&app, Some(&token), MUTES).await;
        assert_eq!(
            response.status,
            StatusCode::OK,
            "a `{scopes}` token can list"
        );
    }

    for scopes in ["follow", "write", "read:accounts", ""] {
        let token = common::create_app_and_token(&pool, &alice, scopes).await;
        let response = get(&app, Some(&token), MUTES).await;
        assert_eq!(
            response.status,
            StatusCode::FORBIDDEN,
            "a `{scopes}` token cannot list"
        );
        assert_eq!(response.text(), NO_READ_SCOPE, "the denial names the scope");
    }
}

/// Both writes' gate is `write:mutes | follow`: broad `write`, the scope itself
/// and the `follow` root all mute and unmute; anything else is the
/// alternatives-joined 403, on both actions.
#[sqlx::test(migrator = "wired_be::MIGRATOR")]
async fn the_writes_take_write_mutes_or_follow(pool: PgPool) {
    let app = common::test_app(pool.clone());
    let alice = common::create_local_user(&pool, "alice").await;
    let bob = common::create_local_user(&pool, "bob").await;

    for (n, scopes) in ["write", "write:mutes", "follow"].iter().enumerate() {
        let token = common::create_app_and_token(&pool, &alice, scopes).await;
        let target = common::create_local_user(&pool, &format!("granted{n}")).await;
        let response = common::TestRequest::post(&action_path(&target.id, "mute"))
            .bearer(&token)
            .send(&app)
            .await;
        assert_eq!(
            response.status,
            StatusCode::OK,
            "a `{scopes}` token can mute"
        );
        assert_eq!(
            response.json_value(),
            relationship(
                &target.id,
                &json!({ "muting": true, "muting_notifications": true })
            ),
            "and the answer states it"
        );
        let lifted = common::TestRequest::post(&action_path(&target.id, "unmute"))
            .bearer(&token)
            .send(&app)
            .await;
        assert_eq!(
            lifted.status,
            StatusCode::OK,
            "a `{scopes}` token can unmute"
        );
    }

    for scopes in ["read", "read:mutes", "write:accounts"] {
        let token = common::create_app_and_token(&pool, &alice, scopes).await;
        for action in ACTIONS {
            let response = common::TestRequest::post(&action_path(&bob.id, action))
                .bearer(&token)
                .send(&app)
                .await;
            assert_eq!(
                response.status,
                StatusCode::FORBIDDEN,
                "a `{scopes}` token cannot {action}"
            );
            assert_eq!(response.text(), NO_WRITE_SCOPE, "the denial names both");
        }
    }

    assert!(
        stored_mutes(&pool, &alice).await.is_empty(),
        "the granted round trips left nothing behind"
    );
}

/// The `mutes.expire` job's pass deletes the lapsed rows the reads already
/// ignore and leaves every live mute alone, so the table cannot grow and the
/// listing does not change.
#[sqlx::test(migrator = "wired_be::MIGRATOR")]
async fn the_expire_job_removes_the_lapsed_rows(pool: PgPool) {
    let (alice, token) = fixture(&pool, "alice", "read:mutes write:mutes").await;
    let lapsed = common::create_local_user(&pool, "lapsed").await;
    let running = common::create_local_user(&pool, "running").await;
    let forever = common::create_local_user(&pool, "forever").await;
    seed_mute(
        &pool,
        &alice,
        &lapsed,
        MuteOptions {
            notifications: true,
            expires_in: Some(60),
        },
    )
    .await;
    seed_mute(
        &pool,
        &alice,
        &running,
        MuteOptions {
            notifications: true,
            expires_in: Some(3_600),
        },
    )
    .await;
    seed_mute(&pool, &alice, &forever, MuteOptions::default()).await;
    set_expiry(&pool, &alice, &lapsed, -1).await;

    let state = common::test_app_state(pool.clone(), common::test_config());
    let outcome = handle_expire(state, FIRST_ATTEMPT, json!({})).await;
    assert_eq!(outcome, Ok(()), "the sweep reports success");

    let survivors = stored_mutes(&pool, &alice)
        .await
        .into_iter()
        .map(|(_, muted, _)| muted)
        .collect::<Vec<String>>();
    assert!(
        !survivors.contains(&lapsed.id.to_string()),
        "the lapsed mute's row is gone"
    );
    assert_eq!(survivors.len(), 2, "the two live mutes stayed");

    let app = common::test_app(pool);
    let listed = get(&app, Some(&token), MUTES).await;
    assert_eq!(
        listed_ids(&listed.json_value()).len(),
        2,
        "the listing is what it was before the sweep, too"
    );
}
