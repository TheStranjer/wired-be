//! The grouped v2 notification API over HTTP: `GET /api/v2/notifications`
//! and its `unread_count`, `{group_key}`, `{group_key}/accounts` and
//! `{group_key}/dismiss` companions (`src/api/notifications_v2.rs`).
//!
//! Every test seeds the `notifications` rows directly through the
//! `create_notification` fixture — the endpoints that *cause* notifications
//! (favourite, react, follow) belong to their own tickets — and asserts the
//! envelope of `accounts` / `statuses` / `notification_groups`, the key
//! shapes (`<type>-<status_id>` grouped, `ungrouped-<id>` for everything
//! else), the sample cap, the cursor pages keyed on
//! `most_recent_notification_id`, the marker-based unread count, and the
//! two scope gates.

#![cfg(test)]

use axum::{Router, http::StatusCode};
use serde_json::{Value, json};
use sqlx::PgPool;
use uuid::Uuid;
use wired_be::models::{
    marker,
    notification::Notification,
    status::{self, Visibility},
    user::User,
};

use crate::common;

/// The grouped listing's path, shared by every request and the `Link`
/// header's target.
const LIST: &str = "/api/v2/notifications";
/// The scope every grouped read demands.
const READ: &str = "read:notifications";
/// The scope the dismissal demands.
const WRITE: &str = "write:notifications";
/// The message of the shared `AppError::NotFound` body.
const NOT_FOUND: &str = "Record not found";

/// A local user with a `scopes`-granted bearer token: `(user, token)`.
async fn fixture(pool: &PgPool, nickname: &str, scopes: &str) -> (User, String) {
    let user = common::create_local_user(pool, nickname).await;
    let token = common::create_app_and_token(pool, &user, scopes).await;
    (user, token)
}

/// A `favourite` notification of `recipient` by `actor` about `status`.
async fn favourite(
    pool: &PgPool,
    recipient: &User,
    actor: &User,
    status: &status::Status,
) -> Notification {
    common::create_notification(pool, recipient, actor, "favourite", Some(status)).await
}

/// A favourite whose row the test does not need.
async fn favourite_note(pool: &PgPool, recipient: &User, actor: &User, status: &status::Status) {
    let _row = favourite(pool, recipient, actor, status).await;
}

/// The `GET {LIST}?query` body, asserted to be a 200, parsed.
async fn list(app: &Router, token: &str, query: &str) -> Value {
    let response = common::TestRequest::get(&format!("{LIST}{query}"))
        .bearer(token)
        .send(app)
        .await;
    common::assert_answer(&response, StatusCode::OK, &format!("the listing `{query}`"));
    response.json_value()
}

/// The groups of a listing body, as values.
fn groups(body: &Value) -> Vec<&Value> {
    body["notification_groups"]
        .as_array()
        .expect("notification_groups is a list")
        .iter()
        .collect()
}

/// The ids of a JSON array of strings, as strings.
fn id_list(values: &Value) -> Vec<String> {
    values
        .as_array()
        .expect("an id list")
        .iter()
        .map(|value| value.as_str().expect("ids are text").to_owned())
        .collect()
}

/// The `id` of each entity in a JSON array of expanded accounts or
/// statuses, as strings.
fn expanded_ids(items: &Value) -> Vec<String> {
    items
        .as_array()
        .expect("an entity list")
        .iter()
        .map(|item| {
            item["id"]
                .as_str()
                .expect("entities carry id text")
                .to_owned()
        })
        .collect()
}

/// The `notification_groups` keys of a body, in answer order.
fn group_keys(body: &Value) -> Vec<String> {
    groups(body)
        .into_iter()
        .map(|group| group["group_key"].as_str().expect("a group key").to_owned())
        .collect()
}

/// Percent-encode a group key the way clients put it into a path.
fn encoded(key: &str) -> String {
    url::form_urlencoded::byte_serialize(key.as_bytes()).collect()
}

/// `GET /api/v2/notifications` on a fresh account answers the empty
/// envelope with no `Link` header: no groups, no expansions.
#[tokio::test]
async fn an_account_without_notifications_lists_nothing() {
    let db = common::test_db().await;
    let pool = db.pool().clone();
    let (_alice, token) = fixture(&pool, "alice", READ).await;
    let app = common::test_app(pool);

    let response = common::TestRequest::get(LIST)
        .bearer(&token)
        .send(&app)
        .await;
    common::assert_answer(&response, StatusCode::OK, "an empty listing");
    assert_eq!(
        response.json_value(),
        json!({"accounts": [], "statuses": [], "notification_groups": []}),
        "the three envelope keys, all empty"
    );
    assert_eq!(
        response.header("link"),
        None,
        "an empty page sends no Link header"
    );
}

/// The shared seed of the two merged-favourites tests: three favourites of
/// one status (`one`, by bob, carol and dave) and one of another (`two`, by
/// bob).
#[derive(Debug)]
struct Merged {
    /// Alice's `read:notifications` token.
    token: String,
    /// The thrice-favourited status.
    one: status::Status,
    /// The once-favourited status.
    two: status::Status,
    /// The actor of the first and last favourites.
    bob: User,
    /// The actor of the middle favourite of `one`.
    carol: User,
    /// The actor of the newest favourite of `one`.
    dave: User,
    /// The oldest notification: bob's first favourite of `one`.
    first: Notification,
    /// The newest favourite of `one`: dave's.
    third: Notification,
    /// The only favourite of `two`: bob's, and the newest row overall.
    alone: Notification,
}

/// Seeds [`Merged`] into `pool`.
async fn seed_merged_favourites(pool: &PgPool) -> Merged {
    let (alice, token) = fixture(pool, "alice", READ).await;
    let erin = common::create_local_user(pool, "erin").await;
    let bob = common::create_local_user(pool, "bob").await;
    let carol = common::create_local_user(pool, "carol").await;
    let dave = common::create_local_user(pool, "dave").await;
    let one = common::create_status(pool, &erin, "first post", Visibility::Public).await;
    let two = common::create_status(pool, &erin, "second post", Visibility::Public).await;
    let first = favourite(pool, &alice, &bob, &one).await;
    favourite_note(pool, &alice, &carol, &one).await;
    let third = favourite(pool, &alice, &dave, &one).await;
    let alone = favourite(pool, &alice, &bob, &two).await;
    Merged {
        token,
        one,
        two,
        bob,
        carol,
        dave,
        first,
        third,
        alone,
    }
}

/// Three favourites of one status are one group, keyed
/// `favourite-<status_id>` and sampled newest-first; a favourite of another
/// status is another group, and the groups come newest first with
/// whole-group bounds.
#[tokio::test]
async fn favourites_of_one_status_form_one_group() {
    let db = common::test_db().await;
    let pool = db.pool().clone();
    let seed = seed_merged_favourites(&pool).await;
    let app = common::test_app(pool);

    let body = list(&app, &seed.token, "").await;
    let listed = groups(&body);
    assert_eq!(listed.len(), 2, "one group per favourited status");

    let newest = listed.first().expect("the newest group");
    assert_eq!(
        newest["group_key"],
        json!(format!("favourite-{}", seed.two.id)),
        "the key is the type and the status id"
    );
    assert_eq!(newest["type"], "favourite", "the group's type");
    assert_eq!(newest["notifications_count"], 1, "the lone favourite");
    assert_eq!(
        newest["most_recent_notification_id"],
        seed.alone.id.to_string(),
        "its newest (and only) notification"
    );
    assert_eq!(
        newest["page_min_id"],
        seed.alone.id.to_string(),
        "page bounds on one row"
    );
    assert_eq!(
        newest["page_max_id"],
        seed.alone.id.to_string(),
        "the same row again"
    );
    assert_eq!(
        newest["status_id"],
        seed.two.id.to_string(),
        "the status key"
    );
    assert_eq!(
        id_list(&newest["sample_account_ids"]),
        vec![seed.bob.id.to_string()],
        "and its sample"
    );

    let merged = listed.get(1).expect("the older group");
    assert_eq!(
        merged["notifications_count"], 3,
        "the three favourites of one status group together"
    );
    assert_eq!(
        merged["most_recent_notification_id"],
        seed.third.id.to_string(),
        "their newest member leads"
    );
    assert_eq!(
        merged["page_min_id"],
        seed.first.id.to_string(),
        "their oldest bounds the page"
    );
    assert_eq!(
        merged["page_max_id"],
        seed.third.id.to_string(),
        "and the newest closes it"
    );
    assert_eq!(
        id_list(&merged["sample_account_ids"]),
        vec![
            seed.dave.id.to_string(),
            seed.carol.id.to_string(),
            seed.bob.id.to_string(),
        ],
        "samples are the members' actors, newest first"
    );
    assert!(
        merged["latest_page_notification_at"]
            .as_str()
            .is_some_and(|stamp| stamp.ends_with('Z')),
        "the newest member's time, Mastodon-styled: {}",
        merged["latest_page_notification_at"]
    );
    assert_eq!(
        merged["most_recent_notification_id"], merged["page_max_id"],
        "and one grouped page carries the whole group"
    );
}

/// The group page expands every sample actor into `accounts` (deduplicated
/// across groups, group order first) and every group status into
/// `statuses`, and carries the `Link` header keyed on the first and last
/// group's `most_recent_notification_id`.
#[tokio::test]
async fn the_group_page_expands_accounts_statuses_and_links() {
    let db = common::test_db().await;
    let pool = db.pool().clone();
    let seed = seed_merged_favourites(&pool).await;
    let app = common::test_app(pool);

    let body = list(&app, &seed.token, "").await;
    assert_eq!(
        expanded_ids(&body["statuses"]),
        vec![seed.two.id.to_string(), seed.one.id.to_string()],
        "statuses are expanded once per group, in group order"
    );
    assert_eq!(
        expanded_ids(&body["accounts"]),
        vec![
            seed.bob.id.to_string(),
            seed.dave.id.to_string(),
            seed.carol.id.to_string(),
        ],
        "sample accounts dedupe across groups, group order first"
    );
    assert_eq!(
        body["accounts"][0]["id"],
        seed.bob.id.to_string(),
        "and they are full Account entities"
    );

    let response = common::TestRequest::get(LIST)
        .bearer(&seed.token)
        .send(&app)
        .await;
    assert_eq!(
        response.header("link"),
        Some(&*format!(
            "<https://test.example{LIST}?max_id={}>; rel=\"next\", \
             <https://test.example{LIST}?min_id={}>; rel=\"prev\"",
            seed.third.id, seed.alone.id
        )),
        "the Link header walks the groups' most_recent ids"
    );
}

/// Two status-less favourites still merge — under `<type>-none` — and a
/// group whose notifications name no status simply omits `status_id`.
#[tokio::test]
async fn status_less_groupable_notifications_group_under_none() {
    let db = common::test_db().await;
    let pool = db.pool().clone();
    let (alice, token) = fixture(&pool, "alice", READ).await;
    let bob = common::create_local_user(&pool, "bob").await;
    let carol = common::create_local_user(&pool, "carol").await;
    let older = common::create_notification(&pool, &alice, &bob, "favourite", None).await;
    let newer = common::create_notification(&pool, &alice, &carol, "favourite", None).await;
    let app = common::test_app(pool);

    let body = list(&app, &token, "").await;
    let listed = groups(&body);
    assert_eq!(listed.len(), 1, "one `favourite-none` group");
    assert_eq!(
        listed[0]["group_key"], "favourite-none",
        "the none subject key"
    );
    assert_eq!(listed[0]["notifications_count"], 2, "both members");
    assert_eq!(
        listed[0]["most_recent_notification_id"],
        newer.id.to_string(),
        "led by the newest"
    );
    assert_eq!(
        listed[0]["page_min_id"],
        older.id.to_string(),
        "bounded by the oldest"
    );
    assert!(
        listed[0].get("status_id").is_none(),
        "and no status to name: {listed:?}"
    );
    assert_eq!(
        body["statuses"],
        json!([]),
        "so the statuses expansion is empty"
    );
}

/// Every type outside the grouped three keeps its own
/// `ungrouped-<notification id>` group — two follows by one actor do not
/// merge — and an ungrouped notification about a status still carries its
/// `status_id`.
#[tokio::test]
async fn other_types_read_as_their_own_ungrouped_keys() {
    let db = common::test_db().await;
    let pool = db.pool().clone();
    let (alice, token) = fixture(&pool, "alice", READ).await;
    let erin = common::create_local_user(&pool, "erin").await;
    let bob = common::create_local_user(&pool, "bob").await;
    let post = common::create_status(&pool, &erin, "a mention", Visibility::Public).await;

    let follow = common::create_notification(&pool, &alice, &bob, "follow", None).await;
    let again = common::create_notification(&pool, &alice, &bob, "follow", None).await;
    let mention = common::create_notification(&pool, &alice, &erin, "mention", Some(&post)).await;
    let app = common::test_app(pool);

    let body = list(&app, &token, "").await;
    let listed = groups(&body);
    assert_eq!(listed.len(), 3, "each ungrouped notification stands alone");
    assert_eq!(
        listed[0]["group_key"],
        json!(format!("ungrouped-{}", mention.id)),
        "newest first"
    );
    assert_eq!(listed[0]["type"], "mention", "the notification's own type");
    assert_eq!(
        listed[0]["status_id"],
        json!(post.id.to_string()),
        "and its status, ungrouped though it is"
    );
    assert_eq!(
        id_list(&listed[0]["sample_account_ids"]),
        vec![erin.id.to_string()],
        "a one-member sample"
    );
    assert_eq!(
        listed[1]["group_key"],
        json!(format!("ungrouped-{}", again.id)),
        "the second follow is its own group, not merged with the first"
    );
    assert_eq!(
        listed[2]["group_key"],
        json!(format!("ungrouped-{}", follow.id)),
        "and so is the first"
    );
    assert!(
        listed[2].get("status_id").is_none(),
        "a status-less notification omits the key"
    );
    assert_eq!(
        expanded_ids(&body["statuses"]),
        vec![post.id.to_string()],
        "the mention's status is the only expansion"
    );
}

/// `grouped_types[]` decides per request which groupable types merge: a
/// request grouping only reblogs shows favourites one-per-row; a follow it
/// names cannot group because it never stored a key; an explicit empty
/// list groups nothing.
#[tokio::test]
async fn grouped_types_narrows_which_types_merge() {
    let db = common::test_db().await;
    let pool = db.pool().clone();
    let (alice, token) = fixture(&pool, "alice", READ).await;
    let erin = common::create_local_user(&pool, "erin").await;
    let bob = common::create_local_user(&pool, "bob").await;
    let carol = common::create_local_user(&pool, "carol").await;
    let post = common::create_status(&pool, &erin, "grouped?", Visibility::Public).await;
    favourite_note(&pool, &alice, &bob, &post).await;
    favourite_note(&pool, &alice, &carol, &post).await;
    let app = common::test_app(pool);

    let default_body = list(&app, &token, "").await;
    assert_eq!(
        groups(&default_body).len(),
        1,
        "by default the favourites merge"
    );
    assert_eq!(
        groups(&default_body)[0]["notifications_count"],
        2,
        "as one group"
    );

    let reblogs_only = list(&app, &token, "?grouped_types[]=reblog").await;
    assert_eq!(
        groups(&reblogs_only).len(),
        2,
        "with favourites ungrouped they do not"
    );
    assert!(
        group_keys(&reblogs_only)
            .iter()
            .all(|key| key.starts_with("ungrouped-")),
        "each favourite reads as its own key: {:?}",
        group_keys(&reblogs_only)
    );
    assert_eq!(
        groups(&reblogs_only)[0]["type"],
        "favourite",
        "the type is still its own"
    );

    let follows_named = list(&app, &token, "?grouped_types[]=follow").await;
    assert_eq!(
        groups(&follows_named).len(),
        2,
        "naming a type that never stores a key groups nothing"
    );

    let none_grouped = list(&app, &token, "?grouped_types[]=").await;
    assert_eq!(
        groups(&none_grouped).len(),
        2,
        "an explicit empty list groups nothing"
    );
}

/// A group's sample never exceeds eight actors — the newest eight members'
/// accounts, in order — however large the group is, and the `accounts`
/// expansion carries exactly those samples.
#[tokio::test]
async fn samples_are_capped_at_eight_actors() {
    let db = common::test_db().await;
    let pool = db.pool().clone();
    let (alice, token) = fixture(&pool, "alice", READ).await;
    let erin = common::create_local_user(&pool, "erin").await;
    let post = common::create_status(&pool, &erin, "popular", Visibility::Public).await;

    let mut actors = Vec::new();
    for index in 0..10 {
        let actor = common::create_local_user(&pool, &format!("fan{index}")).await;
        favourite_note(&pool, &alice, &actor, &post).await;
        actors.push(actor);
    }
    // Newest-first: the last created favourite leads the sample.
    let newest: Vec<String> = actors
        .iter()
        .rev()
        .take(8)
        .map(|actor| actor.id.to_string())
        .collect();
    let app = common::test_app(pool);

    let body = list(&app, &token, "").await;
    let listed = groups(&body);
    assert_eq!(listed.len(), 1, "one merged group");
    assert_eq!(
        listed[0]["notifications_count"], 10,
        "counting every member"
    );
    assert_eq!(
        id_list(&listed[0]["sample_account_ids"]),
        newest,
        "the eight newest members' actors, newest first"
    );
    assert_eq!(
        expanded_ids(&body["accounts"]).len(),
        8,
        "and exactly the samples are expanded"
    );
}

/// The listing pages whole groups, keyed on `most_recent_notification_id`:
/// `max_id` walks older, `since_id` polls newer, a `min_id` page reads
/// forward and answers flipped, and a malformed cursor is the shared 400.
#[tokio::test]
async fn group_pages_walk_the_most_recent_ids() {
    let db = common::test_db().await;
    let pool = db.pool().clone();
    let (alice, token) = fixture(&pool, "alice", READ).await;
    let erin = common::create_local_user(&pool, "erin").await;
    let actor = common::create_local_user(&pool, "actor").await;
    let oldest = common::create_status(&pool, &erin, "oldest", Visibility::Public).await;
    let middle = common::create_status(&pool, &erin, "middle", Visibility::Public).await;
    let newest = common::create_status(&pool, &erin, "newest", Visibility::Public).await;
    let first = favourite(&pool, &alice, &actor, &oldest).await;
    let second = favourite(&pool, &alice, &actor, &middle).await;
    let _third = favourite(&pool, &alice, &actor, &newest).await;
    let app = common::test_app(pool);

    let page = list(&app, &token, "?limit=2").await;
    assert_eq!(
        group_keys(&page),
        vec![
            format!("favourite-{}", newest.id),
            format!("favourite-{}", middle.id),
        ],
        "two groups, newest first"
    );

    let next = list(&app, &token, &format!("?limit=2&max_id={}", second.id)).await;
    assert_eq!(
        group_keys(&next),
        vec![format!("favourite-{}", oldest.id)],
        "max_id on a group's newest id steps to the older groups"
    );

    let polled = list(&app, &token, &format!("?since_id={}", first.id)).await;
    assert_eq!(
        group_keys(&polled),
        vec![
            format!("favourite-{}", newest.id),
            format!("favourite-{}", middle.id),
        ],
        "since_id polls the groups newer than the cursor, newest first"
    );

    let forward = list(&app, &token, &format!("?limit=1&min_id={}", first.id)).await;
    assert_eq!(
        group_keys(&forward),
        vec![format!("favourite-{}", middle.id)],
        "a min_id page reads forward and answers newest-first"
    );

    let empty = list(&app, &token, &format!("?max_id={}", first.id)).await;
    assert_eq!(
        groups(&empty),
        Vec::<&Value>::new(),
        "walking past the oldest group answers nothing, with no Link header"
    );

    let bad = common::TestRequest::get(&format!("{LIST}?max_id=not-an-id"))
        .bearer(&token)
        .send(&app)
        .await;
    common::assert_answer(&bad, StatusCode::BAD_REQUEST, "a malformed cursor");
    common::assert_error(&bad, "Invalid id", "a malformed cursor");
}

/// `unread_count` counts unseen undismissed notifications past the caller's
/// `notifications` marker: every row without one, only newer rows with it,
/// seen and dismissed rows never, `limit` capping the answer (default 100,
/// at most 1000) and the filters narrowing it.
#[tokio::test]
async fn unread_count_follows_the_marker_seen_and_the_cap() {
    let db = common::test_db().await;
    let pool = db.pool().clone();
    let (alice, _) = fixture(&pool, "alice", READ).await;
    let read_token = common::create_app_and_token(&pool, &alice, READ).await;
    let write_token = common::create_app_and_token(&pool, &alice, WRITE).await;
    let bob = common::create_local_user(&pool, "bob").await;
    let carol = common::create_local_user(&pool, "carol").await;
    let first = common::create_notification(&pool, &alice, &bob, "follow", None).await;
    let second = common::create_notification(&pool, &alice, &carol, "follow", None).await;
    let _third = common::create_notification(&pool, &alice, &bob, "mention", None).await;
    let app = common::test_app(pool.clone());

    assert_eq!(
        unread(&app, &read_token, "").await,
        3,
        "with no marker every unseen row counts"
    );
    assert_eq!(
        unread(&app, &read_token, "?limit=1").await,
        1,
        "the limit caps the count"
    );
    assert_eq!(
        unread(&app, &read_token, "?limit=0").await,
        3,
        "an unusable limit falls back to the default of 100, above these rows"
    );
    assert_eq!(
        unread(&app, &read_token, "?types[]=follow").await,
        2,
        "types counts only its own rows"
    );
    assert_eq!(
        unread(&app, &read_token, &format!("?account_id={}", carol.id)).await,
        1,
        "account_id counts only that actor's rows"
    );

    set_marker(&pool, &alice, &second.id.to_string()).await;
    assert_eq!(
        unread(&app, &read_token, "").await,
        1,
        "the marker itself is read; only newer rows count"
    );
    set_marker(&pool, &alice, &first.id.to_string()).await;
    assert_eq!(
        unread(&app, &read_token, "?types[]=follow").await,
        1,
        "the marker and the filter compose"
    );
    set_marker(&pool, &alice, "n-1").await;
    assert_eq!(
        unread(&app, &read_token, "").await,
        3,
        "a marker that names no id is no position"
    );

    mark_seen(&pool, first.id).await;
    assert_eq!(
        unread(&app, &read_token, "").await,
        2,
        "a seen row drops out"
    );

    let dismissed = common::TestRequest::post(&format!("{LIST}/ungrouped-{}/dismiss", second.id))
        .bearer(&write_token)
        .send(&app)
        .await;
    common::assert_answer(&dismissed, StatusCode::OK, "dismissing one row");
    assert_eq!(
        unread(&app, &read_token, "").await,
        1,
        "and a dismissed row drops out too"
    );
}

/// `GET {LIST}/unread_count?query` answers its `count`.
async fn unread(app: &Router, token: &str, query: &str) -> i64 {
    let response = common::TestRequest::get(&format!("{LIST}/unread_count{query}"))
        .bearer(token)
        .send(app)
        .await;
    common::assert_answer(&response, StatusCode::OK, "the unread count");
    response.json_value()["count"]
        .as_i64()
        .expect("count is a number")
}

/// Stores `alice`'s `notifications` marker at `last_read_id`.
async fn set_marker(pool: &PgPool, alice: &User, last_read_id: &str) {
    let _rows: Vec<marker::Marker> =
        marker::upsert_many(pool, alice.id, vec![(marker::NOTIFICATIONS, last_read_id)])
            .await
            .expect("the marker fixture upserts");
}

/// Flags one notification seen, the way a later mark-seen endpoint will.
async fn mark_seen(pool: &PgPool, id: Uuid) {
    let _rows = sqlx::query("UPDATE notifications SET seen = true WHERE id = $1")
        .bind(id)
        .execute(pool)
        .await
        .expect("the seen flag lands");
}

/// `GET /api/v2/notifications/{group_key}` answers the one group in the
/// same envelope, its whole-group bounds serving as the page bounds — and
/// a key the caller owns nothing under, foreign or unknown, is the 404.
#[tokio::test]
async fn a_single_group_is_the_listing_under_one_key() {
    let db = common::test_db().await;
    let pool = db.pool().clone();
    let (alice, token) = fixture(&pool, "alice", READ).await;
    let erin = common::create_local_user(&pool, "erin").await;
    let bob = common::create_local_user(&pool, "bob").await;
    let carol = common::create_local_user(&pool, "carol").await;
    let post = common::create_status(&pool, &erin, "shown", Visibility::Public).await;
    let older = favourite(&pool, &alice, &bob, &post).await;
    let newer = favourite(&pool, &alice, &carol, &post).await;
    let alone = common::create_notification(&pool, &alice, &bob, "follow", None).await;
    let app = common::test_app(pool);

    let key = format!("favourite-{}", post.id);
    let response = common::TestRequest::get(&format!("{LIST}/{}", encoded(&key)))
        .bearer(&token)
        .send(&app)
        .await;
    common::assert_answer(&response, StatusCode::OK, "a known group");
    let body = response.json_value();
    let listed = groups(&body);
    assert_eq!(listed.len(), 1, "the one group");
    assert_eq!(listed[0]["group_key"], key.as_str(), "under its key");
    assert_eq!(listed[0]["notifications_count"], 2, "the whole group");
    assert_eq!(
        listed[0]["most_recent_notification_id"],
        newer.id.to_string(),
        "newest id"
    );
    assert_eq!(
        listed[0]["page_min_id"],
        older.id.to_string(),
        "whole-group min"
    );
    assert_eq!(
        listed[0]["page_max_id"],
        newer.id.to_string(),
        "whole-group max"
    );
    assert_eq!(
        expanded_ids(&body["statuses"]),
        vec![post.id.to_string()],
        "with the group's status expanded"
    );
    assert_eq!(
        expanded_ids(&body["accounts"]),
        vec![carol.id.to_string(), bob.id.to_string()],
        "and its samples"
    );

    let ungrouped = common::TestRequest::get(&format!("{LIST}/ungrouped-{}", alone.id))
        .bearer(&token)
        .send(&app)
        .await;
    common::assert_answer(&ungrouped, StatusCode::OK, "an ungrouped key");
    let alone_body = ungrouped.json_value();
    let single = groups(&alone_body);
    assert_eq!(single.len(), 1, "its own one-member group");
    assert_eq!(single[0]["notifications_count"], 1, "counted once");
    assert_eq!(
        single[0]["page_min_id"],
        alone.id.to_string(),
        "and bounded by itself"
    );
}

/// `GET {group_key}` 404s a key that names nothing — an unknown group, a
/// broken `ungrouped-` prefix, and another account's group alike.
#[tokio::test]
async fn unknown_and_foreign_group_keys_are_the_404() {
    let db = common::test_db().await;
    let pool = db.pool().clone();
    let (alice, token) = fixture(&pool, "alice", READ).await;
    let (_mallory, mallory_token) = fixture(&pool, "mallory", READ).await;
    let erin = common::create_local_user(&pool, "erin").await;
    let bob = common::create_local_user(&pool, "bob").await;
    let post = common::create_status(&pool, &erin, "private show", Visibility::Public).await;
    favourite_note(&pool, &alice, &bob, &post).await;
    let app = common::test_app(pool);

    for (case, path) in [
        (
            "unknown key",
            format!(
                "{LIST}/favourite-{}",
                Uuid::parse_str("01890b9a-2c1d-7f3a-9e4b-7c2d5a8b1e3f").expect("fixture id")
            ),
        ),
        (
            "broken ungrouped prefix",
            format!("{LIST}/ungrouped-not-an-id"),
        ),
        (
            "foreign group",
            format!("{LIST}/{}", encoded(&format!("favourite-{}", post.id))),
        ),
    ] {
        let token_of = if case == "foreign group" {
            &mallory_token
        } else {
            &token
        };
        let missing = common::TestRequest::get(&path)
            .bearer(token_of)
            .send(&app)
            .await;
        common::assert_answer(&missing, StatusCode::NOT_FOUND, case);
        common::assert_error(&missing, NOT_FOUND, case);
    }
}

/// `GET {group_key}/accounts` answers every actor of the group — not just
/// the eight the sample shows — newest actor first, each once; an unknown
/// key answers the empty list, Pleroma's render of no users.
#[tokio::test]
async fn group_accounts_name_every_actor_once() {
    let db = common::test_db().await;
    let pool = db.pool().clone();
    let (alice, token) = fixture(&pool, "alice", READ).await;
    let erin = common::create_local_user(&pool, "erin").await;
    let bob = common::create_local_user(&pool, "bob").await;
    let carol = common::create_local_user(&pool, "carol").await;
    let post = common::create_status(&pool, &erin, "all of them", Visibility::Public).await;
    favourite_note(&pool, &alice, &carol, &post).await;
    favourite_note(&pool, &alice, &bob, &post).await;
    favourite_note(&pool, &alice, &carol, &post).await;
    let alone = common::create_notification(&pool, &alice, &bob, "follow", None).await;
    let mut crowd = Vec::new();
    for index in 0..9 {
        let fan = common::create_local_user(&pool, &format!("fan{index}")).await;
        favourite_note(&pool, &alice, &fan, &post).await;
        crowd.push(fan);
    }
    let app = common::test_app(pool);

    let response = common::TestRequest::get(&format!(
        "{LIST}/{}/accounts",
        encoded(&format!("favourite-{}", post.id))
    ))
    .bearer(&token)
    .send(&app)
    .await;
    common::assert_answer(&response, StatusCode::OK, "the group's accounts");
    let accounts = response.json_value();
    let listed = expanded_ids(&accounts);
    assert_eq!(
        listed.len(),
        crowd.len() + 2,
        "every distinct actor, past the sample's eight"
    );
    assert_eq!(
        listed.first().map(String::as_str),
        crowd.last().map(|fan| fan.id.to_string()).as_deref(),
        "newest actor first"
    );
    assert_eq!(
        accounts[0]["id"],
        crowd.last().expect("the newest fan").id.to_string(),
        "as full Account entities"
    );
    let mut seen: Vec<String> = Vec::new();
    for id in &listed {
        assert!(!seen.contains(id), "{id} named twice");
        seen.push(id.clone());
    }

    let ungrouped = common::TestRequest::get(&format!("{LIST}/ungrouped-{}/accounts", alone.id))
        .bearer(&token)
        .send(&app)
        .await;
    common::assert_answer(&ungrouped, StatusCode::OK, "an ungrouped key's actor");
    assert_eq!(
        expanded_ids(&ungrouped.json_value()),
        vec![bob.id.to_string()],
        "the one member's account"
    );

    let unknown = common::TestRequest::get(&format!(
        "{LIST}/{}/accounts",
        encoded(&format!("favourite-{}", Uuid::now_v7()))
    ))
    .bearer(&token)
    .send(&app)
    .await;
    common::assert_answer(&unknown, StatusCode::OK, "an unknown key's accounts");
    assert_eq!(
        unknown.json_value(),
        json!([]),
        "Pleroma's empty render, not a 404"
    );
}

/// `POST {group_key}/dismiss` hides the whole group from every read — the
/// listing, the single-group show, the count — and answers `{}`; the
/// ungrouped spelling dismisses its one row, and an unknown key changes
/// nothing without complaining.
#[tokio::test]
async fn dismissing_a_group_hides_it_from_every_read() {
    let db = common::test_db().await;
    let pool = db.pool().clone();
    let (alice, read_token) = fixture(&pool, "alice", READ).await;
    let write_token = common::create_app_and_token(&pool, &alice, WRITE).await;
    let erin = common::create_local_user(&pool, "erin").await;
    let bob = common::create_local_user(&pool, "bob").await;
    let carol = common::create_local_user(&pool, "carol").await;
    let gone = common::create_status(&pool, &erin, "dismissed", Visibility::Public).await;
    let kept = common::create_status(&pool, &erin, "kept", Visibility::Public).await;
    let follow = common::create_notification(&pool, &alice, &bob, "follow", None).await;
    favourite_note(&pool, &alice, &bob, &gone).await;
    favourite_note(&pool, &alice, &erin, &gone).await;
    let kept_note = favourite(&pool, &alice, &carol, &kept).await;
    let app = common::test_app(pool.clone());

    let dismiss_path = encoded(&format!("favourite-{}", gone.id));
    let response = common::TestRequest::post(&format!("{LIST}/{dismiss_path}/dismiss"))
        .bearer(&write_token)
        .send(&app)
        .await;
    common::assert_answer(&response, StatusCode::OK, "the dismissal");
    assert_eq!(
        response.json_value(),
        json!({}),
        "the empty object, Pleroma's `json(conn, %{{}})`"
    );
    assert_eq!(
        response.header("content-type"),
        Some("application/json"),
        "and a JSON body"
    );

    let body = list(&app, &read_token, "").await;
    assert_eq!(
        group_keys(&body),
        vec![
            format!("favourite-{}", kept.id),
            format!("ungrouped-{}", follow.id),
        ],
        "the dismissed group is off the listing, the follow stays"
    );
    assert_eq!(
        expanded_ids(&body["statuses"]),
        vec![kept.id.to_string()],
        "and the dismissed group's status is no longer expanded"
    );
    assert_eq!(
        unread(&app, &read_token, "").await,
        2,
        "the follow and the kept favourite are all that is left unread"
    );

    let shown = common::TestRequest::get(&format!("{LIST}/{dismiss_path}"))
        .bearer(&read_token)
        .send(&app)
        .await;
    common::assert_answer(&shown, StatusCode::NOT_FOUND, "a dismissed group");

    let again = common::TestRequest::post(&format!("{LIST}/{dismiss_path}/dismiss"))
        .bearer(&write_token)
        .send(&app)
        .await;
    common::assert_answer(&again, StatusCode::OK, "re-dismissing is not an error");

    let one_row = common::TestRequest::post(&format!("{LIST}/ungrouped-{}/dismiss", kept_note.id))
        .bearer(&write_token)
        .send(&app)
        .await;
    common::assert_answer(&one_row, StatusCode::OK, "dismissing one row by its id key");

    let unknown = common::TestRequest::post(&format!(
        "{LIST}/{}/dismiss",
        encoded(&format!("reblog-{}", Uuid::now_v7()))
    ))
    .bearer(&write_token)
    .send(&app)
    .await;
    common::assert_answer(&unknown, StatusCode::OK, "dismissing an unknown key");

    let final_body = list(&app, &read_token, "").await;
    assert_eq!(
        group_keys(&final_body),
        vec![format!("ungrouped-{}", follow.id)],
        "the kept group lost its only member to the ungrouped dismissal, \
         and the follow outlives all of it"
    );
}

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

/// Seed one `favourite` notification of `alice` and return its group key.
async fn seed_one_favourite(pool: &PgPool, alice: &User) -> String {
    let erin = common::create_local_user(pool, "erin").await;
    let bob = common::create_local_user(pool, "bob").await;
    let post = common::create_status(pool, &erin, "gated", Visibility::Public).await;
    favourite_note(pool, alice, &bob, &post).await;
    format!("favourite-{}", post.id)
}

/// The five grouped routes as `(is_post, path)` pairs, the four reads
/// first and the dismissal last.
fn grouped_routes(key: &str) -> Vec<(bool, String)> {
    let key = encoded(key);
    vec![
        (false, LIST.to_owned()),
        (false, format!("{LIST}/unread_count")),
        (false, format!("{LIST}/{key}")),
        (false, format!("{LIST}/{key}/accounts")),
        (true, format!("{LIST}/{key}/dismiss")),
    ]
}

/// Send `route` with the bearer `token`; empty `token` sends no credential.
async fn call(app: &Router, route: &(bool, String), token: &str) -> common::TestResponse {
    let builder = if route.0 {
        common::TestRequest::post(&route.1)
    } else {
        common::TestRequest::get(&route.1)
    };
    if token.is_empty() {
        builder.send(app).await
    } else {
        builder.bearer(token).send(app).await
    }
}

/// Call every grouped route with `token` (empty: unauthenticated).
async fn call_every_route(app: &Router, token: &str, key: &str) -> Vec<common::TestResponse> {
    let mut responses = Vec::new();
    for route in grouped_routes(key) {
        responses.push(call(app, &route, token).await);
    }
    responses
}

/// Call the four grouped reads (everything but the dismissal) with `token`.
async fn call_the_reads(app: &Router, token: &str, key: &str) -> Vec<common::TestResponse> {
    let mut responses = Vec::new();
    for route in grouped_routes(key).into_iter().take(4) {
        responses.push(call(app, &route, token).await);
    }
    responses
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
