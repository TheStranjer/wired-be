//! Lists over HTTP (`/api/v1/lists`, WBE-95): the Mastodon v1 CRUD, the
//! present-only update, and the "which of my lists hold this account"
//! lookup.
//!
//! The tests pin what the ticket promises: create, read, list, change and
//! delete round-trip the four-key `List` entity; the list comes back in
//! creation order; `replies_policy` accepts exactly `none`, `list` and
//! `followed` and defaults to `list`; a `PUT` changes only the parameters
//! it names; a blank, absent or 101-character title and an unknown policy
//! answer the `{"error":"Validation failed: …"}` 422s; `GET
//! /api/v1/accounts/{id}/lists` sees only the viewer's own memberships and
//! 404s on an unknown account; every route enforces its half of the
//! `AuthUser` + scope pipeline (`read:lists` to look, `write:lists` to
//! change); and an id that is malformed, unknown or another account's is
//! one and the same 404.

#![cfg(test)]

use axum::{Router, http::StatusCode};
use serde_json::{Value, json};
use sqlx::PgPool;
use uuid::Uuid;
use wired_be::models::{
    list::{self, List as Stored, NewList},
    user::User,
};

use crate::common;

/// The root path the list CRUD lives under.
const ROOT: &str = "/api/v1/lists";
/// Every JSON key a list entity must carry, sorted.
const EXPECTED_KEYS: [&str; 4] = ["exclusive", "id", "replies_policy", "title"];
/// The 422 body for an absent or blank title (Mastodon's changeset wording).
const TITLE_BLANK_MESSAGE: &str = "Validation failed: Title can't be blank";
/// The 422 body for a policy outside the three known ones (likewise).
const POLICY_INVALID_MESSAGE: &str =
    "Validation failed: Replies policy is not included in the list";

/// A local user named `nickname` plus a bearer token granting `scopes`.
async fn fixture(pool: &PgPool, nickname: &str, scopes: &str) -> (User, String) {
    let created = common::create_local_user(pool, nickname).await;
    let token = common::create_app_and_token(pool, &created, scopes).await;
    (created, token)
}

/// Creates a list through the model — `title` with the column defaults —
/// and returns its row.
async fn seed(pool: &PgPool, owner: &User, title: &str) -> Stored {
    list::create(
        pool,
        owner.id,
        &NewList {
            title: title.to_owned(),
            ..NewList::default()
        },
    )
    .await
    .expect("fixture list creates")
}

/// Creates a list through the model with every column given, and returns
/// its row.
async fn seed_full(
    pool: &PgPool,
    owner: &User,
    title: &str,
    replies_policy: &str,
    exclusive: bool,
) -> Stored {
    list::create(
        pool,
        owner.id,
        &NewList {
            title: title.to_owned(),
            replies_policy: replies_policy.to_owned(),
            exclusive,
        },
    )
    .await
    .expect("fixture list creates")
}

/// Adds `member` to `list` straight through the join table: the
/// membership endpoints are a later ticket, the tests must still be able
/// to say who is on which list.
async fn add_member(pool: &PgPool, list: &Stored, member: &User) {
    let inserted = sqlx::query("INSERT INTO list_members (list_id, user_id) VALUES ($1, $2)")
        .bind(list.id)
        .bind(member.id)
        .execute(pool)
        .await
        .expect("fixture membership inserts");
    assert_eq!(inserted.rows_affected(), 1, "one membership went in");
}

/// The minimal valid create body.
fn create_body() -> Value {
    json!({"title": "Friends"})
}

/// `POST /api/v1/lists` with the JSON `body` as `token`.
async fn create_json(app: &Router, token: &str, body: Value) -> common::TestResponse {
    common::TestRequest::post(ROOT)
        .bearer(token)
        .json(&body)
        .send(app)
        .await
}

/// `POST /api/v1/lists` with the form `fields` as `token`.
async fn create_form(app: &Router, token: &str, fields: &[(&str, &str)]) -> common::TestResponse {
    common::TestRequest::post(ROOT)
        .bearer(token)
        .form(fields)
        .send(app)
        .await
}

/// `GET /api/v1/lists` as `token`.
async fn list_all(app: &Router, token: &str) -> common::TestResponse {
    common::TestRequest::get(ROOT).bearer(token).send(app).await
}

/// `GET /api/v1/lists` as `token`, asserted 200 and parsed.
async fn listed(app: &Router, token: &str) -> Vec<Value> {
    let response = list_all(app, token).await;
    assert_eq!(response.status, StatusCode::OK, "the list answers 200");
    response.json()
}

/// `GET /api/v1/lists/{id}` as `token`.
async fn show(app: &Router, token: &str, id: Uuid) -> common::TestResponse {
    common::TestRequest::get(&format!("{ROOT}/{id}"))
        .bearer(token)
        .send(app)
        .await
}

/// `PUT /api/v1/lists/{id}` with the JSON `body` as `token`.
async fn update(app: &Router, token: &str, id: Uuid, body: Value) -> common::TestResponse {
    common::TestRequest::put(&format!("{ROOT}/{id}"))
        .bearer(token)
        .json(&body)
        .send(app)
        .await
}

/// `DELETE /api/v1/lists/{id}` as `token`.
async fn destroy(app: &Router, token: &str, id: Uuid) -> common::TestResponse {
    common::TestRequest::delete(&format!("{ROOT}/{id}"))
        .bearer(token)
        .send(app)
        .await
}

/// `GET /api/v1/accounts/{account}/lists` as `token`.
async fn account_lists(app: &Router, token: &str, account: Uuid) -> common::TestResponse {
    common::TestRequest::get(&format!("/api/v1/accounts/{account}/lists"))
        .bearer(token)
        .send(app)
        .await
}

/// The `title` of every element of `rows`, in order.
fn titles(rows: &[Value]) -> Vec<&str> {
    rows.iter()
        .map(|row| row["title"].as_str().expect("every list carries its title"))
        .collect()
}

/// The keys of a JSON object, sorted, for exact key-set assertions.
fn json_keys(value: &Value) -> Vec<&str> {
    let mut keys: Vec<&str> = value
        .as_object()
        .expect("lists render objects")
        .keys()
        .map(String::as_str)
        .collect();
    keys.sort_unstable();
    keys
}

/// An account with nothing stored answers 200 with the empty JSON array.
#[tokio::test]
async fn empty_database_answers_the_empty_list() {
    let db = common::test_db().await;
    let pool = db.pool().clone();
    let (_alice, token) = fixture(&pool, "alice", "read:lists").await;
    let app = common::test_app(pool);

    let response = list_all(&app, &token).await;
    assert_eq!(response.status, StatusCode::OK, "200 for a reader");
    assert_eq!(
        response.header("content-type"),
        Some("application/json"),
        "the list is JSON"
    );
    assert_eq!(response.text(), "[]", "no lists, the empty array");
}

/// The whole lifecycle in order: `POST` answers the entity with its
/// defaults, the list and the show find it (oldest first), `PUT` changes
/// what it names, `DELETE` answers `{}` and takes it away.
#[tokio::test]
async fn crud_round_trip() {
    let db = common::test_db().await;
    let pool = db.pool().clone();
    let (_alice, token) = fixture(&pool, "alice", "read:lists write:lists").await;
    let app = common::test_app(pool);

    let created = create_json(&app, &token, create_body()).await;
    assert_eq!(created.status, StatusCode::OK, "creation answers 200");
    let list = created.json_value();
    assert_eq!(json_keys(&list), EXPECTED_KEYS, "exactly the entity keys");
    let id = list["id"].as_str().expect("the id is text").to_owned();
    assert!(
        Uuid::parse_str(&id).is_ok(),
        "and a `UUIDv7` string at that: {id}"
    );
    assert_eq!(list["title"], "Friends", "the title verbatim");
    assert_eq!(list["replies_policy"], "list", "the default policy");
    assert_eq!(list["exclusive"], false, "and exclusiveness off");

    let second = create_json(&app, &token, json!({"title": "Work"})).await;
    assert_eq!(second.status, StatusCode::OK, "a second list creates");
    assert_eq!(
        titles(&listed(&app, &token).await),
        ["Friends", "Work"],
        "the list reads back in creation order (id ascending)"
    );

    let shown = show(&app, &token, Uuid::parse_str(&id).expect("id parses")).await;
    assert_eq!(shown.status, StatusCode::OK, "and served by id");
    assert_eq!(shown.json_value(), list, "the same entity either way");

    let changed = update(
        &app,
        &token,
        Uuid::parse_str(&id).expect("id parses"),
        json!({
            "title": "Besties",
            "replies_policy": "none",
            "exclusive": true,
        }),
    )
    .await;
    assert_eq!(changed.status, StatusCode::OK, "the update answers 200");
    let after = changed.json_value();
    assert_eq!(after["id"], list["id"], "the same id");
    assert_eq!(after["title"], "Besties", "the changed title");
    assert_eq!(after["replies_policy"], "none", "the changed policy");
    assert_eq!(after["exclusive"], true, "the changed flag");

    let gone = destroy(&app, &token, Uuid::parse_str(&id).expect("id parses")).await;
    assert_eq!(gone.status, StatusCode::OK, "the delete answers 200");
    assert_eq!(gone.json_value(), json!({}), "with the empty object");
    assert_eq!(
        titles(&listed(&app, &token).await),
        ["Work"],
        "and afterwards only the other list is left"
    );
    let missing = show(&app, &token, Uuid::parse_str(&id).expect("id parses")).await;
    assert_eq!(missing.status, StatusCode::NOT_FOUND, "and the row is gone");
}

/// A form body creates the same list a JSON body would, string booleans
/// and policy included.
#[tokio::test]
async fn a_form_body_creates_the_same_list() {
    let db = common::test_db().await;
    let pool = db.pool().clone();
    let (_alice, token) = fixture(&pool, "alice", "write:lists").await;
    let app = common::test_app(pool);

    let created = create_form(
        &app,
        &token,
        &[
            ("title", "Close friends"),
            ("replies_policy", "followed"),
            ("exclusive", "true"),
        ],
    )
    .await;
    assert_eq!(created.status, StatusCode::OK, "the form creates");
    let list = created.json_value();
    assert_eq!(list["title"], "Close friends", "the title");
    assert_eq!(list["replies_policy"], "followed", "the string policy");
    assert_eq!(list["exclusive"], true, "and the string boolean");
}

/// All three policy values create and round trip; anything else is a 422.
#[tokio::test]
async fn the_replies_policy_values_round_trip() {
    let db = common::test_db().await;
    let pool = db.pool().clone();
    let (_alice, token) = fixture(&pool, "alice", "read:lists write:lists").await;
    let app = common::test_app(pool);

    for policy in ["none", "list", "followed"] {
        let created = create_json(
            &app,
            &token,
            json!({"title": "x", "replies_policy": policy}),
        )
        .await;
        assert_eq!(created.status, StatusCode::OK, "`{policy}` creates");
        assert_eq!(
            created.json_value()["replies_policy"],
            policy,
            "and reads back"
        );
    }

    let wrong = create_json(
        &app,
        &token,
        json!({"title": "x", "replies_policy": "everyone"}),
    )
    .await;
    assert_eq!(wrong.status, StatusCode::UNPROCESSABLE_ENTITY, "no");
    assert_eq!(
        wrong.json_value()["error"],
        "Validation failed: Replies policy is not included in the list",
        "a policy outside the three is Mastodon's changeset wording"
    );
}

/// A `PUT` changes only the parameters it names: an absent title, policy
/// or flag keeps its stored value, and an empty body changes nothing (but
/// still answers the list).
#[tokio::test]
async fn update_changes_only_what_it_names() {
    let db = common::test_db().await;
    let pool = db.pool().clone();
    let (alice, token) = fixture(&pool, "alice", "read:lists write:lists").await;
    let stored = seed_full(&pool, &alice, "before", "none", true).await;
    let app = common::test_app(pool);

    let renamed = update(&app, &token, stored.id, json!({"title": "after"})).await;
    assert_eq!(renamed.status, StatusCode::OK, "a lone title updates");
    let after = renamed.json_value();
    assert_eq!(after["title"], "after", "the named column changed");
    assert_eq!(after["replies_policy"], "none", "the unnamed policy stayed");
    assert_eq!(after["exclusive"], true, "and the unnamed flag");

    let untouched = update(&app, &token, stored.id, json!({})).await;
    assert_eq!(
        untouched.status,
        StatusCode::OK,
        "an empty body is no error"
    );
    let kept = untouched.json_value();
    assert_eq!(kept["title"], "after", "nothing named,");
    assert_eq!(kept["replies_policy"], "none", "nothing changed:");
    assert_eq!(kept["exclusive"], true, "the row keeps every column");
}

/// The `{"error":"Validation failed: …"}` bodies the ticket names, and
/// their precedence: validation runs before any lookup, so even a PUT to a
/// nonexistent id answers 422 for a bad body.
#[tokio::test]
async fn invalid_parameters_answer_mastodons_validation_wording() {
    let db = common::test_db().await;
    let pool = db.pool().clone();
    let (_alice, token) = fixture(&pool, "alice", "write:lists").await;
    let app = common::test_app(pool.clone());

    for (what, body, message) in [
        (
            "a missing title",
            json!({}),
            "Validation failed: Title can't be blank",
        ),
        (
            "an empty title",
            json!({"title": ""}),
            "Validation failed: Title can't be blank",
        ),
        (
            "a blank title",
            json!({"title": "   "}),
            "Validation failed: Title can't be blank",
        ),
        (
            "a 101-character title",
            json!({"title": "x".repeat(101)}),
            "Validation failed: Title is too long (maximum is 100 characters)",
        ),
        (
            "an unknown policy",
            json!({"title": "ok", "replies_policy": "bogus"}),
            "Validation failed: Replies policy is not included in the list",
        ),
    ] {
        let response = create_json(&app, &token, body).await;
        assert_eq!(
            response.status,
            StatusCode::UNPROCESSABLE_ENTITY,
            "`{what}` is a 422"
        );
        assert_eq!(
            response.json_value()["error"],
            message,
            "with Mastodon's wording for `{what}`"
        );
    }

    // A present-but-invalid title or policy is the same 422 on `PUT`; an
    // absent one simply names nothing to change (see
    // `update_changes_only_what_it_names`), so the create-only
    // `{"title": ""}` case stands in for the loop above here.
    for (body, message) in [
        (json!({"title": ""}), TITLE_BLANK_MESSAGE),
        (json!({"replies_policy": "bogus"}), POLICY_INVALID_MESSAGE),
    ] {
        let rejected = update(&app, &token, Uuid::now_v7(), body).await;
        assert_eq!(
            rejected.status,
            StatusCode::UNPROCESSABLE_ENTITY,
            "validation precedes the lookup, as Pleroma's plug does"
        );
        assert_eq!(
            rejected.json_value()["error"],
            message,
            "even for an id that names nothing"
        );
    }

    let wrong_type = create_form(&app, &token, &[("title", "x"), ("exclusive", "maybe")]).await;
    assert_eq!(
        wrong_type.status,
        StatusCode::UNPROCESSABLE_ENTITY,
        "a flag that is not a boolean is also a 422"
    );
    assert!(
        wrong_type.json_value()["error"]
            .as_str()
            .unwrap_or_default()
            .contains("expected a boolean"),
        "and the flex message says what it wanted: {}",
        wrong_type.text()
    );
}

/// A foreign id and a malformed one are the same 404 on every list route,
/// and the owner's row is untouched by another account's attempts.
#[tokio::test]
async fn ownership_and_missing_ids_answer_404() {
    let db = common::test_db().await;
    let pool = db.pool().clone();
    let (alice, alice_token) = fixture(&pool, "alice", "read:lists write:lists").await;
    let (_bob, bob_token) = fixture(&pool, "bob", "read:lists write:lists").await;
    let hers = seed(&pool, &alice, "hers").await;
    let app = common::test_app(pool.clone());

    let shown = show(&app, &bob_token, hers.id).await;
    assert_eq!(shown.status, StatusCode::NOT_FOUND, "bob cannot read it");
    assert_eq!(
        shown.json_value()["error"],
        "Record not found",
        "Pleroma's wording, one for foreign and missing"
    );
    let replaced = update(&app, &bob_token, hers.id, json!({"title": "stolen"})).await;
    assert_eq!(
        replaced.status,
        StatusCode::NOT_FOUND,
        "bob cannot rewrite it"
    );
    let deleted = destroy(&app, &bob_token, hers.id).await;
    assert_eq!(
        deleted.status,
        StatusCode::NOT_FOUND,
        "bob cannot delete it"
    );
    assert!(
        listed(&app, &bob_token).await.is_empty(),
        "and nothing reached bob's list"
    );

    let kept = show(&app, &alice_token, hers.id).await;
    assert_eq!(kept.status, StatusCode::OK, "alice still owns her list");
    assert_eq!(kept.json_value()["title"], "hers", "unchanged");

    for (what, target) in [
        ("never minted", Uuid::now_v7().to_string()),
        ("not an id", "not-a-uuid".to_owned()),
    ] {
        let shown_missing = common::TestRequest::get(&format!("{ROOT}/{target}"))
            .bearer(&alice_token)
            .send(&app)
            .await;
        assert_eq!(
            shown_missing.status,
            StatusCode::NOT_FOUND,
            "{what} is a 404 on show"
        );
        let updated = common::TestRequest::put(&format!("{ROOT}/{target}"))
            .bearer(&alice_token)
            .json(&create_body())
            .send(&app)
            .await;
        assert_eq!(
            updated.status,
            StatusCode::NOT_FOUND,
            "{what} is a 404 on update"
        );
        let deleted_missing = common::TestRequest::delete(&format!("{ROOT}/{target}"))
            .bearer(&alice_token)
            .send(&app)
            .await;
        assert_eq!(
            deleted_missing.status,
            StatusCode::NOT_FOUND,
            "{what} is a 404 on delete"
        );
    }
    assert_eq!(
        titles(&listed(&app, &alice_token).await),
        ["hers"],
        "and alice's row survived every attempt on it"
    );
}

/// `GET /api/v1/accounts/{id}/lists` answers the viewer's own lists that
/// hold the account, oldest first — never another owner's, never a list
/// the account is not on — and 404s on an unknown or malformed account id.
#[tokio::test]
async fn account_lists_answers_the_viewers_memberships() {
    let db = common::test_db().await;
    let pool = db.pool().clone();
    let (alice, alice_token) = fixture(&pool, "alice", "read:lists").await;
    let (bob, bob_token) = fixture(&pool, "bob", "read:lists").await;
    let (carol, carol_token) = fixture(&pool, "carol", "read:lists").await;
    let first = seed(&pool, &alice, "first").await;
    let _skipped: Stored = seed(&pool, &alice, "without bob").await;
    let last = seed(&pool, &alice, "last").await;
    add_member(&pool, &first, &bob).await;
    add_member(&pool, &last, &bob).await;
    let carols = seed(&pool, &carol, "carol's own").await;
    add_member(&pool, &carols, &bob).await;
    let app = common::test_app(pool);

    let found = account_lists(&app, &alice_token, bob.id).await;
    assert_eq!(found.status, StatusCode::OK, "alice looks up bob");
    assert_eq!(
        titles(&found.json::<Vec<Value>>()),
        ["first", "last"],
        "her two lists holding him, oldest first; the list without him and \
         carol's list of him are not hers to answer"
    );

    assert_eq!(
        titles(
            &account_lists(&app, &carol_token, bob.id)
                .await
                .json::<Vec<Value>>()
        ),
        ["carol's own"],
        "carol sees only her own list"
    );
    assert!(
        account_lists(&app, &bob_token, bob.id)
            .await
            .json::<Vec<Value>>()
            .is_empty(),
        "bob holds no list of himself"
    );
    assert_eq!(
        account_lists(&app, &alice_token, Uuid::now_v7())
            .await
            .status,
        StatusCode::NOT_FOUND,
        "an unknown account is the 404"
    );
    let malformed = common::TestRequest::get("/api/v1/accounts/not-a-uuid/lists")
        .bearer(&alice_token)
        .send(&app)
        .await;
    assert_eq!(
        malformed.status,
        StatusCode::NOT_FOUND,
        "and a malformed id is the same 404"
    );
}

/// The credential half of the pipeline: no token at all is the 401 with
/// Pleroma's "Invalid credentials", a token no row carries the 401 naming the
/// token, and an app-only token the 403 demanding an authenticated user.
#[tokio::test]
async fn the_credential_gates_refuse_every_route() {
    let db = common::test_db().await;
    let pool = db.pool().clone();
    let (alice, _) = fixture(&pool, "alice", "read write").await;
    let hers = seed(&pool, &alice, "hers").await;
    let app_only = common::create_app_only_token(&pool, "read write").await;
    let app = common::test_app(pool);

    let anonymous = common::TestRequest::get(ROOT).send(&app).await;
    common::assert_answer(&anonymous, StatusCode::UNAUTHORIZED, "no token is a 401");
    common::assert_error(&anonymous, "Invalid credentials", "the absent token");

    let bogus = [
        list_all(&app, "not-a-real-token").await,
        show(&app, "not-a-real-token", hers.id).await,
        account_lists(&app, "not-a-real-token", alice.id).await,
    ];
    common::assert_all_refused(
        &bogus,
        StatusCode::UNAUTHORIZED,
        "The access token is invalid",
        "a token no row carries",
    );

    let userless = [
        list_all(&app, &app_only).await,
        create_json(&app, &app_only, create_body()).await,
    ];
    common::assert_all_refused(
        &userless,
        StatusCode::FORBIDDEN,
        "This API requires an authenticated user",
        "an app-only token",
    );
}

/// The scope half of the pipeline: reading needs a user token with
/// `read:lists` (or the broader `read`) and writing one with `write:lists`
/// (or `write`); every route a token lacks the scope for answers the 403
/// naming it.
#[tokio::test]
async fn tokens_without_the_route_scope_are_refused() {
    let db = common::test_db().await;
    let pool = db.pool().clone();
    let (alice, _) = fixture(&pool, "alice", "read write").await;
    let hers = seed(&pool, &alice, "hers").await;
    let (_reader, read_only) = fixture(&pool, "reader", "read").await;
    let (_writer, write_broad) = fixture(&pool, "writer", "write").await;
    let (_narrow, wrong_scope) = fixture(&pool, "narrow", "read:statuses write:statuses").await;
    let app = common::test_app(pool);

    for (who, token) in [("read:statuses", &wrong_scope), ("write", &write_broad)] {
        let refused = [
            list_all(&app, token).await,
            show(&app, token, hers.id).await,
            account_lists(&app, token, alice.id).await,
        ];
        common::assert_all_refused(
            &refused,
            StatusCode::FORBIDDEN,
            "Insufficient permissions: read:lists.",
            &format!("`{who}` on every reading route"),
        );
    }
    for (who, token) in [("read:statuses", &wrong_scope), ("read", &read_only)] {
        let refused = [
            create_json(&app, token, create_body()).await,
            update(&app, token, hers.id, json!({"title": "x"})).await,
            destroy(&app, token, hers.id).await,
        ];
        common::assert_all_refused(
            &refused,
            StatusCode::FORBIDDEN,
            "Insufficient permissions: write:lists.",
            &format!("`{who}` on every writing route"),
        );
    }
}

/// The granted side of the same pipeline: the narrow sub-scopes pass on their
/// own users, the broad `read` and `write` trees pass everywhere, and what a
/// token was refused it really did not store.
#[tokio::test]
async fn the_granted_scopes_use_every_route() {
    let db = common::test_db().await;
    let pool = db.pool().clone();
    let (alice, alice_token) = fixture(&pool, "alice", "read write").await;
    let hers = seed(&pool, &alice, "hers").await;
    let (_viewer, lists_read) = fixture(&pool, "viewer", "read:lists").await;
    let (_mutator, lists_write) = fixture(&pool, "mutator", "write:lists").await;
    let app = common::test_app(pool);

    let granted_reads = [
        list_all(&app, &lists_read).await,
        account_lists(&app, &lists_read, alice.id).await,
        show(&app, &alice_token, hers.id).await,
    ];
    common::assert_all_answered(
        &granted_reads,
        StatusCode::OK,
        "`read:lists` lists and looks up memberships; the broad read shows",
    );
    let granted_writes = [
        create_json(&app, &lists_write, create_body()).await,
        create_json(&app, &alice_token, create_body()).await,
        update(&app, &alice_token, hers.id, json!({"title": "rewritten"})).await,
        destroy(&app, &alice_token, hers.id).await,
    ];
    common::assert_all_answered(
        &granted_writes,
        StatusCode::OK,
        "`write:lists` creates; the broad write creates, updates and deletes the owner's row",
    );
    assert_eq!(
        titles(&listed(&app, &alice_token).await),
        ["Friends"],
        "alice's lists afterwards: her create, the rewritten one deleted"
    );
    assert_eq!(
        titles(&listed(&app, &lists_read).await),
        Vec::<&str>::new(),
        "while the read-only viewer stored nothing of its own"
    );
}
