//! The admin statuses surface over HTTP (`/api/v1/pleroma/admin/statuses…`,
//! WBE-201): Pleroma's `AdminAPI.StatusController` beside
//! `AdminAPIController.list_user_statuses` and
//! `AdminAPI.InstanceController.list_statuses`.
//!
//! What the tests pin, per the ticket:
//!
//! - `GET …/statuses` is the network ledger, newest post first, off the
//!   `public`/`unlisted` visibilities alone until `godmode` admits the
//!   private ones, narrowed by `local_only` and opened to boost rows by
//!   `with_reblogs`, windowed by `page`/`page_size` with the admin API's
//!   lenient defaulting and the shared 200-row cap — answered as a bare
//!   array of the STANDARD [`Status`] entities, the author's ordinary
//!   [`Account`] entity beside each and no admin extension;
//! - `GET …/statuses/{id}` shows any live post whatever its visibility, and
//!   a malformed, unknown or tombstoned id is the one 404
//!   `{"error":"Not found"}`;
//! - `PUT …/statuses/{id}` moves the two scope fields and nothing else — the
//!   entity answers, the row agrees, `edited_at` holds, the `status_update`
//!   moderation sentence names the parts, a `visibility` off the five is the
//!   422 `{"error":"Invalid visibility"}` with the row left as it was, and a
//!   local post's correction is the `Update` the queue holds while a
//!   mirrored or never-federated one queues nothing;
//! - `DELETE …/statuses/{id}` is the shared deletion service: `200 {}`, the
//!   tombstone, the `status_delete` entry, the federated `Delete`, and the
//!   404 for the second strike;
//! - `GET …/users/{nickname}/statuses` and `GET …/instances/{instance}/statuses`
//!   answer Pleroma's `{total, activities}` envelope — the one account's and
//!   the one domain's (`local` being this instance's own authors) under the
//!   same filters and window, the total the whole set the filters select,
//!   and the unknown nickname the same 404;
//! - every route sits behind [`StaffUser`] — a moderator passes where a
//!   regular account is refused — plus its scope: `admin:read:statuses` on
//!   the four reads, `admin:write:statuses` on the two writes, another
//!   admin area's grant, a bare `read` and an app-only token all refused.
//!
//! Statuses are seeded through [`wired_be::models::status`] directly (the
//! posting endpoint is `statuses.rs`'s business), and the newest-first
//! expectations read the stored `ORDER BY s.id DESC` order back, so nothing
//! depends on which millisecond a fixture's `UUIDv7` landed in.
//!
//! [`StaffUser`]: wired_be::auth::admin::StaffUser
//! [`Status`]: wired_be::entities::status::Status
//! [`Account`]: wired_be::entities::account::Account

#![cfg(test)]

use axum::{Router, http::StatusCode};
use serde_json::{Value, json};
use sqlx::PgPool;
use uuid::Uuid;
use wired_be::{
    config::TEST_BASE_URL,
    federation::publisher,
    models::{
        status::{self, NewStatus, Status as StatusRow, Visibility},
        user::{Flag, NewUser, User},
    },
};

use crate::common;

/// The admin API prefix the status routes live under.
const ADMIN: &str = "/api/v1/pleroma/admin";
/// The whole-network ledger route, and the prefix of its `{id}` twin.
const LEDGER: &str = "/api/v1/pleroma/admin/statuses";
/// The 401 body when no token is presented.
const NO_TOKEN: &str = "Invalid credentials";
/// The 403 body for a valid token whose user is not staff.
const NOT_STAFF: &str = "User is not a staff member.";
/// The 403 body for an app-only (user-unbound) token.
const USER_REQUIRED: &str = "This API requires an authenticated user";
/// The 403 body of a token that grants neither `admin:read` nor
/// `admin:read:statuses`.
const NO_ADMIN_READ: &str = "Insufficient permissions: admin:read.";
/// The 403 body of a token that grants neither `admin:write` nor
/// `admin:write:statuses`.
const NO_ADMIN_WRITE: &str = "Insufficient permissions: admin:write.";
/// The 404 body of every miss on these routes, per the ticket.
const NOT_FOUND: &str = "Not found";
/// The 422 body of a `visibility` off the five the column holds.
const INVALID_VISIBILITY: &str = "Invalid visibility";
/// Both area scopes, for a caller that reads and writes alike.
const BOTH: &str = "admin:read:statuses admin:write:statuses";
/// The `ActivityStreams` public address a public note carries in `to`.
const PUBLIC: &str = "https://www.w3.org/ns/activitystreams#Public";

// --- fixtures -------------------------------------------------------------------------

/// A local user named `nickname` whose account is an admin, with a bearer
/// token granting `scopes`.
async fn admin(pool: &PgPool, nickname: &str, scopes: &str) -> (User, String) {
    let created = common::create_local_user_with(pool, nickname, |user| NewUser {
        is_admin: Flag::new(true),
        ..user
    })
    .await;
    let token = common::create_app_and_token(pool, &created, scopes).await;
    (created, token)
}

/// A local user named `nickname` whose account is a moderator, with a
/// bearer token granting `scopes`.
async fn moderator(pool: &PgPool, nickname: &str, scopes: &str) -> (User, String) {
    let created = common::create_local_user_with(pool, nickname, |user| NewUser {
        is_moderator: Flag::new(true),
        ..user
    })
    .await;
    let token = common::create_app_and_token(pool, &created, scopes).await;
    (created, token)
}

/// A local user named `nickname` with no staff role, with a bearer token
/// granting `scopes`.
async fn regular(pool: &PgPool, nickname: &str, scopes: &str) -> (User, String) {
    let created = common::create_local_user(pool, nickname).await;
    let token = common::create_app_and_token(pool, &created, scopes).await;
    (created, token)
}

/// `poster`'s status of `visibility`, straight through the model.
async fn post(pool: &PgPool, poster: &User, visibility: Visibility) -> StatusRow {
    let new = NewStatus {
        visibility,
        ..NewStatus::local(poster, &format!("a {} post", visibility.as_str()))
    };
    status::insert(pool, TEST_BASE_URL, &new)
        .await
        .expect("the status fixture inserts")
}

/// `poster`'s five visibilities, oldest seeded first: the whole ladder in
/// one account's hands, so a filter's answer is a slice of it.
async fn ladder(pool: &PgPool, poster: &User) -> Vec<StatusRow> {
    let mut rows = Vec::new();
    for visibility in [
        Visibility::Public,
        Visibility::Unlisted,
        Visibility::Private,
        Visibility::Direct,
        Visibility::Local,
    ] {
        rows.push(post(pool, poster, visibility).await);
    }
    rows
}

/// `reblogger`'s public boost of `original`.
async fn boost(pool: &PgPool, reblogger: &User, original: &StatusRow) -> StatusRow {
    status::create_reblog(pool, TEST_BASE_URL, reblogger, original, Visibility::Public)
        .await
        .expect("the reblog fixture writes")
        .0
}

/// `quoter`'s public quote of `original`.
async fn quote(pool: &PgPool, quoter: &User, original: &StatusRow) -> StatusRow {
    let new = NewStatus {
        quote_of_id: Some(original.id),
        ..NewStatus::local(quoter, "quoting this")
    };
    status::insert(pool, TEST_BASE_URL, &new)
        .await
        .expect("the quote fixture inserts")
}

/// A mirror of a post from `host`, authored by that host's `author`: the
/// shape decision 6 stores for a received activity — `local` clear, the
/// remote's own `uri`, and the author's `domain` what the instance listing
/// keys on.
async fn mirror_of(pool: &PgPool, author: &User, host: &str, slug: &str) -> StatusRow {
    status::insert(
        pool,
        TEST_BASE_URL,
        &NewStatus {
            uri: Some(format!("https://{host}/objects/{slug}")),
            user_id: author.id,
            content: format!("a {host} post"),
            raw_text: format!("a {host} post"),
            ..NewStatus::default()
        },
    )
    .await
    .expect("the mirrored fixture inserts")
}

// --- request helpers -------------------------------------------------------------------

/// The `GET` of `path` as `token`, asserted 200 and answered as JSON.
async fn listed(app: &Router, path: &str, token: &str) -> Value {
    let response = get_with(app, path, token).await;
    common::assert_answer(&response, StatusCode::OK, &format!("the read of {path}"));
    response.json_value()
}

/// One `GET` of `path` with `token`, answered whole.
async fn get_with(app: &Router, path: &str, token: &str) -> common::TestResponse {
    common::TestRequest::get(path).bearer(token).send(app).await
}

/// The `GET` of `path` with no token at all.
async fn anonymous(app: &Router, path: &str) -> common::TestResponse {
    common::TestRequest::get(path).send(app).await
}

/// The `PUT` of `body` on status `id`, asserted 200 and answered as JSON.
async fn rescoped(app: &Router, token: &str, id: Uuid, body: Value) -> Value {
    let response = common::TestRequest::put(&format!("{LEDGER}/{id}"))
        .bearer(token)
        .json(&body)
        .send(app)
        .await;
    common::assert_answer(&response, StatusCode::OK, "the staff scope write");
    response.json_value()
}

/// The `PUT` of `body` on status `id`, answered whole for the refusals.
async fn refused_put(app: &Router, token: &str, id: Uuid, body: Value) -> common::TestResponse {
    common::TestRequest::put(&format!("{LEDGER}/{id}"))
        .bearer(token)
        .json(&body)
        .send(app)
        .await
}

/// The `DELETE` of the admin route for status `id`.
async fn removed(app: &Router, token: &str, id: Uuid) -> common::TestResponse {
    common::TestRequest::delete(&format!("{LEDGER}/{id}"))
        .bearer(token)
        .send(app)
        .await
}

// --- answer readers --------------------------------------------------------------------

/// The ids of an answered array, in the answered order.
fn ids_of(answer: &Value) -> Vec<String> {
    id_list(
        answer
            .as_array()
            .expect("the ledger answers an array of entities"),
    )
}

/// The ids of an envelope's `activities` array, in the answered order.
fn activity_ids(answer: &Value) -> Vec<String> {
    id_list(
        answer["activities"]
            .as_array()
            .expect("the page answers an activities array"),
    )
}

/// The `id` text of every entity of `entries`, in order.
fn id_list(entries: &[Value]) -> Vec<String> {
    entries
        .iter()
        .map(|entry| {
            entry["id"]
                .as_str()
                .expect("entities carry id text")
                .to_owned()
        })
        .collect()
}

/// The ids of `rows` in the order a listing answers them: the fixtures seed
/// oldest first and `UUIDv7` ids ascend with creation (decision 4), so the
/// newest-first page is this slice reversed.
fn newest(rows: &[StatusRow]) -> Vec<String> {
    rows.iter().rev().map(|row| row.id.to_string()).collect()
}

/// The moderation-log rows of `action` — actor and sentence — oldest first.
async fn log_lines(pool: &PgPool, action: &str) -> Vec<(String, String)> {
    sqlx::query_as::<_, (String, String)>(
        "SELECT actor_nickname, message FROM moderation_log WHERE action = $1 ORDER BY id",
    )
    .bind(action)
    .fetch_all(pool)
    .await
    .expect("the log reads")
}

/// The activities the publisher's queue holds, in insertion order.
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

/// The stored row behind `id`, tombstone included.
async fn stored(pool: &PgPool, id: Uuid) -> StatusRow {
    status::get_by_id_including_deleted(pool, id)
        .await
        .expect("the row reads")
        .expect("the fixture row is stored")
}

// --- GET /statuses ---------------------------------------------------------------------

/// The default ledger is the public one — the two public visibilities of
/// every author, boost rows held out, newest post first — and each entry is
/// the standard entity with the plain account entity beside it.
#[tokio::test]
async fn the_ledger_lists_the_public_statuses_newest_first() {
    let db = common::test_db().await;
    let pool = db.pool().clone();
    let app = common::test_app(pool.clone());
    let (_root, token) = admin(&pool, "as_root", "admin:read:statuses").await;
    let alice = common::create_local_user(&pool, "as_alice").await;
    let rows = ladder(&pool, &alice).await;

    let answer = listed(&app, LEDGER, &token).await;
    assert_eq!(
        ids_of(&answer),
        newest(&rows[..2]),
        "public and unlisted only, newest first: the private visibilities \
         are held out until godmode is asked for"
    );

    let entity = answer.as_array().expect("an array")[0].clone();
    assert_eq!(
        entity["account"]["acct"].as_str(),
        Some("as_alice"),
        "the entity is the standard Status render, the author's ordinary \
         Account beside it"
    );
    assert_eq!(
        entity["visibility"].as_str(),
        Some("unlisted"),
        "the newest of the two listed visibilities leads the array"
    );
    assert!(
        entity["account"]["roles"].is_null(),
        "no admin account extension rides the entity"
    );
}

/// `godmode` is the visibility lift: with it the ledger holds `private`,
/// `direct` and Pleroma's `local` beside the two public arms, and the
/// explicit `godmode=false` is the default again.
#[tokio::test]
async fn godmode_admits_every_visibility() {
    let db = common::test_db().await;
    let pool = db.pool().clone();
    let app = common::test_app(pool.clone());
    let (_root, token) = admin(&pool, "as_god_root", "admin:read:statuses").await;
    let alice = common::create_local_user(&pool, "as_god_alice").await;
    let rows = ladder(&pool, &alice).await;

    let open = listed(&app, &format!("{LEDGER}?godmode=true"), &token).await;
    assert_eq!(
        ids_of(&open),
        newest(&rows),
        "every stored visibility, newest first"
    );

    let quiet = listed(&app, &format!("{LEDGER}?godmode=false"), &token).await;
    assert_eq!(
        ids_of(&quiet),
        newest(&rows[..2]),
        "the stated default is the public ledger again"
    );
}

/// `local_only` is the local authors' set, `with_reblogs` decides whether
/// boost rows are listed, and the two filters stack.
#[tokio::test]
async fn local_only_and_with_reblogs_narrow_the_ledger() {
    let db = common::test_db().await;
    let pool = db.pool().clone();
    let app = common::test_app(pool.clone());
    let (_root, token) = admin(&pool, "as_filter_root", "admin:read:statuses").await;
    let alice = common::create_local_user(&pool, "as_filter_alice").await;
    let stranger = common::create_remote_user(&pool, "stranger", "remote.example").await;
    let own = post(&pool, &alice, Visibility::Public).await;
    let mirrored = mirror_of(&pool, &stranger, "remote.example", "ledger").await;
    let boosted = boost(&pool, &alice, &own).await;

    let default = listed(&app, LEDGER, &token).await;
    assert_eq!(
        ids_of(&default),
        vec![mirrored.id.to_string(), own.id.to_string()],
        "boosts are held out by default, remote posts are not"
    );

    let reblogs = listed(&app, &format!("{LEDGER}?with_reblogs=true"), &token).await;
    assert_eq!(
        ids_of(&reblogs),
        vec![
            boosted.id.to_string(),
            mirrored.id.to_string(),
            own.id.to_string()
        ],
        "with_reblogs lets the boost row in"
    );
    assert_eq!(
        reblogs.as_array().expect("an array")[0]["reblog"]["id"],
        json!(own.id.to_string()),
        "and it renders as the ordinary entity, nesting what it boosts"
    );

    let locals = listed(&app, &format!("{LEDGER}?local_only=true"), &token).await;
    assert_eq!(
        ids_of(&locals),
        vec![own.id.to_string()],
        "local_only drops the remote post, and the default still holds the \
         boost out"
    );

    let stacked = listed(
        &app,
        &format!("{LEDGER}?local_only=true&with_reblogs=true"),
        &token,
    )
    .await;
    assert_eq!(
        ids_of(&stacked),
        vec![boosted.id.to_string(), own.id.to_string()],
        "and the two filters stack: the local set with the boost row in"
    );
}

/// The window is the admin listing's: `page` from 1 and `page_size`
/// defaulting to 50 under the shared 200-row cap, both read leniently,
/// cutting the newest-first ledger.
#[tokio::test]
async fn the_page_window_walks_the_ledger() {
    let db = common::test_db().await;
    let pool = db.pool().clone();
    let app = common::test_app(pool.clone());
    let (_root, token) = admin(&pool, "as_page_root", "admin:read:statuses").await;
    let alice = common::create_local_user(&pool, "as_page_alice").await;
    let rows = ladder(&pool, &alice).await;
    let every = newest(&rows);

    let first = listed(&app, &format!("{LEDGER}?godmode=true&page_size=2"), &token).await;
    assert_eq!(
        ids_of(&first),
        vec![every[0].clone(), every[1].clone()],
        "the first window is the two newest"
    );
    let second = listed(
        &app,
        &format!("{LEDGER}?godmode=true&page_size=2&page=2"),
        &token,
    )
    .await;
    assert_eq!(
        ids_of(&second),
        vec![every[2].clone(), every[3].clone()],
        "page 2 of the same window is the pair after them"
    );
    let past = listed(&app, &format!("{LEDGER}?godmode=true&page=99"), &token).await;
    assert!(ids_of(&past).is_empty(), "past the end is the empty array");

    let junk = listed(&app, &format!("{LEDGER}?page=nope&page_size=5000"), &token).await;
    assert_eq!(
        ids_of(&junk),
        vec![every[3].clone(), every[4].clone()],
        "an unusable `page` is the first page and a 5000-row ask is the \
         200-row cap, so the whole public ledger answers"
    );
}

// --- GET /statuses/{id} ----------------------------------------------------------------

/// The show route answers ANY live post — `private` included, no `godmode`
/// asked for — and every miss is Pleroma's one `{"error":"Not found"}`.
#[tokio::test]
async fn show_answers_any_live_post_and_404s_a_miss() {
    let db = common::test_db().await;
    let pool = db.pool().clone();
    let app = common::test_app(pool.clone());
    let (_root, token) = admin(&pool, "as_show_root", BOTH).await;
    let alice = common::create_local_user(&pool, "as_show_alice").await;
    let secret = post(&pool, &alice, Visibility::Private).await;
    let gone = post(&pool, &alice, Visibility::Public).await;
    let _deleted = removed(&app, &token, gone.id).await;

    let shown = listed(&app, &format!("{LEDGER}/{}", secret.id), &token).await;
    assert_eq!(shown["id"], secret.id.to_string(), "the entity is the row");
    assert_eq!(
        shown["visibility"], "private",
        "and it renders whatever its visibility is, no godmode asked"
    );

    let unknown = format!("{}/{}", LEDGER, Uuid::now_v7());
    let malformed = format!("{LEDGER}/not-a-uuid");
    let tombstoned = format!("{LEDGER}/{}", gone.id);
    for (who, path) in [
        ("an id that is no post", &unknown),
        ("a malformed id", &malformed),
        ("a deleted post", &tombstoned),
    ] {
        let missed = get_with(&app, path, &token).await;
        common::assert_answer(&missed, StatusCode::NOT_FOUND, who);
        common::assert_error(&missed, NOT_FOUND, who);
    }
}

// --- PUT /statuses/{id} ----------------------------------------------------------------

/// The scope write moves both fields, answers the re-scoped entity, leaves
/// the text and `edited_at` where they were, and logs Pleroma's sentence
/// naming the parts that moved.
#[tokio::test]
async fn update_moves_both_fields_and_logs_the_change() {
    let db = common::test_db().await;
    let pool = db.pool().clone();
    let app = common::test_app(pool.clone());
    let (_root, token) = admin(&pool, "as_upd_root", BOTH).await;
    let alice = common::create_local_user(&pool, "as_upd_alice").await;
    let row = post(&pool, &alice, Visibility::Public).await;

    let answer = rescoped(
        &app,
        &token,
        row.id,
        json!({"sensitive": true, "visibility": "private"}),
    )
    .await;
    assert_eq!(
        answer["visibility"], "private",
        "the entity answers the new scope"
    );
    assert_eq!(answer["sensitive"], json!(true), "and the new flag");

    let moved = stored(&pool, row.id).await;
    assert_eq!(moved.visibility, Visibility::Private, "the row agrees");
    assert!(moved.sensitive, "and so it does with the flag");
    assert_eq!(moved.content, row.content, "the text is where it was");
    assert_eq!(moved.edited_at, None, "a scope change is no content edit");
    assert_eq!(
        moved.uri, row.uri,
        "and the object id it was federated under is reserved"
    );

    assert_eq!(
        log_lines(&pool, "status_update").await,
        vec![(
            "as_upd_root".to_owned(),
            format!(
                "@as_upd_root updated status ##{}, set sensitive: 'true', visibility: 'private'",
                row.id
            )
        )],
        "one entry, Pleroma's status_update sentence"
    );
}

/// One field at a time, and neither: the column the body does not present
/// keeps its value, and an empty body is Pleroma's fall-through that still
/// logs and still answers the entity.
#[tokio::test]
async fn update_moves_only_the_fields_the_body_presents() {
    let db = common::test_db().await;
    let pool = db.pool().clone();
    let app = common::test_app(pool.clone());
    let (_root, token) = admin(&pool, "as_part_root", BOTH).await;
    let alice = common::create_local_user(&pool, "as_part_alice").await;
    let row = post(&pool, &alice, Visibility::Public).await;

    let folded = rescoped(&app, &token, row.id, json!({"sensitive": "true"})).await;
    assert_eq!(
        folded["sensitive"],
        json!(true),
        "the string spelling counts"
    );
    assert_eq!(folded["visibility"], "public", "and the visibility holds");

    let private = rescoped(&app, &token, row.id, json!({"visibility": "private"})).await;
    assert_eq!(
        private["visibility"], "private",
        "the next write moves the other field"
    );
    assert_eq!(
        private["sensitive"],
        json!(true),
        "and the flag the first write set still stands"
    );

    let bare = rescoped(&app, &token, row.id, json!({})).await;
    assert_eq!(
        bare["visibility"], "private",
        "an empty body names nothing to move"
    );
    assert_eq!(
        log_lines(&pool, "status_update").await,
        vec![
            (
                "as_part_root".to_owned(),
                format!(
                    "@as_part_root updated status ##{}, set sensitive: 'true'",
                    row.id
                )
            ),
            (
                "as_part_root".to_owned(),
                format!(
                    "@as_part_root updated status ##{}, set visibility: 'private'",
                    row.id
                )
            ),
            (
                "as_part_root".to_owned(),
                format!(
                    "@as_part_root updated status ##{}, set visibility: ''",
                    row.id
                )
            ),
        ],
        "one entry per write, each naming what it moved, and Pleroma's \
         fall-through sentence for the one that moved nothing"
    );
}

/// A `visibility` the column would not hold is the ticket's 422 with the row
/// left exactly as it was; an id that is no post is the 404; and a
/// `sensitive` that is no boolean is the body extractor's own 422.
#[tokio::test]
async fn update_refuses_an_unusable_visibility_and_a_miss() {
    let db = common::test_db().await;
    let pool = db.pool().clone();
    let app = common::test_app(pool.clone());
    let (_root, token) = admin(&pool, "as_bad_root", BOTH).await;
    let alice = common::create_local_user(&pool, "as_bad_alice").await;
    let row = post(&pool, &alice, Visibility::Public).await;

    for visibility in ["everything", "Public", ""] {
        let refused = refused_put(&app, &token, row.id, json!({"visibility": visibility})).await;
        common::assert_answer(
            &refused,
            StatusCode::UNPROCESSABLE_ENTITY,
            &format!("{visibility} is no visibility"),
        );
        common::assert_error(&refused, INVALID_VISIBILITY, "the refusal body");
    }
    assert_eq!(
        stored(&pool, row.id).await.visibility,
        Visibility::Public,
        "and no refusal wrote anything"
    );
    assert!(
        log_lines(&pool, "status_update").await.is_empty(),
        "nor did one log"
    );

    let missing = refused_put(&app, &token, Uuid::now_v7(), json!({"sensitive": true})).await;
    common::assert_answer(&missing, StatusCode::NOT_FOUND, "an id that is no post");
    common::assert_error(&missing, NOT_FOUND, "an id that is no post");

    let unusable = refused_put(&app, &token, row.id, json!({"sensitive": "maybe"})).await;
    common::assert_answer(
        &unusable,
        StatusCode::UNPROCESSABLE_ENTITY,
        "a boolean that is no boolean",
    );
}

/// A post of this instance is corrected in the network's copy: the write
/// queues the `Update` of the re-drafted note. A mirrored row belongs to its
/// origin and queues nothing at all.
#[tokio::test]
async fn update_publishes_an_update_for_a_local_post_only() {
    let db = common::test_db().await;
    let pool = db.pool().clone();
    let app = common::test_app(pool.clone());
    let (_root, token) = admin(&pool, "as_fed_root", BOTH).await;
    let alice = common::create_local_user(&pool, "as_fed_alice").await;
    let stranger = common::create_remote_user(&pool, "fedstranger", "remote.example").await;
    let own = post(&pool, &alice, Visibility::Public).await;
    let theirs = mirror_of(&pool, &stranger, "remote.example", "edit").await;

    let _quiet = rescoped(&app, &token, theirs.id, json!({"visibility": "unlisted"})).await;
    assert!(
        published(&pool).await.is_empty(),
        "a mirrored post is not this instance's to correct"
    );

    let _loud = rescoped(&app, &token, own.id, json!({"visibility": "unlisted"})).await;
    let activities = published(&pool).await;
    assert_eq!(activities.len(), 1, "the local edit is announced once");
    assert_eq!(activities[0]["type"], "Update", "as the correction it is");
    assert_eq!(
        activities[0]["object"]["id"], own.uri,
        "and it names the same reserved object uri"
    );
    assert_eq!(
        activities[0]["object"]["to"],
        json!([format!("{TEST_BASE_URL}/users/as_fed_alice/followers")]),
        "addressed as the re-scoped note states it: an unlisted post is \
         written to its author's followers, not to the public"
    );
    assert_eq!(
        activities[0]["object"]["cc"],
        json!([PUBLIC]),
        "and the public address rides the carbon copy, which is what sets \
         unlisted apart from public"
    );
}

/// A local post whose visibility is the never-federated `local` has no
/// document to correct: the write lands, the entity answers, and the queue
/// stays empty.
#[tokio::test]
async fn update_of_a_local_visibility_post_publishes_nothing() {
    let db = common::test_db().await;
    let pool = db.pool().clone();
    let app = common::test_app(pool.clone());
    let (_root, token) = admin(&pool, "as_quiet_root", BOTH).await;
    let alice = common::create_local_user(&pool, "as_quiet_alice").await;
    let row = post(&pool, &alice, Visibility::Local).await;

    let answer = rescoped(&app, &token, row.id, json!({"sensitive": true})).await;
    assert_eq!(
        answer["visibility"], "local",
        "the scope it keeps is answered"
    );
    assert!(
        published(&pool).await.is_empty(),
        "and a local-visibility post has no Update to send"
    );
}

// --- DELETE /statuses/{id} -------------------------------------------------------------

/// The admin withdrawal is the shared deletion service: `200 {}`, the
/// tombstone, the `status_delete` entry and the federated `Delete` — and the
/// 404 for the second strike at the same post.
#[tokio::test]
async fn delete_takes_the_post_down_and_reports_it() {
    let db = common::test_db().await;
    let pool = db.pool().clone();
    let app = common::test_app(pool.clone());
    let (_root, token) = admin(&pool, "as_del_root", BOTH).await;
    let alice = common::create_local_user(&pool, "as_del_alice").await;
    let row = post(&pool, &alice, Visibility::Public).await;

    let answer = removed(&app, &token, row.id).await;
    common::assert_answer(&answer, StatusCode::OK, "the staff delete");
    assert_eq!(
        answer.json_value(),
        json!({}),
        "Pleroma's `json(conn, %{{}})` answer"
    );

    assert!(
        stored(&pool, row.id).await.deleted_at.is_some(),
        "the row is a tombstone now"
    );
    assert_eq!(
        log_lines(&pool, "status_delete").await,
        vec![(
            "as_del_root".to_owned(),
            format!("@as_del_root deleted status ##{}", row.id)
        )],
        "the moderation entry names the withdrawal"
    );
    let activities = published(&pool).await;
    assert_eq!(activities.len(), 1, "the network is told once");
    assert_eq!(activities[0]["type"], "Delete", "with the withdrawal it is");

    let again = removed(&app, &token, row.id).await;
    common::assert_answer(&again, StatusCode::NOT_FOUND, "the second strike");
    common::assert_error(&again, NOT_FOUND, "the second strike");
    assert_eq!(
        log_lines(&pool, "status_delete").await.len(),
        1,
        "a refusal logs nothing, and the withdrawal was logged once"
    );
}

// --- the two envelopes -----------------------------------------------------------------

/// One account's page: the `{total, activities}` envelope of that account's
/// statuses alone, the total the whole set the filters select and not the
/// window, the id naming an account as the handle does, and the unknown
/// nickname being the 404.
#[tokio::test]
async fn the_user_page_lists_one_account_with_its_total() {
    let db = common::test_db().await;
    let pool = db.pool().clone();
    let app = common::test_app(pool.clone());
    let (_root, token) = admin(&pool, "as_user_root", "admin:read:statuses").await;
    let alice = common::create_local_user(&pool, "as_user_alice").await;
    let bob = common::create_local_user(&pool, "as_user_bob").await;
    let mine = ladder(&pool, &alice).await;
    let theirs = post(&pool, &bob, Visibility::Public).await;
    let my_boost = boost(&pool, &alice, &theirs).await;

    let path = format!("{ADMIN}/users/as_user_alice/statuses");
    let quiet = listed(&app, &path, &token).await;
    assert_eq!(
        activity_ids(&quiet),
        newest(&mine[..2]),
        "this account's two public visibilities, newest first, and none of \
         bob's"
    );
    assert_eq!(quiet["total"], json!(2), "the total of that same set");

    let open = listed(&app, &format!("{path}?godmode=true&page_size=3"), &token).await;
    assert_eq!(
        activity_ids(&open),
        newest(&mine[2..]),
        "godmode admits the private visibilities too, in the three-row window"
    );
    assert_eq!(
        open["total"],
        json!(5),
        "the total is the whole set, not the window it is cut with"
    );

    let boosts = listed(&app, &format!("{path}?with_reblogs=true"), &token).await;
    assert!(
        activity_ids(&boosts).contains(&my_boost.id.to_string()),
        "alice's own boost is hers to list when reblogs are asked for"
    );

    let by_id = listed(&app, &format!("{ADMIN}/users/{}/statuses", bob.id), &token).await;
    assert_eq!(
        activity_ids(&by_id),
        vec![theirs.id.to_string()],
        "the id names an account as well as the handle does"
    );

    let missing = get_with(&app, &format!("{ADMIN}/users/nobody/statuses"), &token).await;
    common::assert_answer(&missing, StatusCode::NOT_FOUND, "a nickname nobody carries");
    common::assert_error(&missing, NOT_FOUND, "a nickname nobody carries");
}

/// One domain's page: the mirrors of one host and not another's, and the
/// `local` literal for this instance's own authors, both in the same
/// envelope and under the same filters.
#[tokio::test]
async fn the_instance_page_lists_one_domain() {
    let db = common::test_db().await;
    let pool = db.pool().clone();
    let app = common::test_app(pool.clone());
    let (_root, token) = admin(&pool, "as_inst_root", "admin:read:statuses").await;
    let alice = common::create_local_user(&pool, "as_inst_alice").await;
    let first = common::create_remote_user(&pool, "one", "first.example").await;
    let second = common::create_remote_user(&pool, "two", "second.example").await;
    let home = post(&pool, &alice, Visibility::Public).await;
    let of_first = mirror_of(&pool, &first, "first.example", "a").await;
    let of_second = mirror_of(&pool, &second, "second.example", "b").await;

    let hosts = listed(
        &app,
        &format!("{ADMIN}/instances/first.example/statuses"),
        &token,
    )
    .await;
    assert_eq!(
        activity_ids(&hosts),
        vec![of_first.id.to_string()],
        "one host's mirrors only"
    );
    assert_eq!(hosts["total"], json!(1), "and that host's total");

    let local = listed(&app, &format!("{ADMIN}/instances/local/statuses"), &token).await;
    assert_eq!(
        activity_ids(&local),
        vec![home.id.to_string()],
        "the `local` literal is this instance's own authors"
    );

    let other = listed(
        &app,
        &format!("{ADMIN}/instances/second.example/statuses?with_reblogs=false"),
        &token,
    )
    .await;
    assert_eq!(
        activity_ids(&other),
        vec![of_second.id.to_string()],
        "the filters ride the domain page as they do the ledger"
    );

    let nowhere = listed(
        &app,
        &format!("{ADMIN}/instances/absent.example/statuses"),
        &token,
    )
    .await;
    assert!(
        activity_ids(&nowhere).is_empty() && nowhere["total"] == json!(0),
        "a domain nobody came from is the empty page, not a miss"
    );
}

/// The admin view asks no visibility question of what a post quotes: a
/// public status quoting a private one nests the quoted original on the
/// staff read, where the client surface leaves it null.
#[tokio::test]
async fn the_admin_view_nests_a_quote_the_public_view_hides() {
    let db = common::test_db().await;
    let pool = db.pool().clone();
    let app = common::test_app(pool.clone());
    let (_root, token) = admin(&pool, "as_quote_root", "admin:read:statuses").await;
    let alice = common::create_local_user(&pool, "as_quote_alice").await;
    let secret = post(&pool, &alice, Visibility::Private).await;
    let quoting = quote(&pool, &alice, &secret).await;

    let answer = listed(&app, &format!("{LEDGER}/{}", quoting.id), &token).await;
    assert_eq!(
        answer["quote_id"],
        secret.id.to_string(),
        "the id names the quote either way"
    );
    assert_eq!(
        answer["quote"]["id"],
        secret.id.to_string(),
        "and the admin view renders the private quote itself"
    );
    assert_eq!(answer["pleroma"]["quote_visible"], json!(true));

    let client = anonymous(&app, &format!("/api/v1/statuses/{}", quoting.id)).await;
    common::assert_answer(&client, StatusCode::OK, "the anonymous client read");
    assert_eq!(
        client.json_value()["quote"],
        Value::Null,
        "the anonymous client view keeps the same quote hidden"
    );
}

// --- the gate --------------------------------------------------------------------------

/// A moderator is Pleroma's expected caller of this controller: the staff
/// extractor lets them read the ledger, re-scope a post and delete it, and
/// the entries they leave name them as the actor.
#[tokio::test]
async fn a_moderator_reads_and_writes_the_ledger() {
    let db = common::test_db().await;
    let pool = db.pool().clone();
    let app = common::test_app(pool.clone());
    let (_mod, token) = moderator(&pool, "as_mod", BOTH).await;
    let alice = common::create_local_user(&pool, "as_mod_alice").await;
    let row = post(&pool, &alice, Visibility::Private).await;

    let shown = listed(&app, &format!("{LEDGER}/{}", row.id), &token).await;
    assert_eq!(shown["id"], row.id.to_string(), "the moderator sees it");
    let moved = rescoped(&app, &token, row.id, json!({"visibility": "public"})).await;
    assert_eq!(moved["visibility"], "public", "and re-scopes it");
    let gone = removed(&app, &token, row.id).await;
    common::assert_answer(&gone, StatusCode::OK, "and deletes it");

    let actors: Vec<String> = log_lines(&pool, "status_update")
        .await
        .into_iter()
        .map(|(actor, _message)| actor)
        .collect();
    assert_eq!(
        actors,
        vec!["as_mod".to_owned()],
        "the entry names the moderator, not the author"
    );
}

/// Every read refuses what it must, whatever its filters: no token at all is
/// the 401 `Invalid credentials`, an app-only token is the 403 that asks for
/// a user, and a valid regular account's token is the 403 that says it is no
/// staff member.
#[tokio::test]
async fn the_reads_demand_a_token_of_a_staff_account() {
    let db = common::test_db().await;
    let pool = db.pool().clone();
    let app = common::test_app(pool.clone());
    let (alice, user_token) = regular(&pool, "as_gate_alice", "admin:read:statuses").await;
    let app_only = common::create_app_only_token(&pool, "admin:read:statuses").await;
    let posted = post(&pool, &alice, Visibility::Public).await;

    for path in read_paths("as_gate_alice", posted.id) {
        let open = anonymous(&app, &path).await;
        common::assert_answer(&open, StatusCode::UNAUTHORIZED, "no token at all");
        common::assert_error(&open, NO_TOKEN, "no token at all");

        let client = get_with(&app, &path, &app_only).await;
        common::assert_answer(&client, StatusCode::FORBIDDEN, "an app-only token");
        common::assert_error(&client, USER_REQUIRED, "an app-only token");

        let outsider = get_with(&app, &path, &user_token).await;
        common::assert_answer(&outsider, StatusCode::FORBIDDEN, "a non-staff token");
        common::assert_error(&outsider, NOT_STAFF, "a non-staff token");
    }
}

/// The scope half of the gate is Pleroma's and per area: another admin
/// area's grant and a bare `read` are both refused on every read, the class
/// root `admin:read` serves the whole class, and the narrow
/// `admin:read:statuses` opens the four reads and neither write.
#[tokio::test]
async fn the_reads_ask_their_own_area() {
    let db = common::test_db().await;
    let pool = db.pool().clone();
    let app = common::test_app(pool.clone());
    let (_root, narrow) = admin(&pool, "as_area_root", "admin:read:statuses").await;
    let (_wrong, wrong_area) = admin(&pool, "as_area_other", "admin:read:chats").await;
    let (_bare, no_admin) = admin(&pool, "as_area_bare", "read").await;
    let (_wide, all_reads) = admin(&pool, "as_area_wide", "admin:read").await;
    let alice = common::create_local_user(&pool, "as_area_alice").await;
    let row = post(&pool, &alice, Visibility::Public).await;

    for path in read_paths("as_area_alice", row.id) {
        for (token, who) in [
            (&wrong_area, "another admin area only"),
            (&no_admin, "no admin grant at all"),
        ] {
            let refused = get_with(&app, &path, token).await;
            common::assert_answer(&refused, StatusCode::FORBIDDEN, who);
            common::assert_error(&refused, NO_ADMIN_READ, who);
        }
        let _open = listed(&app, &path, &all_reads).await;
    }

    let ledger = listed(&app, LEDGER, &narrow).await;
    assert_eq!(
        ids_of(&ledger),
        vec![row.id.to_string()],
        "the narrow read scope reads the ledger"
    );
    let write = removed(&app, &narrow, row.id).await;
    common::assert_answer(&write, StatusCode::FORBIDDEN, "a read-scope DELETE");
    common::assert_error(&write, NO_ADMIN_WRITE, "a read-scope DELETE");
}

/// The writes ask the write class and no other: `admin:read:statuses` is
/// refused on both, the narrow `admin:write:statuses` writes both and is
/// refused on the reads, and the class root serves either.
#[tokio::test]
async fn the_writes_ask_their_own_area() {
    let db = common::test_db().await;
    let pool = db.pool().clone();
    let app = common::test_app(pool.clone());
    let (_root, read_only) = admin(&pool, "as_w_root", "admin:read:statuses").await;
    let (_other, write_only) = admin(&pool, "as_w_other", "admin:write:statuses").await;
    let (_wide, all_writes) = admin(&pool, "as_w_wide", "admin:write").await;
    let alice = common::create_local_user(&pool, "as_w_alice").await;
    let one = post(&pool, &alice, Visibility::Public).await;
    let two = post(&pool, &alice, Visibility::Public).await;
    let three = post(&pool, &alice, Visibility::Public).await;

    let refused = refused_put(&app, &read_only, one.id, json!({"sensitive": true})).await;
    common::assert_answer(&refused, StatusCode::FORBIDDEN, "a read-scope PUT");
    common::assert_error(&refused, NO_ADMIN_WRITE, "a read-scope PUT");

    let shown = get_with(&app, &format!("{LEDGER}/{}", one.id), &write_only).await;
    common::assert_answer(&shown, StatusCode::FORBIDDEN, "a write-scope read");
    common::assert_error(&shown, NO_ADMIN_READ, "a write-scope read");

    let moved = rescoped(&app, &write_only, one.id, json!({"visibility": "direct"})).await;
    assert_eq!(
        moved["visibility"], "direct",
        "the narrow write scope writes"
    );
    let gone = removed(&app, &write_only, two.id).await;
    common::assert_answer(&gone, StatusCode::OK, "and deletes");
    let wide = rescoped(&app, &all_writes, three.id, json!({"sensitive": true})).await;
    assert_eq!(wide["sensitive"], json!(true), "and so does the class root");

    assert_eq!(
        log_lines(&pool, "status_update").await.len(),
        2,
        "one entry per accepted write, and none for the refusal"
    );
}

// --- helpers kept below the tests they serve -------------------------------------------

/// Every read route of this surface, for the gate walks: the ledger with and
/// without its filters, its `{id}` twin on the post `id`, and the two
/// envelope listings — the account one keyed on the nickname `author`.
fn read_paths(author: &str, id: Uuid) -> Vec<String> {
    vec![
        LEDGER.to_owned(),
        format!("{LEDGER}?godmode=true&with_reblogs=true&local_only=true"),
        format!("{LEDGER}/{id}"),
        format!("{ADMIN}/users/{author}/statuses"),
        format!("{ADMIN}/instances/local/statuses"),
    ]
}
