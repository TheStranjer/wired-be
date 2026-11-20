//! The move and the cancel: `PUT` takes the row and its fire to the new
//! moment over every body shape, and refuses a moment inside the window or
//! an unreachable id without disturbing either; `DELETE` takes the row, its
//! fire and its media reservations with it, and answers `{}`.

use axum::http::StatusCode;
use chrono::{TimeDelta, Utc};
use serde_json::json;
use uuid::Uuid;
use wired_be::{
    jobs::scheduled_statuses::KIND_PUBLISH, models::scheduled_status, util::time::mastodon_time,
};

use super::routes_support::{
    NOT_AN_ID, SCHEDULES, TOO_SOON, assert_one_fire, cancel, day, entity_ids, fires_for, list,
    manager, move_form, move_to, parked, seeded, seeded_text, show,
};
use super::support::{
    INVALID_SCHEDULED_AT, NOT_FOUND, in_minutes, jobs_of_kind, keys, object_keys, post_count,
    schedule, scheduled_count, stored_schedule, upload,
};
use crate::common;

/// `PUT` moves the row and its fire together: the parked request is untouched,
/// exactly one `scheduled_statuses.publish` job stands for the schedule and it
/// is due at the new moment, as a fresh row — the waiting one was deleted by
/// its `unique_key` rather than edited in place.
#[tokio::test]
async fn a_move_takes_the_row_and_its_fire_to_the_new_moment() {
    let db = common::test_db().await;
    let pool = db.pool().clone();
    let (_alice, read, write) = manager(&pool, "alice").await;
    let app = common::test_app(pool.clone());

    let created = schedule(
        &app,
        &write,
        json!({"status": "  move me  ", "scheduled_at": mastodon_time(&day(3))}),
    )
    .await;
    common::assert_answer(&created, StatusCode::OK, "a scheduled create");
    let entity = created.json_value();
    let id = entity["id"]
        .as_str()
        .expect("the entity carries its id")
        .parse::<Uuid>()
        .expect("the id is a uuid");
    let before = stored_schedule(&pool, id).await;
    let waiting = fires_for(&pool, id).await;
    assert_eq!(waiting.len(), 1, "the create queued one fire");
    assert_eq!(waiting[0].run_at, day(3), "due at the submitted moment");

    let moved = move_to(
        &app,
        &write,
        &id.to_string(),
        json!({"scheduled_at": mastodon_time(&day(10))}),
    )
    .await;
    common::assert_answer(&moved, StatusCode::OK, "a moved schedule");
    let after = moved.json_value();
    assert_eq!(
        after["id"], entity["id"],
        "the same schedule, not a new one"
    );
    assert_eq!(
        after["scheduled_at"],
        mastodon_time(&day(10)),
        "answered at the moment it now names"
    );
    assert_eq!(
        after["params"]["text"], "move me",
        "when a post fires is all a move touches"
    );
    assert_eq!(
        object_keys(&after),
        keys(&["id", "scheduled_at", "params", "media_attachments"]),
        "the whole entity, as the create answered it"
    );

    let row = stored_schedule(&pool, id).await;
    assert_eq!(row.scheduled_at, day(10), "the row holds the new moment");
    assert_eq!(row.params, before.params, "and the same normalised request");
    assert!(
        row.updated_at > row.inserted_at,
        "and knows it was written again"
    );

    assert_one_fire(&pool, id, day(10)).await;
    assert_ne!(
        fires_for(&pool, id).await[0].id,
        waiting[0].id,
        "a fresh job row: the waiting one was deleted, not edited"
    );
    assert_eq!(
        show(&app, &read, &id.to_string()).await.json_value()["scheduled_at"],
        mastodon_time(&day(10)),
        "the single read follows the move"
    );
    assert_eq!(
        list(&app, &read, "")
            .await
            .json_value()
            .as_array()
            .expect("the listing is an array")
            .first()
            .expect("the schedule is listed")["scheduled_at"],
        mastodon_time(&day(10)),
        "and so does the listing"
    );
}

/// A form body and a query parameter move the schedule the way a JSON body
/// does (decision 10), and an offset spelling of the instant is normalised to
/// the same UTC moment the `Z` form names.
#[tokio::test]
async fn a_move_reads_json_form_and_query_alike() {
    let db = common::test_db().await;
    let pool = db.pool().clone();
    let (_alice, _read, write) = manager(&pool, "alice").await;
    let app = common::test_app(pool.clone());
    let row = parked(&app, &pool, &write, "every shape", day(3)).await;
    let id = row.id.to_string();

    let by_form = move_form(&write, &id, &mastodon_time(&day(11)))
        .send(&app)
        .await;
    common::assert_answer(&by_form, StatusCode::OK, "a form body's scheduled_at");
    assert_eq!(
        by_form.json_value()["scheduled_at"],
        mastodon_time(&day(11)),
        "the form moved it"
    );

    let in_query = format!("{SCHEDULES}/{id}?scheduled_at={}", mastodon_time(&day(12)));
    let by_query = common::TestRequest::put(&in_query)
        .bearer(&write)
        .send(&app)
        .await;
    common::assert_answer(
        &by_query,
        StatusCode::OK,
        "a query parameter's scheduled_at",
    );
    assert_eq!(
        stored_schedule(&pool, row.id).await.scheduled_at,
        day(12),
        "and so did the query"
    );

    let offset = day(13) + TimeDelta::hours(1);
    let by_offset = move_to(
        &app,
        &write,
        &id,
        json!({"scheduled_at": "2027-03-14T02:00:00+01:00"}),
    )
    .await;
    common::assert_answer(&by_offset, StatusCode::OK, "an offset spelling");
    assert_eq!(
        by_offset.json_value()["scheduled_at"],
        mastodon_time(&offset),
        "normalised to the UTC instant it names"
    );
    assert_one_fire(&pool, row.id, offset).await;
}

/// A moment inside the window is the create's own refusal — a past instant and
/// a too-soon one alike, the wording naming the configured minutes — and text
/// that is no instant, or none at all, is `Invalid scheduled_at`. Not one of
/// them touches the row or its fire.
#[tokio::test]
async fn a_move_inside_the_window_is_refused_like_a_create() {
    let db = common::test_db().await;
    let pool = db.pool().clone();
    let (_alice, _read, write) = manager(&pool, "alice").await;
    let app = common::test_app(pool.clone());
    let row = parked(&app, &pool, &write, "stay put", day(3)).await;
    let id = row.id.to_string();

    for (body, who) in [
        (json!({"scheduled_at": in_minutes(-3_600)}), "an hour ago"),
        (json!({"scheduled_at": in_minutes(0)}), "this instant"),
        (json!({"scheduled_at": in_minutes(1)}), "one minute out"),
        (
            json!({"scheduled_at": mastodon_time(&(Utc::now() + TimeDelta::minutes(4)))}),
            "a minute inside the window",
        ),
    ] {
        let refused = move_to(&app, &write, &id, body).await;
        common::assert_answer(&refused, StatusCode::UNPROCESSABLE_ENTITY, who);
        common::assert_error(&refused, TOO_SOON, who);
    }

    for (body, who) in [
        (
            json!({"scheduled_at": "not-a-date"}),
            "text that is no instant",
        ),
        (json!({"scheduled_at": ""}), "empty text"),
        (json!({"scheduled_at": null}), "a null"),
        (json!({}), "no scheduled_at at all"),
        (
            json!({"scheduled_at": "2027-02-30T00:00:00Z"}),
            "a date that never happened",
        ),
    ] {
        let refused = move_to(&app, &write, &id, body).await;
        common::assert_answer(&refused, StatusCode::UNPROCESSABLE_ENTITY, who);
        common::assert_error(&refused, INVALID_SCHEDULED_AT, who);
    }

    let intact = stored_schedule(&pool, row.id).await;
    assert_eq!(
        intact.scheduled_at,
        day(3),
        "not one refusal moved the row off its moment"
    );
    assert_eq!(
        intact.updated_at, intact.inserted_at,
        "and not one restamped it either"
    );
    assert_one_fire(&pool, row.id, day(3)).await;
}

/// Pleroma's plug order, kept: the body is read, then the row is looked for,
/// and only then is the moment weighed. So a stranger's move — or a move of an
/// id that is no id — is the 404 the id alone already was, whatever it asked
/// the schedule to do.
#[tokio::test]
async fn a_move_checks_the_row_before_it_checks_the_moment() {
    let db = common::test_db().await;
    let pool = db.pool().clone();
    let (alice, _read, write) = manager(&pool, "alice").await;
    let (_bob, _bob_read, bob_write) = manager(&pool, "bob").await;
    let row = seeded_text(&pool, &alice, day(3), "not yours to move").await;
    let app = common::test_app(pool.clone());

    let too_soon = in_minutes(1);
    let far_off = mastodon_time(&day(9));
    let probes = [
        (
            bob_write.as_str(),
            row.id.to_string(),
            too_soon.clone(),
            "a stranger's move of a moment inside the window",
        ),
        (
            bob_write.as_str(),
            row.id.to_string(),
            far_off.clone(),
            "a stranger's move of a moment outside it",
        ),
        (
            write.as_str(),
            NOT_AN_ID.to_owned(),
            too_soon,
            "a move over id text that is no id",
        ),
    ];
    for (token, id, at, who) in probes {
        let refused = move_to(&app, token, &id, json!({"scheduled_at": at})).await;
        common::assert_answer(&refused, StatusCode::NOT_FOUND, who);
        common::assert_error(&refused, NOT_FOUND, who);
    }

    assert_eq!(
        scheduled_count(&pool).await,
        1,
        "nothing was stored or removed"
    );
    let intact = stored_schedule(&pool, row.id).await;
    assert_eq!(
        intact.scheduled_at,
        day(3),
        "and the row every probe named stands where it was"
    );
}

/// `DELETE` takes the row and the fire waiting for it, answers `{}`, and leaves
/// nothing behind: the listing no longer carries it, the queue holds no
/// publish for it, and every later probe is the shared 404.
#[tokio::test]
async fn a_cancel_takes_the_row_and_its_fire() {
    let db = common::test_db().await;
    let pool = db.pool().clone();
    let (_alice, read, write) = manager(&pool, "alice").await;
    let app = common::test_app(pool.clone());
    let row = parked(&app, &pool, &write, "cancel me", day(3)).await;
    let id = row.id;
    assert_one_fire(&pool, id, day(3)).await;

    let deleted = cancel(&app, &write, &id.to_string()).await;
    common::assert_answer(&deleted, StatusCode::OK, "a cancelled schedule");
    assert_eq!(
        deleted.json_value(),
        json!({}),
        "the body is the empty object Pleroma's delete renders"
    );

    assert_eq!(scheduled_count(&pool).await, 0, "the row is gone");
    assert!(
        scheduled_status::get_by_id(&pool, id)
            .await
            .expect("the schedule reads")
            .is_none(),
        "and it is that row that went"
    );
    assert!(
        fires_for(&pool, id).await.is_empty(),
        "and its waiting fire went with it"
    );
    assert!(
        jobs_of_kind(&pool, KIND_PUBLISH).await.is_empty(),
        "no publish job of any schedule is left"
    );
    assert!(
        entity_ids(&list(&app, &read, "").await.json_value()).is_empty(),
        "and the listing has nothing left to answer"
    );

    common::assert_all_refused(
        &[
            cancel(&app, &write, &id.to_string()).await,
            show(&app, &read, &id.to_string()).await,
            move_to(
                &app,
                &write,
                &id.to_string(),
                json!({"scheduled_at": mastodon_time(&day(9))}),
            )
            .await,
        ],
        StatusCode::NOT_FOUND,
        NOT_FOUND,
        "what was cancelled",
    );
    assert_eq!(
        post_count(&pool).await,
        0,
        "a cancelled schedule posts nothing"
    );
}

/// Cancelling is also what releases a schedule's attachments: while the row
/// stands its uploads are reserved from every other post, and the cancel takes
/// the reservation with them.
#[tokio::test]
async fn a_cancel_releases_the_uploads_it_reserved() {
    let db = common::test_db().await;
    let pool = db.pool().clone();
    let (alice, _read, write) = manager(&pool, "alice").await;
    let pic = upload(&pool, &alice).await;
    let row = seeded(
        &pool,
        &alice,
        day(4),
        json!({ "status": "pics", "media_ids": [pic.id.to_string()] }),
    )
    .await;
    let app = common::test_app(pool.clone());

    assert_eq!(
        scheduled_status::reserved_media_ids(&pool, &[pic.id])
            .await
            .expect("the reservation reads"),
        vec![pic.id],
        "the pending schedule holds the upload"
    );
    common::assert_answer(
        &cancel(&app, &write, &row.id.to_string()).await,
        StatusCode::OK,
        "the cancel",
    );
    assert!(
        scheduled_status::reserved_media_ids(&pool, &[pic.id])
            .await
            .expect("the reservation reads again")
            .is_empty(),
        "and the cancel released it"
    );

    let reposted = schedule(
        &app,
        &write,
        json!({
            "status": "now",
            "media_ids": [pic.id.to_string()],
            "scheduled_at": mastodon_time(&day(5)),
        }),
    )
    .await;
    common::assert_answer(
        &reposted,
        StatusCode::OK,
        "a schedule claiming the released upload",
    );
    assert_eq!(
        reposted.json_value()["media_attachments"][0]["id"],
        pic.id.to_string(),
        "and it answers as reserved once more"
    );
}
