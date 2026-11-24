//! Familiar followers, birthdays and the route inventory.

use axum::http::StatusCode;
use chrono::{Datelike as _, NaiveDate, Utc};
use serde_json::json;
use uuid::Uuid;
use wired_be::models::user::Flag;

use super::support::{
    BAD_DAY, BAD_MONTH, BIRTHDAYS_PATH, FAMILIAR_PATH, INVALID_TOKEN, NO_READ_ACCOUNTS,
    NO_READ_FOLLOWS, NOT_IMPLEMENTED, USER_REQUIRED, born, deactivate, familiar_path, fixture, get,
    newest_first, seed_follow,
};
use crate::common;

/// `GET /api/v1/accounts/familiar_followers` answers one entry per *known*
/// requested id, in the order asked, skipping the values that are not ids
/// here; an entry holds the viewer's follows that follow that account.
#[tokio::test]
async fn familiar_followers_answers_one_entry_per_known_id() {
    let db = common::test_db().await;
    let pool = db.pool().clone();
    let (alice, token) = fixture(&pool, "alice", "read:follows").await;
    let target = common::create_local_user(&pool, "target").await;
    let other = common::create_local_user(&pool, "other").await;
    let friend = common::create_local_user(&pool, "friend").await;
    let stranger = common::create_local_user(&pool, "stranger").await;
    seed_follow(&pool, &alice, &friend).await;
    seed_follow(&pool, &friend, &target).await;
    seed_follow(&pool, &stranger, &target).await;
    seed_follow(&pool, &friend, &other).await;
    let app = common::test_app(pool);

    let answer = get(
        &app,
        Some(&token),
        &familiar_path(&[
            target.id.to_string(),
            "not-an-id".to_owned(),
            Uuid::now_v7().to_string(),
            other.id.to_string(),
        ]),
    )
    .await;
    common::assert_answer(&answer, StatusCode::OK, "the familiar batch");
    let body = answer.json_value();
    assert_eq!(
        common::ids_of(&body),
        vec![target.id.to_string(), other.id.to_string()],
        "the two known ids in the order asked, the junk and the unknown one skipped"
    );
    assert_eq!(
        common::ids_of(&body[0]["accounts"]),
        vec![friend.id.to_string()],
        "the one account alice follows that follows the target, and not \
         the stranger who follows it too"
    );
    assert_eq!(
        common::ids_of(&body[1]["accounts"]),
        vec![friend.id.to_string()],
        "friend follows both, so both entries name them"
    );

    let empty = get(&app, Some(&token), FAMILIAR_PATH).await;
    common::assert_answer(&empty, StatusCode::OK, "a batch with no ids");
    assert_eq!(
        empty.json_value(),
        json!([]),
        "an absent `id` is the empty array, never an error"
    );
    let single = get(
        &app,
        Some(&token),
        &format!("{FAMILIAR_PATH}?id={}", target.id),
    )
    .await
    .json_value();
    assert_eq!(
        common::ids_of(&single),
        vec![target.id.to_string()],
        "a lone `id=` is read too, Pleroma's List.wrap"
    );
}

/// The two privacy rules of the batch: an account that hides its followers
/// answers the empty set to everyone but itself, and an account that hides
/// its own follows is never named as a familiar follower.
#[tokio::test]
async fn familiar_followers_respects_the_hidden_collections() {
    let db = common::test_db().await;
    let pool = db.pool().clone();
    let (alice, alice_token) = fixture(&pool, "alice", "read:follows").await;
    let hidden = common::create_local_user_with(&pool, "hidden", |mut new| {
        new.hide_followers = Flag::new(true);
        new
    })
    .await;
    let private = common::create_local_user_with(&pool, "private", |mut new| {
        new.hide_follows = Flag::new(true);
        new
    })
    .await;
    let open = common::create_local_user(&pool, "open").await;
    seed_follow(&pool, &alice, &private).await;
    seed_follow(&pool, &private, &hidden).await;
    seed_follow(&pool, &alice, &open).await;
    seed_follow(&pool, &open, &hidden).await;
    // The target follows both of them too, so both are its own familiar
    // followers and only the collection flags tell the two views apart.
    seed_follow(&pool, &hidden, &private).await;
    seed_follow(&pool, &hidden, &open).await;
    let hidden_token = common::create_app_and_token(&pool, &hidden, "read:follows").await;
    let app = common::test_app(pool);
    let path = familiar_path(&[hidden.id.to_string()]);

    let stranger = get(&app, Some(&alice_token), &path).await.json_value();
    assert_eq!(
        common::ids_of(&stranger[0]["accounts"]),
        Vec::<String>::new(),
        "a target that hides its followers answers nobody else"
    );

    let own = get(&app, Some(&hidden_token), &path).await.json_value();
    assert_eq!(
        common::ids_of(&own[0]["accounts"]),
        vec![open.id.to_string()],
        "asked about themself the target sees its followers, and the one of \
         them that hides its follows is still never named"
    );
}

/// A familiar set is capped at twenty: the batch answers twenty of a
/// twenty-one-strong crowd, never all of it.
#[tokio::test]
async fn familiar_followers_caps_the_set_at_twenty() {
    let db = common::test_db().await;
    let pool = db.pool().clone();
    let (alice, token) = fixture(&pool, "alice", "read:follows").await;
    let target = common::create_local_user(&pool, "target").await;
    for number in 0..21 {
        let friend = common::create_local_user(&pool, &format!("friend-{number}")).await;
        seed_follow(&pool, &alice, &friend).await;
        seed_follow(&pool, &friend, &target).await;
    }
    let app = common::test_app(pool);

    let body = get(&app, Some(&token), &familiar_path(&[target.id.to_string()]))
        .await
        .json_value();
    assert_eq!(
        body[0]["accounts"]
            .as_array()
            .expect("an account array")
            .len(),
        20,
        "twenty of the twenty-one familiar followers"
    );
}

/// The batch is a `read:follows` endpoint behind `AuthUser`: no token and an
/// invalid one are 401s, a token that cannot read follows is the scope 403,
/// an app-only token is the user-required 403, and the `read` root passes.
#[tokio::test]
async fn familiar_followers_demands_read_follows() {
    let db = common::test_db().await;
    let pool = db.pool().clone();
    let (alice, _) = fixture(&pool, "alice", "read").await;
    let narrow = common::create_app_and_token(&pool, &alice, "read:accounts").await;
    let broad = common::create_app_and_token(&pool, &alice, "read").await;
    let app_only = common::create_app_only_token(&pool, "read:follows").await;
    let app = common::test_app(pool);
    let path = format!("{FAMILIAR_PATH}?id={}", Uuid::now_v7());

    common::assert_answer(
        &get(&app, None, &path).await,
        StatusCode::UNAUTHORIZED,
        "the batch without a token",
    );
    common::assert_all_refused(
        &[get(&app, Some("nope"), &path).await],
        StatusCode::UNAUTHORIZED,
        INVALID_TOKEN,
        "and on one that does not resolve",
    );
    common::assert_all_refused(
        &[get(&app, Some(&narrow), &path).await],
        StatusCode::FORBIDDEN,
        NO_READ_FOLLOWS,
        "a token that cannot read follows",
    );
    common::assert_all_refused(
        &[get(&app, Some(&app_only), &path).await],
        StatusCode::FORBIDDEN,
        USER_REQUIRED,
        "an app-only token names no viewer to read as",
    );
    common::assert_answer(
        &get(&app, Some(&broad), &path).await,
        StatusCode::OK,
        "the `read` root scope covers `read:follows`",
    );
}

/// `GET /api/v1/pleroma/birthdays` answers the viewer and the accounts of
/// their accepted follows whose birthday falls on the asked-for day, and
/// only when they chose to show it.
#[tokio::test]
async fn birthdays_list_the_viewer_and_their_follows_on_the_day() {
    let db = common::test_db().await;
    let pool = db.pool().clone();
    let alice = born(&pool, "alice", 1990, 5, 17).await;
    let token = common::create_app_and_token(&pool, &alice, "read:accounts").await;
    let same_day = born(&pool, "same-day", 1988, 5, 17).await;
    let other_day = born(&pool, "other-day", 1988, 5, 18).await;
    let undisclosed = common::create_local_user_with(&pool, "undisclosed", |mut new| {
        new.birthday = NaiveDate::from_ymd_opt(1988, 5, 17);
        new
    })
    .await;
    let unfollowed = born(&pool, "unfollowed", 1988, 5, 17).await;
    let closed = born(&pool, "closed", 1988, 5, 17).await;
    deactivate(&pool, &closed).await;
    for known in [&same_day, &other_day, &undisclosed] {
        seed_follow(&pool, &alice, known).await;
    }
    let expected = newest_first([alice.id, same_day.id]);
    let app = common::test_app(pool);

    let answer = get(
        &app,
        Some(&token),
        &format!("{BIRTHDAYS_PATH}?month=5&day=17"),
    )
    .await;
    common::assert_answer(&answer, StatusCode::OK, "the birthday listing");
    let body = answer.json_value();
    assert_eq!(
        common::ids_of(&body),
        expected,
        "the viewer and the one follow born that day, newest account first"
    );
    for absent in [other_day.id, undisclosed.id, unfollowed.id, closed.id] {
        assert!(
            !common::ids_of(&body).contains(&absent.to_string()),
            "{absent} is out: another day, an undisclosed birthday, nobody \
             the viewer follows, and a closed account"
        );
    }
}

/// With no parameters the list asks about today's UTC date, and a `day` or
/// `month` on its own replaces only the part it names.
#[tokio::test]
async fn birthdays_default_to_today_in_utc() {
    let db = common::test_db().await;
    let pool = db.pool().clone();
    let (alice, token) = fixture(&pool, "alice", "read:accounts").await;
    let today = Utc::now().date_naive();
    let year = today.year();
    let other_day = if today.day() == 1 { 2 } else { today.day() - 1 };
    let party = born(&pool, "party", year, today.month(), today.day()).await;
    let shifted = born(&pool, "shifted", year, today.month(), other_day).await;
    seed_follow(&pool, &alice, &party).await;
    seed_follow(&pool, &alice, &shifted).await;
    let app = common::test_app(pool);

    let now = get(&app, Some(&token), BIRTHDAYS_PATH).await.json_value();
    assert_eq!(
        common::ids_of(&now),
        vec![party.id.to_string()],
        "today's birthday is on the list with no parameters at all, and the \
         viewer is absent because alice named no birthday"
    );
    let wrong_month = if today.month() == 1 {
        12
    } else {
        today.month() - 1
    };
    let moved = get(
        &app,
        Some(&token),
        &format!("{BIRTHDAYS_PATH}?month={wrong_month}"),
    )
    .await
    .json_value();
    assert_eq!(
        common::ids_of(&moved),
        Vec::<String>::new(),
        "`month` alone keeps today's day and asks about another month"
    );
    let by_day = get(
        &app,
        Some(&token),
        &format!("{BIRTHDAYS_PATH}?day={other_day}"),
    )
    .await
    .json_value();
    assert_eq!(
        common::ids_of(&by_day),
        vec![shifted.id.to_string()],
        "and `day` alone asks about another day of this month"
    );
}

/// An empty `day` or `month` is absent, not a 400: the listing falls back to
/// today, the same answer a request without the parameter gets.
#[tokio::test]
async fn an_empty_birthday_part_is_treated_as_absent() {
    let db = common::test_db().await;
    let pool = db.pool().clone();
    let (alice, token) = fixture(&pool, "alice", "read:accounts").await;
    let today = Utc::now().date_naive();
    let party = born(&pool, "party", today.year(), today.month(), today.day()).await;
    seed_follow(&pool, &alice, &party).await;
    let app = common::test_app(pool);

    for query in ["day=", "month=", "month=&day="] {
        let asked = get(&app, Some(&token), &format!("{BIRTHDAYS_PATH}?{query}")).await;
        common::assert_answer(&asked, StatusCode::OK, query);
        assert_eq!(
            common::ids_of(&asked.json_value()),
            vec![party.id.to_string()],
            "`?{query}` asks about today, as no parameter at all does"
        );
    }
}

/// A `29 February` birthday matches `month=2&day=29` and nothing else: no
/// other year, month or day ever finds it.
#[tokio::test]
async fn birthdays_match_a_leap_day_only_on_the_leap_day() {
    let db = common::test_db().await;
    let pool = db.pool().clone();
    let (alice, token) = fixture(&pool, "alice", "read:accounts").await;
    let leaper = born(&pool, "leaper", 2000, 2, 29).await;
    assert!(
        leaper.birthday.is_some(),
        "2000 was a leap year, so the fixture birthday was stored"
    );
    seed_follow(&pool, &alice, &leaper).await;
    let app = common::test_app(pool);

    let on = get(
        &app,
        Some(&token),
        &format!("{BIRTHDAYS_PATH}?month=2&day=29"),
    )
    .await
    .json_value();
    assert_eq!(
        common::ids_of(&on),
        vec![leaper.id.to_string()],
        "2/29 finds the account born on it"
    );
    for query in ["month=2&day=28", "month=3&day=29", "day=29", "month=2"] {
        let miss = get(&app, Some(&token), &format!("{BIRTHDAYS_PATH}?{query}"))
            .await
            .json_value();
        assert!(
            !common::ids_of(&miss).contains(&leaper.id.to_string()),
            "`{query}` does not find the leap-day account"
        );
    }
}

/// The birthday route needs a user token granting `read:accounts`, and its
/// two parameters are checked before anything is read.
#[tokio::test]
async fn birthdays_demand_read_accounts_and_valid_parts() {
    let db = common::test_db().await;
    let pool = db.pool().clone();
    let (alice, token) = fixture(&pool, "alice", "read:accounts").await;
    let narrow = common::create_app_and_token(&pool, &alice, "read:follows").await;
    let app_only = common::create_app_only_token(&pool, "read:accounts").await;
    let app = common::test_app(pool);

    common::assert_answer(
        &get(&app, None, BIRTHDAYS_PATH).await,
        StatusCode::UNAUTHORIZED,
        "the birthday list without a token",
    );
    common::assert_all_refused(
        &[get(&app, Some("nope"), BIRTHDAYS_PATH).await],
        StatusCode::UNAUTHORIZED,
        INVALID_TOKEN,
        "and on one that does not resolve",
    );
    common::assert_all_refused(
        &[get(&app, Some(&narrow), BIRTHDAYS_PATH).await],
        StatusCode::FORBIDDEN,
        NO_READ_ACCOUNTS,
        "a token that cannot read accounts",
    );
    common::assert_all_refused(
        &[get(&app, Some(&app_only), BIRTHDAYS_PATH).await],
        StatusCode::FORBIDDEN,
        USER_REQUIRED,
        "an app-only token names no viewer to read for",
    );
    common::assert_all_refused(
        &[
            get(&app, Some(&token), &format!("{BIRTHDAYS_PATH}?month=13")).await,
            get(&app, Some(&token), &format!("{BIRTHDAYS_PATH}?month=0")).await,
            get(&app, Some(&token), &format!("{BIRTHDAYS_PATH}?month=x")).await,
        ],
        StatusCode::BAD_REQUEST,
        BAD_MONTH,
        "a month that is not a month",
    );
    common::assert_all_refused(
        &[
            get(&app, Some(&token), &format!("{BIRTHDAYS_PATH}?day=32")).await,
            get(&app, Some(&token), &format!("{BIRTHDAYS_PATH}?day=-1")).await,
        ],
        StatusCode::BAD_REQUEST,
        BAD_DAY,
        "a day that is not a day",
    );
    common::assert_answer(
        &get(
            &app,
            Some(&token),
            &format!("{BIRTHDAYS_PATH}?month=2&day=29"),
        )
        .await,
        StatusCode::OK,
        "the leap day is a fine thing to ask about",
    );
}

/// The area's five routes answer where the ticket puts them, and no other
/// path joins it: an unrouted sibling still reaches the app-wide fallback.
#[tokio::test]
async fn the_account_listing_routes_are_the_tickets() {
    let db = common::test_db().await;
    let pool = db.pool().clone();
    let (_, token) = fixture(&pool, "alice", "read write").await;
    let app = common::test_app(pool);

    for path in [
        "/api/v1/directory",
        "/api/v1/suggestions",
        "/api/v2/suggestions",
        FAMILIAR_PATH,
        BIRTHDAYS_PATH,
    ] {
        let answer = get(&app, Some(&token), path).await;
        assert_ne!(
            answer.text(),
            json!({ "error": NOT_IMPLEMENTED }).to_string(),
            "`{path}` is routed, not the fallback: {}",
            answer.text()
        );
    }
    common::assert_all_refused(
        &[
            get(&app, Some(&token), "/api/v1/directories").await,
            get(&app, Some(&token), "/api/v2/directory").await,
            get(&app, Some(&token), "/api/v1/pleroma/birthday").await,
            get(&app, Some(&token), "/api/v2/suggestions/not-an-id").await,
        ],
        StatusCode::NOT_FOUND,
        NOT_IMPLEMENTED,
        "a path this area does not claim",
    );
}
