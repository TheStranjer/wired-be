//! The listing and the single schedule: the page is the caller's own rows
//! newest id first with the whole entity per row, the cursors walk it from
//! both ends at Pleroma's sizes, `GET /{id}` answers the same entity back,
//! and an unreachable schedule is one 404 on every id-keyed route.

use axum::http::StatusCode;
use chrono::TimeDelta;
use serde_json::json;
use uuid::Uuid;
use wired_be::util::time::mastodon_time;

use super::routes_support::{
    NOT_AN_ID, READ, cancel, day, entity_ids, entity_texts, expected_link, list, manager, move_to,
    reader, row_ids, seeded, seeded_text, show,
};
use super::support::{NOT_FOUND, keys, object_keys, stored_schedule, upload};
use crate::common;

/// The listing answers the caller's own schedules, keyed and ordered on the
/// row id exactly as Pleroma's `Pagination.fetch_paginated` leaves them:
/// newest created first, the due time nowhere in the ordering, and the whole
/// `ScheduledStatus` entity per row.
#[tokio::test]
async fn the_listing_answers_the_callers_own_schedules_newest_id_first() {
    let db = common::test_db().await;
    let pool = db.pool().clone();
    let (alice, read) = reader(&pool, "alice").await;
    let (bob, bob_read) = reader(&pool, "bob").await;
    // Created oldest-first, with the due times the other way round: a page
    // ordered by `scheduled_at` would answer `newest` last, so the order
    // below can only be the id's.
    let oldest = seeded_text(&pool, &alice, day(9), "oldest").await;
    let middle = seeded_text(&pool, &alice, day(5), "middle").await;
    let newest = seeded_text(&pool, &alice, day(1), "newest").await;
    let theirs = seeded_text(&pool, &bob, day(2), "not yours").await;
    let app = common::test_app(pool.clone());

    let first = list(&app, &read, "").await;
    common::assert_answer(&first, StatusCode::OK, "the caller's schedules");
    let body = first.json_value();
    assert_eq!(
        entity_texts(&body),
        vec!["newest", "middle", "oldest"],
        "newest id first, which is not the due-time order"
    );
    assert_eq!(
        entity_ids(&body),
        row_ids(&[&newest, &middle, &oldest]),
        "the three ids, in that order"
    );
    assert!(
        !entity_ids(&body).contains(&theirs.id.to_string()),
        "and not one word of another account's schedule"
    );
    assert_eq!(
        first.header("link"),
        Some(expected_link(&newest.id.to_string(), &oldest.id.to_string()).as_str(),),
        "next walks to the oldest row shown, prev back to the newest"
    );

    let shown = body
        .as_array()
        .expect("the page is an array")
        .first()
        .expect("the page holds a row")
        .clone();
    assert_eq!(
        object_keys(&shown),
        keys(&["id", "scheduled_at", "params", "media_attachments"]),
        "every entry is a full ScheduledStatus entity"
    );
    assert_eq!(
        shown["scheduled_at"],
        mastodon_time(&newest.scheduled_at),
        "the row's own moment, in the Mastodon format"
    );
    assert_eq!(
        shown["params"]["text"], "newest",
        "the parked post echoed as the API names it"
    );
    assert_eq!(shown["media_attachments"], json!([]), "nothing reserved");

    assert_eq!(
        entity_ids(&list(&app, &bob_read, "").await.json_value()),
        row_ids(&[&theirs]),
        "and bob's page is his own one row, not alice's three"
    );
}

/// The cursors are ids and they walk the listing from both ends: `limit`
/// shortens a page and its `Link` header with it, `max_id` continues where a
/// page ended, `min_id` answers the newer page (newest first all the same),
/// `since_id` reads everything after its cursor, and an account with nothing
/// answers `[]` with no `Link` header.
#[tokio::test]
async fn the_listing_walks_its_id_cursors_in_both_directions() {
    let db = common::test_db().await;
    let pool = db.pool().clone();
    let (alice, read) = reader(&pool, "alice").await;
    let oldest = seeded_text(&pool, &alice, day(9), "oldest").await;
    let middle = seeded_text(&pool, &alice, day(5), "middle").await;
    let newest = seeded_text(&pool, &alice, day(1), "newest").await;
    let app = common::test_app(pool.clone());

    let page = list(&app, &read, "?limit=2").await;
    common::assert_answer(&page, StatusCode::OK, "a two-row page");
    assert_eq!(
        entity_ids(&page.json_value()),
        row_ids(&[&newest, &middle]),
        "the limit cuts the oldest row off the end"
    );
    assert_eq!(
        page.header("link"),
        Some(expected_link(&newest.id.to_string(), &middle.id.to_string()).as_str()),
        "and the cursors move with the page"
    );

    let older = list(&app, &read, &format!("?max_id={}", middle.id)).await;
    assert_eq!(
        entity_ids(&older.json_value()),
        row_ids(&[&oldest]),
        "max_id continues where the page ended"
    );
    let newer = list(&app, &read, &format!("?min_id={}", middle.id)).await;
    assert_eq!(
        entity_ids(&newer.json_value()),
        row_ids(&[&newest]),
        "min_id answers the newer page, still newest first"
    );
    let unseen = list(&app, &read, &format!("?since_id={}", middle.id)).await;
    assert_eq!(
        entity_ids(&unseen.json_value()),
        row_ids(&[&newest]),
        "since_id reads everything made after its cursor"
    );

    let carol = common::create_local_user(&pool, "carol").await;
    let carol_read = common::create_app_and_token(&pool, &carol, READ).await;
    let nothing = list(&app, &carol_read, "").await;
    common::assert_answer(&nothing, StatusCode::OK, "an account with no schedule");
    assert_eq!(nothing.json_value(), json!([]), "none is the empty array");
    assert_eq!(
        nothing.header("link"),
        None,
        "and an empty page links nowhere"
    );
}

/// The page size is Pleroma's: 20 rows when the request asks for none (or for
/// something unusable), 40 at the most however hard a client pushes, and the
/// `max_id` walk reaches every schedule exactly once.
#[tokio::test]
async fn the_listing_is_twenty_rows_by_default_and_forty_at_most() {
    let db = common::test_db().await;
    let pool = db.pool().clone();
    let (alice, read) = reader(&pool, "alice").await;
    let mut made = Vec::new();
    for index in 0..45_i64 {
        let when = day(1) + TimeDelta::minutes(index);
        made.push(seeded_text(&pool, &alice, when, "x").await);
    }
    let app = common::test_app(pool.clone());
    // Creation order is id order (decision 4), so the pages run the seed back
    // to front.
    let newest_first: Vec<String> = made.iter().rev().map(|row| row.id.to_string()).collect();

    let first = list(&app, &read, "").await;
    common::assert_answer(&first, StatusCode::OK, "the default page");
    assert_eq!(
        entity_ids(&first.json_value()),
        newest_first[..20],
        "20 of the 45, newest id first"
    );
    for query in ["?limit=0", "?limit=-5", "?limit=abc"] {
        assert_eq!(
            entity_ids(&list(&app, &read, query).await.json_value()).len(),
            20,
            "`{query}` is no usable limit, so the default page answers"
        );
    }
    assert_eq!(
        entity_ids(&list(&app, &read, "?limit=5").await.json_value()).len(),
        5,
        "a usable limit goes straight through"
    );
    for query in ["?limit=40", "?limit=41", "?limit=1000"] {
        assert_eq!(
            entity_ids(&list(&app, &read, query).await.json_value()).len(),
            40,
            "`{query}` is past the cap, and the cap is what the page holds"
        );
    }

    let mut seen: Vec<String> = Vec::new();
    let mut cursor: Option<Uuid> = None;
    for _ in 0..10 {
        let query = cursor.map_or_else(String::new, |id| format!("?max_id={id}"));
        let page = list(&app, &read, &query).await;
        let ids = entity_ids(&page.json_value());
        if ids.is_empty() {
            assert_eq!(
                page.header("link"),
                None,
                "the walk's last page is empty and links nowhere"
            );
            break;
        }
        seen.extend(ids.iter().cloned());
        cursor = Some(
            ids.last()
                .expect("a non-empty page has a last row")
                .parse::<Uuid>()
                .expect("the ids are ids"),
        );
    }
    assert_eq!(
        seen, newest_first,
        "two full pages and a last of five: every schedule exactly once, in one order"
    );
}

/// `GET /api/v1/scheduled_statuses/{id}` answers the one schedule as the same
/// entity the listing gives it, its reserved attachments read back in the
/// order its params name them.
#[tokio::test]
async fn show_answers_the_one_schedule_it_holds_back() {
    let db = common::test_db().await;
    let pool = db.pool().clone();
    let (alice, read) = reader(&pool, "alice").await;
    let older = upload(&pool, &alice).await;
    let newer = upload(&pool, &alice).await;
    let row = seeded(
        &pool,
        &alice,
        day(3),
        json!({
            "status": "pics tomorrow",
            "visibility": "private",
            "media_ids": [newer.id.to_string(), older.id.to_string()],
        }),
    )
    .await;
    let app = common::test_app(pool.clone());

    let response = show(&app, &read, &row.id.to_string()).await;
    common::assert_answer(&response, StatusCode::OK, "the caller's own schedule");
    let entity = response.json_value();
    assert_eq!(
        object_keys(&entity),
        keys(&["id", "scheduled_at", "params", "media_attachments"]),
        "the entity's own keys, no more"
    );
    assert_eq!(
        entity["id"],
        row.id.to_string(),
        "the id as hyphenated text"
    );
    assert_eq!(
        entity["scheduled_at"],
        mastodon_time(&row.scheduled_at),
        "the moment it is due"
    );
    assert_eq!(
        entity["params"]["text"], "pics tomorrow",
        "the parked post echoed, as is its audience"
    );
    assert_eq!(entity["params"]["visibility"], "private");
    assert_eq!(
        entity["params"]["media_ids"],
        json!([newer.id.to_string(), older.id.to_string()]),
        "and the reservation, in the stored order"
    );
    let attachments = entity["media_attachments"]
        .as_array()
        .expect("media_attachments is an array");
    assert_eq!(attachments.len(), 2, "both reserved uploads are entities");
    assert_eq!(
        attachments[0]["id"],
        newer.id.to_string(),
        "in the order the params name them, not the id order"
    );
    assert_eq!(attachments[1]["id"], older.id.to_string());

    let listed = list(&app, &read, "").await.json_value();
    assert_eq!(
        listed
            .as_array()
            .expect("the listing is an array")
            .first()
            .expect("the row is listed"),
        &entity,
        "and the listing answers the very same entity"
    );
}

/// A schedule the caller cannot reach is one 404 `{"error":"Record not
/// found"}` however it is unreachable — somebody else's row, an id the table
/// never held, id text that is no id — on each of the three id-keyed routes,
/// and none of the probes touches the row it aimed at.
#[tokio::test]
async fn an_unreachable_schedule_is_one_404_for_every_route() {
    let db = common::test_db().await;
    let pool = db.pool().clone();
    let (alice, alice_read, alice_write) = manager(&pool, "alice").await;
    let (_bob, bob_read, bob_write) = manager(&pool, "bob").await;
    let row = seeded_text(&pool, &alice, day(3), "alice's own").await;
    let unknown = Uuid::now_v7().to_string();
    let at = mastodon_time(&day(8));
    let body = json!({"scheduled_at": at});
    let app = common::test_app(pool.clone());

    let probes = [
        (
            show(&app, &bob_read, &row.id.to_string()).await,
            "bob looking",
        ),
        (
            move_to(&app, &bob_write, &row.id.to_string(), body.clone()).await,
            "bob moving",
        ),
        (
            cancel(&app, &bob_write, &row.id.to_string()).await,
            "bob cancelling",
        ),
        (
            show(&app, &alice_read, &unknown).await,
            "an id never stored",
        ),
        (
            move_to(&app, &alice_write, &unknown, body.clone()).await,
            "a move of an id never stored",
        ),
        (
            cancel(&app, &alice_write, &unknown).await,
            "a cancel of an id never stored",
        ),
        (
            show(&app, &alice_read, NOT_AN_ID).await,
            "id text that is no id",
        ),
        (
            move_to(&app, &alice_write, NOT_AN_ID, body).await,
            "a move over id text that is no id",
        ),
        (
            cancel(&app, &alice_write, NOT_AN_ID).await,
            "a cancel over id text that is no id",
        ),
    ];
    for (response, who) in probes {
        common::assert_answer(&response, StatusCode::NOT_FOUND, who);
        common::assert_error(&response, NOT_FOUND, who);
    }

    let intact = stored_schedule(&pool, row.id).await;
    assert_eq!(intact.scheduled_at, day(3), "none of them moved it");
    assert_eq!(
        intact.params,
        json!({ "status": "alice's own" }),
        "none of them rewrote it"
    );
    common::assert_answer(
        &cancel(&app, &alice_write, &row.id.to_string()).await,
        StatusCode::OK,
        "and alice still can",
    );
}
