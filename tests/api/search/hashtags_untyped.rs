//! The hashtag arm and the untyped combined search: the name-prefix matches
//! v1 spells as plain names and v2 as tag entities, the one query that
//! fills all three lists when no `type` is given, the `#`-leading term read
//! as hashtag-only, the `SEARCH_STATUSES_UNAUTHENTICATED` gate on the
//! anonymous statuses arm, and the empty or sigil-only query that fills no
//! list anywhere.

use axum::http::StatusCode;
use serde_json::json;
use wired_be::{
    config::Toggle,
    models::{hashtag, status::Visibility},
};

use super::support::{
    BASE, V1_SEARCH, V2_SEARCH, accounts_of, assert_all_lists_empty, assert_document,
    assert_list_empty, fixture, get, statuses_of,
};
use crate::common;

/// The hashtag arm is a name-prefix search that folds the typed `#` and
/// the case; v1 answers plain name strings and v2 answers tag entities
/// whose fresh `history` is the empty list.
#[tokio::test]
async fn hashtag_search_prefix_matches_v1_names_and_v2_tag_entities() {
    let db = common::test_db().await;
    let pool = db.pool().clone();
    let app = common::test_app(pool.clone());
    let alice = common::create_local_user(&pool, "alice").await;
    let _tagged =
        common::create_status(&pool, &alice, "#alpha #alphabet #beta", Visibility::Public).await;

    let v1 = get(&app, None, &format!("{V1_SEARCH}?q=alp&type=hashtags")).await;
    common::assert_answer(&v1, StatusCode::OK, "the anonymous hashtag arm");
    assert_eq!(
        v1.json_value()["hashtags"],
        json!(["alpha", "alphabet"]),
        "v1 spells plain names, prefix-only and in name order"
    );

    let v2 = get(&app, None, &format!("{V2_SEARCH}?q=%23ALP&type=hashtags")).await;
    common::assert_answer(&v2, StatusCode::OK, "the typed sigil and case fold");
    assert_eq!(
        v2.json_value()["hashtags"],
        json!([
            { "name": "alpha", "url": format!("{BASE}/tags/alpha"), "history": [] },
            { "name": "alphabet", "url": format!("{BASE}/tags/alphabet"), "history": [] },
        ]),
        "v2 renders tag entities, the uncomputed history an empty list"
    );

    let none = get(&app, None, &format!("{V1_SEARCH}?q=zz&type=hashtags")).await;
    assert_list_empty(&none.json_value(), "hashtags", "a prefix no tag carries");
    let capped = get(
        &app,
        None,
        &format!("{V1_SEARCH}?q=alp&type=hashtags&limit=1"),
    )
    .await;
    assert_eq!(
        capped.json_value()["hashtags"],
        json!(["alpha"]),
        "the page size cuts the name-ordered list"
    );
}

/// With no `type`, one authenticated search fills all three lists at once
/// from the one query.
#[tokio::test]
async fn untyped_search_fills_all_three_lists() {
    let db = common::test_db().await;
    let pool = db.pool().clone();
    let app = common::test_app(pool.clone());
    let (_viewer, token) = fixture(&pool, "finder", "read").await;
    let united = common::create_local_user(&pool, "unitedfan").await;
    let posted = common::create_status(&pool, &united, "united we stand", Visibility::Public).await;
    // The tag row is stored directly, so exactly one status matches the word.
    let _tag = hashtag::find_or_create(&pool, "#united")
        .await
        .expect("the tag fixture stores")
        .expect("united is a usable name");

    let found = get(&app, Some(&token), &format!("{V1_SEARCH}?q=united")).await;
    common::assert_answer(&found, StatusCode::OK, "the combined search");
    let body = found.json_value();
    assert_document(&body);
    assert_eq!(
        accounts_of(&body),
        vec![united.id.to_string()],
        "the account arm carries the prefix match"
    );
    assert_eq!(
        statuses_of(&body),
        vec![posted.id.to_string()],
        "the status arm carries the word match"
    );
    assert_eq!(
        body["hashtags"],
        json!(["united"]),
        "the hashtag arm carries the name match"
    );

    let v2 = get(&app, Some(&token), &format!("{V2_SEARCH}?q=united")).await;
    assert_eq!(
        v2.json_value()["hashtags"],
        json!([{ "name": "united", "url": format!("{BASE}/tags/united"), "history": [] }]),
        "the same combined search, v2's hashtag spelling"
    );
    assert_eq!(
        statuses_of(&v2.json_value()),
        vec![posted.id.to_string()],
        "statuses render identically on both versions"
    );
}

/// A query that opens with `#` is a hashtag-only search term: untyped, it
/// fills the hashtag list alone, whatever else in the corpus spells the
/// same word.
#[tokio::test]
async fn a_hashtag_query_searches_hashtags_only() {
    let db = common::test_db().await;
    let pool = db.pool().clone();
    let app = common::test_app(pool.clone());
    let (_viewer, token) = fixture(&pool, "finder", "read").await;
    let united = common::create_local_user(&pool, "unitedfan").await;
    let _posted =
        common::create_status(&pool, &united, "united we stand", Visibility::Public).await;
    let _tagged = common::create_status(&pool, &united, "#united", Visibility::Public).await;

    let found = get(&app, Some(&token), &format!("{V1_SEARCH}?q=%23united")).await;
    common::assert_answer(&found, StatusCode::OK, "the sigil query");
    let body = found.json_value();
    assert_document(&body);
    assert_eq!(
        body["hashtags"],
        json!(["united"]),
        "the hashtag list answers the term"
    );
    assert_list_empty(&body, "accounts", "a `#`-query is hashtag-only");
    assert_list_empty(&body, "statuses", "a `#`-query is hashtag-only");
}

/// The statuses gate: an anonymous request searches no statuses while
/// `SEARCH_STATUSES_UNAUTHENTICATED` is off (the default) — the accounts
/// and hashtag arms answer it fine — and once an operator turns the flag
/// on, it finds the public text, with `public` still the ceiling.
#[tokio::test]
async fn anonymous_status_search_needs_the_config_flag() {
    let db = common::test_db().await;
    let pool = db.pool().clone();
    let app = common::test_app(pool.clone());
    let keyworduser = common::create_local_user(&pool, "keyworduser").await;
    let open = common::create_status(
        &pool,
        &keyworduser,
        "keyword public post",
        Visibility::Public,
    )
    .await;
    let _kept = common::create_status(
        &pool,
        &keyworduser,
        "keyword private post",
        Visibility::Private,
    )
    .await;
    // The tag row is stored directly, so exactly the two posts carry the
    // word in their text and the hashtag arm still answers it.
    let _tag = hashtag::find_or_create(&pool, "#keyword")
        .await
        .expect("the tag fixture stores")
        .expect("keyword is a usable name");

    let gated = get(&app, None, &format!("{V1_SEARCH}?q=keyword&type=statuses")).await;
    common::assert_answer(
        &gated,
        StatusCode::OK,
        "the gated arm is an answer, not an error",
    );
    assert_list_empty(&gated.json_value(), "statuses", "anonymous, flag off");
    let untyped = get(&app, None, &format!("{V1_SEARCH}?q=keyword")).await;
    let body = untyped.json_value();
    assert_list_empty(&body, "statuses", "anonymous untyped, flag off");
    assert_eq!(
        body["hashtags"],
        json!(["keyword"]),
        "the ungated arms still answer the anonymous request"
    );
    assert_eq!(
        accounts_of(&body),
        vec![keyworduser.id.to_string()],
        "accounts included"
    );

    let mut config = common::test_config();
    config.search_statuses_unauthenticated = Toggle::new(true);
    let open_app = common::test_app_with_config(pool.clone(), config);
    let found = get(
        &open_app,
        None,
        &format!("{V1_SEARCH}?q=keyword&type=statuses"),
    )
    .await;
    common::assert_answer(&found, StatusCode::OK, "the flag opens the arm");
    assert_eq!(
        statuses_of(&found.json_value()),
        vec![open.id.to_string()],
        "public text answers, `public` and `unlisted` being the anonymous ceiling"
    );
}

/// An absent, empty, whitespace or bare-`#` query fills no list for an
/// authenticated viewer either — the new arms answer nothing, never the
/// whole corpus.
#[tokio::test]
async fn an_empty_query_fills_no_list_anywhere() {
    let db = common::test_db().await;
    let pool = db.pool().clone();
    let app = common::test_app(pool.clone());
    let (_viewer, token) = fixture(&pool, "finder", "read").await;
    let alice = common::create_local_user(&pool, "alice").await;
    let _posted = common::create_status(&pool, &alice, "corpus keyword", Visibility::Public).await;
    let _tagged = common::create_status(&pool, &alice, "#corpus", Visibility::Public).await;

    for query in ["", "q=", "q=%20%20", "q=%23"] {
        for kind in ["&type=statuses", "&type=hashtags", ""] {
            let found = get(&app, Some(&token), &format!("{V1_SEARCH}?{query}{kind}")).await;
            common::assert_answer(&found, StatusCode::OK, "`{query:?}` is an answer");
            assert_all_lists_empty(
                &found.json_value(),
                &format!("`{query:?}` with `{kind}` asked nothing"),
            );
        }
    }
}
