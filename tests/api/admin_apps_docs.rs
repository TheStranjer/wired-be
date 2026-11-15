//! The admin OAuth-app maintenance and instance-document endpoints over
//! HTTP (WBE-205): `GET`/`POST /api/v1/pleroma/admin/oauth_app`,
//! `PATCH`/`DELETE …/oauth_app/{id}` — the filters, the window, the
//! seven-key entity, the `can't be blank` refusals and the `trusted`
//! flag — and the `instance_document/{name}` trio with the public
//! `GET /instance/{name}.html`, its headers, its 1 MiB cap, its atomic
//! store and the traversal gate, against the real router and a document
//! directory in `tempfile`. The auth half covers both areas'
//! `admin:read` / `admin:write` scope gates.

#![cfg(test)]

use axum::http::StatusCode;
use bytes::Bytes;
use serde_json::{Value, json};
use sqlx::PgPool;
use uuid::Uuid;
use wired_be::models::{
    oauth_app,
    user::{Flag, NewUser},
};

use crate::common::{self, MultipartPart, TestRequest};

/// The app listing/creation route.
const APPS: &str = "/api/v1/pleroma/admin/oauth_app";
/// The public document route for the terms of service.
const PUBLIC_TOS: &str = "/instance/terms-of-service.html";
/// The admin document route for the terms of service.
const DOC_TOS: &str = "/api/v1/pleroma/admin/instance_document/terms-of-service";
/// The document store's 1 MiB ceiling.
const MAX_DOC: usize = 1024 * 1024;
/// The 401 body when no token is presented.
const NO_TOKEN: &str = "Invalid credentials";
/// The 403 body for an app-only (user-unbound) token.
const USER_REQUIRED: &str = "This API requires an authenticated user";
/// The 403 body for a valid token whose user is not an admin.
const NOT_ADMIN: &str = "User is not an admin.";
/// The 403 body of a missing `admin:read` grant.
const NO_ADMIN_READ: &str = "Insufficient permissions: admin:read.";
/// The 403 body of a missing `admin:write` grant.
const NO_ADMIN_WRITE: &str = "Insufficient permissions: admin:write.";
/// The 404 body of the unknown id and the unknown document name alike.
const NOT_FOUND: &str = "Not found";
/// The CSP the public document route answers with.
const DOCUMENT_CSP: &str = "default-src 'none'; style-src 'unsafe-inline'; img-src * data:";
/// The redirect URI every app body in these tests registers.
const OOB: &str = "urn:ietf:wg:oauth:2.0:oob";
/// The multipart boundary every test body frames with.
const BOUNDARY: &str = "wired-docs-boundary";

/// A local admin named `nickname` with a bearer token granting `scopes`.
async fn admin(pool: &PgPool, nickname: &str, scopes: &str) -> String {
    let created = common::create_local_user_with(pool, nickname, |user| NewUser {
        is_admin: Flag::new(true),
        ..user
    })
    .await;
    common::create_app_and_token(pool, &created, scopes).await
}

/// Creates one app over HTTP as `token` and returns the raw response.
async fn create_app(app: &axum::Router, token: &str, body: Value) -> common::TestResponse {
    TestRequest::post(APPS)
        .bearer(token)
        .json(&body)
        .send(app)
        .await
}

/// The total row count of `oauth_apps`, the number the listing's
/// `count` mirrors (Pleroma's whole-table aggregate).
async fn total_apps(pool: &PgPool) -> i64 {
    sqlx::query_scalar::<_, i64>("SELECT count(*) FROM oauth_apps")
        .fetch_one(pool)
        .await
        .expect("the app count reads")
}

/// The `entity["id"]` text of a create answer, owned.
fn entity_id(response: &common::TestResponse) -> String {
    response.json_value()["id"]
        .as_str()
        .expect("an id text")
        .to_owned()
}

/// The `entity["client_id"]` text of a create answer, owned.
fn entity_client_id(response: &common::TestResponse) -> String {
    response.json_value()["client_id"]
        .as_str()
        .expect("a client id")
        .to_owned()
}

/// The id list of a listing answer's `apps` array.
fn listed_ids(page: &Value) -> Vec<String> {
    page["apps"]
        .as_array()
        .expect("the apps array")
        .iter()
        .map(|row| row["id"].as_str().expect("an id text").to_owned())
        .collect()
}

/// The app-CRUD fixture pair: an untrusted `Alpha` and a trusted `Beta`
/// created in that order (so `Beta` is the newer row).
async fn alpha_and_beta(app: &axum::Router, token: &str) -> (String, String, String, String) {
    let first = create_app(app, token, json!({"name": "Alpha", "redirect_uris": OOB})).await;
    let second = create_app(
        app,
        token,
        json!({"name": "Beta", "redirect_uris": OOB, "trusted": true}),
    )
    .await;
    (
        entity_id(&first),
        entity_client_id(&second),
        entity_id(&second),
        first.text(),
    )
}

/// The create half of the walk: the entity carries exactly the seven
/// pinned keys, and the listing finds the new app newest-first beside
/// the whole-table count and the 50-row window.
#[tokio::test]
async fn create_answers_the_entity_the_listing_finds_first() {
    let db = common::test_db().await;
    let pool = db.pool().clone();
    let app = common::test_app(pool.clone());
    let token = admin(&pool, "root", "admin:write").await;
    let read_token = admin(&pool, "reader", "admin:read").await;

    let created = create_app(
        &app,
        &token,
        json!({
            "name": "Console",
            "redirect_uris": ["https://a.example/cb", "https://b.example/cb"],
            "scopes": ["read", "write"],
            "website": "https://console.example",
            "trusted": true,
        }),
    )
    .await;
    common::assert_answer(&created, StatusCode::OK, "create answers the entity");
    let entity = created.json_value();
    let keys: Vec<&str> = entity
        .as_object()
        .expect("an object")
        .keys()
        .map(String::as_str)
        .collect();
    assert_eq!(
        keys,
        vec![
            "client_id",
            "client_secret",
            "id",
            "name",
            "redirect_uri",
            "trusted",
            "website"
        ],
        "exactly the seven pinned keys, {keys:?}"
    );
    assert_eq!(entity["name"], "Console", "the name");
    assert_eq!(
        entity["redirect_uri"], "https://a.example/cb\nhttps://b.example/cb",
        "the list normalized to the stored newline spelling"
    );
    assert_eq!(entity["trusted"], json!(true), "the flag answers");
    assert_eq!(entity["website"], "https://console.example", "the homepage");
    assert!(
        entity["client_id"]
            .as_str()
            .is_some_and(|id| id.len() == 43)
            && entity["client_secret"]
                .as_str()
                .is_some_and(|secret| secret.len() == 43),
        "the credentials were minted"
    );

    let listed = TestRequest::get(APPS).bearer(&read_token).send(&app).await;
    common::assert_answer(&listed, StatusCode::OK, "the admin read lists");
    let page = listed.json_value();
    assert_eq!(page["page_size"], json!(50), "the admin default window");
    assert_eq!(
        page["count"].as_i64(),
        Some(total_apps(&pool).await),
        "count is the whole-table total"
    );
    assert_eq!(page["apps"][0]["id"], entity["id"], "newest app first");
}

/// The patch and delete half of the walk: the patch moves only what it
/// sends, the delete answers 204, and every later write to the id is
/// the 404 `{"error":"Not found"}`.
#[tokio::test]
async fn patch_moves_only_the_sent_fields_and_delete_ends_the_row() {
    let db = common::test_db().await;
    let pool = db.pool().clone();
    let app = common::test_app(pool.clone());
    let token = admin(&pool, "root", "admin:write").await;

    let created = create_app(
        &app,
        &token,
        json!({
            "name": "Console",
            "redirect_uris": OOB,
            "scopes": "read write",
            "website": "https://console.example",
            "trusted": true,
        }),
    )
    .await;
    let id = entity_id(&created);

    let patched = TestRequest::patch(&format!("{APPS}/{id}"))
        .bearer(&token)
        .json(&json!({"trusted": false}))
        .send(&app)
        .await;
    common::assert_answer(&patched, StatusCode::OK, "the patch answers the entity");
    let patched_app = patched.json_value();
    assert_eq!(patched_app["trusted"], json!(false), "the flag moved");
    assert_eq!(patched_app["name"], "Console", "everything else stayed");
    assert_eq!(
        patched_app["website"], "https://console.example",
        "present-only patching"
    );

    let stored = oauth_app::get_by_id(&pool, Uuid::parse_str(&id).expect("an id"))
        .await
        .expect("the model reads")
        .expect("the row is there");
    assert_eq!(stored.scopes, "read write", "the scopes survived");
    assert!(!stored.trusted, "and the patch landed in the row");

    let deleted = TestRequest::delete(&format!("{APPS}/{id}"))
        .bearer(&token)
        .send(&app)
        .await;
    common::assert_answer(&deleted, StatusCode::NO_CONTENT, "delete answers 204");
    assert!(deleted.body.is_empty(), "with no body");
    assert!(
        oauth_app::get_by_id(&pool, Uuid::parse_str(&id).expect("an id"))
            .await
            .expect("the model reads")
            .is_none(),
        "the row is gone"
    );

    let gone = TestRequest::patch(&format!("{APPS}/{id}"))
        .bearer(&token)
        .json(&json!({"name": "Ghost"}))
        .send(&app)
        .await;
    common::assert_answer(&gone, StatusCode::NOT_FOUND, "the deleted id is a 404");
    common::assert_error(&gone, NOT_FOUND, "the pinned wording");
    let gone_again = TestRequest::delete(&format!("{APPS}/{id}"))
        .bearer(&token)
        .send(&app)
        .await;
    common::assert_answer(&gone_again, StatusCode::NOT_FOUND, "delete 404s too");
}

/// The text filters: `name` and `client_id` are exact matches, not
/// patterns.
#[tokio::test]
async fn index_filters_by_exact_name_and_client_id() {
    let db = common::test_db().await;
    let pool = db.pool().clone();
    let app = common::test_app(pool.clone());
    let token = admin(&pool, "root", "admin:read admin:write").await;
    let (_alpha_id, beta_client_id, beta_id, _first_body) = alpha_and_beta(&app, &token).await;

    let by_name = TestRequest::get(&format!("{APPS}?name=Beta"))
        .bearer(&token)
        .send(&app)
        .await;
    let page = by_name.json_value();
    assert_eq!(listed_ids(&page), vec![beta_id.clone()], "one exact match");

    let near_miss = TestRequest::get(&format!("{APPS}?name=alph"))
        .bearer(&token)
        .send(&app)
        .await;
    assert!(
        listed_ids(&near_miss.json_value()).is_empty(),
        "the name filter is exact, not a pattern"
    );

    let by_client = TestRequest::get(&format!("{APPS}?client_id={beta_client_id}"))
        .bearer(&token)
        .send(&app)
        .await;
    assert_eq!(
        listed_ids(&by_client.json_value()),
        vec![beta_id],
        "the client id finds its app"
    );
}

/// The boolean filter and the window: `trusted` keeps one kind, absent
/// keeps both, `page` / `page_size` cut the window while `count` stays
/// the whole-table total, and a non-boolean `trusted` is a 400.
#[tokio::test]
async fn index_windows_pages_and_filters_on_trusted() {
    let db = common::test_db().await;
    let pool = db.pool().clone();
    let app = common::test_app(pool.clone());
    let token = admin(&pool, "root", "admin:read admin:write").await;
    let (alpha_id, _beta_client_id, beta_id, _first_body) = alpha_and_beta(&app, &token).await;

    let trusted = TestRequest::get(&format!("{APPS}?trusted=true"))
        .bearer(&token)
        .send(&app)
        .await;
    let trusted_ids = listed_ids(&trusted.json_value());
    assert!(
        trusted_ids.contains(&beta_id) && !trusted_ids.contains(&alpha_id),
        "the trusted app is listed, the untrusted one is not: {trusted_ids:?}"
    );

    let untrusted = TestRequest::get(&format!("{APPS}?trusted=0"))
        .bearer(&token)
        .send(&app)
        .await;
    let untrusted_ids = listed_ids(&untrusted.json_value());
    assert!(
        untrusted_ids.contains(&alpha_id) && !untrusted_ids.contains(&beta_id),
        "trusted=0 is the mirror filter: {untrusted_ids:?}"
    );

    let both = TestRequest::get(APPS).bearer(&token).send(&app).await;
    let both_ids = listed_ids(&both.json_value());
    assert!(
        both_ids.contains(&alpha_id) && both_ids.contains(&beta_id),
        "absent trusted lists every kind"
    );

    let window = TestRequest::get(&format!("{APPS}?page=1&page_size=1"))
        .bearer(&token)
        .send(&app)
        .await;
    let cut = window.json_value();
    assert_eq!(listed_ids(&cut).len(), 1, "one row");
    assert_eq!(cut["page_size"], json!(1), "the echo of the window");
    assert_eq!(
        cut["count"].as_i64(),
        Some(total_apps(&pool).await),
        "the count ignores the window"
    );

    let junk = TestRequest::get(&format!("{APPS}?trusted=yes"))
        .bearer(&token)
        .send(&app)
        .await;
    common::assert_answer(&junk, StatusCode::BAD_REQUEST, "junk trusted is a 400");
}

/// The create refusals, in the `{"errors": …}` shape the ticket pins:
/// absent and blank `name`, absent and all-blank `redirect_uris` (both
/// in one answer when both miss), an unknown scope, and the defaults —
/// `read` scopes and an untrusted flag — a bare registration gets.
#[tokio::test]
async fn create_names_its_refusals_and_its_defaults() {
    let db = common::test_db().await;
    let pool = db.pool().clone();
    let app = common::test_app(pool.clone());
    let token = admin(&pool, "root", "admin:write").await;

    let nothing = create_app(&app, &token, json!({})).await;
    common::assert_answer(&nothing, StatusCode::BAD_REQUEST, "nothing is a 400");
    let body = nothing.json_value();
    assert_eq!(body["errors"]["name"], json!(["can't be blank"]), "name");
    assert_eq!(
        body["errors"]["redirect_uris"],
        json!(["can't be blank"]),
        "and redirect_uris, one answer"
    );

    let blank = create_app(
        &app,
        &token,
        json!({"name": "   ", "redirect_uris": ["", "  "]}),
    )
    .await;
    assert_eq!(
        blank.json_value()["errors"]["name"],
        json!(["can't be blank"]),
        "blank is absent here"
    );

    let junk_scope = create_app(
        &app,
        &token,
        json!({"name": "Ok", "redirect_uris": OOB, "scopes": ["bogus"]}),
    )
    .await;
    common::assert_answer(
        &junk_scope,
        StatusCode::BAD_REQUEST,
        "a bogus scope is a 400",
    );
    assert!(
        junk_scope.json_value()["errors"]["scopes"][0]
            .as_str()
            .expect("a message")
            .contains("bogus"),
        "the refusal names the offender: {}",
        junk_scope.text()
    );

    let bare = create_app(&app, &token, json!({"name": "Bare", "redirect_uris": OOB})).await;
    common::assert_answer(&bare, StatusCode::OK, "the minimal body creates");
    let client_id = entity_client_id(&bare);
    assert_eq!(
        bare.json_value()["website"],
        json!(null),
        "no website is null"
    );
    assert_eq!(bare.json_value()["trusted"], json!(false), "and untrusted");
    let stored = oauth_app::get_by_client_id(&pool, &client_id)
        .await
        .expect("the model reads")
        .expect("the row is there");
    assert_eq!(stored.scopes, "read", "the ticket's default scope");

    let form = TestRequest::post(APPS)
        .bearer(&token)
        .form(&[
            ("name", "Form App"),
            ("redirect_uris", OOB),
            ("trusted", "true"),
            ("scopes", "read write"),
        ])
        .send(&app)
        .await;
    common::assert_answer(&form, StatusCode::OK, "the form body creates");
    assert_eq!(
        form.json_value()["trusted"],
        json!(true),
        "flex reads the string"
    );
}

/// The patch's refusals: an unknown id and a non-uuid id are both the
/// 404 `{"error":"Not found"}`, and a sent-but-blank name still refuses
/// with the create's wording.
#[tokio::test]
async fn patch_refuses_the_unknown_id_and_the_blank_name() {
    let db = common::test_db().await;
    let pool = db.pool().clone();
    let app = common::test_app(pool.clone());
    let token = admin(&pool, "root", "admin:write").await;

    let ghost = Uuid::now_v7();
    let missed = TestRequest::patch(&format!("{APPS}/{ghost}"))
        .bearer(&token)
        .json(&json!({"name": "Ghost"}))
        .send(&app)
        .await;
    common::assert_answer(&missed, StatusCode::NOT_FOUND, "no row is a 404");
    common::assert_error(&missed, NOT_FOUND, "the pinned wording");

    let malformed = TestRequest::patch(&format!("{APPS}/1337"))
        .bearer(&token)
        .json(&json!({"name": "Integer"}))
        .send(&app)
        .await;
    common::assert_answer(
        &malformed,
        StatusCode::NOT_FOUND,
        "Pleroma's integer id 404s here",
    );

    let created = create_app(
        &app,
        &token,
        json!({"name": "Patchable", "redirect_uris": OOB}),
    )
    .await;
    let id = entity_id(&created);
    let blank = TestRequest::patch(&format!("{APPS}/{id}"))
        .bearer(&token)
        .json(&json!({"name": "  "}))
        .send(&app)
        .await;
    common::assert_answer(&blank, StatusCode::BAD_REQUEST, "a present blank refuses");
    assert_eq!(
        blank.json_value()["errors"]["name"],
        json!(["can't be blank"]),
        "the same wording as on create"
    );
}

/// The delete's refusals: an unknown id and an unparsable id both
/// answer the 404 before any row is touched.
#[tokio::test]
async fn delete_answers_the_gate_then_the_404() {
    let db = common::test_db().await;
    let pool = db.pool().clone();
    let app = common::test_app(pool.clone());
    let token = admin(&pool, "root", "admin:write").await;

    let ghost = Uuid::now_v7();
    let missed = TestRequest::delete(&format!("{APPS}/{ghost}"))
        .bearer(&token)
        .send(&app)
        .await;
    common::assert_answer(&missed, StatusCode::NOT_FOUND, "no row to delete");
    common::assert_error(&missed, NOT_FOUND, "the pinned wording");
    let malformed = TestRequest::delete(&format!("{APPS}/not-a-uuid"))
        .bearer(&token)
        .send(&app)
        .await;
    common::assert_answer(&malformed, StatusCode::NOT_FOUND, "no id to parse");
}

/// The trusted flag travels through every answer whole — and no route
/// here acts on it: the authorize page still renders the consent form
/// for a trusted app (the documented note in
/// `wired_be::api::oauth::authorize`).
#[tokio::test]
async fn the_trusted_flag_travels_through_every_answer() {
    let db = common::test_db().await;
    let pool = db.pool().clone();
    let app = common::test_app(pool.clone());
    let token = admin(&pool, "root", "admin:read admin:write").await;

    let created = create_app(
        &app,
        &token,
        json!({"name": "Trusted", "redirect_uris": OOB, "trusted": true}),
    )
    .await;
    assert_eq!(
        created.json_value()["trusted"],
        json!(true),
        "create answers it"
    );
    let id = entity_id(&created);

    let listed = TestRequest::get(&format!("{APPS}?trusted=true"))
        .bearer(&token)
        .send(&app)
        .await;
    assert_eq!(
        listed.json_value()["apps"][0]["trusted"],
        json!(true),
        "the listing carries it"
    );

    let flipped = TestRequest::patch(&format!("{APPS}/{id}"))
        .bearer(&token)
        .json(&json!({"trusted": "false"}))
        .send(&app)
        .await;
    assert_eq!(
        flipped.json_value()["trusted"],
        json!(false),
        "the patch flips it, form spelling included"
    );
    let stored = oauth_app::get_by_id(&pool, Uuid::parse_str(&id).expect("an id"))
        .await
        .expect("the model reads")
        .expect("the row is there");
    assert!(!stored.trusted, "and the row says so");
}

/// The admin document read of a never-uploaded name and the public read
/// of the same absence: both the 404 `{"error":"Not found"}`, the
/// public one as JSON and with no auth asked.
#[tokio::test]
async fn absent_documents_are_the_json_404_on_both_reads() {
    let db = common::test_db().await;
    let pool = db.pool().clone();
    let dir = tempfile::tempdir().expect("a docs dir");
    let app = common::test_app_with_instance_docs_dir(pool.clone(), dir.path());
    let read_token = admin(&pool, "reader", "admin:read").await;

    let missing = TestRequest::get(DOC_TOS)
        .bearer(&read_token)
        .send(&app)
        .await;
    common::assert_answer(&missing, StatusCode::NOT_FOUND, "never uploaded is a 404");
    common::assert_error(&missing, NOT_FOUND, "the pinned wording");

    let missing_public = TestRequest::get(PUBLIC_TOS).send(&app).await;
    common::assert_answer(
        &missing_public,
        StatusCode::NOT_FOUND,
        "the public 404 needs no auth",
    );
    common::assert_error(&missing_public, NOT_FOUND, "as JSON like everything else");
    assert_eq!(
        missing_public.header("content-type"),
        Some("application/json"),
        "the public miss is the JSON 404, not an HTML page"
    );
}

/// The document store's full walk: the multipart upload answers the
/// public URL, the admin read returns the bytes as `text/html`, the
/// public route serves them with the pinned headers and no auth at all,
/// a re-upload replaces the file whole, and the delete empties both
/// routes behind it.
#[tokio::test]
async fn documents_upload_serve_and_delete_round_trip() {
    let db = common::test_db().await;
    let pool = db.pool().clone();
    let dir = tempfile::tempdir().expect("a docs dir");
    let app = common::test_app_with_instance_docs_dir(pool.clone(), dir.path());
    let token = admin(&pool, "root", "admin:write").await;
    let read_token = admin(&pool, "reader", "admin:read").await;

    let uploaded = TestRequest::patch(DOC_TOS)
        .bearer(&token)
        .multipart(
            BOUNDARY,
            &[MultipartPart::file(
                "file",
                "terms.html",
                "text/html",
                Bytes::from_static(b"<h1>Terms</h1>"),
            )],
        )
        .send(&app)
        .await;
    common::assert_answer(&uploaded, StatusCode::OK, "the upload answers");
    assert_eq!(
        uploaded.json_value()["url"],
        "https://test.example/instance/terms-of-service.html",
        "the public URL of the document"
    );
    assert!(
        dir.path().join("terms-of-service.html").is_file(),
        "it landed"
    );

    let read = TestRequest::get(DOC_TOS)
        .bearer(&read_token)
        .send(&app)
        .await;
    common::assert_answer(&read, StatusCode::OK, "the admin read answers");
    assert_eq!(read.text(), "<h1>Terms</h1>", "the bytes verbatim");
    assert_eq!(
        read.header("content-type"),
        Some("text/html"),
        "as text/html, as Pleroma's show sets it"
    );

    let served = TestRequest::get(PUBLIC_TOS).send(&app).await;
    common::assert_answer(&served, StatusCode::OK, "the public read needs no auth");
    assert_eq!(served.text(), "<h1>Terms</h1>", "the same bytes");
    assert_eq!(
        served.header("content-type"),
        Some("text/html; charset=utf-8"),
        "with the charset"
    );
    assert_eq!(
        served.header("content-security-policy"),
        Some(DOCUMENT_CSP),
        "and the document CSP"
    );

    let no_suffix = TestRequest::get("/instance/terms-of-service")
        .send(&app)
        .await;
    common::assert_answer(
        &no_suffix,
        StatusCode::NOT_FOUND,
        "the suffix is part of the route",
    );
    let wrong_suffix = TestRequest::get("/instance/terms-of-service.htm")
        .send(&app)
        .await;
    common::assert_answer(
        &wrong_suffix,
        StatusCode::NOT_FOUND,
        "and it is exactly .html",
    );

    let deleted = TestRequest::delete(DOC_TOS).bearer(&token).send(&app).await;
    common::assert_answer(&deleted, StatusCode::OK, "the delete answers 200");
    assert_eq!(deleted.json_value(), json!({}), "with the empty object");
    assert!(
        !dir.path().join("terms-of-service.html").exists(),
        "the file is gone"
    );
    let after_delete = TestRequest::get(PUBLIC_TOS).send(&app).await;
    common::assert_answer(&after_delete, StatusCode::NOT_FOUND, "and both reads 404");
    let delete_again = TestRequest::delete(DOC_TOS).bearer(&token).send(&app).await;
    common::assert_answer(&delete_again, StatusCode::NOT_FOUND, "Pleroma's enoent 404");
}

/// A re-upload replaces the document whole and leaves no staging file:
/// the atomic write swaps bytes, it does not append or linger.
#[tokio::test]
async fn a_reupload_replaces_the_document_whole() {
    let db = common::test_db().await;
    let pool = db.pool().clone();
    let dir = tempfile::tempdir().expect("a docs dir");
    let app = common::test_app_with_instance_docs_dir(pool.clone(), dir.path());
    let token = admin(&pool, "root", "admin:write").await;

    for body in [b"<h1>Terms</h1>".as_slice(), b"<h1>Terms v2</h1>"] {
        let uploaded = TestRequest::patch(DOC_TOS)
            .bearer(&token)
            .multipart(
                BOUNDARY,
                &[MultipartPart::file(
                    "file",
                    "terms.html",
                    "text/html",
                    Bytes::from_static(body),
                )],
            )
            .send(&app)
            .await;
        common::assert_answer(&uploaded, StatusCode::OK, "the upload answers");
    }
    let served = TestRequest::get(PUBLIC_TOS).send(&app).await;
    assert_eq!(served.text(), "<h1>Terms v2</h1>", "whole, not appended");

    let strays = std::fs::read_dir(dir.path())
        .expect("the dir reads")
        .filter_map(Result::ok)
        .count();
    assert_eq!(strays, 1, "no staging file survives");
}

/// The allow-list and the traversal gate: an unlisted name is the 404 on
/// every document route, and `../`-shaped names — percent-encoded or
/// raw — never reach the filesystem, on the write or on either read.
#[tokio::test]
async fn unlisted_and_traversing_names_are_the_404() {
    let db = common::test_db().await;
    let pool = db.pool().clone();
    let dir = tempfile::tempdir().expect("a docs dir");
    let app = common::test_app_with_instance_docs_dir(pool.clone(), dir.path());
    let token = admin(&pool, "root", "admin:read admin:write").await;

    for path in [
        "/api/v1/pleroma/admin/instance_document/privacy",
        "/api/v1/pleroma/admin/instance_document/%2e%2e%2fevil",
        "/api/v1/pleroma/admin/instance_document/..%2f..%2fetc%2fpasswd",
        "/api/v1/pleroma/admin/instance_document/%2e",
        "/api/v1/pleroma/admin/instance_document/terms-of-service.html",
    ] {
        let refused = TestRequest::get(path).bearer(&token).send(&app).await;
        common::assert_answer(&refused, StatusCode::NOT_FOUND, &format!("GET {path}"));
        let upload = TestRequest::patch(path)
            .bearer(&token)
            .multipart(
                BOUNDARY,
                &[MultipartPart::file(
                    "file",
                    "x.html",
                    "text/html",
                    Bytes::from_static(b"<p>nope</p>"),
                )],
            )
            .send(&app)
            .await;
        common::assert_answer(&upload, StatusCode::NOT_FOUND, &format!("PATCH {path}"));
        let removed = TestRequest::delete(path).bearer(&token).send(&app).await;
        common::assert_answer(&removed, StatusCode::NOT_FOUND, &format!("DELETE {path}"));
    }
    for stray in [
        "evil.html",
        "passwd",
        "privacy.html",
        "terms-of-service.html.html",
    ] {
        assert!(
            !dir.path().join(stray).exists(),
            "the store holds no `{stray}` after the refusals"
        );
    }

    let public_slip = TestRequest::get("/instance/%2e%2e%2f.env").send(&app).await;
    common::assert_answer(
        &public_slip,
        StatusCode::NOT_FOUND,
        "the public traversal 404s",
    );
    let raw_slip = TestRequest::get("/instance/../uploads/x.html")
        .send(&app)
        .await;
    common::assert_answer(
        &raw_slip,
        StatusCode::NOT_FOUND,
        "the raw ../ never matches",
    );
    assert_eq!(raw_slip.json_value()["error"], NOT_FOUND, "as the JSON 404");
}

/// The upload's own gates: the body must carry the `file` part, and the
/// file must fit 1 MiB — exactly 1 MiB lands, one byte more is the 413
/// naming the ceiling, and the rejected upload changed nothing.
#[tokio::test]
async fn the_upload_reads_its_file_part_at_its_ceiling() {
    let db = common::test_db().await;
    let pool = db.pool().clone();
    let dir = tempfile::tempdir().expect("a docs dir");
    let app = common::test_app_with_instance_docs_dir(pool.clone(), dir.path());
    let token = admin(&pool, "root", "admin:write").await;

    let no_file = TestRequest::patch(DOC_TOS)
        .bearer(&token)
        .multipart(BOUNDARY, &[MultipartPart::text("not_the_file", "hello")])
        .send(&app)
        .await;
    common::assert_answer(&no_file, StatusCode::BAD_REQUEST, "no file part is a 400");
    common::assert_error(&no_file, "Missing parameter: file", "the named parameter");

    let json_body = TestRequest::patch(DOC_TOS)
        .bearer(&token)
        .json(&json!({"content": "<h1>x</h1>"}))
        .send(&app)
        .await;
    common::assert_answer(
        &json_body,
        StatusCode::BAD_REQUEST,
        "a json body has no file",
    );

    let fits = upload_sized(&app, &token, MAX_DOC).await;
    common::assert_answer(&fits, StatusCode::OK, "exactly 1 MiB uploads");
    assert_eq!(
        std::fs::read(dir.path().join("terms-of-service.html"))
            .expect("the file reads")
            .len(),
        MAX_DOC,
        "the whole file landed"
    );

    let oversized = upload_sized(&app, &token, MAX_DOC + 1).await;
    common::assert_answer(
        &oversized,
        StatusCode::PAYLOAD_TOO_LARGE,
        "1 MiB + 1 is a 413",
    );
    assert!(
        oversized.text().contains("1048576"),
        "the refusal names the ceiling: {}",
        oversized.text()
    );
    assert_eq!(
        std::fs::read(dir.path().join("terms-of-service.html"))
            .expect("the file still reads")
            .len(),
        MAX_DOC,
        "the rejected upload changed nothing"
    );
}

/// Uploads a `terms-of-service` document whose file is `size` bytes of
/// `x`, and answers what the route said.
async fn upload_sized(app: &axum::Router, token: &str, size: usize) -> common::TestResponse {
    TestRequest::patch(DOC_TOS)
        .bearer(token)
        .multipart(
            BOUNDARY,
            &[MultipartPart::file(
                "file",
                "big.html",
                "text/html",
                Bytes::from(vec![b'x'; size]),
            )],
        )
        .send(app)
        .await
}

/// A name list is what the config says: an operator's `about` joins the
/// routes and the stock defaults stop existing when replaced.
#[tokio::test]
async fn the_configured_names_are_the_allow_list() {
    let db = common::test_db().await;
    let pool = db.pool().clone();
    let dir = tempfile::tempdir().expect("a docs dir");
    let mut config = common::test_config_with_instance_docs_dir(dir.path());
    config.statics.documents.names = vec!["about".to_owned()];
    let app = common::test_app_with_config(pool.clone(), config);
    let token = admin(&pool, "root", "admin:write").await;

    let uploaded = TestRequest::patch("/api/v1/pleroma/admin/instance_document/about")
        .bearer(&token)
        .multipart(
            BOUNDARY,
            &[MultipartPart::file(
                "file",
                "about.html",
                "text/html",
                Bytes::from_static(b"<p>about</p>"),
            )],
        )
        .send(&app)
        .await;
    common::assert_answer(&uploaded, StatusCode::OK, "the configured name uploads");
    assert_eq!(
        uploaded.json_value()["url"],
        "https://test.example/instance/about.html",
        "at its own URL"
    );
    let served = TestRequest::get("/instance/about.html").send(&app).await;
    common::assert_answer(&served, StatusCode::OK, "and the public route serves it");
    assert_eq!(served.text(), "<p>about</p>", "whole");

    let stock = TestRequest::patch(DOC_TOS)
        .bearer(&token)
        .multipart(
            BOUNDARY,
            &[MultipartPart::file(
                "file",
                "tos.html",
                "text/html",
                Bytes::from_static(b"<h1>terms</h1>"),
            )],
        )
        .send(&app)
        .await;
    common::assert_answer(
        &stock,
        StatusCode::NOT_FOUND,
        "a replaced list drops the defaults",
    );
}

/// The auth matrix over both areas: no token is 401, an app-only token
/// and a non-admin user are 403, and the scope gates answer per class
/// and area — `admin:read:oauth_apps` opens the listing but no write,
/// `admin:write:instance_documents` the document writes but not their
/// read, and a foreign area scope opens nothing.
#[tokio::test]
async fn the_auth_gates_answer_per_class_and_area() {
    let db = common::test_db().await;
    let pool = db.pool().clone();
    let app = common::test_app(pool.clone());
    let plain = common::create_local_user(&pool, "plain").await;
    let plain_token = common::create_app_and_token(&pool, &plain, "admin:write").await;
    let app_only = common::create_app_only_token(&pool, "admin:read").await;
    let reports_admin = admin(&pool, "reports", "admin:read:reports").await;
    let apps_read_admin = admin(&pool, "appsreader", "admin:read:oauth_apps").await;
    let docs_write_admin = admin(&pool, "docswriter", "admin:write:instance_documents").await;

    let anonymous = TestRequest::get(APPS).send(&app).await;
    common::assert_answer(&anonymous, StatusCode::UNAUTHORIZED, "no token, no listing");
    common::assert_error(&anonymous, NO_TOKEN, "the shared 401 wording");

    let app_only_read = TestRequest::get(APPS).bearer(&app_only).send(&app).await;
    common::assert_answer(&app_only_read, StatusCode::FORBIDDEN, "an app-only token");
    common::assert_error(&app_only_read, USER_REQUIRED, "needs a user");

    let plain_write = TestRequest::post(APPS)
        .bearer(&plain_token)
        .json(&json!({"name": "Nope", "redirect_uris": OOB}))
        .send(&app)
        .await;
    common::assert_answer(&plain_write, StatusCode::FORBIDDEN, "a non-admin");
    common::assert_error(&plain_write, NOT_ADMIN, "is no admin");

    let wrong_area = TestRequest::get(APPS)
        .bearer(&reports_admin)
        .send(&app)
        .await;
    common::assert_answer(&wrong_area, StatusCode::FORBIDDEN, "a foreign area scope");
    common::assert_error(&wrong_area, NO_ADMIN_READ, "names the class root");

    let read_write = TestRequest::post(APPS)
        .bearer(&apps_read_admin)
        .json(&json!({"name": "X", "redirect_uris": OOB}))
        .send(&app)
        .await;
    common::assert_answer(
        &read_write,
        StatusCode::FORBIDDEN,
        "a read grant writes nothing",
    );
    common::assert_error(&read_write, NO_ADMIN_WRITE, "naming the write root");

    let docs_read = TestRequest::get(DOC_TOS)
        .bearer(&docs_write_admin)
        .send(&app)
        .await;
    common::assert_answer(
        &docs_read,
        StatusCode::FORBIDDEN,
        "a documents-write grant reads nothing",
    );
    common::assert_error(&docs_read, NO_ADMIN_READ, "the read root");

    let docs_write = TestRequest::delete(DOC_TOS)
        .bearer(&docs_write_admin)
        .send(&app)
        .await;
    common::assert_answer(
        &docs_write,
        StatusCode::NOT_FOUND,
        "the right grant reaches the handler (which 404s the empty store)",
    );

    let listing = TestRequest::get(APPS)
        .bearer(&apps_read_admin)
        .send(&app)
        .await;
    common::assert_answer(
        &listing,
        StatusCode::OK,
        "the narrow read scope opens its own area",
    );
}
