//! The Mastodon admin account reads over HTTP (`/api/v{1,2}/admin/accounts`,
//! WBE-210): Pleroma's `MastodonAPI.Admin.AccountController` `index`, `index2`
//! and `show` — the sixteen-key entity with its role object, the v1 boolean and
//! v2 word spellings of the same criteria, the criteria this schema cannot
//! weigh and accepts anyway, the page under a `Link` header keyed on the
//! account ids, the show route's two misses, and the staff-and-
//! `admin:read:accounts` gate in front of all of it.
//!
//! The shared fixture seeds one account per state a filter weighs — an admin, a
//! moderator, a plain active account, a deactivated one, one awaiting approval,
//! one awaiting its email check, a tagged `Service` and a remote mirror —
//! beside the instance actor, which the listings must never answer with. Every
//! test reads those rows through the real router, so the order the page comes
//! back in, the cursors the `Link` header names and the entity's keys are
//! asserted against stored facts rather than a hand-built row.

#![cfg(test)]

use axum::{Router, http::StatusCode};
use serde_json::{Value, json};
use sqlx::PgPool;
use uuid::Uuid;
use wired_be::{
    config::TEST_BASE_URL,
    models::user::{Flag, NewUser, User},
    services::instance_actor::ensure_instance_actor,
    test_db::TestDb,
    util::time::mastodon_time,
};

use crate::common::{self, TestRequest};

/// The v1 listing's prefix, and the show route's.
const V1: &str = "/api/v1/admin/accounts";
/// The v2 listing's prefix.
const V2: &str = "/api/v2/admin/accounts";
/// The origin the test configuration answers its URLs from.
const BASE: &str = TEST_BASE_URL;
/// The 401 body when no token is presented.
const NO_TOKEN: &str = "Invalid credentials";
/// The 401 body when a presented token does not resolve.
const INVALID_TOKEN: &str = "The access token is invalid";
/// The 403 body for an app-only (user-unbound) token.
const USER_REQUIRED: &str = "This API requires an authenticated user";
/// The 403 body for a valid token whose user is neither admin nor moderator.
const NOT_STAFF: &str = "User is not a staff member.";
/// The 403 body of a token that grants neither `admin:read` nor
/// `admin:read:accounts`.
const NO_ADMIN_READ: &str = "Insufficient permissions: admin:read.";
/// The 404 body of these routes' misses.
const NOT_FOUND: &str = "Record not found";
/// The 400 body of a cursor that is no id.
const INVALID_ID: &str = "Invalid id";

/// The accounts the fixture seeds, in the order both listings answer them:
/// newest row first (`UUIDv7` ids sort by creation, decision 4), the instance
/// actor absent. The fixture inserts in the order root, moddy, plain,
/// suspended, newbie, unminded, beeper, mirror, so the mirror is the newest row
/// and the admin the oldest.
const LISTED: [&str; 8] = [
    "faraway@remote.example",
    "beeper",
    "unminded",
    "newbie",
    "suspended",
    "plain",
    "moddy",
    "root",
];
/// The seven accounts this instance owns.
const LOCALS: [&str; 7] = [
    "beeper",
    "unminded",
    "newbie",
    "suspended",
    "plain",
    "moddy",
    "root",
];
/// The accounts that are active, approved and confirmed at once.
const ACTIVE: [&str; 5] = ["faraway@remote.example", "beeper", "plain", "moddy", "root"];
/// Every account but the deactivated one.
const ENABLED: [&str; 7] = [
    "faraway@remote.example",
    "beeper",
    "unminded",
    "newbie",
    "plain",
    "moddy",
    "root",
];
/// Every account but the one still awaiting approval.
const APPROVED: [&str; 7] = [
    "faraway@remote.example",
    "beeper",
    "unminded",
    "suspended",
    "plain",
    "moddy",
    "root",
];
/// Every account but the two staff ones.
const MEMBERS: [&str; 6] = [
    "faraway@remote.example",
    "beeper",
    "unminded",
    "newbie",
    "suspended",
    "plain",
];
/// The entity's sixteen keys, sorted the way [`keys`] reports them.
const ENTITY_KEYS: [&str; 16] = [
    "account",
    "approved",
    "confirmed",
    "created_at",
    "disabled",
    "domain",
    "email",
    "id",
    "invite_request",
    "ip",
    "ips",
    "locale",
    "role",
    "silenced",
    "suspended",
    "username",
];

/// What the fixture leaves behind for one test.
struct Seeded {
    /// The router built on the slot's pool.
    app: Router,
    /// The admin account, whose tokens the scope tests mint.
    root: User,
    /// The moderator, whose token the staff gate is checked with.
    moddy: User,
    /// The plain account a stranger's token is minted from.
    plain: User,
    /// The remote mirror, whose handle the entity splits in two.
    mirror: User,
    /// The instance actor, which no listing answers with.
    actor: User,
    /// A bearer token for `root` granting the accounts read scope.
    token: String,
}

/// Seeds one account per state the filters weigh, plus the instance actor, and
/// returns them beside the admin who reads them and the app to read them
/// through.
async fn seed(db: &TestDb) -> Seeded {
    let pool = db.pool().clone();
    let root = create(&pool, "root", |user| NewUser {
        is_admin: Flag::new(true),
        email: Some("root@example.org".to_owned()),
        ..user
    })
    .await;
    let moddy = create(&pool, "moddy", |user| NewUser {
        is_moderator: Flag::new(true),
        ..user
    })
    .await;
    let plain = create(&pool, "plain", |user| NewUser {
        name: "Plain Person".to_owned(),
        email: Some("plain@example.org".to_owned()),
        ..user
    })
    .await;
    seeded(&pool, "suspended", |user| NewUser {
        is_active: Flag::new(false),
        ..user
    })
    .await;
    seeded(&pool, "newbie", |user| NewUser {
        approval_pending: Flag::new(true),
        registration_reason: Some("please let me in".to_owned()),
        ..user
    })
    .await;
    seeded(&pool, "unminded", |user| NewUser {
        email_confirmed: Flag::new(false),
        ..user
    })
    .await;
    seeded(&pool, "beeper", |user| NewUser {
        actor_type: "Service".to_owned(),
        tags: vec!["bots".to_owned()],
        ..user
    })
    .await;
    let mirror = common::create_remote_user(&pool, "faraway", "remote.example").await;
    let actor = ensure_instance_actor(&pool, &common::test_config())
        .await
        .expect("the instance actor is seeded");
    let token = common::create_app_and_token(&pool, &root, "admin:read:accounts").await;
    Seeded {
        app: common::test_app(pool),
        root,
        moddy,
        plain,
        mirror,
        actor,
        token,
    }
}

/// Inserts a local account named `nickname` with `customize` applied on top of
/// [`NewUser::local`].
async fn create<F>(pool: &PgPool, nickname: &str, customize: F) -> User
where
    F: FnOnce(NewUser) -> NewUser,
{
    common::create_local_user_with(pool, nickname, customize).await
}

/// The same insert, spending the row: the tests below read these accounts back
/// through the API rather than from Rust, and only the four whose ids or
/// stamps they name are kept.
async fn seeded<F>(pool: &PgPool, nickname: &str, customize: F)
where
    F: FnOnce(NewUser) -> NewUser,
{
    let _row: User = create(pool, nickname, customize).await;
}

/// `GET`s `path` as `token` and returns the parsed body, asserting the 200 a
/// staff read answers with.
async fn read(app: &Router, token: &str, path: &str) -> Value {
    let response = TestRequest::get(path).bearer(token).send(app).await;
    common::assert_answer(&response, StatusCode::OK, "a staff read");
    response.json_value()
}

/// The v1 listing of `query` (an empty string is the bare route).
async fn v1_page(app: &Router, token: &str, query: &str) -> Value {
    read(app, token, &route(V1, query)).await
}

/// The v2 listing of `query`.
async fn v2_page(app: &Router, token: &str, query: &str) -> Value {
    read(app, token, &route(V2, query)).await
}

/// The route with its query string: an empty one leaves the `?` off.
fn route(prefix: &str, query: &str) -> String {
    if query.is_empty() {
        prefix.to_owned()
    } else {
        format!("{prefix}?{query}")
    }
}

/// The handles one listing answer names, in the order it names them.
fn listed(body: &Value) -> Vec<&str> {
    body.as_array()
        .expect("an array of admin accounts")
        .iter()
        .filter_map(|found| found["account"]["acct"].as_str())
        .collect()
}

/// The keys of one rendered object, sorted so a missing or an extra one shows
/// up as a diff rather than a lucky pass.
fn keys(value: &Value) -> Vec<&str> {
    let mut found = value
        .as_object()
        .expect("an object")
        .keys()
        .map(String::as_str)
        .collect::<Vec<&str>>();
    found.sort_unstable();
    found
}

/// The entity of `acct` out of a listing answer.
fn entity<'a>(body: &'a Value, acct: &str) -> &'a Value {
    body.as_array()
        .expect("an array of admin accounts")
        .iter()
        .find(|found| found["account"]["acct"] == json!(acct))
        .unwrap_or_else(|| panic!("the answer holds no {acct}"))
}

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
        "suspended=true",
        "sensitized=true",
        "ip=203.0.113.7",
        "silenced=false&suspended=false&sensitized=false&ip=203.0.113.7",
    ] {
        assert_eq!(
            listed(&v1_page(&seeded.app, &seeded.token, query).await),
            LISTED.to_vec(),
            "`{query}` filters nothing"
        );
    }
    assert_eq!(
        listed(&v1_page(&seeded.app, &seeded.token, "suspended=true&staff=true").await),
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
        ("status=suspended", &LISTED),
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
    assert_eq!(
        asleep["account"]["suspended"],
        json!(true),
        "and its profile entity says so too"
    );
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

/// Every route is staff's: no token, an unknown token and an app-only token are
/// refused, and so is a valid user token whose account holds neither role —
/// while a moderator gets the very same answers as the admin who seeded them.
#[tokio::test]
async fn the_gate_demands_a_staff_user() {
    let db = common::test_db().await;
    let seeded = seed(&db).await;
    let pool = db.pool().clone();
    let moderator = common::create_app_and_token(&pool, &seeded.moddy, "admin:read").await;
    let stranger = common::create_app_and_token(&pool, &seeded.plain, "admin:read").await;
    let app_only = common::create_app_only_token(&pool, "admin:read").await;

    for (prefix, label) in [(V1, "the v1 listing"), (V2, "the v2 listing")] {
        let anonymous = TestRequest::get(prefix).send(&seeded.app).await;
        common::assert_error(&anonymous, NO_TOKEN, &format!("no token on {label}"));
        let bogus = TestRequest::get(prefix)
            .bearer("nope")
            .send(&seeded.app)
            .await;
        common::assert_error(&bogus, INVALID_TOKEN, &format!("a bogus token on {label}"));
        let unbound = TestRequest::get(prefix)
            .bearer(&app_only)
            .send(&seeded.app)
            .await;
        common::assert_error(
            &unbound,
            USER_REQUIRED,
            &format!("an app-only token on {label}"),
        );
        let outsider = TestRequest::get(prefix)
            .bearer(&stranger)
            .send(&seeded.app)
            .await;
        common::assert_error(
            &outsider,
            NOT_STAFF,
            &format!("a non-staff token on {label}"),
        );
        let answered = read(&seeded.app, &moderator, prefix).await;
        assert_eq!(
            listed(&answered),
            LISTED.to_vec(),
            "a moderator reads what an admin reads on {label}"
        );
    }

    let show = format!("{V1}/{}", seeded.root.id);
    let outsider = TestRequest::get(&show)
        .bearer(&stranger)
        .send(&seeded.app)
        .await;
    common::assert_error(&outsider, NOT_STAFF, "a non-staff token on the show route");
    let anonymous = TestRequest::get(&show).send(&seeded.app).await;
    common::assert_error(&anonymous, NO_TOKEN, "no token on the show route");
}

/// The scope half of the gate is `admin:read:accounts`: the broad `admin:read`
/// root and the accounts area each pass, and another area, the write class and
/// an ordinary `read` all refuse with the class root named.
#[tokio::test]
async fn the_gate_demands_the_accounts_read_scope() {
    let db = common::test_db().await;
    let seeded = seed(&db).await;
    let pool = db.pool().clone();
    let show = format!("{V1}/{}", seeded.root.id);

    for (scopes, why) in [
        ("admin:read", "the class root"),
        ("admin:read:accounts", "the area scope"),
    ] {
        let token = common::create_app_and_token(&pool, &seeded.root, scopes).await;
        for route in [V1, V2] {
            let answered = read(&seeded.app, &token, route).await;
            assert_eq!(
                listed(&answered),
                LISTED.to_vec(),
                "{why} reads the {route} listing"
            );
        }
        let shown = read(&seeded.app, &token, &show).await;
        assert_eq!(
            shown["id"],
            json!(seeded.root.id.to_string()),
            "{why} reads one account too"
        );
    }

    for (scopes, why) in [
        ("admin:read:reports", "another admin area"),
        ("admin:write:accounts", "the write class"),
        ("admin:write", "the write root"),
        ("read", "an ordinary read"),
        ("", "no scope at all"),
    ] {
        let token = common::create_app_and_token(&pool, &seeded.root, scopes).await;
        let refused = [V1.to_owned(), V2.to_owned(), show.clone()];
        for route in &refused {
            let response = TestRequest::get(route)
                .bearer(&token)
                .send(&seeded.app)
                .await;
            common::assert_error(&response, NO_ADMIN_READ, &format!("{why} on {route}"));
        }
    }
}
