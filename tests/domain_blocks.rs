//! Integration tests for `/api/v1/domain_blocks`: adding, listing and
//! removing domain blocks as a user's own set — the plain JSON array of
//! domain strings with keyset pagination whose cursors are the
//! `user_domain_blocks` row ids (and no `Link` header on an empty page),
//! the writes' `{}` answers, the normalisation that stores one canonical
//! spelling per block (so duplicates are idempotent whatever the
//! spelling), `Missing parameter: domain` as the 400 and `Invalid domain`
//! as the 422, `DELETE` reading its parameter from the query, a form body
//! or a JSON body, `AuthUser` in front of everything with `read:blocks`
//! for the listing and `write:blocks | follow` for the writes — and the
//! `domain_blocking` flag of `GET /api/v1/accounts/relationships`, true
//! for a remote account on a blocked domain and never lit for a local
//! one.

#![cfg(test)]

// `pub` because Clippy's nursery `redundant_pub_crate` forbids `pub(crate)`
// items inside a private module, while `unreachable_pub` forbids `pub` ones:
// only a `pub` module makes the harness helpers lint-clean.
pub mod common;

use std::cmp::Reverse;

use axum::{Router, http::StatusCode};
use serde_json::{Value, json};
use sqlx::PgPool;
use uuid::Uuid;
use wired_be::models::user::User;

/// The instance origin the fixtures live on (`Config::for_tests`).
const BASE: &str = "https://test.example";
/// The listing's path, as the `Link` header links back to it.
const BLOCKS: &str = "/api/v1/domain_blocks";
/// The 401 body `AuthUser` renders when no token was presented at all.
const NO_TOKEN: &str = r#"{"error":"Invalid credentials"}"#;
/// The 401 body for a token that does not resolve.
const INVALID_TOKEN: &str = r#"{"error":"The access token is invalid"}"#;
/// The 403 body an app-only token gets from the user-requiring extractor.
const USER_REQUIRED: &str = r#"{"error":"This API requires an authenticated user"}"#;
/// The 403 body for a listing token without `read:blocks`.
const NO_READ_SCOPE: &str = r#"{"error":"Insufficient permissions: read:blocks."}"#;
/// The 403 body for a write token without either accepted scope;
/// `require_any` joins the alternatives with the `|` operator.
const NO_WRITE_SCOPE: &str = r#"{"error":"Insufficient permissions: write:blocks | follow."}"#;
/// The 400 body for a write that names no domain.
const MISSING_DOMAIN: &str = r#"{"error":"Missing parameter: domain"}"#;
/// The 422 body for a domain that normalises to nothing blockable.
const INVALID_DOMAIN: &str = r#"{"error":"Invalid domain"}"#;
/// The 400 body of a malformed page cursor (decision 8, Pleroma's text).
const INVALID_ID: &str = r#"{"error":"Invalid id"}"#;
/// The `{}` body both writes answer (Pleroma's `json(conn, %{})`).
const EMPTY_OBJECT: &str = "{}";

/// One fixture block: the `user_domain_blocks` row id the `Link` header
/// keys on, next to the domain string the listing answers.
type Block = (Uuid, String);

/// A test verb, so the auth and scope loops can drive all three routes
/// from one body.
#[derive(Clone, Copy, Debug)]
enum Verb {
    /// `GET /api/v1/domain_blocks`.
    Get,
    /// `POST /api/v1/domain_blocks`.
    Post,
    /// `DELETE /api/v1/domain_blocks`.
    Delete,
}

/// All three routes, the loop fixtures run over.
const ALL_VERBS: [Verb; 3] = [Verb::Get, Verb::Post, Verb::Delete];

impl Verb {
    /// The start of a request for `path` under this verb.
    fn request(self, path: &str) -> common::TestRequest {
        match self {
            Self::Get => common::TestRequest::get(path),
            Self::Post => common::TestRequest::post(path),
            Self::Delete => common::TestRequest::delete(path),
        }
    }
}

/// `GET /api/v1/domain_blocks{query}` as `token` (`None` anonymous).
async fn list(app: &Router, token: Option<&str>, query: &str) -> common::TestResponse {
    let request = Verb::Get.request(&format!("{BLOCKS}{query}"));
    match token {
        Some(found) => request.bearer(found).send(app).await,
        None => request.send(app).await,
    }
}

/// `POST /api/v1/domain_blocks` as `token`, body `body`.
async fn post_json(app: &Router, token: &str, body: &Value) -> common::TestResponse {
    Verb::Post
        .request(BLOCKS)
        .bearer(token)
        .json(body)
        .send(app)
        .await
}

/// `POST /api/v1/domain_blocks` as `token` with a form body naming
/// `domain`.
async fn post_form(app: &Router, token: &str, domain: &str) -> common::TestResponse {
    Verb::Post
        .request(BLOCKS)
        .bearer(token)
        .form(&[("domain", domain)])
        .send(app)
        .await
}

/// `DELETE /api/v1/domain_blocks{query}` as `token` (the query-carried
/// parameter form).
async fn delete_query(app: &Router, token: &str, query: &str) -> common::TestResponse {
    Verb::Delete
        .request(&format!("{BLOCKS}{query}"))
        .bearer(token)
        .send(app)
        .await
}

/// Blocks `domain` over the JSON body, asserting the write answered its
/// `{}` — the fixture every listing test starts from.
async fn block(app: &Router, token: &str, domain: &str) {
    let response = post_json(app, token, &json!({ "domain": domain })).await;
    assert_eq!(response.status, StatusCode::OK, "`{domain}` blocks");
    assert_eq!(response.text(), EMPTY_OBJECT, "the write answers {{}}");
}

/// Asserts a write answered the 200 empty object — idempotence means a
/// repeat is indistinguishable from the first write.
fn expect_empty_object(response: &common::TestResponse, what: &str) {
    assert_eq!(response.status, StatusCode::OK, "{what} answers a 200");
    assert_eq!(response.text(), EMPTY_OBJECT, "{what} answers {{}}");
}

/// The stored blocks of `user`, newest row first — the order the listing
/// answers in. `UUIDv7` is only millisecond precise, so expectations sort
/// from the stored ids, never from the order the fixtures were written in
/// (decision 4).
async fn stored_blocks(pool: &PgPool, user: &User) -> Vec<Block> {
    let mut rows: Vec<Block> = sqlx::query_as::<_, (Uuid, String)>(
        "SELECT id, domain FROM user_domain_blocks WHERE user_id = $1",
    )
    .bind(user.id)
    .fetch_all(pool)
    .await
    .expect("the stored blocks read");
    rows.sort_by_key(|(id, _)| Reverse(*id));
    rows
}

/// The domains of `blocks`, in the order given.
fn domains(blocks: &[Block]) -> Vec<String> {
    blocks.iter().map(|(_, domain)| domain.clone()).collect()
}

/// The `Link` header Pleroma's `add_link_headers` writes for a page of
/// the listing: `next` walks to the oldest block row, `prev` back to the
/// newest.
fn expected_link(oldest: &Uuid, newest: &Uuid) -> String {
    format!(
        "<{BASE}{BLOCKS}?max_id={oldest}>; rel=\"next\", \
         <{BASE}{BLOCKS}?min_id={newest}>; rel=\"prev\""
    )
}

/// The domain strings a listing response answered.
fn listed_domains(response: &common::TestResponse) -> Vec<String> {
    response
        .json_value()
        .as_array()
        .expect("the listing is a JSON array")
        .iter()
        .map(|value| {
            value
                .as_str()
                .expect("entries are domain strings")
                .to_owned()
        })
        .collect()
}

/// Add over HTTP (JSON and form bodies alike), list the array of domain
/// strings, remove one over the query, and see the empty listing carry no
/// `Link` header at all.
#[sqlx::test(migrator = "wired_be::MIGRATOR")]
async fn add_list_and_remove(pool: PgPool) {
    let app = common::test_app(pool.clone());
    let alice = common::create_local_user(&pool, "alice").await;
    let token = common::create_app_and_token(&pool, &alice, "read write").await;

    let empty = list(&app, Some(&token), "").await;
    assert_eq!(empty.status, StatusCode::OK, "the empty listing is a 200");
    assert_eq!(
        empty.header("content-type"),
        Some("application/json"),
        "and JSON"
    );
    assert_eq!(empty.json_value(), json!([]), "the empty array");
    assert_eq!(empty.header("link"), None, "with nothing to link from");

    block(&app, &token, "spammers.example").await;
    expect_empty_object(
        &post_form(&app, &token, "annoying.example").await,
        "the form body",
    );

    let stored = stored_blocks(&pool, &alice).await;
    let body = list(&app, Some(&token), "").await;
    assert_eq!(
        listed_domains(&body),
        domains(&stored),
        "the array is the blocked domains, newest row first"
    );
    assert_eq!(
        body.header("link"),
        Some(expected_link(&stored[1].0, &stored[0].0).as_str()),
        "the Link cursors are the block row ids, not the domains"
    );

    expect_empty_object(
        &delete_query(&app, &token, "?domain=spammers.example").await,
        "the delete",
    );
    let remaining = stored_blocks(&pool, &alice).await;
    assert_eq!(remaining.len(), 1, "one block left");
    assert_eq!(
        listed_domains(&list(&app, Some(&token), "").await),
        domains(&remaining),
        "and the listing lost exactly that entry"
    );
}

/// `DELETE` reads its `domain` from the query string, a form body or a
/// JSON body alike.
#[sqlx::test(migrator = "wired_be::MIGRATOR")]
async fn delete_accepts_query_form_and_json_bodies(pool: PgPool) {
    let app = common::test_app(pool.clone());
    let alice = common::create_local_user(&pool, "alice").await;
    let token = common::create_app_and_token(&pool, &alice, "read write").await;
    for domain in ["q.example", "form.example", "json.example"] {
        block(&app, &token, domain).await;
    }

    expect_empty_object(
        &delete_query(&app, &token, "?domain=q.example").await,
        "the query parameter",
    );
    expect_empty_object(
        &Verb::Delete
            .request(BLOCKS)
            .bearer(&token)
            .form(&[("domain", "form.example")])
            .send(&app)
            .await,
        "the form body",
    );
    expect_empty_object(
        &Verb::Delete
            .request(BLOCKS)
            .bearer(&token)
            .json(&json!({ "domain": "json.example" }))
            .send(&app)
            .await,
        "the JSON body",
    );

    assert!(
        stored_blocks(&pool, &alice).await.is_empty(),
        "and every form took its block"
    );
}

/// The domain normalisation the store keeps: lowercase, no leading `@`,
/// no scheme, no trailing slash.
#[sqlx::test(migrator = "wired_be::MIGRATOR")]
async fn the_domain_is_stored_canonical(pool: PgPool) {
    let app = common::test_app(pool.clone());
    let alice = common::create_local_user(&pool, "alice").await;
    let token = common::create_app_and_token(&pool, &alice, "read write").await;

    for input in [
        "MixedCase.Example",
        "@mention.example",
        "https://schemed.example",
        "HTTP://TRAILING.EXAMPLE/",
        "  padded.example  ",
    ] {
        let response = post_json(&app, &token, &json!({ "domain": input })).await;
        assert_eq!(response.status, StatusCode::OK, "`{input}` is a domain");
    }

    let mut listed = listed_domains(&list(&app, Some(&token), "").await);
    listed.sort_unstable();
    assert_eq!(
        listed,
        vec![
            "mention.example",
            "mixedcase.example",
            "padded.example",
            "schemed.example",
            "trailing.example",
        ],
        "every spelling landed in its one canonical form"
    );
}

/// Blocking what the same account already blocked — in any spelling that
/// normalises alike — stays one row that keeps its id, and deleting it is
/// idempotent too: repeats answer `{}` and change nothing.
#[sqlx::test(migrator = "wired_be::MIGRATOR")]
async fn duplicate_blocks_are_idempotent(pool: PgPool) {
    let app = common::test_app(pool.clone());
    let alice = common::create_local_user(&pool, "alice").await;
    let token = common::create_app_and_token(&pool, &alice, "read write").await;

    block(&app, &token, "spam.example").await;
    let first = stored_blocks(&pool, &alice).await;
    block(&app, &token, "spam.example").await;
    expect_empty_object(
        &post_form(&app, &token, "@SPAM.Example").await,
        "the other spelling",
    );

    let stored = stored_blocks(&pool, &alice).await;
    assert_eq!(stored, first, "the repeat blocks wrote nothing new");
    assert_eq!(
        listed_domains(&list(&app, Some(&token), "").await),
        vec!["spam.example".to_owned()],
        "one entry, the canonical spelling"
    );

    expect_empty_object(
        &delete_query(&app, &token, "?domain=https://spam.example/").await,
        "the canonicalising delete",
    );
    expect_empty_object(
        &delete_query(&app, &token, "?domain=spam.example").await,
        "the repeat delete",
    );
    assert_eq!(
        listed_domains(&list(&app, Some(&token), "").await),
        Vec::<String>::new(),
        "and the block is gone for good"
    );
}

/// A write that names no domain — absent, empty, or blank — is the 400
/// `Missing parameter: domain`; a domain that normalises to no dotted
/// host is the 422 `Invalid domain`. Both on `POST` and on `DELETE`.
#[sqlx::test(migrator = "wired_be::MIGRATOR")]
async fn missing_domain_is_400_and_invalid_domain_is_422(pool: PgPool) {
    let app = common::test_app(pool.clone());
    let alice = common::create_local_user(&pool, "alice").await;
    let token = common::create_app_and_token(&pool, &alice, "read write").await;

    for body in [
        json!({}),
        json!({ "domain": "" }),
        json!({ "domain": "   " }),
    ] {
        let post = post_json(&app, &token, &body).await;
        assert_eq!(post.status, StatusCode::BAD_REQUEST, "{body} POSTs 400");
        assert_eq!(post.text(), MISSING_DOMAIN, "the missing-parameter body");

        let delete = Verb::Delete
            .request(BLOCKS)
            .bearer(&token)
            .json(&body)
            .send(&app)
            .await;
        assert_eq!(delete.status, StatusCode::BAD_REQUEST, "{body} DELETEs 400");
        assert_eq!(delete.text(), MISSING_DOMAIN);
    }

    for domain in ["nodot", "localhost", "@", "https://"] {
        let post = post_json(&app, &token, &json!({ "domain": domain })).await;
        assert_eq!(
            post.status,
            StatusCode::UNPROCESSABLE_ENTITY,
            "`{domain}` is no domain"
        );
        assert_eq!(post.text(), INVALID_DOMAIN, "the invalid-domain body");

        let delete = delete_query(&app, &token, &format!("?domain={domain}")).await;
        assert_eq!(
            delete.status,
            StatusCode::UNPROCESSABLE_ENTITY,
            "`{domain}` is no domain to delete either"
        );
        assert_eq!(delete.text(), INVALID_DOMAIN);
    }

    assert!(
        stored_blocks(&pool, &alice).await.is_empty(),
        "and nothing of the refusals was stored"
    );
}

/// The page walks by `user_domain_blocks` row id: the `Link` header's
/// cursors, the `max_id` continuation, the `min_id` page (answered
/// newest-first again), and the ends of the walk, which carry no header.
#[sqlx::test(migrator = "wired_be::MIGRATOR")]
async fn the_listing_pages_key_on_block_row_ids_and_link(pool: PgPool) {
    let app = common::test_app(pool.clone());
    let alice = common::create_local_user(&pool, "alice").await;
    let token = common::create_app_and_token(&pool, &alice, "read write").await;
    for n in 0..5 {
        block(&app, &token, &format!("block{n}.example")).await;
    }
    let stored = stored_blocks(&pool, &alice).await;

    let first = list(&app, Some(&token), "?limit=2").await;
    assert_eq!(
        listed_domains(&first),
        vec![stored[0].1.clone(), stored[1].1.clone()],
        "two newest first"
    );
    assert_eq!(
        first.header("link"),
        Some(expected_link(&stored[1].0, &stored[0].0).as_str()),
        "the Link cursors are the page's block row ids"
    );

    let older = list(
        &app,
        Some(&token),
        &format!("?limit=2&max_id={}", stored[1].0),
    )
    .await;
    assert_eq!(
        listed_domains(&older),
        vec![stored[2].1.clone(), stored[3].1.clone()],
        "max_id continues where the page ended"
    );

    let newer = list(&app, Some(&token), &format!("?min_id={}", stored[3].0)).await;
    assert_eq!(
        listed_domains(&newer),
        vec![
            stored[0].1.clone(),
            stored[1].1.clone(),
            stored[2].1.clone()
        ],
        "min_id answers the newest-first page just after the cursor"
    );

    let polled = list(&app, Some(&token), &format!("?since_id={}", stored[0].0)).await;
    assert_eq!(
        polled.json_value(),
        json!([]),
        "nothing is newer than the head"
    );
    assert_eq!(
        polled.header("link"),
        None,
        "and an empty page sends no header"
    );

    let past = list(&app, Some(&token), &format!("?max_id={}", stored[4].0)).await;
    assert_eq!(past.status, StatusCode::OK, "walking off the end is a 200");
    assert_eq!(past.json_value(), json!([]), "with the empty array");
    assert_eq!(past.header("link"), None, "and no header");

    let malformed = list(&app, Some(&token), "?max_id=not-an-id").await;
    assert_eq!(
        malformed.status,
        StatusCode::BAD_REQUEST,
        "a bad cursor is Pleroma's 400"
    );
    assert_eq!(malformed.text(), INVALID_ID);
}

/// All three routes demand a user token: no token is the 401, an invalid
/// one the 401, and an app-only credential the 403.
#[sqlx::test(migrator = "wired_be::MIGRATOR")]
async fn the_listing_requires_a_user_token(pool: PgPool) {
    let app_only = common::create_app_only_token(&pool, "read write follow").await;
    let app = common::test_app(pool);

    for verb in ALL_VERBS {
        let anonymous = verb.request(BLOCKS).send(&app).await;
        assert_eq!(
            anonymous.status,
            StatusCode::UNAUTHORIZED,
            "no token cannot use {verb:?}"
        );
        assert_eq!(anonymous.text(), NO_TOKEN, "AuthUser's no-token body");

        let invalid = verb.request(BLOCKS).bearer("bogus-token").send(&app).await;
        assert_eq!(
            invalid.status,
            StatusCode::UNAUTHORIZED,
            "an invalid token is not treated as anonymous ({verb:?})"
        );
        assert_eq!(invalid.text(), INVALID_TOKEN, "AuthUser's invalid body");

        let machine = verb.request(BLOCKS).bearer(&app_only).send(&app).await;
        assert_eq!(
            machine.status,
            StatusCode::FORBIDDEN,
            "an app-only token is not a user ({verb:?})"
        );
        assert_eq!(machine.text(), USER_REQUIRED, "the user-required body");
    }
}

/// The listing's scope gate is exactly `read:blocks`: broad `read` and
/// the scope itself pass; `follow` — Pleroma's write-side alias, not the
/// ticket's read side — and unrelated grants answer the
/// insufficient-permissions 403.
#[sqlx::test(migrator = "wired_be::MIGRATOR")]
async fn the_listing_requires_the_read_blocks_scope(pool: PgPool) {
    let app = common::test_app(pool.clone());
    let alice = common::create_local_user(&pool, "alice").await;

    for scopes in ["read", "read:blocks"] {
        let token = common::create_app_and_token(&pool, &alice, scopes).await;
        let response = list(&app, Some(&token), "").await;
        assert_eq!(
            response.status,
            StatusCode::OK,
            "a `{scopes}` token can list"
        );
    }

    for scopes in ["follow", "write", "read:accounts", ""] {
        let token = common::create_app_and_token(&pool, &alice, scopes).await;
        let response = list(&app, Some(&token), "").await;
        assert_eq!(
            response.status,
            StatusCode::FORBIDDEN,
            "a `{scopes}` token cannot list"
        );
        assert_eq!(response.text(), NO_READ_SCOPE, "the denial names the scope");
    }
}

/// The writes' gate is `write:blocks | follow`: broad `write`, the scope
/// itself and the `follow` root all block and unblock; anything else is
/// the alternatives-joined 403, on both `POST` and `DELETE`.
#[sqlx::test(migrator = "wired_be::MIGRATOR")]
async fn the_writes_take_write_blocks_or_follow(pool: PgPool) {
    let app = common::test_app(pool.clone());
    let alice = common::create_local_user(&pool, "alice").await;

    for (n, scopes) in ["write", "write:blocks", "follow"].iter().enumerate() {
        let token = common::create_app_and_token(&pool, &alice, scopes).await;
        let domain = format!("grant{n}.example");
        expect_empty_object(
            &post_json(&app, &token, &json!({ "domain": &domain })).await,
            &format!("a `{scopes}` POST"),
        );
        expect_empty_object(
            &delete_query(&app, &token, &format!("?domain={domain}")).await,
            &format!("a `{scopes}` DELETE"),
        );
    }

    for scopes in ["read", "read:blocks", "write:accounts"] {
        let token = common::create_app_and_token(&pool, &alice, scopes).await;
        let post = post_json(&app, &token, &json!({ "domain": "denied.example" })).await;
        assert_eq!(
            post.status,
            StatusCode::FORBIDDEN,
            "a `{scopes}` token cannot block"
        );
        assert_eq!(post.text(), NO_WRITE_SCOPE, "the denial names both");

        let delete = delete_query(&app, &token, "?domain=denied.example").await;
        assert_eq!(
            delete.status,
            StatusCode::FORBIDDEN,
            "nor unblock a `{scopes}` token"
        );
        assert_eq!(delete.text(), NO_WRITE_SCOPE);
    }

    assert!(
        stored_blocks(&pool, &alice).await.is_empty(),
        "the granted round trips left nothing behind"
    );
}

/// The `domain_blocking` flag of `GET /api/v1/accounts/relationships`:
/// true for a remote account on a domain the caller blocked (whatever
/// spelling the block was written in), false for a remote account on
/// another domain, and false for local accounts — even when the local
/// instance's own domain is on the block list.
#[sqlx::test(migrator = "wired_be::MIGRATOR")]
async fn relationships_carry_the_domain_blocking_flag(pool: PgPool) {
    let app = common::test_app(pool.clone());
    let alice = common::create_local_user(&pool, "alice").await;
    let spammer = common::create_remote_user(&pool, "bob", "spam.example").await;
    let neighbour = common::create_remote_user(&pool, "carol", "nice.example").await;
    let dave = common::create_local_user(&pool, "dave").await;
    let token = common::create_app_and_token(&pool, &alice, "read write follow").await;
    block(&app, &token, "@SPAM.Example").await;
    block(&app, &token, "test.example").await;

    let response = common::TestRequest::get(&format!(
        "/api/v1/accounts/relationships?id%5B%5D={}&id%5B%5D={}&id%5B%5D={}",
        spammer.id, neighbour.id, dave.id
    ))
    .bearer(&token)
    .send(&app)
    .await;
    assert_eq!(response.status, StatusCode::OK, "the batch answers");
    let value = response.json_value();
    let rows = value.as_array().expect("a JSON array of relationships");
    assert_eq!(rows.len(), 3, "one relationship per requested id");
    let flag = |index: usize| {
        rows[index]["domain_blocking"]
            .as_bool()
            .expect("domain_blocking is a bool")
    };
    assert!(flag(0), "the remote account on the blocked domain");
    assert!(!flag(1), "the remote account on another domain");
    assert!(
        !flag(2),
        "and a local account never, not even under its own instance's domain"
    );
}
