//! The `resolve=true` arm of the search routes: an actor URL the instance
//! has never seen fetched from the mock peer and mirrored into `accounts`,
//! a status URL fetched and rendered into `statuses` with its author left
//! out, local URLs read from the database alone, a private post visible
//! only to its author, an unreachable host answered silently, the whole
//! arm gated on an authenticated viewer and on the requested `type`, an
//! exact handle match left as it is, and the `search` bucket a resolving
//! request spends.

use axum::http::StatusCode;
use wired_be::models::{status::Visibility, user};

use super::resolve_mock::{MockPeer, dead_origin, status_rows_by_uri, stored_notes, zoe_id};
use super::support::{
    ACCOUNTS_SEARCH, BASE, THROTTLED, V1_SEARCH, V2_SEARCH, accounts_of, assert_all_lists_empty,
    assert_document, assert_list_empty, fixture, get, listed_ids, statuses_of,
};
use crate::common;

/// A resolving search on an actor URL the instance has never seen runs the
/// federation round trip: the mock peer's actor is fetched, mirrored into
/// `users`, and rides first in `accounts` — on the combined document of both
/// versions and on the flat account search alike.
#[tokio::test]
async fn resolve_fetches_an_unknown_actor_by_url() {
    let db = common::test_db().await;
    let pool = db.pool().clone();
    let (_viewer, token) = fixture(&pool, "resolver", "read").await;
    let peer = MockPeer::start().await;
    let url = peer.actor_url();
    let app = common::test_app(pool.clone());

    for path in [V1_SEARCH, V2_SEARCH] {
        let found = get(&app, Some(&token), &format!("{path}?q={url}&resolve=true")).await;
        common::assert_answer(&found, StatusCode::OK, "the resolving search");
        let body = found.json_value();
        assert_document(&body);
        assert_eq!(
            accounts_of(&body),
            vec![zoe_id(&pool, &url).await],
            "{path} answers the fetched account first"
        );
        assert_list_empty(&body, "statuses", "an actor URL is no post");
    }

    let flat = get(
        &app,
        Some(&token),
        &format!("{ACCOUNTS_SEARCH}?q={url}&resolve=true"),
    )
    .await;
    common::assert_answer(&flat, StatusCode::OK, "the flat account search");
    assert_eq!(
        listed_ids(&flat.json_value()),
        vec![zoe_id(&pool, &url).await],
        "and it answers the same account, as a flat array"
    );
    assert_eq!(
        flat.json_value()[0]["acct"],
        format!("zoe@{}", peer.authority()),
        "the handle decision 7 stores the mirror under, loopback port included"
    );
}

/// A resolving search on a status URL fetches the remote note, stores it, and
/// answers it rendered for the viewer in `statuses` — with its author left
/// out of `accounts`: the account arm ran on the rows stored before the
/// resolve, and the resolution adds nothing to it.
#[tokio::test]
async fn resolve_fetches_a_remote_status_by_url() {
    let db = common::test_db().await;
    let pool = db.pool().clone();
    let (_viewer, token) = fixture(&pool, "resolver", "read").await;
    let peer = MockPeer::start().await;
    let url = peer.object_url("first");
    let app = common::test_app(pool.clone());

    let found = get(
        &app,
        Some(&token),
        &format!("{V1_SEARCH}?q={url}&resolve=true"),
    )
    .await;
    common::assert_answer(&found, StatusCode::OK, "the resolving status search");
    let body = found.json_value();
    assert_document(&body);
    let resolved = status_rows_by_uri(&pool, &url).await;
    assert_eq!(
        statuses_of(&body),
        resolved,
        "the fetched note is the statuses list"
    );
    assert_list_empty(&body, "accounts", "its author is not duplicated there");
    let entity = &body["statuses"][0];
    assert_eq!(entity["uri"], url, "the entity carries the object id");
    assert_eq!(
        entity["content"], "<p>remote post first</p>",
        "and its content"
    );
    assert_eq!(
        entity["account"]["acct"],
        format!("zoe@{}", peer.authority()),
        "rendered with its author, port-carrying handle and all"
    );

    let repeat = get(
        &app,
        Some(&token),
        &format!("{V2_SEARCH}?q={url}&resolve=true"),
    )
    .await;
    assert_eq!(
        statuses_of(&repeat.json_value()),
        resolved,
        "the stored row answers the next search too"
    );
}

/// Every local URL form the ticket names resolves through the database, with
/// no peer to ask: the account behind `/users/{nick}`, and the post behind
/// `/objects/{id}`, `/notice/{id}` and `/users/{nick}/statuses/{id}`.
#[tokio::test]
async fn resolve_reads_local_urls_from_the_database() {
    let db = common::test_db().await;
    let pool = db.pool().clone();
    let (_viewer, token) = fixture(&pool, "resolver", "read").await;
    let posted = common::create_local_user(&pool, "poster").await;
    let row =
        common::create_status(&pool, &posted, "a local post to find", Visibility::Public).await;
    let app = common::test_app(pool.clone());

    let cases = vec![
        (
            posted.ap_id.clone(),
            vec![posted.id.to_string()],
            Vec::new(),
        ),
        (
            format!("{BASE}/users/poster"),
            vec![posted.id.to_string()],
            Vec::new(),
        ),
        (row.uri.clone(), Vec::new(), vec![row.id.to_string()]),
        (
            row.url
                .clone()
                .expect("a local status carries its notice url"),
            Vec::new(),
            vec![row.id.to_string()],
        ),
        (
            format!("{BASE}/users/poster/statuses/{}", row.id),
            Vec::new(),
            vec![row.id.to_string()],
        ),
    ];
    for (url, want_accounts, want_statuses) in cases {
        let found = get(
            &app,
            Some(&token),
            &format!("{V1_SEARCH}?q={url}&resolve=true"),
        )
        .await;
        common::assert_answer(&found, StatusCode::OK, "the local resolve");
        let body = found.json_value();
        assert_eq!(accounts_of(&body), want_accounts, "`{url}` accounts");
        assert_eq!(statuses_of(&body), want_statuses, "`{url}` statuses");
    }

    let gone = get(
        &app,
        Some(&token),
        &format!("{V1_SEARCH}?q={BASE}/users/nobody&resolve=true"),
    )
    .await;
    common::assert_answer(&gone, StatusCode::OK, "an unknown local actor");
    assert_all_lists_empty(&gone.json_value(), "a local URL with no row behind it");
}

/// A resolved status the viewer may not see is left out of the answer: the
/// same `/notice/{id}` is the author's own and nobody else's.
#[tokio::test]
async fn resolve_omits_a_status_the_viewer_may_not_see() {
    let db = common::test_db().await;
    let pool = db.pool().clone();
    let (author, author_token) = fixture(&pool, "author", "read").await;
    let (_other, other_token) = fixture(&pool, "other", "read").await;
    let secret = common::create_status(&pool, &author, "author only", Visibility::Private).await;
    let url = secret.url.clone().expect("a local notice url");
    let app = common::test_app(pool.clone());

    let hidden = get(
        &app,
        Some(&other_token),
        &format!("{V1_SEARCH}?q={url}&resolve=true"),
    )
    .await;
    common::assert_answer(&hidden, StatusCode::OK, "the refusing answer is an answer");
    assert_all_lists_empty(&hidden.json_value(), "a viewer the private post excludes");

    let shown = get(
        &app,
        Some(&author_token),
        &format!("{V1_SEARCH}?q={url}&resolve=true"),
    )
    .await;
    assert_eq!(
        statuses_of(&shown.json_value()),
        vec![secret.id.to_string()],
        "the author reads their own post"
    );
}

/// A peer that cannot be reached is the empty answer, not an error: both
/// readings fail quietly, and the stored search's own lists still answer
/// beside them.
#[tokio::test]
async fn resolve_is_silent_for_an_unreachable_host() {
    let db = common::test_db().await;
    let pool = db.pool().clone();
    let (_viewer, token) = fixture(&pool, "resolver", "read").await;
    let known = common::create_local_user(&pool, "resolvable").await;
    let dead = dead_origin().await;
    let app = common::test_app(pool.clone());

    for path in ["/users/ghost", "/objects/ghost"] {
        let query = format!("{dead}{path}");
        let found = get(
            &app,
            Some(&token),
            &format!("{V1_SEARCH}?q={query}&resolve=true"),
        )
        .await;
        common::assert_answer(&found, StatusCode::OK, "an unreachable peer is no error");
        let body = found.json_value();
        assert_document(&body);
        assert_list_empty(&body, "statuses", &format!("`{query}` resolves nothing"));
        assert_list_empty(&body, "accounts", &format!("`{query}` resolves nothing"));
    }

    let flagged = get(
        &app,
        Some(&token),
        &format!("{ACCOUNTS_SEARCH}?q=resolv&resolve=true"),
    )
    .await;
    let plain = get(&app, Some(&token), &format!("{ACCOUNTS_SEARCH}?q=resolv")).await;
    let ids = listed_ids(&flagged.json_value());
    assert!(
        ids.contains(&known.id.to_string()),
        "the stored search still answers beside the flag: {ids:?}"
    );
    assert_eq!(
        ids,
        listed_ids(&plain.json_value()),
        "and a resolve that failed changes nothing about the answer"
    );
}

/// Resolution is a viewer's action: the anonymous `resolve=true` request is
/// the ordinary search, even with a live peer behind the URL, while the same
/// request with a token fetches.
#[tokio::test]
async fn resolve_needs_an_authenticated_viewer() {
    let db = common::test_db().await;
    let pool = db.pool().clone();
    let (_viewer, token) = fixture(&pool, "resolver", "read").await;
    let peer = MockPeer::start().await;
    let url = peer.actor_url();
    let app = common::test_app(pool.clone());

    let anonymous = get(&app, None, &format!("{V1_SEARCH}?q={url}&resolve=true")).await;
    common::assert_answer(
        &anonymous,
        StatusCode::OK,
        "the anonymous search is answered",
    );
    assert_all_lists_empty(
        &anonymous.json_value(),
        "an anonymous resolve=true asks nothing of the network",
    );
    assert_eq!(peer.hits(), 0, "and the peer never heard about it");

    let signed = get(
        &app,
        Some(&token),
        &format!("{V1_SEARCH}?q={url}&resolve=true"),
    )
    .await;
    assert_eq!(
        accounts_of(&signed.json_value()).len(),
        1,
        "the same request with a viewer fetches"
    );
    assert!(peer.hits() > 0, "and the peer answered it");
}

/// `type` keeps a resolving search to the resource it names: no account is
/// fetched for `type=statuses` (the peer is never asked), and no post for
/// `type=accounts` (the status reading never runs).
#[tokio::test]
async fn resolve_attempts_only_the_requested_type() {
    let db = common::test_db().await;
    let pool = db.pool().clone();
    let (_viewer, token) = fixture(&pool, "resolver", "read").await;
    let peer = MockPeer::start().await;
    let app = common::test_app(pool.clone());

    let statuses_only = get(
        &app,
        Some(&token),
        &format!(
            "{V1_SEARCH}?q={}&resolve=true&type=statuses",
            peer.actor_url()
        ),
    )
    .await;
    common::assert_answer(&statuses_only, StatusCode::OK, "the typed search");
    assert_list_empty(
        &statuses_only.json_value(),
        "accounts",
        "type=statuses resolves no account",
    );
    assert!(
        user::get_by_ap_id(&pool, &peer.actor_url())
            .await
            .unwrap()
            .is_none(),
        "the actor reading never ran: no account was mirrored"
    );

    let accounts_only = get(
        &app,
        Some(&token),
        &format!(
            "{V1_SEARCH}?q={}&resolve=true&type=accounts",
            peer.object_url("second")
        ),
    )
    .await;
    let body = accounts_only.json_value();
    assert_list_empty(&body, "statuses", "type=accounts resolves no post");
    assert_list_empty(&body, "accounts", "and a note is no actor");
    assert_eq!(
        stored_notes(&pool, "second").await,
        0,
        "the status reading never ran: nothing was stored"
    );
}

/// A handle the instance already stores for exact is answered by that row:
/// `resolve=true` changes nothing about the result (and, with no peer behind
/// its domain, could not have fetched anything else).
#[tokio::test]
async fn resolve_leaves_an_exact_handle_match_as_it_is() {
    let db = common::test_db().await;
    let pool = db.pool().clone();
    let (_viewer, token) = fixture(&pool, "resolver", "read").await;
    let eve = common::create_remote_user(&pool, "eve", "unlisted.example").await;
    let app = common::test_app(pool.clone());

    for flag in ["", "&resolve=true"] {
        let found = get(
            &app,
            Some(&token),
            &format!("{ACCOUNTS_SEARCH}?q=%40eve%40unlisted.example{flag}"),
        )
        .await;
        common::assert_answer(&found, StatusCode::OK, "the stored handle");
        assert_eq!(
            listed_ids(&found.json_value()),
            vec![eve.id.to_string()],
            "`resolve=true` is the same answer as without it"
        );
    }
}

/// A resolving request spends the `search` bucket like any other: the same
/// token's two requests inside a budget of two, the third refused before its
/// lookup ever runs.
#[tokio::test]
async fn a_resolving_request_spends_the_search_bucket() {
    let db = common::test_db().await;
    let pool = db.pool().clone();
    let (_viewer, token) = fixture(&pool, "resolver", "read").await;
    let dead = dead_origin().await;
    let config = common::test_config_with_rate_limits("search=2:600");
    let app = common::test_app_with_config(pool, config);

    for remaining in ["1", "0"] {
        let answer =
            common::TestRequest::get(&format!("{V1_SEARCH}?q={dead}/users/ghost&resolve=true"))
                .bearer(&token)
                .send(&app)
                .await;
        common::assert_answer(&answer, StatusCode::OK, "inside the budget");
        assert_eq!(
            answer.header("x-ratelimit-remaining"),
            Some(remaining),
            "the resolving request counted down the bucket"
        );
    }

    let refused =
        common::TestRequest::get(&format!("{V1_SEARCH}?q={dead}/users/ghost&resolve=true"))
            .bearer(&token)
            .send(&app)
            .await;
    common::assert_answer(&refused, StatusCode::TOO_MANY_REQUESTS, "the spent window");
    common::assert_error(&refused, THROTTLED, "decision 8's body");
}
