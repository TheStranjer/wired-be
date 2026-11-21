//! What narrows and gates the grouped API: the `account_id`, `types[]` and
//! `exclude_types[]` filters and their 404, the `expand_accounts` modes,
//! the emoji reaction groups keyed per status *and* emoji, and the two
//! gates — every route demands a user credential and the reads and the
//! dismissal demand their scopes.

use axum::http::StatusCode;
use serde_json::json;
use uuid::Uuid;
use wired_be::models::status::Visibility;

use super::support::{
    LIST, NOT_FOUND, READ, WRITE, call_every_route, call_the_reads, encoded, expanded_ids,
    favourite_note, fixture, group_keys, grouped_routes, groups, id_list, list, seed_one_favourite,
};
use crate::common;

/// The `account_id`, `types[]` and `exclude_types[]` filters narrow the
/// groups before they page, and an `account_id` naming no readable account
/// is the 404 Pleroma's `grouped_index` answers before listing.
#[tokio::test]
async fn filters_narrow_the_page_and_a_stranger_id_is_the_404() {
    let db = common::test_db().await;
    let pool = db.pool().clone();
    let (alice, token) = fixture(&pool, "alice", READ).await;
    let erin = common::create_local_user(&pool, "erin").await;
    let bob = common::create_local_user(&pool, "bob").await;
    let carol = common::create_local_user(&pool, "carol").await;
    let shared = common::create_status(&pool, &erin, "two fans", Visibility::Public).await;
    let lonely = common::create_status(&pool, &erin, "one fan", Visibility::Public).await;
    let follow = common::create_notification(&pool, &alice, &bob, "follow", None).await;
    favourite_note(&pool, &alice, &bob, &shared).await;
    favourite_note(&pool, &alice, &carol, &shared).await;
    favourite_note(&pool, &alice, &carol, &lonely).await;
    let app = common::test_app(pool);

    let all = list(&app, &token, "").await;
    assert_eq!(
        group_keys(&all),
        vec![
            format!("favourite-{}", lonely.id),
            format!("favourite-{}", shared.id),
            format!("ungrouped-{}", follow.id),
        ],
        "the two favourites of one status merge; the rest stand alone"
    );

    let by_actor = list(&app, &token, &format!("?account_id={}", carol.id)).await;
    assert_eq!(
        group_keys(&by_actor),
        vec![
            format!("favourite-{}", lonely.id),
            format!("favourite-{}", shared.id),
        ],
        "only carol's groups, newest first"
    );
    assert_eq!(
        expanded_ids(&by_actor["accounts"]),
        vec![carol.id.to_string()],
        "and only her account is expanded"
    );

    let follows = list(&app, &token, "?types[]=follow").await;
    assert_eq!(
        group_keys(&follows),
        vec![format!("ungrouped-{}", follow.id)],
        "types[] keeps only the named type"
    );
    let not_favourites = list(&app, &token, "?exclude_types[]=favourite").await;
    assert_eq!(
        group_keys(&not_favourites),
        vec![format!("ungrouped-{}", follow.id)],
        "and exclude_types[] drops the rest"
    );
    let combined = list(&app, &token, "?types[]=favourite&exclude_types[]=favourite").await;
    assert!(
        groups(&combined).is_empty(),
        "a type both kept and dropped leaves nothing"
    );

    for query in [
        format!("account_id={}", Uuid::now_v7()),
        "account_id=not-an-id".to_owned(),
    ] {
        let missing = common::TestRequest::get(&format!("{LIST}?{query}"))
            .bearer(&token)
            .send(&app)
            .await;
        common::assert_answer(&missing, StatusCode::NOT_FOUND, &query);
        common::assert_error(&missing, NOT_FOUND, &query);
    }
}

/// `expand_accounts` accepts the two documented modes — both answering
/// full accounts — rejects anything else with a 422, and an `account_id`
/// on the unread count resolves the same 404.
#[tokio::test]
async fn expand_accounts_validates_against_the_documented_modes() {
    let db = common::test_db().await;
    let pool = db.pool().clone();
    let (alice, token) = fixture(&pool, "alice", READ).await;
    let erin = common::create_local_user(&pool, "erin").await;
    let bob = common::create_local_user(&pool, "bob").await;
    let post = common::create_status(&pool, &erin, "expanded", Visibility::Public).await;
    favourite_note(&pool, &alice, &bob, &post).await;
    let app = common::test_app(pool.clone());

    let full = list(&app, &token, "?expand_accounts=full").await;
    let partial = list(&app, &token, "?expand_accounts=partial_avatars").await;
    assert_eq!(
        partial, full,
        "both modes answer the same envelope (one avatar per account, \
         Pleroma ignores the parameter entirely)"
    );
    assert_eq!(
        expanded_ids(&partial["accounts"]),
        vec![bob.id.to_string()],
        "with full Account entities either way"
    );

    for mode in ["", "avatars", "FULL", "partial"] {
        let response = common::TestRequest::get(&format!("{LIST}?expand_accounts={mode}"))
            .bearer(&token)
            .send(&app)
            .await;
        common::assert_answer(
            &response,
            StatusCode::UNPROCESSABLE_ENTITY,
            &format!("expand_accounts={mode:?}"),
        );
        assert!(
            response.json_value()["error"]
                .as_str()
                .is_some_and(|message| message.starts_with("Invalid parameters")),
            "the extractor-worded 422: {}",
            response.text()
        );
    }

    let stranger = common::TestRequest::get(&format!(
        "{LIST}/unread_count?account_id={}",
        Uuid::now_v7()
    ))
    .bearer(&token)
    .send(&app)
    .await;
    common::assert_answer(&stranger, StatusCode::NOT_FOUND, "a stranger on the count");
}

/// Emoji reactions group per status *and* emoji — the emoji rides in the
/// key — and the encoded key reads, accounts and dismisses like any other.
#[tokio::test]
async fn emoji_reaction_groups_carry_their_emoji() {
    let db = common::test_db().await;
    let pool = db.pool().clone();
    let (alice, read_token) = fixture(&pool, "alice", READ).await;
    let write_token = common::create_app_and_token(&pool, &alice, WRITE).await;
    let erin = common::create_local_user(&pool, "erin").await;
    let bob = common::create_local_user(&pool, "bob").await;
    let carol = common::create_local_user(&pool, "carol").await;
    let dave = common::create_local_user(&pool, "dave").await;
    let post = common::create_status(&pool, &erin, "react to me", Visibility::Public).await;

    let party_first = common::create_reaction(&pool, &alice, &bob, &post, "🎉").await;
    let _party_second = common::create_reaction(&pool, &alice, &carol, &post, "🎉").await;
    let heart = common::create_reaction(&pool, &alice, &dave, &post, "❤️").await;
    let app = common::test_app(pool.clone());

    let body = list(&app, &read_token, "").await;
    let listed = groups(&body);
    assert_eq!(listed.len(), 2, "one group per status-and-emoji");
    assert_eq!(
        listed[0]["group_key"],
        json!(format!("pleroma:emoji_reaction-{}-❤️", post.id)),
        "the newest emoji first"
    );
    assert_eq!(
        listed[0]["most_recent_notification_id"],
        heart.id.to_string(),
        "its own row"
    );
    assert_eq!(
        listed[1]["group_key"],
        json!(format!("pleroma:emoji_reaction-{}-🎉", post.id)),
        "then the party group…"
    );
    assert_eq!(
        listed[1]["notifications_count"], 2,
        "…counting both reactors"
    );
    assert_eq!(
        listed[1]["page_min_id"],
        party_first.id.to_string(),
        "bounded by the older party reaction"
    );
    assert_eq!(
        id_list(&listed[1]["sample_account_ids"]),
        vec![carol.id.to_string(), bob.id.to_string()],
        "newest reactor first"
    );
    assert_eq!(
        expanded_ids(&body["statuses"]),
        vec![post.id.to_string()],
        "one status expanded for all three groups' worth of rows"
    );

    let party_key = format!("pleroma:emoji_reaction-{}-🎉", post.id);
    let shown = common::TestRequest::get(&format!("{LIST}/{}", encoded(&party_key)))
        .bearer(&read_token)
        .send(&app)
        .await;
    common::assert_answer(&shown, StatusCode::OK, "the encoded key reads back");
    assert_eq!(groups(&shown.json_value()).len(), 1, "as one group");

    let actors = common::TestRequest::get(&format!("{LIST}/{}/accounts", encoded(&party_key)))
        .bearer(&read_token)
        .send(&app)
        .await;
    common::assert_answer(&actors, StatusCode::OK, "the encoded key's accounts");
    assert_eq!(
        expanded_ids(&actors.json_value()),
        vec![carol.id.to_string(), bob.id.to_string()],
        "both reactors, newest first"
    );

    let dismissed = common::TestRequest::post(&format!("{}/{}/dismiss", LIST, encoded(&party_key)))
        .bearer(&write_token)
        .send(&app)
        .await;
    common::assert_answer(&dismissed, StatusCode::OK, "the encoded key dismisses");
    let after = list(&app, &read_token, "").await;
    assert_eq!(
        group_keys(&after),
        vec![format!("pleroma:emoji_reaction-{}-❤️", post.id)],
        "the 🎉 group is gone, emoji aside"
    );
}

/// No credential answers 401 "Invalid credentials" on every grouped route,
/// and an app-only token — a live credential that names no user — answers
/// 403 "This API requires an authenticated user" on all of them too.
#[tokio::test]
async fn every_route_demands_a_user_credential() {
    let db = common::test_db().await;
    let pool = db.pool().clone();
    let (alice, _) = fixture(&pool, "alice", READ).await;
    let app_only = common::create_app_only_token(&pool, "read write").await;
    let key = seed_one_favourite(&pool, &alice).await;
    let app = common::test_app(pool);

    let anonymous = call_every_route(&app, "", &key).await;
    common::assert_all_refused(
        &anonymous,
        StatusCode::UNAUTHORIZED,
        "Invalid credentials",
        "an unauthenticated grouped notification request",
    );

    let app_only_refusals = call_every_route(&app, &app_only, &key).await;
    common::assert_all_refused(
        &app_only_refusals,
        StatusCode::FORBIDDEN,
        "This API requires an authenticated user",
        "an app-only credential on the grouped API",
    );
}

/// The four grouped reads need `read:notifications` and the dismissal
/// needs `write:notifications`; the root `read` and `write` grants pass
/// the hierarchy for their respective halves.
#[tokio::test]
async fn the_notification_scopes_gate_reads_and_the_dismissal() {
    let db = common::test_db().await;
    let pool = db.pool().clone();
    let (alice, _) = fixture(&pool, "alice", READ).await;
    let read_root = common::create_app_and_token(&pool, &alice, "read").await;
    let write_root = common::create_app_and_token(&pool, &alice, "write").await;
    let narrow = common::create_app_and_token(&pool, &alice, "read:statuses").await;
    let key = seed_one_favourite(&pool, &alice).await;
    let app = common::test_app(pool);
    let dismiss = grouped_routes(&key).pop().expect("the dismissal is last").1;

    let read_denied = call_the_reads(&app, &narrow, &key).await;
    common::assert_all_refused(
        &read_denied,
        StatusCode::FORBIDDEN,
        "Insufficient permissions: read:notifications.",
        "a read:statuses token on the grouped reads",
    );
    let dismiss_denied = common::TestRequest::post(&dismiss)
        .bearer(&narrow)
        .send(&app)
        .await;
    common::assert_answer(
        &dismiss_denied,
        StatusCode::FORBIDDEN,
        "a read scope cannot dismiss",
    );

    let read_granted = call_the_reads(&app, &read_root, &key).await;
    for response in &read_granted {
        assert!(
            response.status != StatusCode::FORBIDDEN && response.status != StatusCode::UNAUTHORIZED,
            "`read` covers `read:notifications`: {}",
            response.status
        );
    }

    let wrong_side = common::TestRequest::post(&dismiss)
        .bearer(&read_root)
        .send(&app)
        .await;
    common::assert_answer(
        &wrong_side,
        StatusCode::FORBIDDEN,
        "a read token cannot dismiss",
    );
    common::assert_error(
        &wrong_side,
        "Insufficient permissions: write:notifications.",
        "a read token cannot dismiss",
    );

    let dismissed = common::TestRequest::post(&dismiss)
        .bearer(&write_root)
        .send(&app)
        .await;
    common::assert_answer(
        &dismissed,
        StatusCode::OK,
        "`write` covers `write:notifications`",
    );
}
