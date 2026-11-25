//! Editing a post's poll: the `poll[...]` parameters of `PUT /api/v1/statuses/{id}`
//! (WBE-806).

use wired_be::{
    federation::publisher,
    models::{
        follow::{self, FollowState},
        poll,
        user::{self, User},
    },
};

use super::*;

/// The scopes a voter's token needs to read the post and its notifications.
const READ_WRITE: &str = "read write";

/// A local author with the endpoints and keys a registered one carries, so
/// the edit's `Update` is published.
async fn signing_author(pool: &PgPool, nickname: &str) -> (User, String) {
    let ap_id = format!("{TEST_BASE_URL}/users/{nickname}");
    let stored = ap_id.clone();
    let inbox = user::inbox(&ap_id);
    let followers = user::followers(&ap_id);
    let author = common::create_local_user_with(pool, nickname, move |mut new| {
        new.ap_id = stored;
        new.inbox = Some(inbox);
        new.followers_url = Some(followers);
        new.private_key = Some("PEM".to_owned());
        new.public_key = Some("PUB".to_owned());
        new
    })
    .await;
    let token = common::create_app_and_token(pool, &author, "write:statuses read:statuses").await;
    (author, token)
}

/// A remote follower of `target`, so the `Update` has somewhere to go.
async fn remote_follower(pool: &PgPool, target: &User) {
    let carol = common::create_remote_user(pool, "carol", "peer.example").await;
    let row = follow::follow(pool, TEST_BASE_URL, &carol, target)
        .await
        .expect("the fixture follow records");
    assert_eq!(row.state, FollowState::Accepted, "an open account accepts");
}

/// The queued `federator.publish` activities, in insertion order.
async fn published(pool: &PgPool) -> Vec<Value> {
    sqlx::query_as::<_, (Value,)>(
        "SELECT args->'activity' FROM jobs WHERE kind = $1 ORDER BY inserted_at, id",
    )
    .bind(publisher::KIND_PUBLISH)
    .fetch_all(pool)
    .await
    .expect("the publish jobs read")
    .into_iter()
    .map(|(activity,)| activity)
    .collect()
}

/// A poll of `a`, `b`, `c` through `POST /api/v1/statuses`, as `multiple`
/// says.
async fn question(app: &Router, token: &str, multiple: bool) -> Value {
    posted(
        app,
        token,
        &json!({
            "status": "which?",
            "poll": {"options": ["a", "b", "c"], "multiple": multiple, "expires_in": 3600},
        }),
    )
    .await
}

/// `voter`'s ballot for the option at `position` of the entity's poll.
async fn cast(pool: &PgPool, entity: &Value, voter: &User, position: usize) {
    let poll_id = Uuid::parse_str(entity["poll"]["id"].as_str().expect("a poll id"))
        .expect("the poll id is a uuid");
    let options = poll::options(pool, poll_id)
        .await
        .expect("the options read");
    let landed = poll::cast_vote(pool, poll_id, voter.id, &[options[position].id])
        .await
        .expect("the vote casts");
    assert!(landed, "the fixture ballot counts");
}

/// `GET /api/v1/statuses/{id}` as `token`.
async fn shown(app: &Router, token: &str, id: &str) -> Value {
    let response = common::TestRequest::get(&format!("/api/v1/statuses/{id}"))
        .bearer(token)
        .send(app)
        .await;
    common::assert_answer(&response, StatusCode::OK, "the status reads");
    response.json_value()
}

/// The `type` of every notification `token` can read, newest first.
async fn notification_kinds(app: &Router, token: &str) -> Vec<String> {
    let told = common::TestRequest::get("/api/v1/notifications")
        .bearer(token)
        .send(app)
        .await
        .json_value();
    told.as_array()
        .expect("the notifications are an array")
        .iter()
        .filter_map(|item| item["type"].as_str().map(str::to_owned))
        .collect()
}

/// The option titles and counts of an entity's poll.
fn tally(entity: &Value) -> Value {
    entity["poll"]["options"].clone()
}

/// An edit that changes the options answers the new options at zero, clears
/// the earlier voter's ballot, keeps the old options in the history, tells
/// the voter with an `update` and publishes a `Question` carrying the new
/// `oneOf`.
#[tokio::test]
async fn changing_the_options_restarts_the_poll_and_federates_the_question() {
    let db = common::test_db().await;
    let pool = db.pool().clone();
    let app = common::test_app(pool.clone());
    let (alice, token) = signing_author(&pool, "poll-restarter").await;
    remote_follower(&pool, &alice).await;
    let bob = common::create_local_user(&pool, "poll-bob").await;
    let bob_token = common::create_app_and_token(&pool, &bob, READ_WRITE).await;
    let entity = question(&app, &token, true).await;
    let id = entity["id"].as_str().expect("an id").to_owned();
    cast(&pool, &entity, &bob, 0).await;
    assert_eq!(shown(&app, &bob_token, &id).await["poll"]["voted"], true);

    let response = edit(
        &app,
        &token,
        &id,
        json!({
            "status": "which? EDITED",
            "poll": {"options": ["n1", "n2"], "multiple": false, "expires_in": 7200},
        }),
    )
    .await;
    common::assert_answer(&response, StatusCode::OK, "the poll edit is accepted");
    let edited = response.json_value();
    assert_eq!(
        tally(&edited),
        json!([
            {"title": "n1", "votes_count": 0},
            {"title": "n2", "votes_count": 0},
        ]),
        "the new options, nobody's vote on them"
    );
    assert_eq!(edited["poll"]["multiple"], false);
    assert_eq!(edited["poll"]["votes_count"], 0);
    assert_eq!(edited["poll"]["voters_count"], 0);

    let seen = shown(&app, &bob_token, &id).await;
    assert_eq!(seen["poll"]["voted"], false, "the old ballot is gone");
    assert_eq!(seen["poll"]["own_votes"], json!([]));

    let activities = published(&pool).await;
    let update = activities.last().expect("an Update is queued");
    assert_eq!(update["type"], "Update");
    assert_eq!(update["object"]["type"], "Question");
    let names: Vec<&Value> = update["object"]["oneOf"]
        .as_array()
        .expect("a single-choice question lists oneOf")
        .iter()
        .map(|choice| &choice["name"])
        .collect();
    assert_eq!(names, vec!["n1", "n2"], "the new options ride the Update");
    assert_eq!(
        update["object"]["oneOf"][0]["replies"]["totalItems"], 0,
        "at zero"
    );
    assert!(
        update["object"].get("anyOf").is_none(),
        "no longer any-choice"
    );

    assert_eq!(
        notification_kinds(&app, &bob_token).await,
        vec!["update"],
        "the voter is told of the edit"
    );

    let versions = history(&app, Some(&token), &id).await.json_value();
    let items = versions.as_array().expect("the history is an array");
    assert_eq!(items.len(), 2, "the old question is a version");
    assert_eq!(
        items[0]["poll"],
        json!({"options": [{"title": "a"}, {"title": "b"}, {"title": "c"}]}),
        "the old version keeps its options"
    );
    assert_eq!(
        items[1]["poll"],
        json!({"options": [{"title": "n1"}, {"title": "n2"}]}),
        "the current one has the new"
    );
}

/// Flipping a single-choice poll to multiple restarts it and publishes the
/// question under `anyOf`.
#[tokio::test]
async fn single_to_multiple_publishes_any_of() {
    let db = common::test_db().await;
    let pool = db.pool().clone();
    let app = common::test_app(pool.clone());
    let (alice, token) = signing_author(&pool, "poll-flipper").await;
    remote_follower(&pool, &alice).await;
    let bob = common::create_local_user(&pool, "poll-flip-bob").await;
    let entity = question(&app, &token, false).await;
    let id = entity["id"].as_str().expect("an id").to_owned();
    cast(&pool, &entity, &bob, 1).await;

    let response = edit(
        &app,
        &token,
        &id,
        json!({"poll": {"options": ["a", "b", "c"], "multiple": true}}),
    )
    .await;
    common::assert_answer(&response, StatusCode::OK, "the mode edit is accepted");
    let edited = response.json_value();
    assert_eq!(edited["poll"]["multiple"], true);
    assert_eq!(edited["poll"]["votes_count"], 0, "a new mode restarts it");

    let activities = published(&pool).await;
    let update = activities.last().expect("an Update is queued");
    assert_eq!(update["object"]["type"], "Question");
    assert_eq!(
        update["object"]["anyOf"]
            .as_array()
            .expect("a multiple-choice question lists anyOf")
            .len(),
        3
    );
    assert!(update["object"].get("oneOf").is_none());
}

/// An edit without `poll` keeps the poll and its votes, and so does one
/// that restates the same options and only moves the deadline.
#[tokio::test]
async fn an_edit_without_a_new_question_keeps_the_votes() {
    let db = common::test_db().await;
    let pool = db.pool().clone();
    let app = common::test_app(pool.clone());
    let (_alice, token) = signing_author(&pool, "poll-keeper").await;
    let bob = common::create_local_user(&pool, "poll-keep-bob").await;
    let bob_token = common::create_app_and_token(&pool, &bob, READ_WRITE).await;
    let entity = question(&app, &token, false).await;
    let id = entity["id"].as_str().expect("an id").to_owned();
    cast(&pool, &entity, &bob, 0).await;

    let response = edit(&app, &token, &id, json!({"status": "which? reworded"})).await;
    common::assert_answer(&response, StatusCode::OK, "the text edit is accepted");
    let edited = response.json_value();
    assert_eq!(edited["poll"]["options"][0]["votes_count"], 1);
    assert_eq!(edited["poll"]["votes_count"], 1);
    assert_eq!(shown(&app, &bob_token, &id).await["poll"]["voted"], true);

    let before = edited["poll"]["expires_at"].clone();
    let extended = edit(
        &app,
        &token,
        &id,
        json!({"poll": {"options": ["a", "b", "c"], "expires_in": 86_400}}),
    )
    .await;
    common::assert_answer(&extended, StatusCode::OK, "the deadline edit is accepted");
    let moved = extended.json_value();
    assert_ne!(moved["poll"]["expires_at"], before, "the deadline moved");
    assert_eq!(moved["poll"]["votes_count"], 1, "the vote stayed");
    assert_eq!(shown(&app, &bob_token, &id).await["poll"]["voted"], true);
    assert!(
        notification_kinds(&app, &bob_token).await.is_empty(),
        "a kept question tells no voter"
    );
}

/// The poll parameters meet the create's rules, and a poll never lands on a
/// post that has media.
#[tokio::test]
async fn invalid_poll_parameters_are_refused_as_on_create() {
    let db = common::test_db().await;
    let pool = db.pool().clone();
    let app = common::test_app(pool.clone());
    let (alice, token) = signing_author(&pool, "poll-refuser").await;
    let entity = question(&app, &token, false).await;
    let id = entity["id"].as_str().expect("an id").to_owned();

    for (poll_body, message) in [
        (
            json!({"options": ["only"]}),
            "Poll must contain at least 2 options",
        ),
        (
            json!({"options": ["a", "b"], "expires_in": 1}),
            "Expiration date is too soon",
        ),
    ] {
        let response = edit(&app, &token, &id, json!({"poll": poll_body})).await;
        common::assert_answer(&response, StatusCode::UNPROCESSABLE_ENTITY, message);
        common::assert_error(&response, message, "the poll rule");
    }
    let stored = shown(&app, &token, &id).await;
    assert_eq!(
        tally(&stored),
        tally(&entity),
        "a refused edit changed nothing"
    );

    let plain = post(&pool, &alice, "pictured").await;
    let _kept = attach(&pool, &alice, &plain, 0).await;
    let response = edit_row(
        &app,
        &token,
        &plain,
        json!({"poll": {"options": ["a", "b"]}}),
    )
    .await;
    common::assert_answer(
        &response,
        StatusCode::UNPROCESSABLE_ENTITY,
        "poll beside media",
    );
    common::assert_error(
        &response,
        "Polls cannot have attachments",
        "poll beside media",
    );
}

/// A post that has no poll can be given one, and the history keeps the
/// poll-less version.
#[tokio::test]
async fn a_poll_can_be_added_to_a_plain_post() {
    let db = common::test_db().await;
    let pool = db.pool().clone();
    let app = common::test_app(pool.clone());
    let (_alice, token) = signing_author(&pool, "poll-adder").await;
    let entity = posted(&app, &token, &json!({"status": "no question yet"})).await;
    let id = entity["id"].as_str().expect("an id").to_owned();

    let response = edit(
        &app,
        &token,
        &id,
        json!({"poll": {"options": ["x", "y"], "expires_in": 3600}}),
    )
    .await;
    common::assert_answer(&response, StatusCode::OK, "the poll is added");
    assert_eq!(
        tally(&response.json_value()),
        json!([
            {"title": "x", "votes_count": 0},
            {"title": "y", "votes_count": 0},
        ])
    );
    let versions = history(&app, Some(&token), &id).await.json_value();
    assert_eq!(
        versions[0]["poll"],
        Value::Null,
        "the earlier version asked nothing"
    );
}
