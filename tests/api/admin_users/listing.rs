//! The users listing over HTTP (WBE-195): the criterion-free page in
//! nickname order beside its total, the `page`/`page_size` window with the
//! defaults and the cap, the eight `filters` with their combinations and
//! the unknown names they drop, and the `query` and named criteria that
//! narrow the page.

use serde_json::{Value, json};

use super::support::{page, seed};
use crate::common;

/// The `page_size` the request never states.
const DEFAULT_PAGE_SIZE: i64 = 50;
/// The `page_size` ceiling the ticket pins.
const MAX_PAGE_SIZE: i64 = 200;

/// The accounts the shared fixture seeds, in the order the listing answers
/// them: nickname order over the citext column, the instance actor absent.
const LISTED: [&str; 8] = [
    "beeper",
    "faraway@remote.example",
    "moddy",
    "newbie",
    "plain",
    "root",
    "suspended",
    "unminded",
];

/// The nicknames of one listing answer, in the order it answers them.
fn listed(body: &Value) -> Vec<&str> {
    body["users"]
        .as_array()
        .expect("the users array")
        .iter()
        .filter_map(|found| found["nickname"].as_str())
        .collect()
}

/// A listing with one `filters` parameter.
fn filtered(names: &str) -> String {
    format!("filters={names}&page_size={MAX_PAGE_SIZE}")
}

/// The criterion-free listing answers the whole seeded set in nickname
/// order, wrapped in the ticket's three keys, and never lists the instance
/// actor.
#[tokio::test]
async fn the_listing_wraps_the_ordered_page_with_its_total() {
    let db = common::test_db().await;
    let seeded = seed(&db).await;
    let body = page(&seeded.app, &seeded.token, "").await;
    assert_eq!(
        listed(&body),
        LISTED.to_vec(),
        "eight accounts in nickname order, `internal.fetch` absent"
    );
    assert_eq!(body["count"], json!(8), "the total is the whole set");
    assert_eq!(
        body["page_size"],
        json!(DEFAULT_PAGE_SIZE),
        "an unstated size is Pleroma's 50"
    );
    assert_eq!(
        body.as_object().expect("an object").len(),
        3,
        "exactly users, count and page_size"
    );
}

/// The window moves page by page while `count` stays the whole total, a
/// page past the last row answers nothing, and both an unusable and an
/// oversized `page_size` land on the ticket's defaults and cap.
#[tokio::test]
async fn the_window_moves_while_the_total_stays() {
    let db = common::test_db().await;
    let seeded = seed(&db).await;
    let first = page(&seeded.app, &seeded.token, "page_size=3").await;
    assert_eq!(
        listed(&first),
        vec!["beeper", "faraway@remote.example", "moddy"],
        "the first three nicknames"
    );
    assert_eq!(first["count"], json!(8), "the total counts every page");
    assert_eq!(first["page_size"], json!(3), "the size echoes the request");

    let second = page(&seeded.app, &seeded.token, "page=2&page_size=3").await;
    assert_eq!(
        listed(&second),
        vec!["newbie", "plain", "root"],
        "the next three, total untouched"
    );
    assert_eq!(second["count"], json!(8), "and still the whole set");

    let last = page(&seeded.app, &seeded.token, "page=3&page_size=3").await;
    assert_eq!(
        listed(&last),
        vec!["suspended", "unminded"],
        "the short final page"
    );
    let past = page(&seeded.app, &seeded.token, "page=9&page_size=3").await;
    assert_eq!(listed(&past), Vec::<&str>::new(), "a page past the end");
    assert_eq!(past["count"], json!(8), "still counts the whole set");

    let junk = page(&seeded.app, &seeded.token, "page=zero&page_size=-4").await;
    assert_eq!(
        junk["page_size"],
        json!(DEFAULT_PAGE_SIZE),
        "unusable numbers keep the defaults"
    );
    assert_eq!(listed(&junk), LISTED.to_vec(), "and the first page stands");

    let oversized = page(&seeded.app, &seeded.token, "page_size=9000").await;
    assert_eq!(
        oversized["page_size"],
        json!(MAX_PAGE_SIZE),
        "a request past the cap is capped"
    );
    assert_eq!(oversized["count"], json!(8), "and the total is the set");
}

/// Each of the eight filters selects exactly its own accounts, against rows
/// that differ from every other row in exactly one state.
#[tokio::test]
async fn every_filter_selects_its_own_accounts() {
    let db = common::test_db().await;
    let seeded = seed(&db).await;
    let cases: &[(&str, &[&str])] = &[
        (
            "local",
            &[
                "beeper",
                "moddy",
                "newbie",
                "plain",
                "root",
                "suspended",
                "unminded",
            ],
        ),
        ("external", &["faraway@remote.example"]),
        (
            "active",
            &["beeper", "faraway@remote.example", "moddy", "plain", "root"],
        ),
        ("deactivated", &["suspended"]),
        ("need_approval", &["newbie"]),
        ("unconfirmed", &["unminded"]),
        ("is_admin", &["root"]),
        ("is_moderator", &["moddy"]),
    ];
    for (name, expected) in cases {
        let body = page(&seeded.app, &seeded.token, &filtered(name)).await;
        assert_eq!(listed(&body), expected.to_vec(), "the {name} filter");
        assert_eq!(
            body["count"],
            json!(i64::try_from(expected.len()).expect("a small count")),
            "the {name} total counts only its own rows"
        );
    }
}

/// Filters narrow the set together, so a pair a row satisfies survives and
/// a pair none satisfies answers the empty page with a zero total; a name
/// off Pleroma's list is no filter at all.
#[tokio::test]
async fn filter_combinations_and_unknown_names_narrow_the_set() {
    let db = common::test_db().await;
    let seeded = seed(&db).await;
    let both = page(&seeded.app, &seeded.token, &filtered("local,deactivated")).await;
    assert_eq!(listed(&both), vec!["suspended"], "local AND deactivated");
    assert_eq!(both["count"], json!(1), "one in the total");

    let staff = page(
        &seeded.app,
        &seeded.token,
        &filtered("is_admin,is_moderator"),
    )
    .await;
    assert_eq!(
        listed(&staff),
        Vec::<&str>::new(),
        "nobody holds both roles"
    );
    assert_eq!(staff["count"], json!(0), "and the total says so");

    let impossible = page(&seeded.app, &seeded.token, &filtered("local,external")).await;
    assert_eq!(
        listed(&impossible),
        Vec::<&str>::new(),
        "no account is both local and remote"
    );

    let unknown = page(&seeded.app, &seeded.token, &filtered("ghost,local")).await;
    assert_eq!(
        listed(&unknown),
        vec![
            "beeper",
            "moddy",
            "newbie",
            "plain",
            "root",
            "suspended",
            "unminded",
        ],
        "an unknown name is dropped, the known one still filters"
    );
}

/// `query` is a case-insensitive substring of nickname, display name or
/// email — and of none of the other text an admin sees, so the
/// registration reason stays unsearched.
#[tokio::test]
async fn the_query_searches_nickname_name_and_email() {
    let db = common::test_db().await;
    let seeded = seed(&db).await;
    let cases: &[(&str, &str)] = &[
        ("far", "faraway@remote.example"),
        ("PLAIN", "plain"),
        ("person", "plain"),
        ("ROOT@EXAMPLE.ORG", "root"),
        ("bee", "beeper"),
    ];
    for (needle, expected) in cases {
        let body = page(&seeded.app, &seeded.token, &format!("query={needle}")).await;
        assert_eq!(listed(&body), vec![*expected], "the {needle} substring");
        assert_eq!(body["count"], json!(1), "the {needle} total");
    }
    let reason = page(&seeded.app, &seeded.token, "query=please%20let%20me%20in").await;
    assert_eq!(
        listed(&reason),
        Vec::<&str>::new(),
        "the registration reason is no search column"
    );
    let wildcards = page(&seeded.app, &seeded.token, "query=%25a%25").await;
    assert_eq!(
        listed(&wildcards),
        Vec::<&str>::new(),
        "a percent in the needle is a literal percent"
    );
}

/// `name`, `email`, `tags[]` and `actor_types[]` each narrow the page, and
/// an array key arrives bracketed or bare; a criterion nothing matches
/// answers nothing rather than everything.
#[tokio::test]
async fn the_named_criteria_narrow_the_page() {
    let db = common::test_db().await;
    let seeded = seed(&db).await;
    let by_name = page(&seeded.app, &seeded.token, "name=Plain").await;
    assert_eq!(listed(&by_name), vec!["plain"], "a display-name substring");
    let by_email = page(&seeded.app, &seeded.token, "email=plain%40example.org").await;
    assert_eq!(listed(&by_email), vec!["plain"], "an exact address");
    let partial = page(&seeded.app, &seeded.token, "email=plain%40example").await;
    assert_eq!(
        listed(&partial),
        Vec::<&str>::new(),
        "`email` is equality, not a substring"
    );
    let by_tag = page(&seeded.app, &seeded.token, "tags[]=bots&tags[]=nobody").await;
    assert_eq!(listed(&by_tag), vec!["beeper"], "one of two tags is enough");
    let bare_tag = page(&seeded.app, &seeded.token, "tags=bots").await;
    assert_eq!(
        listed(&bare_tag),
        vec!["beeper"],
        "the bare spelling fills the same list"
    );
    let by_type = page(
        &seeded.app,
        &seeded.token,
        "actor_types[]=Service&actor_types[]=Application",
    )
    .await;
    assert_eq!(
        listed(&by_type),
        vec!["beeper"],
        "one of two types is enough"
    );
    let unmatched = page(&seeded.app, &seeded.token, "tags[]=ghost&name=Ghost").await;
    assert_eq!(
        listed(&unmatched),
        Vec::<&str>::new(),
        "two criteria that miss together miss"
    );
    assert_eq!(unmatched["count"], json!(0), "and count nothing");
    let combined = page(&seeded.app, &seeded.token, "filters=local&query=root").await;
    assert_eq!(listed(&combined), vec!["root"], "filters AND query");
}
