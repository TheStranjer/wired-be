//! Scenario (2): the v2 grouped listing — one group per status and kind,
//! derived from the recipient's own flat page.

use axum::Router;
use serde_json::{Value, json};
use sqlx::PgPool;
use wired_be::models::user::User;

use super::support::{
    HEART, HEART_SEGMENT, PARTY, PARTY_SEGMENT, clear, dismiss, entity_id, flat, follow, grouped,
    groups, post, react, row_of, sorted, status_action, unread, user,
};
use crate::common;

/// One group the v2 listing must answer, derived row by row from the
/// recipient's own flat page: the key, the counted members, the bounds
/// the page walks, the newest-first deduplicated samples and the status.
#[derive(Debug)]
struct ExpectedGroup {
    /// The group key (`<type>-<status>[-<emoji>]` or `ungrouped-<id>`).
    key: String,
    /// The group's notification type.
    kind: String,
    /// How many flat rows merged into it.
    count: i64,
    /// Its newest member's notification id.
    most_recent: String,
    /// Its oldest member's notification id.
    page_min: String,
    /// The member actors, newest first, each named once.
    samples: Vec<String>,
    /// The status the group names, if any.
    status_id: Option<String>,
}

/// Collapses a newest-first flat page into the groups the v2 listing must
/// answer: first-seen group order is exactly the grouped listing's walk of
/// `most_recent_notification_id` values (one page carries a whole group,
/// so its newest member is seen first).
fn expected_groups(rows: &[Value]) -> Vec<ExpectedGroup> {
    let mut listed: Vec<ExpectedGroup> = Vec::new();
    for row in rows {
        let key = row["group_key"].as_str().expect("every entity has a key");
        let actor = row["account"]["id"]
            .as_str()
            .expect("an actor id")
            .to_owned();
        let id = entity_id(row);
        match listed.iter_mut().find(|group| group.key == key) {
            Some(group) => {
                group.count += 1;
                group.page_min = id;
                if !group.samples.contains(&actor) {
                    group.samples.push(actor);
                }
            }
            None => listed.push(ExpectedGroup {
                key: key.to_owned(),
                kind: row["type"].as_str().expect("a type").to_owned(),
                count: 1,
                most_recent: id.clone(),
                page_min: id,
                samples: vec![actor],
                status_id: row["status"]["id"].as_str().map(str::to_owned),
            }),
        }
    }
    listed
}

/// Compares a v2 page's groups with the expected table entry by entry:
/// keys, types, counts, bounds and samples.
fn assert_groups(listed: &[Value], want: &[ExpectedGroup]) {
    assert_eq!(listed.len(), want.len(), "one group per key");
    for (entry, expected) in listed.iter().zip(want) {
        assert_eq!(entry["group_key"], expected.key, "the key {expected:?}");
        assert_eq!(entry["type"], expected.kind, "the type of {expected:?}");
        assert_eq!(
            entry["notifications_count"], expected.count,
            "every member collapsed into one counted group"
        );
        assert_eq!(
            entry["most_recent_notification_id"], expected.most_recent,
            "the group's newest member"
        );
        assert_eq!(entry["page_min_id"], expected.page_min, "its oldest");
        assert_eq!(
            entry["page_max_id"], expected.most_recent,
            "and one grouped page carries the whole group"
        );
        assert_eq!(
            entry["sample_account_ids"],
            json!(expected.samples),
            "samples newest first, each actor once"
        );
        match &expected.status_id {
            Some(status) => {
                assert_eq!(entry["status_id"], json!(status), "the group's status");
            }
            None => assert!(
                entry.get("status_id").is_none(),
                "a status-less group omits the key: {entry}"
            ),
        }
    }
}

/// The recipient's two views of the same inbox: the newest-first flat page
/// and the groups the v2 listing must answer for it, re-read as one step
/// (the listing and its expectation always come from the same moment).
async fn pages(app: &Router, token: &str) -> (Vec<Value>, Vec<ExpectedGroup>) {
    let rows = flat(app, token, "").await;
    let expected = expected_groups(&rows);
    (rows, expected)
}

/// The ids of the two posts [`seed_grouped`] had alice publish: the
/// thrice-favourited, twice-boosted one and the singly-favourited one.
#[derive(Debug)]
struct Posts {
    /// The status every groupable telling piles onto.
    first: String,
    /// The lone favourite's status.
    second: String,
}

/// Seeds the grouped inbox of alice over HTTP: three favourites, two
/// boosts, two party reactions and one heart on her first post, a lone
/// favourite of a second account on her second post, and an ungrouped
/// follow by that account.
async fn seed_grouped(app: &Router, pool: &PgPool, alice: &User, alice_token: &str) -> Posts {
    let first = entity_id(&post(app, alice_token, "merge me").await);
    let second = entity_id(&post(app, alice_token, "one fan only").await);
    let (_bob, bob_token) = user(pool, "bob").await;
    let (_carol, carol_token) = user(pool, "carol").await;
    let (_dave, dave_token) = user(pool, "dave").await;
    let (_erin, erin_token) = user(pool, "erin").await;
    for token in [&bob_token, &carol_token, &dave_token] {
        let _liked = status_action(app, token, &first, "favourite").await;
    }
    for token in [&bob_token, &carol_token] {
        let _boosted = status_action(app, token, &first, "reblog").await;
    }
    for token in [&bob_token, &carol_token] {
        let _partied = react(app, token, &first, PARTY_SEGMENT).await;
    }
    let _hearted = react(app, &dave_token, &first, HEART_SEGMENT).await;
    let _lone = status_action(app, &erin_token, &second, "favourite").await;
    let _followed = follow(app, &erin_token, &alice.id.to_string()).await;
    Posts { first, second }
}

/// The v2 list collapses one status's favourites, boosts and reactions
/// into counted groups keyed `favourite-<id>`, `reblog-<id>` and
/// `pleroma:emoji_reaction-<id>-<emoji>` (one key per emoji), leaves the
/// follow alone on its `ungrouped-<id>` key, and lets `unread_count` track
/// every undismissed row until a dismissal steps the count (and its
/// group) down and `POST {V1}/clear` empties both listings.
#[tokio::test]
async fn the_grouped_listing_collapses_one_status_and_counts_unread() {
    let db = common::test_db().await;
    let pool = db.pool().clone();
    let app = common::test_app(pool.clone());
    let (alice, alice_token) = user(&pool, "alice").await;
    let posts = seed_grouped(&app, &pool, &alice, &alice_token).await;

    let (rows, want) = pages(&app, &alice_token).await;
    assert_groups(&groups(&grouped(&app, &alice_token, "").await), &want);
    assert_group_keys(&rows, &want, &posts);

    assert_eq!(
        unread(&app, &alice_token).await,
        i64::try_from(rows.len()).expect("a page fits"),
        "every undismissed row counts while nothing is read away"
    );
    let merged = find_group(&want, &format!("favourite-{}", posts.first));
    assert_eq!(merged.count, 3, "three favourites of one status merged");
    dismiss(&app, &alice_token, &merged.page_min).await;

    let (left, again) = pages(&app, &alice_token).await;
    assert_groups(&groups(&grouped(&app, &alice_token, "").await), &again);
    assert_eq!(
        unread(&app, &alice_token).await,
        i64::try_from(left.len()).expect("a page fits"),
        "the undismissed rows still equal the count, one fewer"
    );
    assert_eq!(
        find_group(&again, &format!("favourite-{}", posts.first)).count,
        2,
        "the dismissed member left the group"
    );

    clear(&app, &alice_token).await;
    assert!(
        flat(&app, &alice_token, "").await.is_empty(),
        "the flat page cleared"
    );
    assert!(
        groups(&grouped(&app, &alice_token, "").await).is_empty(),
        "and the grouped one with it"
    );
    assert_eq!(
        unread(&app, &alice_token).await,
        0,
        "nothing undismissed is left"
    );
}

/// The group keys the page must carry: the stored `favourite-`/`reblog-`/
/// `pleroma:emoji_reaction-` keys (the emoji appended for reactions) for
/// `posts`, and the follow's derived `ungrouped-<id>`.
fn assert_group_keys(rows: &[Value], want: &[ExpectedGroup], posts: &Posts) {
    let keys = sorted(
        want.iter()
            .map(|group| group.key.clone())
            .collect::<Vec<String>>(),
    );
    assert_eq!(
        keys,
        sorted(vec![
            format!("favourite-{}", posts.first),
            format!("reblog-{}", posts.first),
            format!("pleroma:emoji_reaction-{}-{PARTY}", posts.first),
            format!("pleroma:emoji_reaction-{}-{HEART}", posts.first),
            format!("favourite-{}", posts.second),
            format!("ungrouped-{}", entity_id(row_of(rows, "follow"))),
        ]),
        "the groupable types key on the status (and the emoji); the \
         follow keeps its unique ungrouped key"
    );
}

/// The expected group `key` names, or a test panic.
fn find_group<'a>(want: &'a [ExpectedGroup], key: &str) -> &'a ExpectedGroup {
    want.iter()
        .find(|group| group.key == key)
        .unwrap_or_else(|| panic!("no group {key} on the page: {want:?}"))
}
