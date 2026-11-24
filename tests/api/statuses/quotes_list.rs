//! `GET /api/v1/statuses/{id}/quotes` and its Pleroma twin: the live
//! quoters newest-first as rendered entities, the page narrowed to what
//! this viewer may see, `limit`/`max_id`/`min_id`/`since_id` walked with a
//! `Link` header keyed on the first and last rendered quoter row ids, the
//! empty page without a link, the shared 404 target and the show route's
//! optional-auth gate.

use std::cmp::Reverse;

use axum::{Router, http::StatusCode};
use serde_json::json;
use sqlx::PgPool;
use uuid::Uuid;
use wired_be::{
    config::TEST_BASE_URL,
    models::status::{Status, Visibility},
};

use super::support::{
    INVALID_TOKEN, NO_SCOPE, NOT_FOUND, SCOPES, UNKNOWN_ID, deleted, quote_of, user_with_token,
};
use crate::common;

/// The quoting listing's two mounts: the path prefix and a name for the
/// assertions, so one test body runs both routes.
const QUOTES_MOUNTS: [(&str, &str); 2] = [
    ("/api/v1", "the Mastodon route"),
    ("/api/v1/pleroma", "the Pleroma twin"),
];

/// The quoting listing's path on `mount` for status id `id`, with an
/// optional query suffix. The id stays text so the miss probes can feed any
/// spelling as the path segment.
fn quotes_path(mount: &str, id: &str, query: &str) -> String {
    let base = format!("{mount}/statuses/{id}/quotes");
    if query.is_empty() {
        base
    } else {
        format!("{base}?{query}")
    }
}

/// `GET {path}` as `token` (`None`: anonymous).
async fn fetch(app: &Router, path: &str, token: Option<&str>) -> common::TestResponse {
    let request = common::TestRequest::get(path);
    match token {
        Some(found) => request.bearer(found).send(app).await,
        None => request.send(app).await,
    }
}

/// The ids as the wire spells them, for an expectation.
fn texts(ids: &[Uuid]) -> Vec<String> {
    ids.iter().map(ToString::to_string).collect()
}

/// The fixture rows' ids newest-first: same-millisecond `UUIDv7`s sort by
/// their random bits, not by the order the fixtures ran in, so the expected
/// page order comes from the ids (decision 4), never from the seeding order.
fn newest_first(rows: &[&Status]) -> Vec<Uuid> {
    let mut ids: Vec<Uuid> = rows.iter().map(|row| row.id).collect();
    ids.sort_unstable_by_key(|id| Reverse(*id));
    ids
}

/// A `Link` header keyed on quoter row ids: `next` walks to `oldest`, `prev`
/// back to `newest`, both on the route's own `path`.
fn assert_quoter_keyed_link(link: Option<&str>, path: &str, newest: Uuid, oldest: Uuid) {
    let value = link.expect("a non-empty page carries a Link header");
    assert!(
        value.contains(&format!(
            "{TEST_BASE_URL}{path}?max_id={oldest}>; rel=\"next\""
        )),
        "next keys on the oldest rendered quoter row id: {value}"
    );
    assert!(
        value.contains(&format!(
            "{TEST_BASE_URL}{path}?min_id={newest}>; rel=\"prev\""
        )),
        "prev keys on the newest rendered quoter row id: {value}"
    );
}

/// Both mounts answer every live quoter of the named status as a rendered
/// [`Status`] entity, newest quoter first.
#[tokio::test]
async fn the_quotes_routes_answer_the_quoters_newest_first() {
    let db = common::test_db().await;
    let pool = db.pool().clone();
    let app = common::test_app(pool.clone());
    let (alice, _token) = user_with_token(&pool, "alice", SCOPES).await;
    let original = common::create_status(&pool, &alice, "quote me", Visibility::Public).await;
    let id = original.id.to_string();
    let mut quoters: Vec<(Status, String)> = Vec::new();
    for n in 0..3 {
        let nickname = format!("quoter{n}");
        let author = common::create_local_user(&pool, &nickname).await;
        let quote = quote_of(&pool, &author, &original, Visibility::Public).await;
        quoters.push((quote, nickname));
    }
    quoters.sort_unstable_by_key(|(row, _)| Reverse(row.id));
    let want_ids: Vec<String> = quoters.iter().map(|(row, _)| row.id.to_string()).collect();
    let want_accts: Vec<&str> = quoters
        .iter()
        .map(|(_, nickname)| nickname.as_str())
        .collect();

    for (mount, route) in QUOTES_MOUNTS {
        let page = fetch(&app, &quotes_path(mount, &id, ""), None).await;
        common::assert_answer(&page, StatusCode::OK, route);
        let body = page.json_value();
        assert_eq!(
            common::ids_of(&body),
            want_ids,
            "{route}: every quoter, newest first"
        );
        let accts: Vec<&str> = body
            .as_array()
            .expect("the answer is an array")
            .iter()
            .map(|entry| entry["account"]["acct"].as_str().expect("an account"))
            .collect();
        assert_eq!(
            accts, want_accts,
            "{route}: each entry speaks as its own author"
        );
        assert_eq!(
            body[0]["uri"],
            format!("{TEST_BASE_URL}/objects/{}", want_ids[0]),
            "{route}: an entry is the quoter row's own entity"
        );
    }
}

/// The quoting page is narrowed for the viewer: somebody else's `private`
/// quoter drops out, the viewer's own stays, and the anonymous request sees
/// only the `public` and `unlisted` quoters.
#[tokio::test]
async fn the_quotes_page_drops_what_the_viewer_may_not_see() {
    let db = common::test_db().await;
    let pool = db.pool().clone();
    let app = common::test_app(pool.clone());
    let (alice, alice_token) = user_with_token(&pool, "alice", SCOPES).await;
    let original = common::create_status(&pool, &alice, "quote me", Visibility::Public).await;
    let id = original.id.to_string();
    let bob = common::create_local_user(&pool, "bob").await;
    let carol = common::create_local_user(&pool, "carol").await;
    let dave = common::create_local_user(&pool, "dave").await;
    let public = quote_of(&pool, &bob, &original, Visibility::Public).await;
    let quiet = quote_of(&pool, &carol, &original, Visibility::Unlisted).await;
    let hidden = quote_of(&pool, &dave, &original, Visibility::Private).await;
    let own = quote_of(&pool, &alice, &original, Visibility::Private).await;

    let as_alice = fetch(
        &app,
        &quotes_path("/api/v1", &id, ""),
        Some(alice_token.as_str()),
    )
    .await;
    common::assert_answer(&as_alice, StatusCode::OK, "alice reads her post's quoters");
    let alice_page = common::ids_of(&as_alice.json_value());
    assert_eq!(
        alice_page,
        texts(&newest_first(&[&public, &quiet, &own])),
        "the stranger's private quoter drops out and alice's own quote stays"
    );
    assert!(
        !alice_page.contains(&hidden.id.to_string()),
        "dave's private quote is nobody's page entry"
    );

    let anonymous = fetch(&app, &quotes_path("/api/v1/pleroma", &id, ""), None).await;
    common::assert_answer(&anonymous, StatusCode::OK, "the anonymous Pleroma twin");
    assert_eq!(
        common::ids_of(&anonymous.json_value()),
        texts(&newest_first(&[&public, &quiet])),
        "the anonymous page carries only the public and unlisted quoters"
    );
}

/// Three public quotes of `original` by fresh accounts, the rows in
/// newest-first order (decision 4: the id is the order, and same-millisecond
/// `UUIDv7`s sort by their random bits, not by the seeding order).
async fn quoted_three_times(pool: &PgPool, original: &Status) -> Vec<Status> {
    let mut rows: Vec<Status> = Vec::new();
    for n in 0..3 {
        let author = common::create_local_user(pool, &format!("quoter{n}")).await;
        rows.push(quote_of(pool, &author, original, Visibility::Public).await);
    }
    rows.sort_unstable_by_key(|row| Reverse(row.id));
    rows
}

/// Both mounts page the quoting listing with `limit` and `max_id`, carry the
/// `Link` header keyed on the first and last rendered quoter row ids, and
/// answer a walk past the end with the empty page and no link.
#[tokio::test]
async fn the_quotes_routes_page_with_limit_and_max_id_and_link_their_pages() {
    let db = common::test_db().await;
    let pool = db.pool().clone();
    let app = common::test_app(pool.clone());
    let (alice, _token) = user_with_token(&pool, "alice", SCOPES).await;
    let original = common::create_status(&pool, &alice, "quote me", Visibility::Public).await;
    let id = original.id.to_string();
    let all: Vec<Uuid> = quoted_three_times(&pool, &original)
        .await
        .iter()
        .map(|row| row.id)
        .collect();

    for (mount, route) in QUOTES_MOUNTS {
        let base = quotes_path(mount, &id, "");
        let page = fetch(&app, &base, None).await;
        common::assert_answer(&page, StatusCode::OK, route);
        assert_eq!(
            common::ids_of(&page.json_value()),
            texts(&all),
            "{route}: the whole page"
        );
        assert_quoter_keyed_link(page.header("link"), &base, all[0], all[2]);

        let first = fetch(&app, &quotes_path(mount, &id, "limit=2"), None).await;
        assert_eq!(
            common::ids_of(&first.json_value()),
            texts(&all[..2]),
            "{route}: the limit cuts the page at the newest two"
        );
        assert_quoter_keyed_link(first.header("link"), &base, all[0], all[1]);

        let tail = fetch(
            &app,
            &quotes_path(mount, &id, &format!("max_id={}", all[1])),
            None,
        )
        .await;
        assert_eq!(
            common::ids_of(&tail.json_value()),
            texts(&all[2..]),
            "{route}: max_id walks to the older quoters, keyed on quoter row ids"
        );

        let past = fetch(
            &app,
            &quotes_path(mount, &id, &format!("max_id={}", all[2])),
            None,
        )
        .await;
        assert_eq!(
            past.json_value(),
            json!([]),
            "{route}: a walk past the oldest is empty"
        );
        assert_eq!(
            past.header("link"),
            None,
            "{route}: an empty page sends no Link header"
        );
    }
}

/// The forward cursors answer on both mounts — `min_id` and `since_id` give
/// the newer quoters, newest-first — a malformed cursor is the shared 400,
/// and a tombstoned quoter leaves the page and its `Link` edges alike: the
/// cursors name only the rows that actually came back.
#[tokio::test]
async fn the_quotes_routes_walk_the_forward_cursors_and_the_link_edges() {
    let db = common::test_db().await;
    let pool = db.pool().clone();
    let app = common::test_app(pool.clone());
    let (alice, _token) = user_with_token(&pool, "alice", SCOPES).await;
    let original = common::create_status(&pool, &alice, "quote me", Visibility::Public).await;
    let id = original.id.to_string();
    let rows = quoted_three_times(&pool, &original).await;
    let all: Vec<Uuid> = rows.iter().map(|row| row.id).collect();

    for (mount, route) in QUOTES_MOUNTS {
        let forward = fetch(
            &app,
            &quotes_path(mount, &id, &format!("min_id={}", all[2])),
            None,
        )
        .await;
        assert_eq!(
            common::ids_of(&forward.json_value()),
            texts(&all[..2]),
            "{route}: min_id answers the newer page, still newest-first"
        );

        let fresh = fetch(
            &app,
            &quotes_path(mount, &id, &format!("since_id={}", all[1])),
            None,
        )
        .await;
        assert_eq!(
            common::ids_of(&fresh.json_value()),
            texts(&all[..1]),
            "{route}: since_id polls the newer rows, descending like a first page"
        );

        let malformed = fetch(&app, &quotes_path(mount, &id, "min_id=nope"), None).await;
        common::assert_answer(&malformed, StatusCode::BAD_REQUEST, "a bogus cursor");
        common::assert_error(&malformed, "Invalid id", "a bogus cursor");
    }

    deleted(&pool, &rows[2]).await;
    let base = quotes_path("/api/v1", &id, "");
    let rest = fetch(&app, &base, None).await;
    assert_eq!(
        common::ids_of(&rest.json_value()),
        texts(&all[..2]),
        "a tombstoned quoter leaves the page"
    );
    assert_quoter_keyed_link(rest.header("link"), &base, all[0], all[1]);
}

/// A status nobody quotes answers the empty array with no `Link` header, on
/// both mounts.
#[tokio::test]
async fn the_quotes_routes_answer_the_empty_page() {
    let db = common::test_db().await;
    let pool = db.pool().clone();
    let app = common::test_app(pool.clone());
    let (alice, _token) = user_with_token(&pool, "alice", SCOPES).await;
    let quiet = common::create_status(&pool, &alice, "unquoted", Visibility::Public).await;
    let id = quiet.id.to_string();

    for (mount, route) in QUOTES_MOUNTS {
        let empty = fetch(&app, &quotes_path(mount, &id, ""), None).await;
        common::assert_answer(&empty, StatusCode::OK, route);
        assert_eq!(
            empty.json_value(),
            json!([]),
            "{route}: the empty page is []"
        );
        assert_eq!(
            empty.header("link"),
            None,
            "{route}: and it sends no Link header"
        );
    }
}

/// The target resolves through the shared status gate: the malformed id, the
/// unknown id, a deleted target and one the caller may not see are the one
/// 404 `{"error":"Record not found"}` on both mounts, quoters or not.
#[tokio::test]
async fn the_quotes_routes_answer_the_shared_gate_with_not_found() {
    let db = common::test_db().await;
    let pool = db.pool().clone();
    let app = common::test_app(pool.clone());
    let (alice, alice_token) = user_with_token(&pool, "alice", SCOPES).await;
    let (bob, _token) = user_with_token(&pool, "bob", SCOPES).await;
    let quoted = common::create_status(&pool, &alice, "quote me", Visibility::Public).await;
    let carol = common::create_local_user(&pool, "carol").await;
    let _live = quote_of(&pool, &carol, &quoted, Visibility::Public).await;
    let doomed = common::create_status(&pool, &alice, "was quoted", Visibility::Public).await;
    let dave = common::create_local_user(&pool, "dave").await;
    let _kept = quote_of(&pool, &dave, &doomed, Visibility::Public).await;
    deleted(&pool, &doomed).await;
    let secret = common::create_status(&pool, &bob, "not for you", Visibility::Private).await;

    let probes: Vec<(String, &str)> = vec![
        ("not-an-id".to_owned(), "text that is not a UUID"),
        (UNKNOWN_ID.to_owned(), "a well-formed id that names nothing"),
        (doomed.id.to_string(), "a deleted target with a live quoter"),
        (secret.id.to_string(), "a target the caller may not see"),
    ];
    for (mount, route) in QUOTES_MOUNTS {
        for (probe, who) in &probes {
            let anonymous = fetch(&app, &quotes_path(mount, probe, ""), None).await;
            let as_alice = fetch(
                &app,
                &quotes_path(mount, probe, ""),
                Some(alice_token.as_str()),
            )
            .await;
            common::assert_all_refused(
                &[anonymous, as_alice],
                StatusCode::NOT_FOUND,
                NOT_FOUND,
                &format!("{route} for {who}"),
            );
        }
    }
}

/// Both mounts run the show route's optional-auth gate: the scoped user
/// token and the anonymous request answer, an app-only token proceeds
/// unauthenticated, a user token without `read:statuses` is the 403, and a
/// token that does not resolve is the 401.
#[tokio::test]
async fn the_quotes_routes_run_the_optional_read_statuses_gate() {
    let db = common::test_db().await;
    let pool = db.pool().clone();
    let (alice, alice_token) = user_with_token(&pool, "alice", SCOPES).await;
    let (_stranger, narrow_token) = user_with_token(&pool, "stranger", "read:accounts").await;
    let app_only = common::create_app_only_token(&pool, SCOPES).await;
    let app = common::test_app(pool.clone());
    let original = common::create_status(&pool, &alice, "quote me", Visibility::Public).await;
    let id = original.id.to_string();
    let quoter = common::create_local_user(&pool, "quoter").await;
    let quote = quote_of(&pool, &quoter, &original, Visibility::Public).await;

    for (mount, route) in QUOTES_MOUNTS {
        let path = quotes_path(mount, &id, "");
        let scoped = fetch(&app, &path, Some(alice_token.as_str())).await;
        common::assert_answer(&scoped, StatusCode::OK, route);
        assert_eq!(
            common::ids_of(&scoped.json_value()),
            vec![quote.id.to_string()],
            "{route}: the scoped token's page"
        );

        let anonymous = fetch(&app, &path, None).await;
        common::assert_answer(&anonymous, StatusCode::OK, "no token proceeds");

        let ownerless = fetch(&app, &path, Some(app_only.as_str())).await;
        common::assert_answer(
            &ownerless,
            StatusCode::OK,
            "an app-only token proceeds unauthenticated",
        );

        let refused = fetch(&app, &path, Some(narrow_token.as_str())).await;
        common::assert_answer(
            &refused,
            StatusCode::FORBIDDEN,
            "a user token without the scope",
        );
        common::assert_error(&refused, NO_SCOPE, "a user token without the scope");

        let invalid = fetch(&app, &path, Some("token-that-was-never-minted")).await;
        common::assert_answer(
            &invalid,
            StatusCode::UNAUTHORIZED,
            "a token that does not resolve",
        );
        common::assert_error(&invalid, INVALID_TOKEN, "a token that does not resolve");
    }
}
