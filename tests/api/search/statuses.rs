//! The `statuses` arm of the combined search: the full-text matches on the
//! stored text and the content warning, the conjunctive unstemmed reading
//! and the full [`Status`] rendering, the visibility model over strangers,
//! followers and authors, the blocked and muted exclusions, the
//! `account_id` narrowing and its 400, and the `limit`/cursor/`offset`
//! pages.

use axum::http::StatusCode;
use wired_be::models::{
    block,
    mute::{self, MuteOptions},
    status::{self, NewStatus, Visibility},
};

use super::support::{
    ACCOUNTS_SEARCH, BASE, INVALID_ID, V1_SEARCH, V2_SEARCH, assert_list_empty, fixture,
    follow_pair, get, is_descending, status_ids, statuses_of,
};
use crate::common;

/// The `statuses` arm full-text searches the stored text: a word in the
/// body or in the content warning answers its status, newest id first;
/// reblog rows, tombstones and a query whose words no single post carries
/// together answer nothing.
#[tokio::test]
async fn statuses_search_matches_body_and_warning_words() {
    let db = common::test_db().await;
    let pool = db.pool().clone();
    let app = common::test_app(pool.clone());
    let (_viewer, token) = fixture(&pool, "finder", "read").await;
    let alice = common::create_local_user(&pool, "alice").await;
    let bob = common::create_local_user(&pool, "bob").await;
    let both =
        common::create_status(&pool, &alice, "the quick brown fox", Visibility::Public).await;
    let warned = status::insert(
        &pool,
        BASE,
        &NewStatus {
            spoiler_text: "fox warning".to_owned(),
            ..NewStatus::local(&alice, "unrelated body")
        },
    )
    .await
    .expect("the warned fixture inserts");
    let _slow = common::create_status(&pool, &bob, "a slow brown cat", Visibility::Public).await;
    let reblog = status::insert(
        &pool,
        BASE,
        &NewStatus {
            reblog_of_id: Some(both.id),
            ..NewStatus::local(&bob, "")
        },
    )
    .await
    .expect("the reblog fixture inserts");
    let doomed = common::create_status(&pool, &alice, "doomed fox", Visibility::Public).await;
    assert!(
        status::soft_delete(&pool, doomed.id)
            .await
            .expect("the fixture delete runs")
            .is_some(),
        "one matching post is a tombstone now"
    );

    let ids = status_ids(&app, &token, "fox").await;
    assert_eq!(
        ids.len(),
        2,
        "the body match and the warning match, no reblog row, no tombstone: {ids:?}"
    );
    for id in [warned.id.to_string(), both.id.to_string()] {
        assert!(ids.contains(&id), "the {id} answers: {ids:?}");
    }
    assert!(
        !ids.contains(&reblog.id.to_string()),
        "the reblog row itself is never in: {ids:?}"
    );
    assert!(is_descending(&ids), "newest id first: {ids:?}");
}

/// A matching status renders as a full [`Status`] entity in the list: the
/// stored content and the author's entity ride along, and the conjunctive,
/// unstemmed reading of `plainto_tsquery('simple', …)` holds over HTTP —
/// words AND together, and a misspelled stem finds nothing.
#[tokio::test]
async fn statuses_search_ands_words_and_never_stems() {
    let db = common::test_db().await;
    let pool = db.pool().clone();
    let app = common::test_app(pool.clone());
    let (_viewer, token) = fixture(&pool, "finder", "read").await;
    let alice = common::create_local_user(&pool, "alice").await;
    let both =
        common::create_status(&pool, &alice, "the quick brown fox", Visibility::Public).await;

    let conjunct = status_ids(&app, &token, "quick%20fox").await;
    assert_eq!(
        conjunct,
        vec![both.id.to_string()],
        "only the post carrying every word matches"
    );
    assert!(
        status_ids(&app, &token, "quickly").await.is_empty(),
        "`simple` does not stem: `quickly` is not the word `quick`"
    );

    let body = get(
        &app,
        Some(&token),
        &format!("{V1_SEARCH}?q=quick&type=statuses"),
    )
    .await;
    common::assert_answer(&body, StatusCode::OK, "the single-word search");
    let entity = &body.json_value()["statuses"][0];
    assert_eq!(
        entity["content"], "<p>the quick brown fox</p>",
        "the arm renders full Status entities, not ids"
    );
    assert_eq!(entity["account"]["acct"], "alice", "the author rides along");
}

/// The `statuses` arm shows a viewer only what they may see: a private
/// post answers its author and its author's accepted followers, and
/// nobody else.
#[tokio::test]
async fn private_statuses_are_found_only_by_the_follower() {
    let db = common::test_db().await;
    let pool = db.pool().clone();
    let app = common::test_app(pool.clone());
    let alice = common::create_local_user(&pool, "alice").await;
    let bob = common::create_local_user(&pool, "bob").await;
    let carol = common::create_local_user(&pool, "carol").await;
    let alice_token = common::create_app_and_token(&pool, &alice, "read").await;
    let bob_token = common::create_app_and_token(&pool, &bob, "read").await;
    let carol_token = common::create_app_and_token(&pool, &carol, "read").await;
    let edge = common::create_status(&pool, &alice, "followers keyword", Visibility::Private).await;
    let open = common::create_status(&pool, &alice, "everyone keyword", Visibility::Public).await;
    let edge_id = edge.id.to_string();
    let open_id = open.id.to_string();

    assert_eq!(
        status_ids(&app, &carol_token, "keyword").await,
        vec![open_id.clone()],
        "a stranger searches only what is open to them"
    );
    assert_eq!(
        status_ids(&app, &bob_token, "keyword").await,
        vec![open_id.clone()],
        "and a not-yet-follower finds no more"
    );

    follow_pair(&pool, &bob, &alice).await;
    let followed = status_ids(&app, &bob_token, "keyword").await;
    assert!(
        followed.contains(&edge_id) && followed.contains(&open_id),
        "the follower finds the private status too: {followed:?}"
    );
    assert!(
        status_ids(&app, &alice_token, "keyword")
            .await
            .contains(&edge_id),
        "and the author always finds their own"
    );
}

/// Neither direction of a block, nor a mute, leaves an author in the
/// blocking/muting viewer's `statuses` search; an unrelated viewer still
/// finds everything public.
#[tokio::test]
async fn status_search_excludes_blocked_and_muted_authors() {
    let db = common::test_db().await;
    let pool = db.pool().clone();
    let app = common::test_app(pool.clone());
    let (viewer, token) = fixture(&pool, "viewer", "read").await;
    let (_bystander, other_token) = fixture(&pool, "bystander", "read").await;
    let bane = common::create_local_user(&pool, "bane").await;
    let muted = common::create_local_user(&pool, "mutedpal").await;
    let blocked_post =
        common::create_status(&pool, &bane, "keyword from bane", Visibility::Public).await;
    let muted_post =
        common::create_status(&pool, &muted, "keyword from the muted", Visibility::Public).await;
    let edge = block::block(&pool, BASE, &viewer, &bane)
        .await
        .expect("the viewer blocks bane");
    assert_eq!(edge.blocked_id, bane.id, "the fixture block landed");
    let silence = mute::mute(&pool, &viewer, &muted, MuteOptions::default())
        .await
        .expect("the viewer mutes the pal");
    assert_eq!(silence.muted_id, muted.id, "the fixture mute landed");

    let mine = get(
        &app,
        Some(&token),
        &format!("{V1_SEARCH}?q=keyword&type=statuses"),
    )
    .await;
    assert_list_empty(
        &mine.json_value(),
        "statuses",
        "the viewer's blocked and muted authors",
    );

    let theirs = get(
        &app,
        Some(&other_token),
        &format!("{V1_SEARCH}?q=keyword&type=statuses"),
    )
    .await;
    let ids = statuses_of(&theirs.json_value());
    for visible in [blocked_post.id.to_string(), muted_post.id.to_string()] {
        assert!(
            ids.contains(&visible),
            "someone else's block and mute hide nothing from a third account: {ids:?}"
        );
    }
}

/// `account_id` narrows the `statuses` arm to that one author, an id
/// nobody posts with answers the empty page, and a value that is not an
/// id is the shared 400.
#[tokio::test]
async fn status_search_account_id_narrows_the_author() {
    let db = common::test_db().await;
    let pool = db.pool().clone();
    let app = common::test_app(pool.clone());
    let (_viewer, token) = fixture(&pool, "finder", "read").await;
    let alice = common::create_local_user(&pool, "alice").await;
    let bob = common::create_local_user(&pool, "bob").await;
    let hers = common::create_status(&pool, &alice, "keyword hers", Visibility::Public).await;
    let _his = common::create_status(&pool, &bob, "keyword his", Visibility::Public).await;

    let filtered = get(
        &app,
        Some(&token),
        &format!(
            "{V1_SEARCH}?q=keyword&type=statuses&account_id={}",
            alice.id
        ),
    )
    .await;
    assert_eq!(
        statuses_of(&filtered.json_value()),
        vec![hers.id.to_string()],
        "only that author's matching posts answer"
    );

    let nobody = get(
        &app,
        Some(&token),
        &format!(
            "{V1_SEARCH}?q=keyword&type=statuses&account_id={}",
            uuid::Uuid::now_v7()
        ),
    )
    .await;
    common::assert_answer(&nobody, StatusCode::OK, "an unknown author is no error");
    assert_list_empty(
        &nobody.json_value(),
        "statuses",
        "an author with no matches",
    );

    for path in [ACCOUNTS_SEARCH, V1_SEARCH, V2_SEARCH] {
        let junk = get(&app, Some(&token), &format!("{path}?q=bo&account_id=nope")).await;
        common::assert_answer(
            &junk,
            StatusCode::BAD_REQUEST,
            &format!("{path} bad account_id"),
        );
        common::assert_error(&junk, INVALID_ID, &format!("{path} bad account_id"));
    }
}

/// The `statuses` arm pages newest-first with `limit` (default 20, cap 40),
/// the `max_id`/`min_id` keyset cursors, and `offset`.
#[tokio::test]
async fn status_search_pages_by_limit_cursors_and_offset() {
    let db = common::test_db().await;
    let pool = db.pool().clone();
    let app = common::test_app(pool.clone());
    let (_viewer, token) = fixture(&pool, "finder", "read").await;
    let alice = common::create_local_user(&pool, "alice").await;
    for n in 0..5 {
        let _posted = common::create_status(
            &pool,
            &alice,
            &format!("paging keyword {n}"),
            Visibility::Public,
        )
        .await;
    }

    let all = status_ids(&app, &token, "paging").await;
    assert_eq!(all.len(), 5, "the whole page answers");
    assert!(is_descending(&all), "`id DESC` throughout: {all:?}");

    let first = status_ids(&app, &token, "paging&limit=2").await;
    assert_eq!(first, all[..2], "limit cuts the page short");
    let overcap = status_ids(&app, &token, "paging&limit=200").await;
    assert_eq!(overcap, all, "the cap sizes the page, never errors");

    let middle = all.get(2).expect("the five-strong page has a middle");
    let older = status_ids(&app, &token, &format!("paging&max_id={middle}")).await;
    assert_eq!(older, all[3..], "max_id answers the strictly older rows");
    let newer = status_ids(&app, &token, &format!("paging&min_id={middle}")).await;
    assert_eq!(
        newer,
        all[..2],
        "min_id answers the newer rows, newest-first"
    );

    let stepped = status_ids(&app, &token, "paging&offset=2&limit=2").await;
    assert_eq!(stepped, all[2..4], "offset walks the page");
    assert!(
        status_ids(&app, &token, "paging&offset=99")
            .await
            .is_empty(),
        "an offset past the end answers nothing"
    );
}
