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
    config::{Config, TEST_BASE_URL},
    models::{
        report::{self, Category, NewReport},
        user::{Flag, NewUser, User},
    },
    services::instance_actor::ensure_instance_actor,
    test_db::TestDb,
    util::{password::hash_password, time::mastodon_time},
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

// ===== WBE-213: the account writes — delete, enable, approve, reject, action =====

/// The 403 body of the two refusals the writes answer alike: the admin who
/// names their own account in a deletion, and the rejection of an account
/// nobody had left pending.
const NOT_ALLOWED: &str = "This action is not allowed";
/// The 422 body of a `type` word off the five the umbrella knows.
const INVALID_TYPE: &str = "Validation failed: Type is invalid";
/// The 422 body of a request that states no `type` at all.
const MISSING_TYPE: &str = "Validation failed: Type is required";
/// The 400 body of a `report_id` whose report is about another account.
const WRONG_TARGET: &str = "Report does not target this account";
/// The 403 body for a valid user token whose account is not an admin: the
/// writes are the admin's, where the reads are staff's.
const NOT_ADMIN: &str = "User is not an admin.";
/// The 403 body of a token that grants neither `admin:write` nor
/// `admin:write:accounts`.
const NO_ADMIN_WRITE: &str = "Insufficient permissions: admin:write.";
/// The password every write fixture signs in with.
const PASSWORD: &str = "a long enough pass";
/// The 403 the bearer gate answers a deactivated account's request with.
const DISABLED: &str = "Your account is currently disabled";
/// The queue the `users.delete` sweep rides on.
const BACKGROUND: &str = "background";
/// The job kind of an account's deletion sweep.
const DELETE_KIND: &str = "users.delete";
/// The subject of the letter a newly approved account is posted.
const APPROVED_MAIL: &str = "Your account on Wired is approved";
/// The subject of the letter a rejected account is posted.
const REJECTED_MAIL: &str = "Your account on Wired has been rejected";

/// What the write tests leave behind: the admin, one account per state a
/// write weighs, and the `admin:write:accounts` token the writes are signed
/// with.
///
/// The app is built on a config with `SMTP_HOST` set, because the two
/// letters the approval and the rejection post are what those tests count;
/// the suite default configures no relay and every send would drop.
struct Writes {
    /// The pool every row was inserted into.
    pool: PgPool,
    /// The router built on that pool.
    app: Router,
    /// The admin who signs every request.
    root: User,
    /// An ordinary active, approved, confirmed account, with a password.
    plain: User,
    /// A deactivated account, with a password.
    asleep: User,
    /// A registration awaiting staff, with an address.
    newbie: User,
    /// A registration awaiting staff that gave no address at all.
    quiet: User,
    /// The reporter whose reports the umbrella resolves.
    beeper: User,
    /// A remote mirror, which moderation reaches by id like any other row.
    mirror: User,
    /// `root`'s `admin:write:accounts` bearer token.
    token: String,
}

/// Seeds the accounts the writes act on: one per state a write weighs, plus
/// a reporter and a mirror, beside the admin who acts on them.
async fn seed_writes(db: &TestDb) -> Writes {
    let pool = db.pool().clone();
    let root = create(&pool, "root", |user| NewUser {
        is_admin: Flag::new(true),
        ..user
    })
    .await;
    let plain = common::create_user_with_password(&pool, "plain", PASSWORD).await;
    let asleep = common::create_deactivated_user_with_password(&pool, "asleep", PASSWORD).await;
    let newbie = create(&pool, "newbie", |user| NewUser {
        approval_pending: Flag::new(true),
        email: Some("newbie@example.org".to_owned()),
        password_hash: Some(hash_password(PASSWORD).expect("the fixture password hashes")),
        ..user
    })
    .await;
    let quiet = create(&pool, "quiet", |user| NewUser {
        approval_pending: Flag::new(true),
        password_hash: Some(hash_password(PASSWORD).expect("the fixture password hashes")),
        ..user
    })
    .await;
    let beeper = create(&pool, "beeper", |user| NewUser {
        actor_type: "Service".to_owned(),
        ..user
    })
    .await;
    let mirror = common::create_remote_user(&pool, "faraway", "remote.example").await;
    let token = common::create_app_and_token(&pool, &root, "admin:write:accounts").await;
    Writes {
        app: common::test_app_with_config(pool.clone(), mail_config()),
        pool,
        root,
        plain,
        asleep,
        newbie,
        quiet,
        beeper,
        mirror,
        token,
    }
}

/// A config with `SMTP_HOST` set, so the letters the approval and the
/// rejection post really reach the mailer queue.
fn mail_config() -> Config {
    let mut config = common::test_config();
    config.smtp_host = Some("smtp.example.org".to_owned());
    config
}

/// The `{id}` route of one account.
fn account_path(id: Uuid) -> String {
    format!("{V1}/{id}")
}

/// The `{id}/{suffix}` route of one account.
fn account_action_path(id: Uuid, suffix: &str) -> String {
    format!("{V1}/{id}/{suffix}")
}

/// `DELETE`s `path` as `token`.
async fn delete_as(app: &Router, token: &str, path: &str) -> common::TestResponse {
    TestRequest::delete(path).bearer(token).send(app).await
}

/// `POST`s the JSON `body` to `path` as `token`.
async fn post_json(app: &Router, token: &str, path: &str, body: &Value) -> common::TestResponse {
    TestRequest::post(path)
        .bearer(token)
        .json(body)
        .send(app)
        .await
}

/// The umbrella request for `id`, asserted to answer the empty object under
/// a 200 — the pinned shape of every accepted action.
async fn act(app: &Router, token: &str, id: Uuid, body: &Value) {
    let response = post_json(app, token, &account_action_path(id, "action"), body).await;
    common::assert_answer(&response, StatusCode::OK, "the account action");
    assert_eq!(
        response.json_value(),
        json!({}),
        "the umbrella answers the empty object, not Pleroma's 204"
    );
}

/// The three state flags of one row, or `None` once the row is gone:
/// activation, the staff gate and whether a deletion was stamped.
async fn row_state(pool: &PgPool, id: Uuid) -> Option<(bool, bool, bool)> {
    sqlx::query_as::<_, (bool, bool, bool)>(
        "SELECT is_active, approval_pending, deleted_at IS NOT NULL FROM users WHERE id = $1",
    )
    .bind(id)
    .fetch_optional(pool)
    .await
    .expect("the row reads back")
}

/// How many live bearer tokens `account` holds.
async fn token_count(pool: &PgPool, account: &User) -> i64 {
    sqlx::query_scalar::<_, i64>("SELECT count(*) FROM oauth_tokens WHERE user_id = $1")
        .bind(account.id)
        .fetch_one(pool)
        .await
        .expect("the token count reads back")
}

/// The moderation-log entries of one action, oldest first: sentence and data.
async fn logged(pool: &PgPool, action: &str) -> Vec<(String, Value)> {
    sqlx::query_as::<_, (String, Value)>(
        "SELECT message, data FROM moderation_log WHERE action = $1 ORDER BY id",
    )
    .bind(action)
    .fetch_all(pool)
    .await
    .expect("the log reads back")
}

/// The single entry one write must have left: its sentence and the nicknames
/// its `subject` data map names.
async fn one_entry(pool: &PgPool, action: &str) -> (String, Vec<String>) {
    let entries = logged(pool, action).await;
    let (message, data) = entries
        .first()
        .unwrap_or_else(|| panic!("one {action} entry was expected"));
    assert_eq!(
        entries.len(),
        1,
        "exactly one {action} entry, got {entries:?}"
    );
    let subjects = data["subject"]
        .as_array()
        .expect("the subject list")
        .iter()
        .filter_map(|found| found["nickname"].as_str())
        .map(str::to_owned)
        .collect();
    (message.clone(), subjects)
}

/// The arguments of every job on `queue` of `kind`, oldest first.
async fn jobs_of(pool: &PgPool, queue: &str, kind: &str) -> Vec<Value> {
    sqlx::query_as::<_, (Value,)>(
        "SELECT args FROM jobs WHERE queue = $1 AND kind = $2 ORDER BY id",
    )
    .bind(queue)
    .bind(kind)
    .fetch_all(pool)
    .await
    .expect("the queue reads back")
    .into_iter()
    .map(|(args,)| args)
    .collect()
}

/// The sweeps queued for one account.
async fn sweeps_for(pool: &PgPool, account: &User) -> Vec<Value> {
    jobs_of(pool, BACKGROUND, DELETE_KIND)
        .await
        .into_iter()
        .filter(|args| args["user_id"] == json!(account.id.to_string()))
        .collect()
}

/// The letters the mailer queue holds, in the order they were posted.
async fn letters(pool: &PgPool) -> Vec<Value> {
    jobs_of(pool, "mailer", "mailer.send").await
}

/// The stored state of one report, beside whether its action stamp is set.
async fn report_state(pool: &PgPool, id: Uuid) -> (String, bool) {
    sqlx::query_as::<_, (String, bool)>(
        "SELECT state, action_taken_at IS NOT NULL FROM reports WHERE id = $1",
    )
    .bind(id)
    .fetch_one(pool)
    .await
    .expect("the report reads back")
}

/// Files `beeper`'s report about `target` through the model.
async fn file_against(pool: &PgPool, beeper: &User, target: &User) -> Uuid {
    let filed = report::create(
        pool,
        BASE,
        beeper,
        target,
        &NewReport {
            comment: "they did the thing".to_owned(),
            category: Category::Other,
            forwarded: false,
            rule_ids: Vec::new(),
            status_ids: Vec::new(),
        },
    )
    .await
    .expect("the fixture report files");
    filed.id
}

/// The 403 body the login gate and the bearer gate answer an
/// approval-pending account with, and its machine-readable reason.
const AWAITING: &str = "Your account is awaiting approval.";

/// The OAuth `password` grant body for `username`, over the app's credentials.
fn grant_body(client_id: &str, client_secret: &str, username: &str) -> Value {
    json!({
        "grant_type": "password",
        "username": username,
        "password": PASSWORD,
        "scope": "read",
        "client_id": client_id,
        "client_secret": client_secret,
    })
}

/// The `password` grant for `username`.
async fn login(
    app: &Router,
    client_id: &str,
    client_secret: &str,
    username: &str,
) -> common::TestResponse {
    TestRequest::post("/oauth/token")
        .json(&grant_body(client_id, client_secret, username))
        .send(app)
        .await
}

/// An ordinary request to the account's own credentials endpoint, the one
/// call the bearer gate answers with its role-and-flag refusals.
async fn verify(app: &Router, token: &str) -> common::TestResponse {
    TestRequest::get("/api/v1/accounts/verify_credentials")
        .bearer(token)
        .send(app)
        .await
}

/// `DELETE /api/v1/admin/accounts/{id}` starts the same sweep the Pleroma
/// console's `DELETE /users` starts — `deleted_at` stamped, every token
/// revoked, `users.delete` queued — and answers the entity as the row stood,
/// which is the state the client asked to see deleted.
#[tokio::test]
async fn deleting_an_account_starts_its_sweep_and_answers_the_row_as_it_was() {
    let db = common::test_db().await;
    let seeded = seed_writes(&db).await;
    let mine = seeded.token.clone();
    let victim_token = common::create_app_and_token(&seeded.pool, &seeded.plain, "read").await;
    let client = common::create_app(&seeded.pool, "read").await;
    assert_eq!(token_count(&seeded.pool, &seeded.plain).await, 1);

    let response = delete_as(&seeded.app, &mine, &account_path(seeded.plain.id)).await;
    common::assert_answer(&response, StatusCode::OK, "the deletion");
    let body = response.json_value();
    assert_eq!(body["id"], json!(seeded.plain.id.to_string()), "the row");
    assert_eq!(body["username"], json!("plain"), "as the request named it");
    assert_eq!(
        body["disabled"],
        json!(false),
        "as it was: the answer is the pre-deletion snapshot"
    );
    assert_eq!(
        body["approved"],
        json!(true),
        "and not the state it is now in"
    );

    let (active, pending, doomed) = row_state(&seeded.pool, seeded.plain.id)
        .await
        .expect("the row survives its sweep's start");
    assert!(!active, "deleted_at implies the deactivation");
    assert!(!pending, "the approval gate was never the point");
    assert!(doomed, "and the deletion is stamped");
    assert_eq!(
        token_count(&seeded.pool, &seeded.plain).await,
        0,
        "every token went with the start of the deletion"
    );
    assert_eq!(
        sweeps_for(&seeded.pool, &seeded.plain).await.len(),
        1,
        "one sweep queued for the account"
    );

    let denied = verify(&seeded.app, &victim_token).await;
    common::assert_answer(&denied, StatusCode::UNAUTHORIZED, "the revoked credential");
    common::assert_error(&denied, INVALID_TOKEN, "is no credential at all");
    let refused = login(
        &seeded.app,
        &client.client_id,
        &client.client_secret,
        "plain",
    )
    .await;
    common::assert_answer(
        &refused,
        StatusCode::FORBIDDEN,
        "the sign-in of a doomed account",
    );

    let (message, subjects) = one_entry(&seeded.pool, "delete").await;
    assert_eq!(message, "@root deleted users: @plain", "Pleroma's delete");
    assert_eq!(subjects, vec!["plain"], "naming the account");
}

/// The admin's own row is off limits: naming yourself is the 403 this surface
/// answers its refusals with, and nothing is stamped, queued or logged.
#[tokio::test]
async fn deleting_refuses_the_admins_own_account() {
    let db = common::test_db().await;
    let seeded = seed_writes(&db).await;

    let response = delete_as(&seeded.app, &seeded.token, &account_path(seeded.root.id)).await;
    common::assert_answer(&response, StatusCode::FORBIDDEN, "self-deletion");
    common::assert_error(&response, NOT_ALLOWED, "in the pinned words");

    let (active, _, doomed) = row_state(&seeded.pool, seeded.root.id)
        .await
        .expect("the admin is still here");
    assert!(active, "and the refusal is real: the flag did not move");
    assert!(!doomed, "nor was a deletion stamped");
    assert!(
        sweeps_for(&seeded.pool, &seeded.root).await.is_empty(),
        "and no sweep was queued"
    );
    assert!(
        logged(&seeded.pool, "delete").await.is_empty(),
        "and the refused request left no entry behind"
    );
}

/// The 404 of an id that names no account is the reads' own wording, and it
/// answers before any body is read or any state moves.
#[tokio::test]
async fn the_writes_404_for_an_id_that_names_no_account() {
    let db = common::test_db().await;
    let seeded = seed_writes(&db).await;
    let missing = Uuid::now_v7();
    let token = seeded.token.clone();

    let gone = delete_as(&seeded.app, &token, &account_path(missing)).await;
    common::assert_error(&gone, NOT_FOUND, "the deletion of nothing");
    for suffix in ["enable", "approve", "reject"] {
        let response = post_json(
            &seeded.app,
            &token,
            &account_action_path(missing, suffix),
            &json!({}),
        )
        .await;
        common::assert_answer(
            &response,
            StatusCode::NOT_FOUND,
            &format!("the {suffix} of nothing"),
        );
        common::assert_error(&response, NOT_FOUND, "is the shared body");
    }
    let response = post_json(
        &seeded.app,
        &token,
        &account_action_path(missing, "action"),
        &json!({"type": "disable"}),
    )
    .await;
    common::assert_answer(&response, StatusCode::NOT_FOUND, "the action on nothing");
    common::assert_error(
        &response,
        NOT_FOUND,
        "and the account half is checked first",
    );

    let text = TestRequest::delete(&format!("{V1}/not-an-id"))
        .bearer(&token)
        .send(&seeded.app)
        .await;
    common::assert_answer(&text, StatusCode::NOT_FOUND, "text that is no id");
    let enabled = post_json(
        &seeded.app,
        &token,
        &format!("{V1}/not-an-id/enable"),
        &json!({}),
    )
    .await;
    common::assert_answer(
        &enabled,
        StatusCode::NOT_FOUND,
        "on the action suffixes too",
    );
    assert!(
        logged(&seeded.pool, "delete").await.is_empty(),
        "and the misses wrote nothing"
    );
}

/// Enabling is the flag alone: the deactivated account answers active, its
/// untouched credential works again, its password signs in once more, and the
/// `activate` entry Pleroma's `enable` writes names it.
#[tokio::test]
async fn enabling_a_disabled_account_frees_it_again() {
    let db = common::test_db().await;
    let seeded = seed_writes(&db).await;
    let client = common::create_app(&seeded.pool, "read").await;
    let asleep_token = common::create_app_and_token(&seeded.pool, &seeded.asleep, "read").await;
    let blocked = verify(&seeded.app, &asleep_token).await;
    common::assert_answer(&blocked, StatusCode::FORBIDDEN, "the gate before the write");
    common::assert_error(&blocked, DISABLED, "names the flag as the reason");

    let response = post_json(
        &seeded.app,
        &seeded.token,
        &account_action_path(seeded.asleep.id, "enable"),
        &json!({}),
    )
    .await;
    common::assert_answer(&response, StatusCode::OK, "the enable");
    let body = response.json_value();
    assert_eq!(
        body["id"],
        json!(seeded.asleep.id.to_string()),
        "its entity"
    );
    assert_eq!(body["disabled"], json!(false), "answered active");

    let (active, _, _) = row_state(&seeded.pool, seeded.asleep.id)
        .await
        .expect("the row is there");
    assert!(active, "and the flag moved in the row");
    assert_eq!(
        token_count(&seeded.pool, &seeded.asleep).await,
        1,
        "the credential the deactivation left behind is the whole gate"
    );
    let freed = login(
        &seeded.app,
        &client.client_id,
        &client.client_secret,
        "asleep",
    )
    .await;
    common::assert_answer(&freed, StatusCode::OK, "and the account signs in again");
    let allowed = verify(&seeded.app, &asleep_token).await;
    common::assert_answer(&allowed, StatusCode::OK, "with the token it already had");

    let (message, subjects) = one_entry(&seeded.pool, "activate").await;
    assert_eq!(
        message, "@root activated users: @asleep",
        "Pleroma's activate"
    );
    assert_eq!(subjects, vec!["asleep"], "naming the account");
}

/// Enabling an account that was never disabled is not an error: the same
/// `true` is written, the same entity answers and the same entry is logged.
#[tokio::test]
async fn enabling_an_active_account_answers_it_unchanged() {
    let db = common::test_db().await;
    let seeded = seed_writes(&db).await;

    let response = post_json(
        &seeded.app,
        &seeded.token,
        &account_action_path(seeded.plain.id, "enable"),
        &json!({}),
    )
    .await;
    common::assert_answer(&response, StatusCode::OK, "the enable of an active account");
    assert_eq!(
        response.json_value()["disabled"],
        json!(false),
        "answered as it was"
    );
    let (active, _, _) = row_state(&seeded.pool, seeded.plain.id)
        .await
        .expect("the row is there");
    assert!(active, "and it stayed active");

    let (message, subjects) = one_entry(&seeded.pool, "activate").await;
    assert_eq!(message, "@root activated users: @plain", "still the entry");
    assert_eq!(subjects, vec!["plain"]);
}

/// A mirror is a row like any other: the admin who names its id can enable it,
/// because this schema's activation flag is the one column both kinds share.
#[tokio::test]
async fn the_writes_reach_a_remote_mirror_by_id() {
    let db = common::test_db().await;
    let seeded = seed_writes(&db).await;

    let response = post_json(
        &seeded.app,
        &seeded.token,
        &account_action_path(seeded.mirror.id, "enable"),
        &json!({}),
    )
    .await;
    common::assert_answer(&response, StatusCode::OK, "the enable of a mirror");
    assert_eq!(
        response.json_value()["domain"],
        json!("remote.example"),
        "whose entity answers as the reads answer it"
    );
    let (active, _, _) = row_state(&seeded.pool, seeded.mirror.id)
        .await
        .expect("the mirror is still a row");
    assert!(active, "and its flag is written like a local one's");

    let (message, subjects) = one_entry(&seeded.pool, "activate").await;
    assert_eq!(message, "@root activated users: @faraway@remote.example");
    assert_eq!(
        subjects,
        vec!["faraway@remote.example"],
        "by its full handle"
    );
}

/// Approving a pending registration clears the gate, posts its letter and
/// answers the fresh entity — and the account, which the login gate refused
/// as awaiting approval, signs in on the strength of the flag alone: the
/// approval mints nothing.
#[tokio::test]
async fn approving_a_pending_account_letters_it() {
    let db = common::test_db().await;
    let seeded = seed_writes(&db).await;
    let client = common::create_app(&seeded.pool, "read").await;
    let waiting = login(
        &seeded.app,
        &client.client_id,
        &client.client_secret,
        "newbie",
    )
    .await;
    common::assert_answer(
        &waiting,
        StatusCode::FORBIDDEN,
        "the pending account's sign-in",
    );
    common::assert_error(&waiting, AWAITING, "names the gate that holds it");

    let response = post_json(
        &seeded.app,
        &seeded.token,
        &account_action_path(seeded.newbie.id, "approve"),
        &json!({}),
    )
    .await;
    common::assert_answer(&response, StatusCode::OK, "the approval");
    let body = response.json_value();
    assert_eq!(
        body["id"],
        json!(seeded.newbie.id.to_string()),
        "the entity"
    );
    assert_eq!(body["approved"], json!(true), "answered approved");

    let (active, pending, _) = row_state(&seeded.pool, seeded.newbie.id)
        .await
        .expect("the row is there");
    assert!(!pending, "the staff gate is off the row");
    assert!(active, "and the account was active all along");
    assert_eq!(
        token_count(&seeded.pool, &seeded.newbie).await,
        0,
        "approval mints no credential — Pleroma's approval is a flag, not a password"
    );

    let granted = login(
        &seeded.app,
        &client.client_id,
        &client.client_secret,
        "newbie",
    )
    .await;
    common::assert_answer(
        &granted,
        StatusCode::OK,
        "the approved account signs in at last",
    );
    assert!(
        !granted.json_value()["access_token"].is_null(),
        "with a credential of its own now"
    );

    let posted = letters(&seeded.pool).await;
    assert_eq!(
        posted.len(),
        1,
        "one letter, to the account that just passed"
    );
    assert_eq!(
        posted[0]["to"],
        json!("newbie@example.org"),
        "its own address"
    );
    assert_eq!(
        posted[0]["subject"],
        json!(APPROVED_MAIL),
        "the approval letter"
    );

    let (message, subjects) = one_entry(&seeded.pool, "approve").await;
    assert_eq!(
        message, "@root approved users: @newbie",
        "Pleroma's approve"
    );
    assert_eq!(subjects, vec!["newbie"]);
}

/// An account nobody left pending is Pleroma's already-approved arm: the same
/// entity back, no letter, no write — and still a log subject. So is a pending
/// one that never gave an address: approved, and mailed nothing because there
/// is nothing to mail.
#[tokio::test]
async fn approving_answers_an_approved_account_unchanged_and_letters_an_addressless_one() {
    let db = common::test_db().await;
    let seeded = seed_writes(&db).await;

    let again = post_json(
        &seeded.app,
        &seeded.token,
        &account_action_path(seeded.plain.id, "approve"),
        &json!({}),
    )
    .await;
    common::assert_answer(&again, StatusCode::OK, "approval of the already approved");
    assert_eq!(
        again.json_value()["approved"],
        json!(true),
        "answered unchanged"
    );

    let nameless = post_json(
        &seeded.app,
        &seeded.token,
        &account_action_path(seeded.quiet.id, "approve"),
        &json!({}),
    )
    .await;
    common::assert_answer(&nameless, StatusCode::OK, "approval of the addressless one");
    assert_eq!(
        nameless.json_value()["email"],
        json!(null),
        "and its entity carries the absence it stored"
    );
    let (_, pending, _) = row_state(&seeded.pool, seeded.quiet.id)
        .await
        .expect("the row is there");
    assert!(!pending, "the gate is off it");

    assert!(
        letters(&seeded.pool).await.is_empty(),
        "no letter for an account that was not approved now, and none for one with no address"
    );
    let entries = logged(&seeded.pool, "approve").await;
    let messages: Vec<&str> = entries
        .iter()
        .map(|(message, _)| message.as_str())
        .collect();
    assert_eq!(
        messages,
        vec![
            "@root approved users: @plain",
            "@root approved users: @quiet"
        ],
        "one entry per request, moved or not"
    );
}

/// Rejecting a pending registration deletes the account outright — row and
/// tokens gone — posts its rejection letter and answers the snapshot of the
/// row that is no more.
#[tokio::test]
async fn rejecting_a_pending_account_deletes_it() {
    let db = common::test_db().await;
    let seeded = seed_writes(&db).await;
    let their_token = common::create_app_and_token(&seeded.pool, &seeded.newbie, "read").await;

    let response = post_json(
        &seeded.app,
        &seeded.token,
        &account_action_path(seeded.newbie.id, "reject"),
        &json!({}),
    )
    .await;
    common::assert_answer(&response, StatusCode::OK, "the rejection");
    let body = response.json_value();
    assert_eq!(
        body["id"],
        json!(seeded.newbie.id.to_string()),
        "the snapshot"
    );
    assert_eq!(
        body["approved"],
        json!(false),
        "as it stood: pending, which is what was refused"
    );
    assert_eq!(
        body["email"],
        json!("newbie@example.org"),
        "the address it gave"
    );

    assert!(
        row_state(&seeded.pool, seeded.newbie.id).await.is_none(),
        "the row is gone, not purged or deactivated"
    );
    assert_eq!(
        token_count(&seeded.pool, &seeded.newbie).await,
        0,
        "and its credential left on the cascade"
    );
    let denied = verify(&seeded.app, &their_token).await;
    common::assert_answer(
        &denied,
        StatusCode::UNAUTHORIZED,
        "the credential of a deleted account",
    );
    assert!(
        sweeps_for(&seeded.pool, &seeded.newbie).await.is_empty(),
        "rejection deletes the row outright: there is no sweep to wait for"
    );

    let posted = letters(&seeded.pool).await;
    assert_eq!(posted.len(), 1, "the rejected account is written to");
    assert_eq!(
        posted[0]["to"],
        json!("newbie@example.org"),
        "at its address"
    );
    assert_eq!(
        posted[0]["subject"],
        json!(REJECTED_MAIL),
        "the rejection letter"
    );

    let (message, subjects) = one_entry(&seeded.pool, "reject").await;
    assert_eq!(
        message, "@root rejected users: @newbie",
        "Pleroma's reject sentence"
    );
    assert_eq!(
        subjects,
        vec!["newbie"],
        "naming the row the log can no longer join to"
    );
}

/// Rejection is the pending account's refusal: an approved one — active,
/// deactivated, a mirror — is the 403, and survives the request unchanged.
#[tokio::test]
async fn rejecting_an_account_that_was_never_pending_is_refused() {
    let db = common::test_db().await;
    let seeded = seed_writes(&db).await;
    let token = seeded.token.clone();

    for (who, label) in [
        (seeded.plain.id, "an approved account"),
        (seeded.asleep.id, "a deactivated one"),
        (seeded.mirror.id, "a mirror"),
    ] {
        let response = post_json(
            &seeded.app,
            &token,
            &account_action_path(who, "reject"),
            &json!({}),
        )
        .await;
        common::assert_answer(
            &response,
            StatusCode::FORBIDDEN,
            &format!("rejecting {label}"),
        );
        common::assert_error(&response, NOT_ALLOWED, "in the pinned words");
        assert!(
            row_state(&seeded.pool, who).await.is_some(),
            "{label} is still a row"
        );
    }
    assert!(
        letters(&seeded.pool).await.is_empty(),
        "and no refusal posted a letter"
    );
    assert!(
        logged(&seeded.pool, "reject").await.is_empty(),
        "nor left an entry"
    );
}

/// `disable` is the deactivation and nothing more: the flag moves, the row is
/// not stamped for deletion, the credential survives (refused, but live), and
/// the request leaves both the `deactivate` entry of the move and the `disable`
/// entry of the ask.
#[tokio::test]
async fn the_action_disables_the_account_it_is_pointed_at() {
    let db = common::test_db().await;
    let seeded = seed_writes(&db).await;
    let mine = common::create_app_and_token(&seeded.pool, &seeded.plain, "read").await;
    let client = common::create_app(&seeded.pool, "read").await;

    act(
        &seeded.app,
        &seeded.token,
        seeded.plain.id,
        &json!({"type": "disable"}),
    )
    .await;

    let (active, _, doomed) = row_state(&seeded.pool, seeded.plain.id)
        .await
        .expect("the row is there");
    assert!(!active, "the flag moved");
    assert!(
        !doomed,
        "and no deletion was stamped — disable is not delete"
    );
    assert_eq!(
        token_count(&seeded.pool, &seeded.plain).await,
        1,
        "the token is left in place, refused until the account comes back"
    );
    let denied = verify(&seeded.app, &mine).await;
    common::assert_answer(&denied, StatusCode::FORBIDDEN, "its bearer's request");
    common::assert_error(&denied, DISABLED, "is the disabled refusal");
    let refused = login(
        &seeded.app,
        &client.client_id,
        &client.client_secret,
        "plain",
    )
    .await;
    common::assert_answer(&refused, StatusCode::FORBIDDEN, "and its sign-in");
    assert_eq!(
        refused.json_value()["identifier"],
        json!("account_is_disabled"),
        "with the grant's own reason code"
    );
    assert!(
        sweeps_for(&seeded.pool, &seeded.plain).await.is_empty(),
        "and no content sweep was queued"
    );

    let (message, subjects) = one_entry(&seeded.pool, "deactivate").await;
    assert_eq!(
        message, "@root deactivated users: @plain",
        "the move keeps the entry every other writer of the flag leaves"
    );
    assert_eq!(subjects, vec!["plain"]);
    let (asked, asked_subjects) = one_entry(&seeded.pool, "disable").await;
    assert_eq!(
        asked, "@root performed action disable",
        "and the request is recorded under the type it stated"
    );
    assert_eq!(asked_subjects, vec!["plain"], "naming its account");
}

/// `suspend` deactivates and queues the `users.delete` sweep, answering 200
/// `{}`; the three warning parameters the client sent alongside are dropped
/// without a trace, and the two entries name the move and the ask.
#[tokio::test]
async fn the_action_suspends_and_queues_the_content_sweep() {
    let db = common::test_db().await;
    let seeded = seed_writes(&db).await;
    let mine = common::create_app_and_token(&seeded.pool, &seeded.plain, "read").await;

    act(
        &seeded.app,
        &seeded.token,
        seeded.plain.id,
        &json!({
            "type": "suspend",
            "text": "read the rules",
            "warning_preset_id": "7",
            "send_email_notification": true,
        }),
    )
    .await;

    let (active, _, doomed) = row_state(&seeded.pool, seeded.plain.id)
        .await
        .expect("the row is still there for the sweep to work on");
    assert!(!active, "suspension deactivates");
    assert!(
        !doomed,
        "and queues the sweep instead of stamping the deletion itself"
    );
    let queued = sweeps_for(&seeded.pool, &seeded.plain).await;
    assert_eq!(queued.len(), 1, "the content sweep is on the queue");
    assert_eq!(
        queued[0]["user_id"],
        json!(seeded.plain.id.to_string()),
        "and names the account it will clear"
    );
    let denied = verify(&seeded.app, &mine).await;
    common::assert_answer(
        &denied,
        StatusCode::FORBIDDEN,
        "the suspended account's own request",
    );
    common::assert_error(&denied, DISABLED, "is refused as disabled");
    assert_eq!(
        token_count(&seeded.pool, &seeded.plain).await,
        1,
        "the row's own credential is the sweep's business, not this request's"
    );
    assert!(
        letters(&seeded.pool).await.is_empty(),
        "`send_email_notification` is accepted and ignored: there is no warning letter to send"
    );

    let (message, subjects) = one_entry(&seeded.pool, "deactivate").await;
    assert_eq!(
        message, "@root deactivated users: @plain",
        "the suspension's flag move is recorded as the deactivation it is"
    );
    assert_eq!(subjects, vec!["plain"]);
    let (asked, asked_subjects) = one_entry(&seeded.pool, "suspend").await;
    assert_eq!(asked, "@root performed action suspend");
    assert_eq!(
        asked_subjects,
        vec!["plain"],
        "and the ask is recorded by its type"
    );
}

/// `none`, `sensitive` and `silence` are the types with nothing under them:
/// this schema has no sensitive flag and no silence state, so each is the log
/// entry alone, and the account is left exactly as it stood.
#[tokio::test]
async fn the_unmodelled_action_types_are_recorded_alone() {
    let db = common::test_db().await;
    let seeded = seed_writes(&db).await;

    for kind in ["none", "sensitive", "silence"] {
        act(
            &seeded.app,
            &seeded.token,
            seeded.plain.id,
            &json!({"type": kind}),
        )
        .await;
        let (active, pending, doomed) = row_state(&seeded.pool, seeded.plain.id)
            .await
            .expect("the row is there");
        assert!(
            active && !pending && !doomed,
            "`{kind}` changed nothing on the account"
        );
        let (message, subjects) = one_entry(&seeded.pool, kind).await;
        assert_eq!(
            message,
            format!("@root performed action {kind}"),
            "Pleroma's fall-through sentence"
        );
        assert_eq!(subjects, vec!["plain"], "and the account it names");
    }

    assert!(
        logged(&seeded.pool, "deactivate").await.is_empty(),
        "and no type that moves nothing left a deactivation entry"
    );
    assert_eq!(
        logged(&seeded.pool, "none").await.len(),
        1,
        "one entry per request"
    );
}

/// A `report_id` on an action request is resolved as part of it: the row moves
/// to `resolved`, its action stamp lands, and the `report_update` entry and the
/// `report.updated` event are the ones every other state change writes.
#[tokio::test]
async fn the_action_resolves_the_report_it_names() {
    let db = common::test_db().await;
    let seeded = seed_writes(&db).await;
    let report = file_against(&seeded.pool, &seeded.beeper, &seeded.plain).await;
    assert_eq!(
        report_state(&seeded.pool, report).await,
        ("open".to_owned(), false),
        "the fixture report is filed open"
    );

    act(
        &seeded.app,
        &seeded.token,
        seeded.plain.id,
        &json!({"type": "disable", "report_id": report.to_string()}),
    )
    .await;

    assert_eq!(
        report_state(&seeded.pool, report).await,
        ("resolved".to_owned(), true),
        "resolved, with its action stamp"
    );
    let entries = logged(&seeded.pool, "report_update").await;
    assert_eq!(entries.len(), 1, "one resolution entry for one report");
    assert_eq!(
        entries[0].0,
        format!("@root updated report ##{report} ( on user @plain) with 'resolved' state"),
        "the same sentence the console's state change writes, naming its target"
    );
    assert_eq!(
        entries[0].1["subject"]["id"],
        json!(report.to_string()),
        "whose subject is the report itself, Pleroma's report map"
    );
    let (asked, _) = one_entry(&seeded.pool, "disable").await;
    assert_eq!(
        asked, "@root performed action disable",
        "and the action itself is recorded beside the resolution"
    );
    let (moved, _) = one_entry(&seeded.pool, "deactivate").await;
    assert_eq!(
        moved, "@root deactivated users: @plain",
        "with the flag move"
    );
}

/// Pleroma's `associated_report` guard comes with the umbrella: an id that
/// names no report is the 404 the account half gives, and a report about
/// somebody else the 400 that says so — both before anything moves.
#[tokio::test]
async fn the_action_refuses_a_report_it_does_not_belong_to() {
    let db = common::test_db().await;
    let seeded = seed_writes(&db).await;
    let theirs = file_against(&seeded.pool, &seeded.beeper, &seeded.asleep).await;
    let token = seeded.token.clone();

    let unknown = post_json(
        &seeded.app,
        &token,
        &account_action_path(seeded.plain.id, "action"),
        &json!({"type": "disable", "report_id": Uuid::now_v7().to_string()}),
    )
    .await;
    common::assert_answer(
        &unknown,
        StatusCode::NOT_FOUND,
        "a report that names nothing",
    );
    common::assert_error(&unknown, NOT_FOUND, "is the shared 404 body");

    let malformed = post_json(
        &seeded.app,
        &token,
        &account_action_path(seeded.plain.id, "action"),
        &json!({"type": "disable", "report_id": "nope"}),
    )
    .await;
    common::assert_answer(
        &malformed,
        StatusCode::NOT_FOUND,
        "text that is no report id",
    );

    let stranger = post_json(
        &seeded.app,
        &token,
        &account_action_path(seeded.plain.id, "action"),
        &json!({"type": "disable", "report_id": theirs.to_string()}),
    )
    .await;
    common::assert_answer(
        &stranger,
        StatusCode::BAD_REQUEST,
        "a report about another account",
    );
    common::assert_error(&stranger, WRONG_TARGET, "says so in Pleroma's words");

    let (active, _, _) = row_state(&seeded.pool, seeded.plain.id)
        .await
        .expect("the row is there");
    assert!(active, "and the refused request touched nothing");
    assert_eq!(
        report_state(&seeded.pool, theirs).await,
        ("open".to_owned(), false),
        "least of all somebody else's report"
    );
    assert!(
        logged(&seeded.pool, "report_update").await.is_empty(),
        "and no resolution was recorded"
    );
    assert!(
        logged(&seeded.pool, "deactivate").await.is_empty()
            && logged(&seeded.pool, "disable").await.is_empty(),
        "nor the action's own entries"
    );
}

/// A `type` off the five words, and no `type` at all, are the two 422s the
/// endpoint answers a request it cannot name an action for — with the exact
/// bodies the ticket pins, and no effect on the account.
#[tokio::test]
async fn the_action_refuses_an_unusable_type() {
    let db = common::test_db().await;
    let seeded = seed_writes(&db).await;
    let token = seeded.token.clone();
    let path = account_action_path(seeded.plain.id, "action");

    let invalid = post_json(&seeded.app, &token, &path, &json!({"type": "freeze"})).await;
    common::assert_answer(
        &invalid,
        StatusCode::UNPROCESSABLE_ENTITY,
        "an unknown type",
    );
    common::assert_error(
        &invalid,
        INVALID_TYPE,
        "Pleroma's enum refusal, spelled as it is",
    );

    let wrong_case = post_json(&seeded.app, &token, &path, &json!({"type": "Disable"})).await;
    common::assert_answer(
        &wrong_case,
        StatusCode::UNPROCESSABLE_ENTITY,
        "the wrong case",
    );
    common::assert_error(&wrong_case, INVALID_TYPE, "is no more a type");

    let absent = post_json(&seeded.app, &token, &path, &json!({})).await;
    common::assert_answer(&absent, StatusCode::UNPROCESSABLE_ENTITY, "no type stated");
    common::assert_error(&absent, MISSING_TYPE, "is the required-key refusal");

    let number = post_json(&seeded.app, &token, &path, &json!({"type": 4})).await;
    common::assert_answer(
        &number,
        StatusCode::UNPROCESSABLE_ENTITY,
        "a type that is no text",
    );

    let (active, _, _) = row_state(&seeded.pool, seeded.plain.id)
        .await
        .expect("the row is there");
    assert!(active, "and none of the four refusals moved anything");
    for action in ["none", "disable", "suspend", "deactivate", "report_update"] {
        assert!(
            logged(&seeded.pool, action).await.is_empty(),
            "nor left a {action} entry"
        );
    }
}

/// Decision 10: the same request arrives urlencoded, with the ignored keys
/// beside the two that matter, and is answered the same — the umbrella's `type`
/// reaching the endpoint as form text.
#[tokio::test]
async fn the_action_reads_a_form_body() {
    let db = common::test_db().await;
    let seeded = seed_writes(&db).await;
    let fields = [
        ("type", "silence"),
        ("text", "shush"),
        ("warning_preset_id", "3"),
        ("send_email_notification", "true"),
    ];

    let response = TestRequest::post(&account_action_path(seeded.plain.id, "action"))
        .bearer(&seeded.token)
        .form(&fields)
        .send(&seeded.app)
        .await;
    common::assert_answer(&response, StatusCode::OK, "the form action");
    assert_eq!(response.json_value(), json!({}), "the same empty object");

    let (active, _, _) = row_state(&seeded.pool, seeded.plain.id)
        .await
        .expect("the row is there");
    assert!(active, "and `silence` silenced nothing but the request");
    let (message, subjects) = one_entry(&seeded.pool, "silence").await;
    assert_eq!(
        message, "@root performed action silence",
        "the form's type arrived as the umbrella's own word"
    );
    assert_eq!(subjects, vec!["plain"]);
}

/// Sends the write `suffix` names for account `id`: the `DELETE` of the
/// account itself for the empty suffix, a body-less `POST` for the three
/// state routes and the umbrella `POST` for `action`. `token` is `None` for
/// the anonymous request.
async fn write_request(
    app: &Router,
    token: Option<&str>,
    id: Uuid,
    suffix: &str,
) -> common::TestResponse {
    let path = if suffix.is_empty() {
        account_path(id)
    } else {
        account_action_path(id, suffix)
    };
    let body = if suffix == "action" {
        json!({"type": "none"})
    } else {
        json!({})
    };
    match (suffix.is_empty(), token) {
        (true, None) => TestRequest::delete(&path).send(app).await,
        (true, Some(text)) => TestRequest::delete(&path).bearer(text).send(app).await,
        (false, None) => TestRequest::post(&path).json(&body).send(app).await,
        (false, Some(text)) => {
            TestRequest::post(&path)
                .bearer(text)
                .json(&body)
                .send(app)
                .await
        }
    }
}

/// The five writes are an admin's: no token, an unknown one and an app-only
/// one are the shared refusals on every route, and a moderator — who reads
/// this surface — is told they are not an admin, as is a plain account's
/// token, however well scoped.
#[tokio::test]
async fn the_writes_demand_an_admin_user() {
    let db = common::test_db().await;
    let seeded = seed_writes(&db).await;
    let pool = seeded.pool.clone();
    let moddy = create(&pool, "moddy", |user| NewUser {
        is_moderator: Flag::new(true),
        ..user
    })
    .await;
    let moderator = common::create_app_and_token(&pool, &moddy, "admin:write").await;
    let stranger = common::create_app_and_token(&pool, &seeded.plain, "admin:write").await;
    let app_only = common::create_app_only_token(&pool, "admin:write").await;
    let id = seeded.plain.id;

    for suffix in ["", "enable", "approve", "reject", "action"] {
        let anonymous = write_request(&seeded.app, None, id, suffix).await;
        common::assert_answer(&anonymous, StatusCode::UNAUTHORIZED, "no token at all");
        common::assert_error(
            &anonymous,
            NO_TOKEN,
            &format!("the route's suffix {suffix:?}"),
        );

        let bogus = write_request(&seeded.app, Some("nope"), id, suffix).await;
        common::assert_answer(
            &bogus,
            StatusCode::UNAUTHORIZED,
            "a token that resolves nowhere",
        );

        let unbound = write_request(&seeded.app, Some(&app_only), id, suffix).await;
        common::assert_answer(&unbound, StatusCode::FORBIDDEN, "an app-only token");
        common::assert_error(&unbound, USER_REQUIRED, "asks for a user");

        for (credential, label) in [(&moderator, "a moderator"), (&stranger, "a plain account")] {
            let outsider = write_request(&seeded.app, Some(credential), id, suffix).await;
            common::assert_answer(
                &outsider,
                StatusCode::FORBIDDEN,
                &format!("{label} on {suffix:?}"),
            );
            common::assert_error(&outsider, NOT_ADMIN, "is refused as not an admin");
        }
    }

    let (active, _, _) = row_state(&pool, id).await.expect("the row is there");
    assert!(active, "and every refusal left the flag alone");
    let (_, pending, _) = row_state(&pool, seeded.newbie.id)
        .await
        .expect("the pending row is there");
    assert!(pending, "nor did any of them approve or reject anything");
    assert!(
        logged(&pool, "delete").await.is_empty() && logged(&pool, "activate").await.is_empty(),
        "and no refusal wrote the log"
    );
}

/// The scope half of the gate is `admin:write:accounts`: the class root and
/// the area scope both pass on all five routes, and another area, the read
/// class, an ordinary `write` or no scope at all refuse with the class root
/// named.
#[tokio::test]
async fn the_writes_demand_the_accounts_write_scope() {
    let db = common::test_db().await;
    let seeded = seed_writes(&db).await;
    let pool = seeded.pool.clone();

    let granted = common::create_app_and_token(&pool, &seeded.root, "admin:write").await;
    for (suffix, target, label) in [
        ("", seeded.plain.id, "the delete"),
        ("enable", seeded.asleep.id, "the enable"),
        ("approve", seeded.newbie.id, "the approve"),
        ("reject", seeded.quiet.id, "the reject"),
        ("action", seeded.asleep.id, "the umbrella"),
    ] {
        let answered = write_request(&seeded.app, Some(&granted), target, suffix).await;
        common::assert_answer(
            &answered,
            StatusCode::OK,
            &format!("the `admin:write` root on {label}"),
        );
    }
    let scoped = common::create_app_and_token(&pool, &seeded.root, "admin:write:accounts").await;
    let answered = write_request(&seeded.app, Some(&scoped), seeded.asleep.id, "enable").await;
    common::assert_answer(
        &answered,
        StatusCode::OK,
        "the area scope on the enable route",
    );

    for scopes in [
        "admin:write:reports",
        "admin:read:accounts",
        "admin:read",
        "write",
        "",
    ] {
        let token = common::create_app_and_token(&pool, &seeded.root, scopes).await;
        for suffix in ["", "enable", "approve", "reject", "action"] {
            let refused = write_request(&seeded.app, Some(&token), seeded.asleep.id, suffix).await;
            common::assert_answer(
                &refused,
                StatusCode::FORBIDDEN,
                &format!("`{scopes}` on {suffix:?}"),
            );
            common::assert_error(
                &refused,
                NO_ADMIN_WRITE,
                &format!("`{scopes}` names the class root"),
            );
        }
    }
}
