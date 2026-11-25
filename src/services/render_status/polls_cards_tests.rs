//! Unit tests for the poll and rich-media card parts of a page render:
//! the poll embedded in its status with the viewer's marks, `hide_totals`
//! while a poll runs, and the card keyed to each row's own id.

use chrono::{DateTime, TimeZone as _, Utc};
use serde_json::{Value, json};
use sqlx::PgPool;
use uuid::Uuid;

use super::{
    ViewerMode, render_statuses,
    test_support::{BASE, app_state, post, user},
};
use crate::{
    entities::status::Status,
    models::{
        link_card,
        poll::{self, PollParams},
        status::{self, NewStatus, Status as StatusRow, Visibility, insert},
        user::User,
    },
};

/// [`PollParams`] for the given option titles, nothing else set — the
/// fixture polls' starting point.
fn poll_params(titles: &[&str]) -> PollParams {
    PollParams {
        options: titles.iter().map(|title| (*title).to_owned()).collect(),
        ..PollParams::default()
    }
}

/// Creates a poll on `status_id` through a connection of the pool's
/// own — the way a caller outside a transaction reaches
/// [`poll::create`].
async fn create_poll(pool: &PgPool, status_id: Uuid, params: &PollParams) -> poll::Poll {
    let mut conn = pool.acquire().await.expect("slot connection");
    poll::create(&mut conn, status_id, params)
        .await
        .expect("the fixture poll creates")
}

/// Records a mark straight into `poll_votes`: the vote endpoint that
/// normally writes it (and moves the counters) is a later ticket, and
/// the render batch's votes read needs rows to find.
async fn vote(pool: &PgPool, poll_id: Uuid, option_id: Uuid, user_id: Uuid) {
    let inserted = sqlx::query(
        "INSERT INTO poll_votes (poll_id, option_id, user_id, inserted_at)
         VALUES ($1, $2, $3, now())",
    )
    .bind(poll_id)
    .bind(option_id)
    .bind(user_id)
    .execute(pool)
    .await
    .expect("the fixture vote inserts");
    assert_eq!(inserted.rows_affected(), 1, "the mark was written");
}

/// Moves the stored roll-up counters the way the vote endpoint's
/// transaction will — they are columns, not computed reads (see
/// [`crate::models::poll`]) — to one voter, one vote, on `option_id`.
async fn count_one_vote(pool: &PgPool, poll_id: Uuid, option_id: Uuid) {
    let moved = sqlx::query("UPDATE polls SET voters_count = 1, votes_count = 1 WHERE id = $1")
        .bind(poll_id)
        .execute(pool)
        .await
        .expect("the poll counters move");
    assert_eq!(moved.rows_affected(), 1, "the poll row was there to move");
    let option_moved = sqlx::query("UPDATE poll_options SET votes_count = 1 WHERE id = $1")
        .bind(option_id)
        .execute(pool)
        .await
        .expect("the option counter moves");
    assert_eq!(
        option_moved.rows_affected(),
        1,
        "the option row was there to move"
    );
}

/// A deadline a thousand years out: whatever the render instant is, a
/// poll due then is still open, so `hide_totals` still withholds.
fn far_future() -> DateTime<Utc> {
    Utc.with_ymd_and_hms(2999, 1, 1, 0, 0, 0).unwrap()
}

/// The open-poll fixture the render batch's poll tests share: alice's
/// `tea or coffee` post carrying a multiple-choice poll, bob's mark on
/// its second option, and the roll-ups moved to one voter and one vote
/// the way the vote endpoint will.
async fn open_poll_fixture(pool: &PgPool) -> (User, User, StatusRow, poll::Poll) {
    let alice = user(pool, "alice").await;
    let bob = user(pool, "bob").await;
    let polled = post(pool, &alice, "tea or coffee").await;
    let mut params = poll_params(&["tea", "coffee"]);
    params.multiple = true;
    let open = create_poll(pool, polled.id, &params).await;
    let choices = poll::options(pool, open.id).await.expect("options read");
    let coffee = choices.get(1).expect("the fixture poll has two options");
    vote(pool, open.id, coffee.id, bob.id).await;
    count_one_vote(pool, open.id, coffee.id).await;
    (alice, bob, polled, open)
}

/// The whole [`Poll`] entity the batch builds for the fixture poll as
/// bob sees it: stored roll-ups, options in position order, and his
/// mark at position 1 in `voted` and `own_votes`.
///
/// [`Poll`]: crate::entities::poll::Poll
fn expected_open_poll(open: &poll::Poll, voted: bool, own_votes: &[i32]) -> Value {
    json!({
        "id": open.id.to_string(),
        "expires_at": null,
        "expired": false,
        "multiple": true,
        "votes_count": 1,
        "voters_count": 1,
        "voted": voted,
        "own_votes": own_votes,
        "options": [
            {"title": "tea", "votes_count": 0},
            {"title": "coffee", "votes_count": 1},
        ],
        "emojis": [],
    })
}

/// The polls ticket's loaders inside the fixed query set: one page
/// mixing a polled row, a plain one and a reblog of the polled one
/// embeds each poll in its status — the full entity for the viewer who
/// voted, `null` on the poll-less row and on the boost itself, and the
/// original's poll nested in the boost, from the same batch.
#[tokio::test]
async fn the_poll_embeds_in_the_render_batch() {
    let db = crate::test_db::test_db().await.expect("test database slot");
    let pool = db.pool().clone();
    let state = app_state(&pool);
    let (alice, bob, polled, open) = open_poll_fixture(&pool).await;
    let plain = post(&pool, &alice, "nothing asked").await;
    let reblog = insert(
        &pool,
        BASE,
        &NewStatus {
            reblog_of_id: Some(polled.id),
            ..NewStatus::local(&bob, "")
        },
    )
    .await
    .expect("a reblog of a stored post inserts");
    let expected = expected_open_poll(&open, true, &[1]);

    let page = [polled.clone(), plain.clone(), reblog.clone()];
    let for_bob = render_statuses(&state, ViewerMode::user(&bob), &page)
        .await
        .expect("the page renders for bob");
    assert_eq!(
        serde_json::to_value(&for_bob[0]).expect("the polled status serializes")["poll"],
        expected,
        "the whole entity rides its status, key for key"
    );
    assert_eq!(
        serde_json::to_value(&for_bob[1]).expect("the plain status serializes")["poll"],
        Value::Null,
        "a poll-less row renders poll: null"
    );
    let reblog_json = serde_json::to_value(&for_bob[2]).expect("the reblog serializes");
    assert_eq!(
        reblog_json["poll"],
        Value::Null,
        "the boost row carries no poll of its own"
    );
    assert_eq!(
        reblog_json["reblog"]["poll"], expected,
        "the nested original brings its poll along from the same batch"
    );
}

/// The same batch for a viewer with no marks: the votes read runs only
/// when there is one, so an anonymous render reports the poll voted by
/// nobody — `voted: false`, `own_votes: []` — while every stored
/// option and count renders unchanged.
#[tokio::test]
async fn an_anonymous_render_reports_the_page_poll_unvoted() {
    let db = crate::test_db::test_db().await.expect("test database slot");
    let pool = db.pool().clone();
    let state = app_state(&pool);
    let (_alice, _bob, polled, open) = open_poll_fixture(&pool).await;

    let anonymous = render_statuses(&state, ViewerMode::ANONYMOUS, std::slice::from_ref(&polled))
        .await
        .expect("the poll renders anonymously");
    let value = serde_json::to_value(&anonymous[0]).expect("it serializes");
    assert_eq!(
        value["poll"],
        expected_open_poll(&open, false, &[]),
        "the whole entity again, minus nobody's marks"
    );
}

/// `hide_totals` inside the render batch: while an expired-less-due
/// poll runs, its per-option counts come back JSON `null` even to the
/// viewer who voted; the roll-up totals, the deadline and the viewer's
/// own marks are not hidden totals and render as stored.
#[tokio::test]
async fn hide_totals_nulls_the_option_counts_while_the_poll_is_open() {
    let db = crate::test_db::test_db().await.expect("test database slot");
    let pool = db.pool().clone();
    let state = app_state(&pool);
    let alice = user(&pool, "alice").await;
    let bob = user(&pool, "bob").await;
    let secret = post(&pool, &alice, "hush").await;
    let mut params = poll_params(&["yes", "no"]);
    params.expires_at = Some(far_future());
    params.hide_totals = true;
    let hidden = create_poll(&pool, secret.id, &params).await;
    let choices = poll::options(&pool, hidden.id).await.expect("options read");
    let yes = choices.first().expect("the fixture poll has an option");
    vote(&pool, hidden.id, yes.id, bob.id).await;

    let for_bob = render_statuses(
        &state,
        ViewerMode::user(&bob),
        std::slice::from_ref(&secret),
    )
    .await
    .expect("the hidden-totals poll renders for bob");
    let value = serde_json::to_value(&for_bob[0]).expect("it serializes");
    assert_eq!(
        value["poll"]["options"],
        json!([
            {"title": "yes", "votes_count": null},
            {"title": "no", "votes_count": null},
        ]),
        "the per-option counts stay hidden while the poll is open"
    );
    assert_eq!(
        value["poll"]["votes_count"], 0,
        "the roll-up total renders as stored, hidden or not"
    );
    assert_eq!(
        value["poll"]["voted"],
        json!(true),
        "the viewer's own mark is not a hidden total"
    );
    assert_eq!(value["poll"]["own_votes"], json!([0]), "and it is his");
    assert_eq!(
        value["poll"]["expires_at"], "2999-01-01T00:00:00.000Z",
        "the deadline rides along"
    );
    assert_eq!(
        value["poll"]["expired"],
        json!(false),
        "due a thousand years out: the poll runs"
    );
    assert_eq!(value["poll"]["id"], hidden.id.to_string());
    assert_eq!(value["poll"]["multiple"], json!(false));
    assert_eq!(value["poll"]["voters_count"], 0);
}

/// The rich-media ticket's loader inside the fixed query set: one
/// batched read keys each row's card to its *own* id — a reblog row
/// renders its own card beside the original nested in it, a status
/// linked only to a failed fetch renders null, and cards are nobody's
/// private fact: the same row renders the same card for every viewer.
#[tokio::test]
async fn the_card_follows_one_batched_read() {
    let db = crate::test_db::test_db().await.expect("test database slot");
    let pool = db.pool().clone();
    let state = app_state(&pool);
    let alice = user(&pool, "alice").await;
    let bob = user(&pool, "bob").await;
    let original = post(&pool, &alice, "https://example.test/post").await;
    let dead = post(&pool, &bob, "https://dead.test/").await;
    let (boost, made) = status::create_reblog(&pool, BASE, &bob, &original, Visibility::Public)
        .await
        .expect("a reblog of a stored post inserts");
    assert!(made, "the fixture boost is a fresh row");
    let card = json!({
        "url": "https://example.test/post",
        "title": "Example",
        "type": "link",
        "pleroma": { "opengraph": { "og:title": "Example" } },
    });
    link_card::store(&pool, "https://example.test/post", "ok", Some(&card))
        .await
        .expect("the fixture card stores");
    link_card::link_status(&pool, original.id, "https://example.test/post")
        .await
        .expect("the original links its card");
    let own = json!({"url": "https://boost.test", "title": "Boost page", "type": "link"});
    link_card::store(&pool, "https://boost.test", "ok", Some(&own))
        .await
        .expect("the boost's card stores");
    link_card::link_status(&pool, boost.id, "https://boost.test")
        .await
        .expect("the reblog row links its own card");
    link_card::store(&pool, "https://dead.test/", "failed", None)
        .await
        .expect("the failed fetch caches");
    link_card::link_status(&pool, dead.id, "https://dead.test/")
        .await
        .expect("the dead status links its failure");

    let card_of = |entity: &Status| {
        serde_json::to_value(entity).expect("a rendered status serializes")["card"].clone()
    };
    let page = [original.clone(), dead.clone(), boost.clone()];
    let anonymous = render_statuses(&state, ViewerMode::ANONYMOUS, &page)
        .await
        .expect("the page renders anonymously");
    assert_eq!(
        card_of(&anonymous[0]),
        card,
        "the original renders its card"
    );
    assert_eq!(
        card_of(&anonymous[1]),
        Value::Null,
        "a status linked only to a failed fetch renders null"
    );
    let boost_json = serde_json::to_value(&anonymous[2]).expect("the reblog serializes");
    assert_eq!(
        boost_json["card"], own,
        "the reblog row renders its own id's card — status_cards keys \
         on the row"
    );
    assert_eq!(
        boost_json["reblog"]["card"], card,
        "and the nested original its own, from the same batch"
    );

    let for_alice = render_statuses(
        &state,
        ViewerMode::user(&alice),
        std::slice::from_ref(&original),
    )
    .await
    .expect("the post renders for alice");
    assert_eq!(
        card_of(&for_alice[0]),
        card,
        "the viewer's page renders the same card"
    );
}

/// The author's own render of the batch reads the poll as voted — the
/// flag Mastodon and Pleroma give an author — while `own_votes` stays
/// the empty list of marks she never made.
#[tokio::test]
async fn the_author_reads_her_own_poll_as_voted_without_marks() {
    let db = crate::test_db::test_db().await.expect("test database slot");
    let pool = db.pool().clone();
    let state = app_state(&pool);
    let (alice, _bob, polled, open) = open_poll_fixture(&pool).await;

    let for_alice = render_statuses(
        &state,
        ViewerMode::user(&alice),
        std::slice::from_ref(&polled),
    )
    .await
    .expect("the poll renders for its author");
    let value = serde_json::to_value(&for_alice[0]).expect("it serializes");
    assert_eq!(
        value["poll"],
        expected_open_poll(&open, true, &[]),
        "voted for the author, with no marks of hers"
    );
}
