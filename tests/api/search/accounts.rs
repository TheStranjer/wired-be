//! The v1 flat account array (`GET /api/v1/accounts/search`): its prefix
//! and trigram matches and query normalisation, its ranking and pages, the
//! `following=true` restriction, the blocked and deactivated exclusions,
//! the auth gates every search route runs, and the empty query that
//! answers no accounts.

use axum::http::StatusCode;
use wired_be::models::block;

use super::support::{
    ACCOUNTS_SEARCH, BASE, INVALID_TOKEN, NO_READ_ACCOUNTS, NO_READ_SEARCH, OFFSET_UNAUTHENTICATED,
    V1_SEARCH, V2_SEARCH, fixture, follow_pair, get, listed_ids,
};
use crate::common;

/// The v1 accounts search answers prefix matches, anonymously, as
/// rendered [`Account`] entities.
#[tokio::test]
async fn accounts_search_answers_prefix_matches_anonymously() {
    let db = common::test_db().await;
    let pool = db.pool().clone();
    let app = common::test_app(pool.clone());
    let bob = common::create_local_user_with(&pool, "bob", |mut new| {
        new.name = "Bobby Brown".to_owned();
        new
    })
    .await;
    let _alice = common::create_local_user(&pool, "alice").await;

    let found = get(&app, None, &format!("{ACCOUNTS_SEARCH}?q=bo")).await;
    common::assert_answer(&found, StatusCode::OK, "an anonymous prefix search");
    assert_eq!(listed_ids(&found.json_value()), vec![bob.id.to_string()]);
    let entity = &found.json_value()[0];
    assert_eq!(entity["acct"], "bob", "the handle clients know");
    assert_eq!(entity["username"], "bob", "the nickname part");
    assert_eq!(entity["display_name"], "Bobby Brown", "the stored name");

    let by_name = get(&app, None, &format!("{ACCOUNTS_SEARCH}?q=bobby")).await;
    assert_eq!(
        listed_ids(&by_name.json_value()),
        vec![bob.id.to_string()],
        "the display-name prefix finds the account the nickname prefix misses"
    );
}

/// The v1 accounts search matches trigram typos beyond the prefix and
/// normalises the query's sigil and case.
#[tokio::test]
async fn accounts_search_matches_typos_and_normalises_the_query() {
    let db = common::test_db().await;
    let pool = db.pool().clone();
    let app = common::test_app(pool.clone());
    let alice = common::create_local_user(&pool, "alice").await;
    let eve = common::create_remote_user(&pool, "eve", "remote.example").await;

    let typo = get(&app, None, &format!("{ACCOUNTS_SEARCH}?q=ALCE")).await;
    assert_eq!(
        listed_ids(&typo.json_value()),
        vec![alice.id.to_string()],
        "the trigram arm finds the misspelled nickname"
    );
    let typed = get(
        &app,
        None,
        &format!("{ACCOUNTS_SEARCH}?q=%40eve%40remote.example"),
    )
    .await;
    assert_eq!(
        typed.json_value()[0]["acct"],
        "eve@remote.example",
        "the typed handle matches the remote mirror's stored nickname first"
    );
    assert_eq!(
        listed_ids(&typed.json_value()),
        vec![eve.id.to_string()],
        "and nothing else answers it"
    );
}

/// The v1 accounts search ranks exact nicknames above the viewer's follows,
/// follows above other local accounts, and local above remote; `limit` and
/// `offset` walk that order.
#[tokio::test]
async fn accounts_search_ranks_and_pages() {
    let db = common::test_db().await;
    let pool = db.pool().clone();
    let app = common::test_app(pool.clone());
    let (viewer, token) = fixture(&pool, "viewer", "read").await;
    let followed = common::create_local_user(&pool, "someread").await;
    follow_pair(&pool, &viewer, &followed).await;
    let exact = common::create_local_user(&pool, "so").await;
    let local = common::create_local_user(&pool, "someone").await;
    let mirror = common::create_remote_user(&pool, "someuser", "remote.example").await;

    let ranked = get(&app, Some(&token), &format!("{ACCOUNTS_SEARCH}?q=%40SO")).await;
    assert_eq!(
        listed_ids(&ranked.json_value()),
        vec![
            exact.id.to_string(),
            followed.id.to_string(),
            local.id.to_string(),
            mirror.id.to_string(),
        ],
        "exact, followed, local, remote"
    );

    let first = get(
        &app,
        Some(&token),
        &format!("{ACCOUNTS_SEARCH}?q=so&limit=2"),
    )
    .await;
    assert_eq!(
        listed_ids(&first.json_value()),
        vec![exact.id.to_string(), followed.id.to_string()],
        "the caller's limit sizes the page"
    );
    let tail = get(
        &app,
        Some(&token),
        &format!("{ACCOUNTS_SEARCH}?q=so&limit=2&offset=2"),
    )
    .await;
    assert_eq!(
        listed_ids(&tail.json_value()),
        vec![local.id.to_string(), mirror.id.to_string()],
        "and the offset walks the ranking"
    );
    let past = get(
        &app,
        Some(&token),
        &format!("{ACCOUNTS_SEARCH}?q=so&offset=99"),
    )
    .await;
    assert!(
        past.json_value().as_array().is_some_and(Vec::is_empty),
        "past the end is the empty array, never an error"
    );
}

/// `following=true` restricts the search to the viewer's follows; without
/// a viewer the restriction searches an empty list and answers nothing.
#[tokio::test]
async fn accounts_search_following_filter() {
    let db = common::test_db().await;
    let pool = db.pool().clone();
    let app = common::test_app(pool.clone());
    let (viewer, token) = fixture(&pool, "finder", "read:accounts").await;
    let carol = common::create_local_user(&pool, "carol").await;
    follow_pair(&pool, &viewer, &carol).await;
    let _cardu = common::create_local_user(&pool, "cardu").await;

    let open = get(&app, Some(&token), &format!("{ACCOUNTS_SEARCH}?q=car")).await;
    assert_eq!(
        listed_ids(&open.json_value()).len(),
        2,
        "unrestricted, both car-accounts answer (the followed one first)"
    );
    assert_eq!(
        open.json_value()[0]["id"],
        carol.id.to_string(),
        "and the follow leads its rank class"
    );

    let restricted = get(
        &app,
        Some(&token),
        &format!("{ACCOUNTS_SEARCH}?q=car&following=true"),
    )
    .await;
    assert_eq!(
        listed_ids(&restricted.json_value()),
        vec![carol.id.to_string()],
        "the restriction keeps only the followed account"
    );

    let anonymous = get(&app, None, &format!("{ACCOUNTS_SEARCH}?q=car&following=1")).await;
    common::assert_answer(
        &anonymous,
        StatusCode::OK,
        "anonymous, so nobody is followed",
    );
    assert!(
        anonymous.json_value().as_array().is_some_and(Vec::is_empty),
        "an anonymous following search answers nothing"
    );
}

/// The blocked and deactivated exclusions hold over HTTP: neither shows in
/// the viewer's search, and an unrelated viewer still sees the accounts
/// the viewer merely has a block with.
#[tokio::test]
async fn accounts_search_excludes_blocks_and_deactivated() {
    let db = common::test_db().await;
    let pool = db.pool().clone();
    let app = common::test_app(pool.clone());
    let (viewer, token) = fixture(&pool, "viewer", "read").await;
    let bane = common::create_local_user(&pool, "banshee").await;
    let cursed = common::create_local_user(&pool, "banished").await;
    let blithe = common::create_local_user(&pool, "blithe").await;
    let gone = common::create_local_user(&pool, "blunder").await;
    let bane_row = block::block(&pool, BASE, &bane, &viewer)
        .await
        .expect("the first fixture blocks the viewer");
    assert_eq!(
        bane_row.blocked_id, viewer.id,
        "the first block is `banshee` on the viewer"
    );
    let cursed_row = block::block(&pool, BASE, &viewer, &cursed)
        .await
        .expect("the second fixture is blocked by the viewer");
    assert_eq!(
        cursed_row.blocked_id, cursed.id,
        "the second block is the viewer on `banished`"
    );
    let deactivated = sqlx::query("UPDATE users SET is_active = false WHERE id = $1")
        .bind(gone.id)
        .execute(&pool)
        .await
        .expect("the fixture deactivates");
    assert_eq!(deactivated.rows_affected(), 1, "and it was one row");

    let seen = get(&app, Some(&token), &format!("{ACCOUNTS_SEARCH}?q=b")).await;
    assert_eq!(
        listed_ids(&seen.json_value()),
        vec![blithe.id.to_string()],
        "the account that blocked the viewer, the one the viewer blocked, \
         and the deactivated one are all out"
    );
    let stranger = get(&app, None, &format!("{ACCOUNTS_SEARCH}?q=b")).await;
    for visible in [
        bane.id.to_string(),
        cursed.id.to_string(),
        blithe.id.to_string(),
    ] {
        assert!(
            listed_ids(&stranger.json_value()).contains(&visible),
            "an anonymous request sees every live match whatever the viewer's blocks are"
        );
    }
}

/// Auth on the search routes: anonymous passes, a `read`-scoped token
/// passes, a token without `read:accounts` is the 403, an invalid token
/// the 401, and an app-only token proceeds like an anonymous request.
#[tokio::test]
async fn search_auth_gates() {
    let db = common::test_db().await;
    let pool = db.pool().clone();
    let app = common::test_app(pool.clone());
    let _bob = common::create_local_user(&pool, "bob").await;
    let (_, write_token) = fixture(&pool, "writer", "write:accounts").await;
    let (_, read_token) = fixture(&pool, "reader", "read").await;
    let app_only = common::create_app_only_token(&pool, "read").await;

    for path in [ACCOUNTS_SEARCH, V1_SEARCH, V2_SEARCH] {
        let target = format!("{path}?q=bo");
        let allowed = get(&app, None, &target).await;
        common::assert_answer(&allowed, StatusCode::OK, &format!("{path} anonymous"));
        let granted = get(&app, Some(&read_token), &target).await;
        common::assert_answer(&granted, StatusCode::OK, &format!("{path} with read"));
        let shared = get(&app, Some(&app_only), &target).await;
        common::assert_answer(&shared, StatusCode::OK, &format!("{path} app-only"));

        let refused = get(&app, Some(&write_token), &target).await;
        common::assert_answer(
            &refused,
            StatusCode::FORBIDDEN,
            &format!("{path} wrong scope"),
        );
        let expected = if path == ACCOUNTS_SEARCH {
            NO_READ_ACCOUNTS
        } else {
            NO_READ_SEARCH
        };
        common::assert_error(&refused, expected, &format!("{path} wrong scope"));
        let unknown = get(&app, Some("no-such-token"), &target).await;
        common::assert_answer(
            &unknown,
            StatusCode::UNAUTHORIZED,
            &format!("{path} invalid token"),
        );
        common::assert_error(&unknown, INVALID_TOKEN, &format!("{path} invalid token"));
    }
}

/// An anonymous request that pages by `offset` is the 401 on every search
/// route (a deep offset is a cost amplifier), a blank `offset` and an absent
/// one are answered, and a viewer may page.
#[tokio::test]
async fn anonymous_offset_paging_is_refused() {
    let db = common::test_db().await;
    let pool = db.pool().clone();
    let app = common::test_app(pool.clone());
    let _alice = common::create_local_user(&pool, "alice").await;
    let (_, read_token) = fixture(&pool, "reader", "read").await;
    let app_only = common::create_app_only_token(&pool, "read").await;

    for path in [ACCOUNTS_SEARCH, V1_SEARCH, V2_SEARCH] {
        for query in ["q=a&offset=20", "q=a&offset=0", "q=a&offset=junk"] {
            let target = format!("{path}?{query}");
            let who = format!("{path} anonymous `{query}`");
            let refused = get(&app, None, &target).await;
            common::assert_answer(&refused, StatusCode::UNAUTHORIZED, &who);
            common::assert_error(&refused, OFFSET_UNAUTHENTICATED, &who);
        }
        let shared = get(&app, Some(&app_only), &format!("{path}?q=a&offset=20")).await;
        common::assert_answer(&shared, StatusCode::UNAUTHORIZED, "an app-only token");
        common::assert_error(&shared, OFFSET_UNAUTHENTICATED, "an app-only token");

        for query in ["q=a", "q=a&offset=", "q=a&offset=%20"] {
            let allowed = get(&app, None, &format!("{path}?{query}")).await;
            common::assert_answer(
                &allowed,
                StatusCode::OK,
                &format!("{path} anonymous `{query}`"),
            );
        }
        let paged = get(&app, Some(&read_token), &format!("{path}?q=a&offset=20")).await;
        common::assert_answer(&paged, StatusCode::OK, &format!("{path} with a viewer"));
    }
}

/// The combined documents demand `read:search`, the account array
/// `read:accounts`: each token reaches only its own route, `read` reaches
/// both and an anonymous request reaches both.
#[tokio::test]
async fn search_scopes_split_between_the_document_and_the_account_array() {
    let db = common::test_db().await;
    let pool = db.pool().clone();
    let app = common::test_app(pool.clone());
    let _bob = common::create_local_user(&pool, "bob").await;
    let (_, search_token) = fixture(&pool, "searcher", "read:search").await;
    let (_, accounts_token) = fixture(&pool, "accounter", "read:accounts").await;
    let (_, read_token) = fixture(&pool, "reader", "read").await;

    let documents = format!("{V2_SEARCH}?q=bo");
    let flat = format!("{ACCOUNTS_SEARCH}?q=bo");
    let v1 = format!("{V1_SEARCH}?q=bo");

    for path in [&documents, &v1] {
        let granted = get(&app, Some(&search_token), path).await;
        common::assert_answer(&granted, StatusCode::OK, &format!("{path} read:search"));
        let refused = get(&app, Some(&accounts_token), path).await;
        common::assert_answer(
            &refused,
            StatusCode::FORBIDDEN,
            &format!("{path} read:accounts only"),
        );
        common::assert_error(&refused, NO_READ_SEARCH, &format!("{path} read:accounts"));
        let broad = get(&app, Some(&read_token), path).await;
        common::assert_answer(&broad, StatusCode::OK, &format!("{path} read"));
        let anonymous = get(&app, None, path).await;
        common::assert_answer(&anonymous, StatusCode::OK, &format!("{path} anonymous"));
    }

    let by_accounts = get(&app, Some(&accounts_token), &flat).await;
    common::assert_answer(
        &by_accounts,
        StatusCode::OK,
        "accounts/search read:accounts",
    );
    let by_search = get(&app, Some(&search_token), &flat).await;
    common::assert_answer(
        &by_search,
        StatusCode::FORBIDDEN,
        "accounts/search read:search only",
    );
    common::assert_error(&by_search, NO_READ_ACCOUNTS, "accounts/search read:search");
    let by_read = get(&app, Some(&read_token), &flat).await;
    common::assert_answer(&by_read, StatusCode::OK, "accounts/search read");
    let anonymous = get(&app, None, &flat).await;
    common::assert_answer(&anonymous, StatusCode::OK, "accounts/search anonymous");
}

/// An absent, empty or sigil-only `q` answers no accounts on any route,
/// without touching the corpus.
#[tokio::test]
async fn an_empty_query_answers_no_accounts() {
    let db = common::test_db().await;
    let pool = db.pool().clone();
    let app = common::test_app(pool.clone());
    let _bob = common::create_local_user(&pool, "bob").await;

    for query in ["", "q=", "q=%40", "q=%20%20"] {
        let flat = get(&app, None, &format!("{ACCOUNTS_SEARCH}?{query}")).await;
        common::assert_answer(&flat, StatusCode::OK, "an empty query is an answer");
        assert!(
            flat.json_value().as_array().is_some_and(Vec::is_empty),
            "`{query:?}` searches nothing: {}",
            flat.text()
        );
        let document = get(&app, None, &format!("{V1_SEARCH}?{query}")).await;
        common::assert_answer(&document, StatusCode::OK, "and the document too");
        assert!(
            document.json_value()["accounts"]
                .as_array()
                .is_some_and(Vec::is_empty),
            "`{query:?}` answers no accounts in the document"
        );
    }
}
