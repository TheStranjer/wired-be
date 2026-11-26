//! The admin account reads over HTTP (WBE-210): the v1 boolean and v2 word
//! spellings of the filter matrix, the criteria this schema accepts but
//! cannot weigh, the pinned sixteen-key entity with its role object and
//! private columns, the `Link`-keyed page and its window, and the show
//! route's answers and misses.

use axum::http::StatusCode;
use serde_json::json;
use uuid::Uuid;
use wired_be::util::time::mastodon_time;

use super::support::{
    ACTIVE, APPROVED, BASE, ENABLED, ENTITY_KEYS, INVALID_ID, LISTED, LOCALS, MEMBERS, NOT_FOUND,
    V1, V2, entity, keys, listed, read, route, seed, v1_page, v2_page,
};
use crate::common::{self, TestRequest};

/// The v1 filter matrix: each of the six boolean criteria and its negation,
/// the four text criteria and a combination of two, each weighed against rows
/// that differ from every other row in exactly one state.
#[tokio::test]
async fn the_v1_filters_select_their_own_rows() {
    let db = common::test_db().await;
    let seeded = seed(&db).await;
    let unapproved = ["unminded", "newbie", "suspended"];
    let cases: &[(&str, &[&str])] = &[
        ("local=true", &LOCALS),
        ("local=false", &["faraway@remote.example"]),
        ("remote=true", &["faraway@remote.example"]),
        ("remote=false", &LOCALS),
        ("active=true", &ACTIVE),
        ("active=false", &unapproved),
        ("pending=true", &["newbie"]),
        ("pending=false", &APPROVED),
        ("disabled=true", &["suspended"]),
        ("disabled=false", &ENABLED),
        ("suspended=true", &[]),
        ("suspended=false", &LISTED),
        ("staff=true", &["moddy", "root"]),
        ("staff=false", &MEMBERS),
        ("username=mod", &["moddy"]),
        ("username=faraway", &["faraway@remote.example"]),
        ("display_name=person", &["plain"]),
        ("by_domain=remote.example", &["faraway@remote.example"]),
        ("by_domain=nowhere.example", &[]),
        ("email=root@example.org", &["root"]),
        ("email=ROOT@example.org", &["root"]),
        (
            "local=true&active=true",
            &["beeper", "plain", "moddy", "root"],
        ),
        ("staff=true&local=false", &[]),
    ];
    for (query, expected) in cases {
        assert_eq!(
            listed(&v1_page(&seeded.app, &seeded.token, query).await),
            expected.to_vec(),
            "the v1 `{query}` filter"
        );
    }
}

/// The criteria with no column behind them are accepted and ignored: each
/// still answers the whole page, and a criterion this instance can weigh still
/// decides beside one it cannot.
#[tokio::test]
async fn the_v1_listing_accepts_the_filters_it_cannot_weigh() {
    let db = common::test_db().await;
    let seeded = seed(&db).await;
    for query in [
        "silenced=true",
        "sensitized=true",
        "ip=203.0.113.7",
        "silenced=false&sensitized=false&ip=203.0.113.7",
    ] {
        assert_eq!(
            listed(&v1_page(&seeded.app, &seeded.token, query).await),
            LISTED.to_vec(),
            "`{query}` filters nothing"
        );
    }
    assert_eq!(
        listed(&v1_page(&seeded.app, &seeded.token, "silenced=true&staff=true").await),
        vec!["moddy", "root"],
        "and a weighable criterion still decides"
    );
}

/// The v2 listing spells the same six facts as words: `origin` picks a side of
/// the `local` column, `status` weighs the state criteria, `permissions=staff`
/// the role pair — and the words this schema cannot weigh, from `status`
/// `silenced` to a permission level of its own, answer the whole page.
#[tokio::test]
async fn the_v2_listing_spells_the_same_criteria_as_words() {
    let db = common::test_db().await;
    let seeded = seed(&db).await;
    let cases: &[(&str, &[&str])] = &[
        ("origin=local", &LOCALS),
        ("origin=remote", &["faraway@remote.example"]),
        ("origin=both", &LISTED),
        ("status=active", &ACTIVE),
        ("status=pending", &["newbie"]),
        ("status=disabled", &["suspended"]),
        ("status=silenced", &LISTED),
        ("status=suspended", &[]),
        ("status=whatever", &LISTED),
        ("permissions=staff", &["moddy", "root"]),
        ("permissions=admin", &LISTED),
        ("origin=remote&status=active", &["faraway@remote.example"]),
        ("origin=local&status=disabled", &["suspended"]),
        ("status=active&permissions=staff", &["moddy", "root"]),
    ];
    for (query, expected) in cases {
        assert_eq!(
            listed(&v2_page(&seeded.app, &seeded.token, query).await),
            expected.to_vec(),
            "the v2 `{query}` criterion"
        );
    }
}

/// v2 reads the four text criteria exactly as v1 does and shares v1's
/// indifference to `ip` and `invited_by`.
#[tokio::test]
async fn the_v2_listing_reads_the_same_text_criteria() {
    let db = common::test_db().await;
    let seeded = seed(&db).await;
    let cases: &[(&str, &[&str])] = &[
        ("username=PLAIN", &["plain"]),
        ("display_name=away", &["faraway@remote.example"]),
        ("by_domain=remote.example", &["faraway@remote.example"]),
        ("email=plain@example.org", &["plain"]),
        ("ip=203.0.113.7&invited_by=1", &LISTED),
    ];
    for (query, expected) in cases {
        assert_eq!(
            listed(&v2_page(&seeded.app, &seeded.token, query).await),
            expected.to_vec(),
            "the v2 `{query}` criterion"
        );
    }
}

/// The shape both routes and both versions answer with is the ticket's sixteen
/// keys, and the role nested in every one of them is the five it pins.
#[tokio::test]
async fn both_versions_answer_the_pinned_entity_shape() {
    let db = common::test_db().await;
    let seeded = seed(&db).await;
    let whole = v1_page(&seeded.app, &seeded.token, "").await;
    let by_word = v2_page(&seeded.app, &seeded.token, "").await;
    assert_eq!(
        listed(&whole),
        LISTED.to_vec(),
        "the newest row first, the instance actor absent"
    );
    assert_eq!(
        listed(&by_word),
        listed(&whole),
        "and v2 pages the same accounts in the same order"
    );
    for entry in whole.as_array().expect("an array of admin accounts") {
        assert_eq!(
            keys(entry),
            ENTITY_KEYS.to_vec(),
            "exactly the sixteen pinned keys"
        );
        assert_eq!(
            keys(&entry["role"]),
            vec!["color", "highlighted", "id", "name", "permissions"],
            "and the role object's five"
        );
    }
}

/// Inside that shape: the handle split into `username` and `domain`, the
/// private columns an admin is owed, the facts this schema does not keep as
/// their pinned empty readings, and the whole profile entity under `account`.
#[tokio::test]
async fn the_entity_states_the_private_columns_and_their_empty_readings() {
    let db = common::test_db().await;
    let seeded = seed(&db).await;
    let whole = v1_page(&seeded.app, &seeded.token, "").await;
    let plain = entity(&whole, "plain");
    assert_eq!(
        plain["id"],
        json!(seeded.plain.id.to_string()),
        "the row id"
    );
    assert_eq!(plain["username"], json!("plain"), "the handle's head");
    assert_eq!(plain["domain"], json!(null), "and no domain for a local");
    assert_eq!(
        plain["created_at"],
        json!(mastodon_time(&seeded.plain.inserted_at)),
        "the Mastodon-style stamp of the row's insertion"
    );
    assert_eq!(
        plain["email"],
        json!("plain@example.org"),
        "the address an admin is owed"
    );
    assert_eq!(plain["ip"], json!(null), "no login address is kept");
    assert_eq!(plain["ips"], json!([]), "and no list of them");
    assert_eq!(plain["locale"], json!(""), "the pinned empty locale");
    assert_eq!(
        plain["invite_request"],
        json!(null),
        "no reason was stated at registration"
    );
    assert_eq!(plain["confirmed"], json!(true), "the address is checked");
    assert_eq!(
        plain["suspended"],
        json!(false),
        "nothing suspends an account here"
    );
    assert_eq!(plain["silenced"], json!(false), "and nothing silences one");
    assert_eq!(plain["disabled"], json!(false), "the account is live");
    assert_eq!(
        plain["approved"],
        json!(true),
        "and nobody owes it approval"
    );
    assert_eq!(
        plain["account"]["acct"],
        json!("plain"),
        "the profile entity rides along under `account`"
    );

    let mirror = entity(&whole, "faraway@remote.example");
    assert_eq!(mirror["username"], json!("faraway"), "the handle's head");
    assert_eq!(
        mirror["domain"],
        json!("remote.example"),
        "and the host it lives on"
    );
    assert_eq!(mirror["email"], json!(null), "a mirror gave no address");
    assert_eq!(mirror["role"]["id"], json!("-99"), "and no role of ours");

    let pending = entity(&whole, "newbie");
    assert_eq!(
        pending["invite_request"],
        json!("please let me in"),
        "a registration reason is Mastodon's invite request"
    );
    assert_eq!(pending["approved"], json!(false), "and it still waits");
    let asleep = entity(&whole, "suspended");
    assert_eq!(asleep["disabled"], json!(true), "a deactivated account");
    let flags = [
        asleep["suspended"].clone(),
        asleep["account"]["suspended"].clone(),
    ];
    assert_eq!(flags, [json!(false), json!(false)], "is no suspended one");
    let unminded = entity(&whole, "unminded");
    assert_eq!(
        unminded["confirmed"],
        json!(false),
        "an unchecked address is an unconfirmed one"
    );
}

/// The role object reads the two staff flags: the administrator's object, the
/// moderator's, and the unnamed placeholder for everybody else.
#[tokio::test]
async fn the_role_object_names_the_held_role() {
    let db = common::test_db().await;
    let seeded = seed(&db).await;
    let staff = v1_page(&seeded.app, &seeded.token, "staff=true").await;
    assert_eq!(
        entity(&staff, "root")["role"],
        json!({
            "id": "1",
            "name": "Admin",
            "color": "",
            "permissions": "1",
            "highlighted": true,
        }),
        "the administrator's own object"
    );
    assert_eq!(
        entity(&staff, "moddy")["role"],
        json!({
            "id": "2",
            "name": "Moderator",
            "color": "",
            "permissions": "0",
            "highlighted": true,
        }),
        "the moderator's"
    );
    let plain = v1_page(&seeded.app, &seeded.token, "username=plain").await;
    assert_eq!(
        plain[0]["role"],
        json!({
            "id": "-99",
            "name": "",
            "color": "",
            "permissions": "0",
            "highlighted": false,
        }),
        "and the placeholder for an account holding neither flag"
    );
}

/// The page walks the id cursor under a `Link` header keyed on the account
/// rows: `limit` cuts it, `next` and `prev` name the page's oldest and newest
/// ids, the request's own filters and `limit` repeat on both links while the
/// cursors do not, `max_id` continues where a page ended, a `min_id` page comes
/// back newest-first, and the empty answer carries no `Link` at all.
#[tokio::test]
async fn the_page_walks_the_id_cursor_under_the_link_header() {
    let db = common::test_db().await;
    let seeded = seed(&db).await;
    let app = &seeded.app;
    let token = &seeded.token;

    let first = TestRequest::get(&route(V1, "local=true&limit=2"))
        .bearer(token)
        .send(app)
        .await;
    common::assert_answer(&first, StatusCode::OK, "a filtered page");
    let body = first.json_value();
    let ids = body
        .as_array()
        .expect("an array of admin accounts")
        .iter()
        .filter_map(|found| found["id"].as_str())
        .collect::<Vec<&str>>();
    assert_eq!(
        listed(&body),
        vec!["beeper", "unminded"],
        "the two newest locals, `limit` honoured"
    );
    assert_eq!(
        ids.len(),
        2,
        "and both rows carry the id their cursor is keyed on"
    );
    assert_eq!(
        first.header("link"),
        Some(format!(
            "<{BASE}{V1}?local=true&limit=2&max_id={}>; rel=\"next\", \
             <{BASE}{V1}?local=true&limit=2&min_id={}>; rel=\"prev\"",
            ids[1], ids[0]
        ))
        .as_deref(),
        "next walks to the page's oldest id, prev back to its newest, filters repeated, cursors not"
    );

    let next = read(
        app,
        token,
        &route(V1, &format!("local=true&limit=2&max_id={}", ids[1])),
    )
    .await;
    assert_eq!(
        listed(&next),
        vec!["newbie", "suspended"],
        "and the followed link continues where the page ended"
    );

    let above = read(
        app,
        token,
        &route(V1, &format!("limit=2&min_id={}", ids[1])),
    )
    .await;
    assert_eq!(
        listed(&above),
        vec!["faraway@remote.example", "beeper"],
        "a min_id page is read forward and answered newest-first"
    );

    let empty = TestRequest::get(&route(V1, "by_domain=nowhere.example"))
        .bearer(token)
        .send(app)
        .await;
    common::assert_answer(&empty, StatusCode::OK, "a page that matches nothing");
    assert_eq!(empty.json_value(), json!([]), "the empty array");
    assert_eq!(empty.header("link"), None, "and no Link header at all");
}

/// The window is the admin pair the ticket pins rather than Pleroma's global
/// one: a stated `limit` is honoured, an oversized one falls to the cap rather
/// than refusing the request, and a malformed cursor is still the shared 400.
#[tokio::test]
async fn the_window_is_honoured_capped_and_refused() {
    let db = common::test_db().await;
    let seeded = seed(&db).await;
    let app = &seeded.app;
    let token = &seeded.token;

    assert_eq!(
        v1_page(app, token, "")
            .await
            .as_array()
            .expect("an array")
            .len(),
        LISTED.len(),
        "an unstated page is wide enough for the whole fixture"
    );
    assert_eq!(
        v1_page(app, token, "limit=1")
            .await
            .as_array()
            .expect("an array")
            .len(),
        1,
        "and a stated one is honoured"
    );
    assert_eq!(
        v2_page(app, token, "limit=9999")
            .await
            .as_array()
            .expect("an array")
            .len(),
        LISTED.len(),
        "an oversized page is capped, not refused"
    );

    let broken = TestRequest::get(&route(V1, "max_id=nope"))
        .bearer(token)
        .send(app)
        .await;
    common::assert_error(&broken, INVALID_ID, "a cursor that is no id");
    let broken_v2 = TestRequest::get(&route(V2, "since_id=nope"))
        .bearer(token)
        .send(app)
        .await;
    common::assert_error(&broken_v2, INVALID_ID, "in v2 as well");
}

/// The show route answers the one entity its id names — a mirror, a
/// deactivated account and the instance actor included, because an admin's
/// read has no visibility rule on it — and answers 404
/// `{"error":"Record not found"}` for an id that names no row and for text that
/// is no id.
#[tokio::test]
async fn the_show_route_answers_one_entity_or_404s() {
    let db = common::test_db().await;
    let seeded = seed(&db).await;
    let app = &seeded.app;
    let token = &seeded.token;

    let shown = read(app, token, &format!("{V1}/{}", seeded.mirror.id)).await;
    assert_eq!(
        keys(&shown),
        ENTITY_KEYS.to_vec(),
        "the same sixteen keys a listing entry carries"
    );
    assert_eq!(
        shown["id"],
        json!(seeded.mirror.id.to_string()),
        "the row the path named"
    );
    assert_eq!(shown["username"], json!("faraway"), "the handle's head");
    assert_eq!(shown["domain"], json!("remote.example"), "and its host");
    assert_eq!(
        shown["account"]["acct"],
        json!("faraway@remote.example"),
        "the profile entity beside them"
    );

    let asleep = v1_page(app, token, "disabled=true").await;
    let id = asleep[0]["id"].as_str().expect("the deactivated id");
    let deactivated = read(app, token, &format!("{V1}/{id}")).await;
    assert_eq!(
        deactivated["disabled"],
        json!(true),
        "an admin reads a deactivated account's entity, the listing's row included"
    );

    let actor = read(app, token, &format!("{V1}/{}", seeded.actor.id)).await;
    assert_eq!(
        actor["username"],
        json!("internal.fetch"),
        "a listing hides the instance actor, a lookup by id finds it"
    );

    let unknown = format!("{V1}/{}", Uuid::now_v7());
    let gone = TestRequest::get(&unknown).bearer(token).send(app).await;
    common::assert_error(&gone, NOT_FOUND, "an id that names no row");
    let malformed = TestRequest::get(&format!("{V1}/not-an-id"))
        .bearer(token)
        .send(app)
        .await;
    common::assert_error(&malformed, NOT_FOUND, "text that is no id");
}
