//! The v1 application of stored filters to the rendered listings
//! (WBE-129), under `services::filter`'s semantics: a warning filter keeps
//! its matches and sets the status entity's `filtered` array to the v2
//! `FilterResult` shape; an `irreversible` one removes them from the home,
//! public, account and thread pages — and drops the whole notification
//! whose status it matched from the listing. Whole-word and context
//! scoping, expiry, reblog originals and the viewer's own-status exemption
//! all decide the page, and an unauthenticated (or unfiltered) read carries
//! the empty array everywhere.

use axum::http::StatusCode;
use chrono::{Duration, Utc};
use serde_json::json;
use wired_be::models::{
    filter, follow,
    status::{self, Visibility},
};

use super::support::{
    BASE, HOME, NOTES, PUBLIC, expected, filtered_of, fixture, hider, id_of, ids, looser,
    newest_first, page_of, reply, seed, warner,
};
use crate::common;

/// A warning filter annotates the matching home-timeline status with the
/// v2 result — and whole-word matching decides what counts: the glued
/// substring and the clean post stay an empty `filtered` array.
#[tokio::test]
async fn a_home_warning_annotates_whole_word_matches() {
    let db = common::test_db().await;
    let pool = db.pool().clone();
    let (alice, alice_token) = fixture(&pool, "alice", "read:statuses").await;
    let bob = common::create_local_user(&pool, "bob").await;
    let _followed = follow::follow(&pool, BASE, &alice, &bob)
        .await
        .expect("alice follows bob");
    let bounded = common::create_status(&pool, &bob, "a foo bar", Visibility::Public).await;
    let glued = common::create_status(&pool, &bob, "loved the foood", Visibility::Public).await;
    let clean = common::create_status(&pool, &bob, "quiet evening", Visibility::Public).await;
    let phrase = warner(&pool, &alice, "foo", &["home"]).await;
    let app = common::test_app(pool);

    let body = page_of(&app, &alice_token, HOME).await;
    assert_eq!(
        ids(&body),
        newest_first(&[&bounded, &glued, &clean]),
        "a warning never removes a status"
    );
    assert_eq!(
        filtered_of(&body, &id_of(&bounded)),
        json!([expected(&phrase, "home", true)]),
        "the bounded occurrence is annotated with exactly the ticket's shape"
    );
    assert_eq!(
        filtered_of(&body, &id_of(&glued)),
        json!([]),
        "the glued occurrence is no whole word"
    );
    assert_eq!(
        filtered_of(&body, &id_of(&clean)),
        json!([]),
        "and the unmatched status carries the empty array"
    );
}

/// Whole-word off makes the glued occurrence a substring match — while the
/// viewer's own matching status stays exempt and unannotated.
#[tokio::test]
async fn a_loose_home_filter_matches_substrings_but_never_the_viewers_own() {
    let db = common::test_db().await;
    let pool = db.pool().clone();
    let (alice, alice_token) = fixture(&pool, "alice", "read:statuses").await;
    let bob = common::create_local_user(&pool, "bob").await;
    let _followed = follow::follow(&pool, BASE, &alice, &bob)
        .await
        .expect("alice follows bob");
    let glued = common::create_status(&pool, &bob, "loved the foood", Visibility::Public).await;
    let own = common::create_status(&pool, &alice, "my own foo", Visibility::Public).await;
    let phrase = looser(&pool, &alice, "foo", &["home"]).await;
    let app = common::test_app(pool);

    let body = page_of(&app, &alice_token, HOME).await;
    assert_eq!(
        filtered_of(&body, &id_of(&glued)),
        json!([expected(&phrase, "home", false)]),
        "the substring of a followed post annotates"
    );
    assert_eq!(
        ids(&body),
        newest_first(&[&own, &glued]),
        "and the viewer's own match was never in question"
    );
    assert_eq!(
        filtered_of(&body, &id_of(&own)),
        json!([]),
        "exempt means unannotated too"
    );
}

/// An irreversible filter removes its matches from the home timeline — and
/// never the viewer's own statuses.
#[tokio::test]
async fn an_irreversible_home_filter_removes_matches_and_spares_own_posts() {
    let db = common::test_db().await;
    let pool = db.pool().clone();
    let (alice, alice_token) = fixture(&pool, "alice", "read:statuses").await;
    let bob = common::create_local_user(&pool, "bob").await;
    let _followed = follow::follow(&pool, BASE, &alice, &bob)
        .await
        .expect("alice follows bob");
    let own = common::create_status(&pool, &alice, "my own spam", Visibility::Public).await;
    let theirs = common::create_status(&pool, &bob, "their spam", Visibility::Public).await;
    let clean = common::create_status(&pool, &bob, "quiet evening", Visibility::Public).await;
    let _doomed = hider(&pool, &alice, "spam", &["home"]).await;
    let app = common::test_app(pool);

    let body = page_of(&app, &alice_token, HOME).await;
    assert_eq!(
        ids(&body),
        newest_first(&[&own, &clean]),
        "the stranger's match leaves the page and the viewer's own stays"
    );
    assert_eq!(
        filtered_of(&body, &id_of(&own)),
        json!([]),
        "the exempt match carries the empty annotation"
    );
    assert!(
        !ids(&body).contains(&id_of(&theirs)),
        "and it left no annotated trace"
    );
}

/// An expired filter removes and annotates nothing.
#[tokio::test]
async fn an_expired_home_filter_changes_nothing() {
    let db = common::test_db().await;
    let pool = db.pool().clone();
    let (alice, alice_token) = fixture(&pool, "alice", "read:statuses").await;
    let bob = common::create_local_user(&pool, "bob").await;
    let _followed = follow::follow(&pool, BASE, &alice, &bob)
        .await
        .expect("alice follows bob");
    let row = common::create_status(&pool, &bob, "the spam word", Visibility::Public).await;
    let lapsed = Utc::now() - Duration::days(1);
    let _gone = seed(&pool, &alice, "spam", Some(lapsed)).await;
    let app = common::test_app(pool);

    let body = page_of(&app, &alice_token, HOME).await;
    assert_eq!(
        ids(&body),
        vec![id_of(&row)],
        "the expired row hides nothing"
    );
    assert_eq!(
        filtered_of(&body, &id_of(&row)),
        json!([]),
        "and annotates nothing"
    );
}

/// Context scoping over HTTP: on the public page a `public` filter applies
/// and a `home`-only one does not — and the viewer's own post stays even
/// under a matching public filter.
#[tokio::test]
async fn the_public_page_answers_to_public_context_filters_only() {
    let db = common::test_db().await;
    let pool = db.pool().clone();
    let (alice, alice_token) = fixture(&pool, "alice", "read:statuses").await;
    let bob = common::create_local_user(&pool, "bob").await;
    let home_match = common::create_status(&pool, &bob, "the key word", Visibility::Public).await;
    let public_match =
        common::create_status(&pool, &bob, "the spam sign", Visibility::Public).await;
    let own = common::create_status(&pool, &alice, "my spam too", Visibility::Public).await;
    let _home_only = hider(&pool, &alice, "word", &["home"]).await;
    let _public_hider = hider(&pool, &alice, "spam", &["public"]).await;
    let app = common::test_app(pool);

    let body = page_of(&app, &alice_token, PUBLIC).await;
    assert!(
        ids(&body).contains(&id_of(&home_match)),
        "the home-only filter ignores this page"
    );
    assert!(
        !ids(&body).contains(&id_of(&public_match)),
        "the public-context irreversible match is gone, leaving no entry \
         to annotate"
    );
    assert!(
        ids(&body).contains(&id_of(&own)),
        "the viewer's own public match stays, unannotated"
    );
    assert_eq!(
        filtered_of(&body, &id_of(&own)),
        json!([]),
        "the exemption reads as an empty array"
    );
}

/// A boost is filtered by its original's text: the irreversible match
/// takes the reblog row out with the original, and the warning filter
/// annotates the boost that rides the thread.
#[tokio::test]
async fn a_boost_is_filtered_by_its_originals_text() {
    let db = common::test_db().await;
    let pool = db.pool().clone();
    let (alice, alice_token) = fixture(&pool, "alice", "read:statuses").await;
    let bob = common::create_local_user(&pool, "bob").await;
    let carol = common::create_local_user(&pool, "carol").await;
    let original =
        common::create_status(&pool, &bob, "the spam original", Visibility::Public).await;
    let boost = status::create_reblog(&pool, BASE, &carol, &original, Visibility::Public)
        .await
        .expect("a fixture reblog writes")
        .0;
    let hider_row = hider(&pool, &alice, "spam", &["public"]).await;
    let app = common::test_app(pool.clone());

    let body = page_of(&app, &alice_token, PUBLIC).await;
    assert!(
        !ids(&body).contains(&id_of(&boost)) && !ids(&body).contains(&id_of(&original)),
        "the irreversible match takes the boost and its original out"
    );

    let deleted = filter::delete(&pool, hider_row.id, alice.id)
        .await
        .expect("the hider deletes");
    assert!(deleted, "and the row went");
    let warner_row = warner(&pool, &alice, "spam", &["public"]).await;
    let warned = page_of(&app, &alice_token, PUBLIC).await;
    assert_eq!(
        ids(&warned),
        newest_first(&[&boost, &original]),
        "the warning brings both back"
    );
    assert_eq!(
        filtered_of(&warned, &id_of(&boost)),
        json!([expected(&warner_row, "public", true)]),
        "and the boost rides its original's match, annotated"
    );
}

/// The notification listing annotates a warning match on the notification's
/// status, leaves status-less rows and the viewer's own posts alone, and
/// the irreversible match drops the whole notification.
#[tokio::test]
async fn the_notification_listing_applies_notifications_context_filters() {
    let db = common::test_db().await;
    let pool = db.pool().clone();
    let (alice, alice_token) = fixture(&pool, "alice", "read:notifications").await;
    let bob = common::create_local_user(&pool, "bob").await;
    let matched = common::create_status(&pool, &bob, "a spam favourite", Visibility::Public).await;
    let own = common::create_status(&pool, &alice, "my own spam", Visibility::Public).await;
    let on_matched =
        common::create_notification(&pool, &alice, &bob, "favourite", Some(&matched)).await;
    let on_own = common::create_notification(&pool, &alice, &bob, "favourite", Some(&own)).await;
    let no_status = common::create_notification(&pool, &alice, &bob, "follow", None).await;
    let hider_row = hider(&pool, &alice, "favourite", &["notifications"]).await;
    let warn_row = warner(&pool, &alice, "spam", &["notifications"]).await;
    let app = common::test_app(pool.clone());

    let body = page_of(&app, &alice_token, NOTES).await;
    let entities = body.as_array().expect("the listing is an array");
    assert_eq!(
        entities.len(),
        2,
        "the notification about the viewer's own post and the follow stay, \
         the irreversible match is gone"
    );
    assert_eq!(
        entities[0]["id"],
        no_status.id.to_string(),
        "the status-less newest row is untouched"
    );
    let own_note = &entities[1];
    assert_eq!(
        own_note["id"],
        on_own.id.to_string(),
        "and the own-status row"
    );
    assert_eq!(
        own_note["status"]["filtered"],
        json!([]),
        "the viewer's own status is exempt even in the notifications context"
    );

    let _gone = filter::delete(&pool, hider_row.id, alice.id)
        .await
        .expect("the hider deletes");
    let warned = page_of(&app, &alice_token, NOTES).await;
    let notes = warned.as_array().expect("the listing is an array");
    assert_eq!(notes.len(), 3, "with the hider gone all three rows read");
    let liked = notes
        .iter()
        .find(|note| note["id"] == on_matched.id.to_string())
        .expect("the favourite notification is back");
    assert_eq!(
        liked["status"]["filtered"],
        json!([expected(&warn_row, "notifications", true)]),
        "its status carries the warning annotation"
    );
    assert!(
        notes
            .iter()
            .any(|note| note["id"] == no_status.id.to_string() && note.get("status").is_none()),
        "a follow notification carries no status key at all"
    );
}

/// The thread context pass annotates and removes on both arms of
/// `GET /statuses/{id}/context`, and an anonymous read is the no-op.
#[tokio::test]
async fn the_thread_context_filters_both_arms() {
    let db = common::test_db().await;
    let pool = db.pool().clone();
    let (alice, alice_token) = fixture(&pool, "alice", "read:statuses").await;
    let bob = common::create_local_user(&pool, "bob").await;
    let carol = common::create_local_user(&pool, "carol").await;
    let root = common::create_status(&pool, &bob, "the spam root", Visibility::Public).await;
    let middle = reply(&pool, &carol, &root, "quiet middle").await;
    let leaf = reply(&pool, &bob, &middle, "a spam reply").await;
    let hider_row = hider(&pool, &alice, "spam", &["thread"]).await;
    let app = common::test_app(pool.clone());

    let context = page_of(
        &app,
        &alice_token,
        &format!("/api/v1/statuses/{}/context", middle.id),
    )
    .await;
    assert_eq!(
        context["ancestors"],
        json!([]),
        "the matched ancestor is gone"
    );
    assert_eq!(
        context["descendants"],
        json!([]),
        "and the matched descendant"
    );

    let deleted = filter::delete(&pool, hider_row.id, alice.id)
        .await
        .expect("the hider deletes");
    assert!(deleted, "and the row went");
    let warner_row = warner(&pool, &alice, "spam", &["thread"]).await;
    let warned = page_of(
        &app,
        &alice_token,
        &format!("/api/v1/statuses/{}/context", middle.id),
    )
    .await;
    assert_eq!(
        warned["ancestors"][0]["filtered"],
        json!([expected(&warner_row, "thread", true)]),
        "with the hider gone the ancestor reads annotated"
    );
    assert_eq!(
        warned["descendants"][0]["filtered"],
        json!([expected(&warner_row, "thread", true)]),
        "and so does the descendant"
    );

    let anonymous = common::TestRequest::get(&format!("/api/v1/statuses/{}/context", middle.id))
        .send(&app)
        .await;
    assert_eq!(
        anonymous.status,
        StatusCode::OK,
        "an anonymous thread reads"
    );
    let anon = anonymous.json_value();
    assert_eq!(
        anon["ancestors"].as_array().expect("an array").len(),
        1,
        "missing filters (no auth) remove nothing"
    );
    assert_eq!(
        anon["ancestors"][0]["filtered"],
        json!([]),
        "and annotate nothing"
    );
    assert_eq!(
        ids(&anon["descendants"]),
        vec![id_of(&leaf)],
        "the whole thread stands"
    );
}

/// The account-status listing filters for a stranger — and not for the
/// account owner reading their own profile.
#[tokio::test]
async fn the_account_listing_filters_for_strangers_never_for_the_owner() {
    let db = common::test_db().await;
    let pool = db.pool().clone();
    let (alice, alice_token) = fixture(&pool, "alice", "read:statuses").await;
    let (bob, bob_token) = fixture(&pool, "bob", "read:statuses").await;
    let matched = common::create_status(&pool, &bob, "profile spam", Visibility::Public).await;
    let clean = common::create_status(&pool, &bob, "profile clean", Visibility::Public).await;
    let profile = format!("/api/v1/accounts/{}/statuses", bob.id);
    let _stranger_hider = hider(&pool, &alice, "spam", &["account"]).await;
    let _owner_hider = hider(&pool, &bob, "spam", &["account"]).await;
    let app = common::test_app(pool);

    let as_alice = page_of(&app, &alice_token, &profile).await;
    assert_eq!(
        ids(&as_alice),
        vec![id_of(&clean)],
        "the stranger's account-context filter removes the match"
    );
    let as_bob = page_of(&app, &bob_token, &profile).await;
    assert_eq!(
        ids(&as_bob),
        newest_first(&[&matched, &clean]),
        "the owner's own matching filter hides nothing of their profile"
    );
    assert_eq!(
        filtered_of(&as_bob, &id_of(&matched)),
        json!([]),
        "and annotates nothing either"
    );
}

/// Every rendered status carries the `filtered` key, even when the viewer
/// has no filters at all.
#[tokio::test]
async fn a_user_without_filters_reads_the_empty_array() {
    let db = common::test_db().await;
    let pool = db.pool().clone();
    let (_carol, carol_token) = fixture(&pool, "carol", "read:statuses").await;
    let bob = common::create_local_user(&pool, "bob").await;
    let row = common::create_status(&pool, &bob, "plain post", Visibility::Public).await;
    let app = common::test_app(pool);

    let body = page_of(&app, &carol_token, PUBLIC).await;
    assert_eq!(ids(&body), vec![id_of(&row)], "the page is the page");
    assert_eq!(
        filtered_of(&body, &id_of(&row)),
        json!([]),
        "and the entity answers the empty filtered array"
    );
}
