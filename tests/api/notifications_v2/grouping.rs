//! The grouped listing's envelope on one page: the empty answer, how
//! favourites of one status merge into a `favourite-<status_id>` group, the
//! `accounts` / `statuses` expansions and the `Link` header, the
//! `favourite-none` key of status-less rows, the `ungrouped-<id>` keys of
//! the other types, what `grouped_types[]` merges, and the sample cap.

use axum::http::StatusCode;
use serde_json::json;
use wired_be::models::status::Visibility;

use super::support::{
    LIST, READ, expanded_ids, favourite_note, fixture, group_keys, groups, id_list, list,
    seed_merged_favourites,
};
use crate::common;

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
