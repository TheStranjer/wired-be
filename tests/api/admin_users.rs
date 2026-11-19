//! The admin users endpoints over HTTP (`/api/v1/pleroma/admin/users`):
//! Pleroma's `AdminAPI.UserController` `index` and `show`, the eight
//! `filters`, the `query`/`name`/`email`/`tags[]`/`actor_types[]` criteria,
//! the `page`/`page_size` window with its `count`, the admin account
//! entity, and the `admin:read:accounts` gate in front of all of it
//! (WBE-195); and beside them the credentials, security and follow
//! maintenance of WBE-198 — the `"credentials.json"` read, the changeset
//! PATCH with its 400 `{"errors": …}` field map, the issued and forced
//! password resets, the confirmation pair, the MFA removal and the forced
//! follow, every write gated `admin:write:accounts` and every one of them
//! (except the token mint) landing its moderation-log entry.
//!
//! The shared fixture seeds one account per state a filter weighs — an
//! admin, a moderator, a plain active account, a deactivated one, one
//! awaiting approval, one awaiting its email check, a tagged `Service` and
//! a remote mirror — beside the instance actor, which the listing must
//! never answer with. Every test reads the same rows through the real
//! router, so the order the ticket pins, the total that ignores the window
//! and the entity's fifteen keys are asserted against stored facts rather
//! than a hand-built row.

#![cfg(test)]

use axum::{Router, http::StatusCode};
use serde_json::{Value, json};
use sqlx::PgPool;
use wired_be::{
    config::TEST_BASE_URL,
    models::user::{Flag, NewUser, User},
    services::instance_actor::ensure_instance_actor,
    test_db::TestDb,
    util::time::mastodon_time,
};

use crate::common::{self, TestRequest};

/// The listing route.
const USERS: &str = "/api/v1/pleroma/admin/users";
/// The invites listing, whose static route shares the `/users` prefix with
/// the show route's `{nickname}` segment.
const INVITES: &str = "/api/v1/pleroma/admin/users/invites";
/// The 401 body when no token is presented.
const NO_TOKEN: &str = "Invalid credentials";
/// The 401 body when a presented token does not resolve.
const INVALID_TOKEN: &str = "The access token is invalid";
/// The 403 body for an app-only (user-unbound) token.
const USER_REQUIRED: &str = "This API requires an authenticated user";
/// The 403 body for a valid token whose user is not an admin.
const NOT_ADMIN: &str = "User is not an admin.";
/// The 403 body of a token that grants neither `admin:read` nor
/// `admin:read:accounts`.
const NO_ADMIN_READ: &str = "Insufficient permissions: admin:read.";
/// The 404 body of the admin API's misses.
const NOT_FOUND: &str = "Not found";
/// The frontend's default avatar, which an account without one renders.
const DEFAULT_AVATAR: &str = "/images/avi.png";
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

/// What the fixture leaves behind for one test.
struct Seeded {
    /// The pool every row was inserted into.
    pool: PgPool,
    /// The router built on that pool.
    app: Router,
    /// The admin account, whose tokens the tests mint.
    root: User,
    /// The remote mirror, whose handle the show route must resolve.
    mirror: User,
    /// The instance actor, which the listing hides and the show finds.
    actor: User,
    /// A bearer token for `root` granting the accounts read scope.
    token: String,
}

/// Seeds one account per state the filters weigh, plus the instance actor,
/// and returns them beside the admin who reads them and the app to read
/// them through.
///
/// The `db` slot a test leases must outlive this: the test keeps its own
/// binding, as every test here does.
/// Inserts a local account named `nickname` with `customize` applied on top
/// of [`NewUser::local`], and spends the row: the tests below read these
/// accounts back through the API, not from Rust.
async fn seeded<F>(pool: &PgPool, nickname: &str, customize: F)
where
    F: FnOnce(NewUser) -> NewUser,
{
    let _row: User = common::create_local_user_with(pool, nickname, customize).await;
}

async fn seed(db: &TestDb) -> Seeded {
    let pool = db.pool().clone();
    let root = common::create_local_user_with(&pool, "root", |user| NewUser {
        is_admin: Flag::new(true),
        email: Some("root@example.org".to_owned()),
        ..user
    })
    .await;
    seeded(&pool, "moddy", |user| NewUser {
        is_moderator: Flag::new(true),
        ..user
    })
    .await;
    seeded(&pool, "plain", |user| NewUser {
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
        avatar_url: Some(format!("{TEST_BASE_URL}/uploads/bot.png")),
        ..user
    })
    .await;
    let mirror = common::create_remote_user(&pool, "faraway", "remote.example").await;
    let actor = ensure_instance_actor(&pool, &common::test_config())
        .await
        .expect("the instance actor is seeded");
    let token = common::create_app_and_token(&pool, &root, "admin:read:accounts").await;
    Seeded {
        pool,
        app: common::test_app(db.pool().clone()),
        root,
        mirror,
        actor,
        token,
    }
}

/// The nicknames of one listing answer, in the order it answers them.
fn listed(body: &Value) -> Vec<&str> {
    body["users"]
        .as_array()
        .expect("the users array")
        .iter()
        .filter_map(|found| found["nickname"].as_str())
        .collect()
}

/// `GET`s `path` as `token` and returns the parsed body, asserting the 200
/// an admin read answers with.
async fn read(app: &Router, token: &str, path: &str) -> Value {
    let response = TestRequest::get(path).bearer(token).send(app).await;
    common::assert_answer(&response, StatusCode::OK, "an admin read");
    response.json_value()
}

/// The listing of `query` (an empty string is the bare route) as `token`.
async fn page(app: &Router, token: &str, query: &str) -> Value {
    let path = if query.is_empty() {
        USERS.to_owned()
    } else {
        format!("{USERS}?{query}")
    };
    read(app, token, &path).await
}

/// The show route of one path segment.
fn show(segment: &str) -> String {
    format!("{USERS}/{segment}")
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

/// The show endpoint answers the admin entity: the fifteen keys the ticket
/// pins, the moderation columns verbatim, the roles object, the defaults
/// for the unset ones, and the stored timestamp in the Mastodon format.
#[tokio::test]
async fn the_show_renders_the_admin_account_entity() {
    let db = common::test_db().await;
    let seeded = seed(&db).await;
    let config = common::test_config();
    let entity = read(&seeded.app, &seeded.token, &show("root")).await;
    assert_eq!(
        entity,
        json!({
            "id": seeded.root.id.to_string(),
            "email": "root@example.org",
            "avatar": format!("{}{DEFAULT_AVATAR}", config.frontend_base_url),
            "nickname": "root",
            "display_name": "root",
            "is_active": true,
            "local": true,
            "roles": { "admin": true, "moderator": false },
            "tags": [],
            "is_confirmed": true,
            "is_approved": true,
            "is_suggested": false,
            "url": format!("{TEST_BASE_URL}/users/root"),
            "registration_reason": null,
            "actor_type": "Person",
            "created_at": mastodon_time(&seeded.root.inserted_at),
        }),
        "no counters, no note, nothing else"
    );

    let listed = page(&seeded.app, &seeded.token, "query=beeper").await;
    let bot = &listed["users"].as_array().expect("one account")[0];
    assert_eq!(
        bot["avatar"],
        json!(format!("{TEST_BASE_URL}/uploads/bot.png")),
        "a local account's stored avatar passes through"
    );
    assert_eq!(bot["actor_type"], json!("Service"), "and its actor type");
    assert_eq!(bot["tags"], json!(["bots"]), "and its profile tags");

    let mirror = read(&seeded.app, &seeded.token, &show("faraway@remote.example")).await;
    assert_eq!(mirror["local"], json!(false), "the mirror is not ours");
    assert_eq!(
        mirror["url"],
        json!(seeded.mirror.ap_id),
        "so its ap_id is its url"
    );
    assert_eq!(
        mirror["nickname"],
        json!("faraway@remote.example"),
        "the stored handle"
    );
    assert_eq!(mirror["display_name"], json!("faraway"), "its name");
    assert_eq!(mirror["email"], Value::Null, "and no email at all");
}

/// The show route resolves the path segment as a nickname (citext, so in
/// any case), as a remote handle written plainly or percent-encoded, and as
/// the account's id.
#[tokio::test]
async fn the_show_route_resolves_a_handle_or_an_id() {
    let db = common::test_db().await;
    let seeded = seed(&db).await;
    let plain = read(&seeded.app, &seeded.token, &show("PLAIN")).await;
    assert_eq!(plain["nickname"], json!("plain"), "citext folds the case");
    let handle = read(&seeded.app, &seeded.token, &show("faraway@remote.example")).await;
    let encoded = read(
        &seeded.app,
        &seeded.token,
        &show("faraway%40remote.example"),
    )
    .await;
    assert_eq!(
        encoded, handle,
        "the encoded and the plain handle are the same segment"
    );
    assert_eq!(
        handle["nickname"],
        json!("faraway@remote.example"),
        "and both find the mirror"
    );
    let by_id = read(
        &seeded.app,
        &seeded.token,
        &show(&seeded.mirror.id.to_string()),
    )
    .await;
    assert_eq!(
        by_id["nickname"],
        json!("faraway@remote.example"),
        "an id reaches the same row"
    );
    let by_local_id = read(
        &seeded.app,
        &seeded.token,
        &show(&seeded.root.id.to_string()),
    )
    .await;
    assert_eq!(
        by_local_id["nickname"],
        json!("root"),
        "local accounts included"
    );
}

/// An account the listing hides is still readable by name, and text that is
/// no nickname and no id — including a malformed one — is the 404
/// `{"error":"Not found"}`.
#[tokio::test]
async fn the_show_route_reads_the_hidden_actor_and_refuses_a_miss() {
    let db = common::test_db().await;
    let seeded = seed(&db).await;
    let actor = read(&seeded.app, &seeded.token, &show("internal.fetch")).await;
    assert_eq!(
        actor["nickname"],
        json!("internal.fetch"),
        "naming the instance actor is a lookup, not a listing"
    );
    assert_eq!(
        actor["display_name"],
        json!(seeded.actor.name),
        "and it renders like any other row, named after the instance"
    );
    assert_eq!(
        actor["url"],
        json!(seeded.actor.ap_id),
        "with its own actor id as its url"
    );
    assert_eq!(
        actor["actor_type"],
        json!("Application"),
        "and its Application type"
    );

    for segment in ["nobody", "01934567", "not-a-uuid-at-all"] {
        let response = TestRequest::get(&show(segment))
            .bearer(&seeded.token)
            .send(&seeded.app)
            .await;
        common::assert_answer(
            &response,
            StatusCode::NOT_FOUND,
            "a segment that names nobody",
        );
        common::assert_error(&response, NOT_FOUND, "a segment that names nobody");
    }
    let empty = TestRequest::get(USERS)
        .bearer(&seeded.token)
        .send(&seeded.app)
        .await;
    common::assert_answer(
        &empty,
        StatusCode::OK,
        "the bare listing route is not a show",
    );
}

/// The gate: no token is 401, an unusable one 401, an app-only token 403
/// naming the missing user, and a valid non-admin token — a moderator
/// included — 403 `{"error":"User is not an admin."}`. The role is checked
/// before the scope, on both routes.
#[tokio::test]
async fn only_an_admin_reads_the_users_api() {
    let db = common::test_db().await;
    let seeded = seed(&db).await;
    let moddy = common::create_local_user_with(&seeded.pool, "moddy2", |user| NewUser {
        is_moderator: Flag::new(true),
        ..user
    })
    .await;
    let moddy_token = mint(&seeded.pool, &moddy, "admin:read:accounts").await;
    let regular = common::create_local_user(&seeded.pool, "gawker").await;
    let regular_token = common::create_app_and_token(&seeded.pool, &regular, "admin:read").await;
    let app_only = common::create_app_only_token(&seeded.pool, "admin:read:accounts").await;

    for path in [USERS.to_owned(), show("root")] {
        let anonymous = TestRequest::get(&path).send(&seeded.app).await;
        common::assert_answer(&anonymous, StatusCode::UNAUTHORIZED, "no token");
        common::assert_error(&anonymous, NO_TOKEN, "no token");

        let forged = TestRequest::get(&path)
            .bearer("no-such-token")
            .send(&seeded.app)
            .await;
        common::assert_answer(&forged, StatusCode::UNAUTHORIZED, "an unknown token");
        common::assert_error(&forged, INVALID_TOKEN, "an unknown token");

        let unbound = TestRequest::get(&path)
            .bearer(&app_only)
            .send(&seeded.app)
            .await;
        common::assert_answer(&unbound, StatusCode::FORBIDDEN, "an app-only token");
        common::assert_error(&unbound, USER_REQUIRED, "an app-only token");

        let moderator = TestRequest::get(&path)
            .bearer(&moddy_token)
            .send(&seeded.app)
            .await;
        common::assert_answer(&moderator, StatusCode::FORBIDDEN, "a moderator");
        common::assert_error(&moderator, NOT_ADMIN, "a moderator");

        let outsider = TestRequest::get(&path)
            .bearer(&regular_token)
            .send(&seeded.app)
            .await;
        common::assert_answer(&outsider, StatusCode::FORBIDDEN, "a regular user");
        common::assert_error(&outsider, NOT_ADMIN, "a regular user");
    }
}

/// The scope half of the gate: `admin:read` and `admin:read:accounts` both
/// grant the two reads, and every other grant — another admin area, the
/// write class, an ordinary scope — is the 403 naming `admin:read`.
#[tokio::test]
async fn the_accounts_read_scope_grants_both_endpoints() {
    let db = common::test_db().await;
    let seeded = seed(&db).await;
    let granted = [
        ("broad root", "admin:read"),
        ("the area scope", "admin:read:accounts"),
    ];
    let refused = [
        ("another admin area", "admin:read:reports"),
        ("the write class", "admin:write:accounts"),
        ("an ordinary scope", "read"),
        ("nothing at all", ""),
    ];
    for (what, scopes) in granted {
        let token = mint(&seeded.pool, &seeded.root, scopes).await;
        for path in [USERS.to_owned(), show("plain")] {
            let response = TestRequest::get(&path)
                .bearer(&token)
                .send(&seeded.app)
                .await;
            common::assert_answer(&response, StatusCode::OK, &format!("{what} grants {path}"));
        }
    }
    for (what, scopes) in refused {
        let token = mint(&seeded.pool, &seeded.root, scopes).await;
        for path in [USERS.to_owned(), show("plain")] {
            let response = TestRequest::get(&path)
                .bearer(&token)
                .send(&seeded.app)
                .await;
            common::assert_answer(
                &response,
                StatusCode::FORBIDDEN,
                &format!("{what} must not grant {path}"),
            );
            common::assert_error(&response, NO_ADMIN_READ, what);
        }
    }
}

/// The `/users` prefix holds static invite routes beside the show route's
/// `{nickname}` segment: the router prefers the static path, so the invites
/// listing still answers its own `{"invites": …}` key, and an accounts-scoped
/// token is refused by the *invites* scope gate (403) rather than by a show
/// lookup that found no account named `invites` (which would have been 404).
#[tokio::test]
async fn the_static_invite_routes_keep_their_prefix() {
    let db = common::test_db().await;
    let seeded = seed(&db).await;
    let invites_token = mint(&seeded.pool, &seeded.root, "admin:read:invites").await;
    let invites = read(&seeded.app, &invites_token, INVITES).await;
    assert_eq!(
        invites,
        json!({ "invites": [] }),
        "the invites listing answers its own key, not the show entity"
    );
    let wrong_area = TestRequest::get(INVITES)
        .bearer(&seeded.token)
        .send(&seeded.app)
        .await;
    common::assert_answer(
        &wrong_area,
        StatusCode::FORBIDDEN,
        "the accounts scope is not the invites scope",
    );
    common::assert_error(
        &wrong_area,
        NO_ADMIN_READ,
        "the accounts scope is not invites",
    );
}

/// A bearer token for `user` granting `scopes`.
async fn mint(pool: &PgPool, user: &User, scopes: &str) -> String {
    common::create_app_and_token(pool, user, scopes).await
}

// ===== WBE-198: the credentials, security and follow maintenance =====

/// The credentials routes of one account.
fn credentials(nickname: &str) -> String {
    format!("{USERS}/{nickname}/credentials")
}

/// The permission-group read of one account.
fn group(nickname: &str) -> String {
    format!("{USERS}/{nickname}/permission_group")
}

/// The permission-group read of one account, naming one group.
fn group_of(nickname: &str, name: &str) -> String {
    format!("{}/{name}", group(nickname))
}

/// The nickname-list permission-group route for one group.
fn group_bulk(name: &str) -> String {
    format!("{USERS}/permission_group/{name}")
}

/// The reset-token route of one account.
fn issued_reset(nickname: &str) -> String {
    format!("{USERS}/{nickname}/password_reset")
}

/// The forced-reset PATCH route.
const FORCE_RESET: &str = "/api/v1/pleroma/admin/users/password_reset";
/// Pleroma's own path for the forced reset, served by the same handler.
const FORCE_RESET_PLEROMA: &str = "/api/v1/pleroma/admin/users/force_password_reset";
/// The email-confirmation PATCH route.
const CONFIRM: &str = "/api/v1/pleroma/admin/users/confirm_email";
/// The confirmation-resend PATCH route.
const RESEND: &str = "/api/v1/pleroma/admin/users/resend_confirmation_email";
/// The MFA-removal PUT route.
const DISABLE_MFA: &str = "/api/v1/pleroma/admin/users/disable_mfa";
/// The forced-follow POST route.
const FOLLOW: &str = "/api/v1/pleroma/admin/users/follow";
/// The forced-unfollow POST route.
const UNFOLLOW: &str = "/api/v1/pleroma/admin/users/unfollow";
/// The tag pair's route, `PUT` and `DELETE` on one path (WBE-197).
const TAG: &str = "/api/v1/pleroma/admin/users/tag";
/// The suggestion-pool add route.
const SUGGEST: &str = "/api/v1/pleroma/admin/users/suggest";
/// The suggestion-pool remove route.
const UNSUGGEST: &str = "/api/v1/pleroma/admin/users/unsuggest";
/// The completion form the issued link points at.
const RESET_FORM: &str = "/api/v1/pleroma/password_reset";
/// The 403 body of a token that grants neither `admin:write` nor
/// `admin:write:accounts`.
const NO_ADMIN_WRITE: &str = "Insufficient permissions: admin:write.";
/// The 403 `error` of the password grant refused to a forced-reset account.
const RESET_REQUIRED: &str = "Password reset is required";
/// The 404 body of a `permission_group` segment that names no group.
const NO_SUCH_GROUP: &str = "No such permission_group";
/// The 400 body of a role write aimed at a remote mirror.
const REMOTE_ROLE: &str = "Can't set role for a remote user";
/// The 403 body of the admin who revokes a group from their own account.
const SELF_REVOKE: &str = "You can't revoke your own admin status.";

/// A config with `SMTP_HOST` set, so `confirmation::send` really queues
/// the letters the resend tests count (the suite default configures no
/// relay and every send would drop).
fn mail_config() -> wired_be::config::Config {
    let mut config = common::test_config();
    config.smtp_host = Some("smtp.example.org".to_owned());
    config
}

/// The write-scoped token of the freshly seeded fixture's admin.
async fn write_token(seeded: &Seeded) -> String {
    mint(&seeded.pool, &seeded.root, "admin:write:accounts").await
}

/// A `PATCH` of `path` as `token` with the JSON `body`.
async fn patch_json(app: &Router, token: &str, path: &str, body: &Value) -> common::TestResponse {
    TestRequest::patch(path)
        .bearer(token)
        .json(body)
        .send(app)
        .await
}

/// Asserts the 204 the ticket pins, whose body is empty.
fn assert_no_content(response: &common::TestResponse, who: &str) {
    common::assert_answer(response, StatusCode::NO_CONTENT, who);
    assert!(response.body.is_empty(), "{who}: the 204 body is empty");
}

/// The moderation-log rows of one action, oldest first: sentence and data.
async fn logged(pool: &PgPool, action: &str) -> Vec<(String, Value)> {
    sqlx::query_as::<_, (String, Value)>(
        "SELECT message, data FROM moderation_log WHERE action = $1 ORDER BY id",
    )
    .bind(action)
    .fetch_all(pool)
    .await
    .expect("the log reads back")
}

/// The single entry one write action must have left: its sentence and the
/// nicknames its `subject` data map names.
async fn one_entry(pool: &PgPool, action: &str) -> (String, Vec<String>) {
    let entries = logged(pool, action).await;
    let (message, data) = entries
        .first()
        .unwrap_or_else(|| panic!("one {action} entry was expected"));
    let subjects = data["subject"]
        .as_array()
        .expect("the subject list")
        .iter()
        .filter_map(|found| found["nickname"].as_str())
        .map(str::to_owned)
        .collect();
    assert_eq!(
        entries.len(),
        1,
        "exactly one {action} entry, got {entries:?}"
    );
    (message.clone(), subjects)
}

/// The stored `password_reset_pending` of one account.
async fn pending(pool: &PgPool, account: &User) -> bool {
    sqlx::query_scalar::<_, bool>("SELECT password_reset_pending FROM users WHERE id = $1")
        .bind(account.id)
        .fetch_one(pool)
        .await
        .expect("the pending flag reads back")
}

/// The stored boolean of one named `users` column, asked by its literal
/// SQL and `account`'s id.
async fn flag_of(pool: &PgPool, sql: &'static str, account: &User) -> bool {
    sqlx::query_scalar::<_, bool>(sql)
        .bind(account.id)
        .fetch_one(pool)
        .await
        .expect("the flag reads back")
}

/// The state of the `follows` row of one pair, `None` without one.
async fn follow_state(pool: &PgPool, follower: &User, target: &User) -> Option<String> {
    sqlx::query_scalar::<_, Option<String>>(
        "SELECT state FROM follows WHERE follower_id = $1 AND followee_id = $2",
    )
    .bind(follower.id)
    .bind(target.id)
    .fetch_optional(pool)
    .await
    .expect("the follow row reads back")
    .flatten()
}

/// The reset tokens stored for one account.
async fn stored_tokens(
    pool: &PgPool,
    account: &User,
) -> Vec<wired_be::models::password_reset::PasswordResetToken> {
    sqlx::query_as::<_, wired_be::models::password_reset::PasswordResetToken>(
        "SELECT * FROM password_reset_tokens WHERE user_id = $1 ORDER BY inserted_at",
    )
    .bind(account.id)
    .fetch_all(pool)
    .await
    .expect("the token table reads back")
}

/// The plain account the credentials tests write, re-read from the row
/// behind the fixture (the nickname the seed gave, nothing else).
async fn plain(pool: &PgPool) -> User {
    wired_be::models::user::get_by_nickname(pool, "plain")
        .await
        .expect("the account reads back")
        .expect("the fixture seeded it")
}

/// The `GET …/credentials` answer of the seeded `plain` carries exactly
/// the twenty-four keys Pleroma's `"credentials.json"` renders, the stored
/// columns as they are and the images as the entity's own defaults — and
/// the same form is reachable by id as by nickname.
#[tokio::test]
async fn the_credentials_read_answers_the_pinned_form() {
    let db = common::test_db().await;
    let seeded = seed(&db).await;
    let body = read(&seeded.app, &seeded.token, &credentials("plain")).await;
    assert_eq!(
        body,
        json!({
            "id": plain(&seeded.pool).await.id.to_string(),
            "bio": "",
            "email": "plain@example.org",
            "fields": [],
            "name": "Plain Person",
            "nickname": "plain",
            "is_locked": false,
            "no_rich_text": false,
            "default_scope": "public",
            "hide_follows": false,
            "hide_followers_count": false,
            "hide_follows_count": false,
            "hide_followers": false,
            "hide_favorites": true,
            "allow_following_move": true,
            "show_role": true,
            "skip_thread_containment": false,
            "pleroma_settings_store": {},
            "raw_fields": [],
            "is_discoverable": false,
            "actor_type": "Person",
            "avatar": format!("{TEST_BASE_URL}{DEFAULT_AVATAR}"),
            "banner": format!("{TEST_BASE_URL}/images/banner.png"),
            "background": null,
        }),
        "the moderation form, key for key"
    );

    let by_id = read(
        &seeded.app,
        &seeded.token,
        &credentials(&plain(&seeded.pool).await.id.to_string()),
    )
    .await;
    assert_eq!(
        by_id["nickname"],
        json!("plain"),
        "the id spelling resolves to the same form"
    );

    for segment in ["nobody", "faraway@remote.example"] {
        let response = TestRequest::get(&credentials(segment))
            .bearer(&seeded.token)
            .send(&seeded.app)
            .await;
        common::assert_answer(
            &response,
            StatusCode::NOT_FOUND,
            "a credentials miss or a remote mirror",
        );
        common::assert_error(
            &response,
            NOT_FOUND,
            "a credentials miss or a remote mirror",
        );
    }
}

/// The full credentials write: the profile columns, the rendered note, the
/// merged settings blob, the actor-kind-pinned `bot`, the new address and
/// the stated booleans all land, the answer is Pleroma's success map and
/// the entry reads `@root updated users: @plain`.
#[tokio::test]
async fn a_credentials_patch_writes_every_field_it_carries() {
    let db = common::test_db().await;
    let seeded = seed(&db).await;
    let token = write_token(&seeded).await;
    let response = patch_json(
        &seeded.app,
        &token,
        &credentials("plain"),
        &json!({
            "name": "Ada Byron",
            "bio": "hello world",
            "default_scope": "private",
            "actor_type": "Service",
            "email": "replained@example.org",
            "pleroma_settings_store": {"config": {"fe": "mochi-pub"}},
            "is_locked": true,
            "is_discoverable": true,
            "hide_favorites": false,
        }),
    )
    .await;
    common::assert_answer(&response, StatusCode::OK, "an accepted credentials write");
    assert_eq!(
        response.json_value(),
        json!({ "status": "success" }),
        "Pleroma's success map"
    );

    let after = read(&seeded.app, &seeded.token, &credentials("plain")).await;
    assert_eq!(after["name"], json!("Ada Byron"), "the display name");
    assert_eq!(
        after["bio"],
        json!("<p>hello world</p>"),
        "the rendered note"
    );
    assert_eq!(after["default_scope"], json!("private"), "the scope");
    assert_eq!(after["actor_type"], json!("Service"), "the actor kind");
    assert_eq!(
        after["email"],
        json!("replained@example.org"),
        "the new address"
    );
    assert_eq!(
        after["pleroma_settings_store"],
        json!({"config": {"fe": "mochi-pub"}}),
        "the merged blob"
    );
    assert_eq!(after["is_locked"], json!(true), "the locked flag");
    assert_eq!(after["is_discoverable"], json!(true), "the discovery flag");
    assert_eq!(
        after["hide_favorites"],
        json!(false),
        "the favourite list reopened"
    );
    assert!(
        flag_of(
            &seeded.pool,
            "SELECT bot FROM users WHERE id = $1",
            &plain(&seeded.pool).await
        )
        .await,
        "the `Service` kind re-derived bot"
    );

    let (message, subjects) = one_entry(&seeded.pool, "updated_users").await;
    assert_eq!(message, "@root updated users: @plain", "Pleroma's sentence");
    assert_eq!(subjects, vec!["plain".to_owned()], "the named subject");
}

/// One body that fails six rules reports ALL of them at once, Ecto-style,
/// and changes nothing: the entity still reads as stored, the blob kept its
/// old shape and no `updated_users` entry exists.
#[tokio::test]
async fn a_credentials_patch_reports_every_refusal_at_once() {
    let db = common::test_db().await;
    let seeded = seed(&db).await;
    let token = write_token(&seeded).await;
    let response = patch_json(
        &seeded.app,
        &token,
        &credentials("plain"),
        &json!({
            "email": "not-an-address",
            "default_scope": "leaked",
            "actor_type": "Group",
            "pleroma_settings_store": [1, 2],
            "password": "short",
            "password_confirmation": "different",
        }),
    )
    .await;
    common::assert_answer(&response, StatusCode::BAD_REQUEST, "the changeset refusal");
    assert_eq!(
        response.json_value(),
        json!({ "errors": {
            "email": ["has invalid format"],
            "default_scope": ["is not included in the list"],
            "actor_type": ["is not included in the list"],
            "pleroma_settings_store": ["is invalid"],
            "password": ["should be at least 8 character(s)"],
            "password_confirmation": ["does not match password"],
        } }),
        "the whole field map, one entry per refusing field"
    );

    let still = read(&seeded.app, &seeded.token, &credentials("plain")).await;
    assert_eq!(
        still["email"],
        json!("plain@example.org"),
        "the address stands"
    );
    assert_eq!(still["default_scope"], json!("public"), "and the scope");
    assert!(
        logged(&seeded.pool, "updated_users").await.is_empty(),
        "nothing logged"
    );
}

/// The length rules of the user-facing endpoint apply under the admin's
/// parameter names too: an over-long name and bio are refused under
/// `name`/`bio` with the same messages.
#[tokio::test]
async fn the_credentials_checks_mirror_the_user_endpoint() {
    let db = common::test_db().await;
    let seeded = seed(&db).await;
    let token = write_token(&seeded).await;
    let config = common::test_config();
    let long_name = "x".repeat(config.max_display_name_chars + 1);
    let long_bio = "y".repeat(config.max_bio_chars + 1);
    let response = patch_json(
        &seeded.app,
        &token,
        &credentials("plain"),
        &json!({ "name": long_name, "bio": long_bio }),
    )
    .await;
    common::assert_answer(&response, StatusCode::BAD_REQUEST, "the length refusals");
    assert_eq!(
        response.json_value(),
        json!({ "errors": {
            "name": [format!("Name is too long (maximum is {} characters)", config.max_display_name_chars)],
            "bio": [format!("Bio is too long (maximum is {} characters)", config.max_bio_chars)],
        } }),
        "both fields name their own limit"
    );
}

/// The taken-address rule reads the table, not the request: the same
/// address on the same account rewrites happily, another account's is the
/// `has already been taken` refusal.
#[tokio::test]
async fn an_email_held_by_another_account_is_taken() {
    let db = common::test_db().await;
    let seeded = seed(&db).await;
    let token = write_token(&seeded).await;

    let own = patch_json(
        &seeded.app,
        &token,
        &credentials("plain"),
        &json!({ "email": "plain@example.org" }),
    )
    .await;
    common::assert_answer(&own, StatusCode::OK, "the account's own address");

    let refusal = patch_json(
        &seeded.app,
        &token,
        &credentials("root"),
        &json!({ "email": "plain@example.org" }),
    )
    .await;
    common::assert_answer(&refusal, StatusCode::BAD_REQUEST, "the taken address");
    assert_eq!(
        refusal.json_value(),
        json!({ "errors": { "email": ["has already been taken"] } }),
        "Ecto's unique_constraint wording"
    );
}

/// A credentials password replaces the hash and revokes the account's
/// tokens in one stroke: the old bearer is 401, the new password signs in
/// and the old one does not, and — unlike Pleroma's async force behind the
/// same write — the account is NOT put on pending reset.
#[tokio::test]
async fn a_credentials_password_replaces_hash_and_tokens() {
    let db = common::test_db().await;
    let seeded = seed(&db).await;
    let writer =
        common::create_user_with_password(&seeded.pool, "writer", "first secret pass").await;
    let admin_token = write_token(&seeded).await;
    let old_bearer = mint(&seeded.pool, &writer, "read").await;

    let response = patch_json(
        &seeded.app,
        &admin_token,
        &credentials("writer"),
        &json!({ "password": "second secret pass" }),
    )
    .await;
    common::assert_answer(&response, StatusCode::OK, "the password write");

    let stale = TestRequest::get("/api/v1/accounts/verify_credentials")
        .bearer(&old_bearer)
        .send(&seeded.app)
        .await;
    common::assert_answer(&stale, StatusCode::UNAUTHORIZED, "the revoked token");
    common::assert_error(&stale, INVALID_TOKEN, "the revoked token");

    assert!(
        wired_be::models::user::register::authenticate(
            &seeded.pool,
            "writer",
            "second secret pass"
        )
        .await
        .is_some(),
        "the new password signs in"
    );
    assert!(
        wired_be::models::user::register::authenticate(&seeded.pool, "writer", "first secret pass")
            .await
            .is_none(),
        "and the old one does not"
    );
    assert!(
        !pending(&seeded.pool, &writer).await,
        "the credentials write does not force a reset"
    );
    let (message, subjects) = one_entry(&seeded.pool, "updated_users").await;
    assert_eq!(message, "@root updated users: @writer", "one log entry");
    assert_eq!(
        subjects,
        vec!["writer".to_owned()],
        "for the written account"
    );
}

/// A body that names nothing to write is the accepted no-op — 200, the
/// success map, nothing changed — and Pleroma's log clause runs all the
/// same. An empty patch on a mirror or a miss is the 404 before anything
/// else.
#[tokio::test]
async fn an_empty_credentials_patch_changes_nothing_but_lands() {
    let db = common::test_db().await;
    let seeded = seed(&db).await;
    let token = write_token(&seeded).await;
    let response = patch_json(&seeded.app, &token, &credentials("plain"), &json!({})).await;
    common::assert_answer(&response, StatusCode::OK, "the accepted no-op");
    assert_eq!(
        response.json_value(),
        json!({ "status": "success" }),
        "still the success map"
    );

    let after = read(&seeded.app, &seeded.token, &credentials("plain")).await;
    assert_eq!(after["name"], json!("Plain Person"), "the row stands");
    let (message, subjects) = one_entry(&seeded.pool, "updated_users").await;
    assert_eq!(
        message, "@root updated users: @plain",
        "Pleroma's clause logs the empty write too"
    );
    assert_eq!(subjects, vec!["plain".to_owned()], "and names its subject");

    let mirror = patch_json(
        &seeded.app,
        &token,
        &credentials("faraway@remote.example"),
        &json!({}),
    )
    .await;
    common::assert_answer(
        &mirror,
        StatusCode::NOT_FOUND,
        "a mirror has no credentials",
    );
}

/// The JSON `null` reads as absent on every credentials parameter (the
/// changeset drops nils before casting): a null email, name and store
/// write nothing and refuse nothing.
#[tokio::test]
async fn a_credentials_null_reads_as_absent() {
    let db = common::test_db().await;
    let seeded = seed(&db).await;
    let token = write_token(&seeded).await;
    let response = patch_json(
        &seeded.app,
        &token,
        &credentials("plain"),
        &json!({ "email": null, "name": null, "pleroma_settings_store": null, "password": null }),
    )
    .await;
    common::assert_answer(&response, StatusCode::OK, "the all-null body");
    let after = read(&seeded.app, &seeded.token, &credentials("plain")).await;
    assert_eq!(
        after["email"],
        json!("plain@example.org"),
        "nothing cleared"
    );
    assert_eq!(after["name"], json!("Plain Person"), "nothing blanked");
}

/// A form body carries the same write: the bracketed-free `name` and the
/// `pleroma_settings_store` as the JSON TEXT of an object, the way a form
/// client sends it.
#[tokio::test]
async fn a_credentials_form_body_writes_through_the_same_rules() {
    let db = common::test_db().await;
    let seeded = seed(&db).await;
    let token = write_token(&seeded).await;
    let response = TestRequest::patch(&credentials("plain"))
        .bearer(&token)
        .form(&[
            ("name", "Form Filler"),
            ("default_scope", "direct"),
            (
                "pleroma_settings_store",
                "{\"config\":{\"fe\":\"pleroma-fe\"}}",
            ),
        ])
        .send(&seeded.app)
        .await;
    common::assert_answer(&response, StatusCode::OK, "the form write");
    let after = read(&seeded.app, &seeded.token, &credentials("plain")).await;
    assert_eq!(after["name"], json!("Form Filler"), "the name landed");
    assert_eq!(after["default_scope"], json!("direct"), "and the scope");
    assert_eq!(
        after["pleroma_settings_store"],
        json!({"config": {"fe": "pleroma-fe"}}),
        "and the text-parsed blob merged"
    );
}

/// The metadata pair runs the user endpoint's own validator: a submitted
/// `raw_fields` array stores the raw entry beside its rendered twin, and
/// one entry past the configured cap is refused under `raw_fields`.
#[tokio::test]
async fn submitted_raw_fields_land_rendered_and_refused() {
    let db = common::test_db().await;
    let seeded = seed(&db).await;
    let token = write_token(&seeded).await;
    let response = patch_json(
        &seeded.app,
        &token,
        &credentials("plain"),
        &json!({ "raw_fields": [{"name": "web", "value": "https://example.org"}] }),
    )
    .await;
    common::assert_answer(&response, StatusCode::OK, "the metadata write");
    let after = read(&seeded.app, &seeded.token, &credentials("plain")).await;
    assert_eq!(
        after["raw_fields"],
        json!([{"name": "web", "value": "https://example.org"}]),
        "the typed pair is stored as submitted"
    );
    assert_eq!(
        after["fields"][0]["name"],
        json!("web"),
        "and the rendered twin came along"
    );

    let config = common::test_config();
    let too_many: Vec<Value> = (0..=config.max_profile_fields)
        .map(|index| json!({"name": format!("f{index}"), "value": "v"}))
        .collect();
    let refused = patch_json(
        &seeded.app,
        &token,
        &credentials("plain"),
        &json!({ "raw_fields": too_many }),
    )
    .await;
    common::assert_answer(&refused, StatusCode::BAD_REQUEST, "the over-cap metadata");
    assert_eq!(
        refused.json_value(),
        json!({ "errors": { "raw_fields": ["Too many fields"] } }),
        "the user endpoint's own message, under the admin's parameter name"
    );
}

/// A blob that is too large or no object at all is refused with the Ecto
/// shape; a valid patch only ever ADDS to the stored blob — the shallow
/// merge Pleroma's `Map.merge` runs.
#[tokio::test]
async fn the_settings_store_merges_and_measures() {
    let db = common::test_db().await;
    let seeded = seed(&db).await;
    let token = write_token(&seeded).await;
    let stored = patch_json(
        &seeded.app,
        &token,
        &credentials("plain"),
        &json!({ "pleroma_settings_store": {"config": {"a": 1}, "other": true} }),
    )
    .await;
    common::assert_answer(&stored, StatusCode::OK, "the first store write");

    let merged = patch_json(
        &seeded.app,
        &token,
        &credentials("plain"),
        &json!({ "pleroma_settings_store": {"config": {"b": 2}} }),
    )
    .await;
    common::assert_answer(&merged, StatusCode::OK, "the second store write");
    let after = read(&seeded.app, &seeded.token, &credentials("plain")).await;
    assert_eq!(
        after["pleroma_settings_store"],
        json!({"config": {"b": 2}, "other": true}),
        "the inner config was replaced whole, the top-level entry stayed"
    );

    let scalar = patch_json(
        &seeded.app,
        &token,
        &credentials("plain"),
        &json!({ "pleroma_settings_store": 42 }),
    )
    .await;
    common::assert_answer(&scalar, StatusCode::BAD_REQUEST, "a scalar store");
    assert_eq!(
        scalar.json_value(),
        json!({ "errors": { "pleroma_settings_store": ["is invalid"] } }),
        "no object, no keys to merge"
    );
}

/// The issued reset: gated WRITE (the read scope is refused), answered for
/// a local account with `{token, link}` whose link completes through the
/// existing form, and 404 for a mirror or a miss.
#[tokio::test]
async fn the_reset_token_endpoint_issues_a_spendable_link() {
    let db = common::test_db().await;
    let seeded = seed(&db).await;
    let token = write_token(&seeded).await;

    let read_only = TestRequest::get(&issued_reset("plain"))
        .bearer(&seeded.token)
        .send(&seeded.app)
        .await;
    common::assert_answer(
        &read_only,
        StatusCode::FORBIDDEN,
        "the read scope must not mint",
    );
    common::assert_error(&read_only, NO_ADMIN_WRITE, "the read scope must not mint");

    let response = TestRequest::get(&issued_reset("plain"))
        .bearer(&token)
        .send(&seeded.app)
        .await;
    common::assert_answer(&response, StatusCode::OK, "the mint");
    let body = response.json_value();
    let secret = body["token"].as_str().expect("the issued token text");
    assert_eq!(
        body["link"],
        json!(format!("{TEST_BASE_URL}{RESET_FORM}/{secret}")),
        "the absolute link the answer pins, hung on the instance origin"
    );
    assert_eq!(
        body.as_object().expect("an object").len(),
        2,
        "exactly token and link"
    );
    let stored = stored_tokens(&seeded.pool, &plain(&seeded.pool).await).await;
    assert_eq!(stored.len(), 1, "the mint landed in the table");
    assert_eq!(stored[0].token, secret, "and is the answered token");

    for segment in ["nobody", "faraway@remote.example"] {
        let missing = TestRequest::get(&issued_reset(segment))
            .bearer(&token)
            .send(&seeded.app)
            .await;
        common::assert_answer(&missing, StatusCode::NOT_FOUND, "a reset miss or mirror");
    }

    // The link completes through the existing form, and the new password
    // then signs in: the admin's token is spendable exactly like the
    // emailed one.
    let _resettable =
        common::create_user_with_password(&seeded.pool, "resettable", "first secret pass").await;
    let minted = TestRequest::get(&issued_reset("resettable"))
        .bearer(&token)
        .send(&seeded.app)
        .await;
    let issued = minted.json_value();
    let fresh = issued["token"].as_str().expect("the token text");
    let completed = TestRequest::post(RESET_FORM)
        .form(&[
            ("data[token]", fresh),
            ("data[password]", "second secret pass"),
            ("data[password_confirmation]", "second secret pass"),
        ])
        .send(&seeded.app)
        .await;
    common::assert_answer(&completed, StatusCode::OK, "the completion");
    assert!(
        wired_be::models::user::register::authenticate(
            &seeded.pool,
            "resettable",
            "second secret pass"
        )
        .await
        .is_some(),
        "the reset landed"
    );
    assert!(
        logged(&seeded.pool, "updated_users").await.is_empty(),
        "the user-facing reset completion logs no admin write"
    );
}

/// Pleroma's `PATCH /admin/users/force_password_reset` path reaches the
/// same forced reset as the `password_reset` alias: 204 and the flag raised.
#[tokio::test]
async fn force_password_reset_answers_on_pleromas_path() {
    let db = common::test_db().await;
    let seeded = seed(&db).await;
    let token = write_token(&seeded).await;
    let victim =
        common::create_user_with_password(&seeded.pool, "forced", "their secret pass").await;

    let response = patch_json(
        &seeded.app,
        &token,
        FORCE_RESET_PLEROMA,
        &json!({ "nicknames": ["forced"] }),
    )
    .await;
    assert_no_content(&response, "the forced reset on Pleroma's path");
    assert!(pending(&seeded.pool, &victim).await, "the flag is raised");
}

/// The forced reset: 204, the flag raised and the tokens revoked per
/// account, unknown names skipped inside the same success, one log entry
/// naming who was found — and a body with no `nicknames` is the
/// extractor's 422.
#[tokio::test]
async fn force_password_reset_blocks_login_until_a_new_password_lands() {
    let db = common::test_db().await;
    let seeded = seed(&db).await;
    let token = write_token(&seeded).await;
    let victim =
        common::create_user_with_password(&seeded.pool, "forced", "their secret pass").await;
    let bearer = mint(&seeded.pool, &victim, "read").await;
    let login_app = common::create_app(&seeded.pool, "read write").await;

    let response = patch_json(
        &seeded.app,
        &token,
        FORCE_RESET,
        &json!({ "nicknames": ["forced", "ghost"] }),
    )
    .await;
    assert_no_content(&response, "the forced reset");

    assert!(pending(&seeded.pool, &victim).await, "the flag is raised");
    let stale = TestRequest::get("/api/v1/accounts/verify_credentials")
        .bearer(&bearer)
        .send(&seeded.app)
        .await;
    common::assert_answer(&stale, StatusCode::UNAUTHORIZED, "the revoked bearer");
    let blocked = TestRequest::post("/oauth/token")
        .json(&json!({
            "grant_type": "password",
            "username": "forced",
            "password": "their secret pass",
            "scope": "read write",
            "client_id": login_app.client_id,
            "client_secret": login_app.client_secret,
        }))
        .send(&seeded.app)
        .await;
    common::assert_answer(&blocked, StatusCode::FORBIDDEN, "the pending grant");
    assert_eq!(
        blocked.json_value(),
        json!({ "error": RESET_REQUIRED, "identifier": "password_reset_required" }),
        "the login refusal names the reason"
    );

    let (message, subjects) = one_entry(&seeded.pool, "force_password_reset").await;
    assert_eq!(
        message, "@root forced password reset for users: @forced",
        "Pleroma's sentence, the skipped ghost unmentioned"
    );
    assert_eq!(subjects, vec!["forced".to_owned()], "one found subject");

    // A form's bracketed spelling reaches the same write, and the missing
    // parameter is the extractor's 422.
    let second =
        common::create_user_with_password(&seeded.pool, "forced2", "their secret pass").await;
    let by_form = TestRequest::patch(FORCE_RESET)
        .bearer(&token)
        .form(&[("nicknames[]", "forced2")])
        .send(&seeded.app)
        .await;
    assert_no_content(&by_form, "the form spelling");
    assert!(pending(&seeded.pool, &second).await, "and it forced too");

    let bare = patch_json(&seeded.app, &token, FORCE_RESET, &json!({})).await;
    common::assert_answer(
        &bare,
        StatusCode::UNPROCESSABLE_ENTITY,
        "the body without nicknames",
    );
}

/// Confirming emails by list: the flag flips, an outstanding token dies,
/// the unknown name is skipped and the entry names the found pair.
#[tokio::test]
async fn confirm_email_confirms_the_named_accounts() {
    let db = common::test_db().await;
    let seeded = seed(&db).await;
    let token = write_token(&seeded).await;
    let unminded = wired_be::models::user::get_by_nickname(&seeded.pool, "unminded")
        .await
        .expect("the account reads back")
        .expect("the fixture seeded it");
    let unminded_id = unminded.id;
    let _wild =
        sqlx::query("UPDATE users SET confirmation_token = 'a-letter-in-the-wild' WHERE id = $1")
            .bind(unminded_id)
            .execute(&seeded.pool)
            .await
            .expect("the token fixture writes");

    let response = patch_json(
        &seeded.app,
        &token,
        CONFIRM,
        &json!({ "nicknames": ["unminded", "plain", "ghost"] }),
    )
    .await;
    assert_no_content(&response, "the confirmation");
    assert!(
        flag_of(
            &seeded.pool,
            "SELECT email_confirmed FROM users WHERE id = $1",
            &unminded
        )
        .await,
        "the address is confirmed"
    );
    let token_row = sqlx::query_scalar::<_, Option<String>>(
        "SELECT confirmation_token FROM users WHERE id = $1",
    )
    .bind(unminded_id)
    .fetch_one(&seeded.pool)
    .await
    .expect("the token column reads back");
    assert!(
        token_row.is_none(),
        "the outstanding letter confirms nothing now"
    );

    let (message, subjects) = one_entry(&seeded.pool, "confirm_email").await;
    assert_eq!(
        message, "@root confirmed email for users: @unminded, @plain",
        "Pleroma's sentence"
    );
    assert_eq!(
        subjects,
        vec!["unminded".to_owned(), "plain".to_owned()],
        "the found pair, in the submitted order"
    );
}

/// The resend mails only the unconfirmed: with a relay configured, exactly
/// one letter queues for the unconfirmed named account, the confirmed one
/// is skipped, and the log entry still names both found subjects.
#[tokio::test]
async fn resend_confirmation_emails_only_the_unconfirmed() {
    let db = common::test_db().await;
    let seeded = seed(&db).await;
    let app = common::test_app_with_config(seeded.pool.clone(), mail_config());
    let token = mint(&seeded.pool, &seeded.root, "admin:write:accounts").await;
    let mailed = common::create_local_user_with(&seeded.pool, "postme", |user| NewUser {
        email: Some("postme@example.org".to_owned()),
        email_confirmed: Flag::new(false),
        ..user
    })
    .await;

    let response = patch_json(
        &app,
        &token,
        RESEND,
        &json!({ "nicknames": ["postme", "plain", "ghost"] }),
    )
    .await;
    assert_no_content(&response, "the resend");

    let letters = sqlx::query_scalar::<_, Value>(
        "SELECT args FROM jobs WHERE queue = 'mailer' AND kind = 'mailer.send' ORDER BY id",
    )
    .fetch_all(&seeded.pool)
    .await
    .expect("the mailer queue reads back");
    assert_eq!(
        letters.len(),
        1,
        "one letter, the confirmed account got none"
    );
    assert_eq!(
        letters[0]["to"],
        json!("postme@example.org"),
        "the unconfirmed address"
    );
    assert!(
        sqlx::query_scalar::<_, Option<String>>(
            "SELECT confirmation_token FROM users WHERE id = $1"
        )
        .bind(mailed.id)
        .fetch_one(&seeded.pool)
        .await
        .expect("the token column reads back")
        .is_some(),
        "the resend rotated a live token"
    );

    let (message, subjects) = one_entry(&seeded.pool, "resend_confirmation_email").await;
    assert_eq!(
        message, "@root re-sent confirmation email for users: @postme, @plain",
        "Pleroma's sentence"
    );
    assert_eq!(
        subjects,
        vec!["postme".to_owned(), "plain".to_owned()],
        "every resolved subject, mailed or not"
    );
}

/// Removing MFA as an admin: the settings column returns to the migration
/// default, the answer is the nickname itself, an unknown name is the 404
/// and a missing `nickname` the 422 — and the write logs.
#[tokio::test]
async fn disable_mfa_resets_the_settings_and_answers_the_nickname() {
    let db = common::test_db().await;
    let seeded = seed(&db).await;
    let token = write_token(&seeded).await;
    let (twofa, _secret, _codes) =
        common::create_mfa_user_with_password(&seeded.pool, "twofa", "their secret pass", 3).await;

    let response = TestRequest::put(DISABLE_MFA)
        .bearer(&token)
        .json(&json!({ "nickname": "twofa" }))
        .send(&seeded.app)
        .await;
    common::assert_answer(&response, StatusCode::OK, "the MFA removal");
    assert_eq!(
        response.json_value(),
        json!("twofa"),
        "Pleroma echoes the nickname"
    );

    let stored: Value =
        sqlx::query_scalar::<_, Value>("SELECT mfa_settings FROM users WHERE id = $1")
            .bind(twofa.id)
            .fetch_one(&seeded.pool)
            .await
            .expect("the settings read back");
    assert_eq!(
        stored,
        json!({"enabled": false, "totp": {"secret": null, "confirmed": false}, "backup_codes": []}),
        "the column is the migration default again"
    );
    let (message, subjects) = one_entry(&seeded.pool, "disable_mfa").await;
    assert_eq!(
        message, "@root disabled MFA for users: @twofa",
        "the added entry"
    );
    assert_eq!(subjects, vec!["twofa".to_owned()], "for the account");

    let missing = TestRequest::put(DISABLE_MFA)
        .bearer(&token)
        .json(&json!({ "nickname": "ghost" }))
        .send(&seeded.app)
        .await;
    common::assert_answer(&missing, StatusCode::NOT_FOUND, "an unknown handle");
    let bare = TestRequest::put(DISABLE_MFA)
        .bearer(&token)
        .json(&json!({}))
        .send(&seeded.app)
        .await;
    common::assert_answer(
        &bare,
        StatusCode::UNPROCESSABLE_ENTITY,
        "the body without nickname",
    );
}

/// The forced follow connects the pair even across a locked target — the
/// request is accepted on the spot — logs Pleroma's `made … follow`
/// sentence, and the unfollow takes it back and logs the pair's other
/// sentence.
#[tokio::test]
async fn the_admin_follow_pair_connects_reports_and_disconnects() {
    let db = common::test_db().await;
    let seeded = seed(&db).await;
    let token = write_token(&seeded).await;
    let lockedy = common::create_local_user_with(&seeded.pool, "lockedy", |user| NewUser {
        locked: Flag::new(true),
        ..user
    })
    .await;
    let follower = plain(&seeded.pool).await;

    let response = TestRequest::post(FOLLOW)
        .bearer(&token)
        .json(&json!({ "follower": "plain", "followed": "lockedy" }))
        .send(&seeded.app)
        .await;
    common::assert_answer(&response, StatusCode::OK, "the forced follow");
    assert_eq!(response.json_value(), json!("ok"), "the ticket's answer");
    assert_eq!(
        follow_state(&seeded.pool, &follower, &lockedy)
            .await
            .as_deref(),
        Some("accepted"),
        "the admin's act is the answer a locked target would have given"
    );
    let entries = logged(&seeded.pool, "follow").await;
    assert_eq!(entries.len(), 1, "one entry");
    assert_eq!(
        entries[0].0, "@root made @plain follow @lockedy",
        "Pleroma's sentence"
    );
    assert_eq!(
        entries[0].1["follower"]["nickname"],
        json!("plain"),
        "both maps ride in the data"
    );
    assert_eq!(
        entries[0].1["followed"]["nickname"],
        json!("lockedy"),
        "on both sides"
    );

    let withdrawn = TestRequest::post(UNFOLLOW)
        .bearer(&token)
        .json(&json!({ "follower": "plain", "followed": "lockedy" }))
        .send(&seeded.app)
        .await;
    common::assert_answer(&withdrawn, StatusCode::OK, "the forced unfollow");
    assert_eq!(withdrawn.json_value(), json!("ok"), "the same answer");
    assert!(
        follow_state(&seeded.pool, &follower, &lockedy)
            .await
            .is_none(),
        "the row is gone"
    );
    let withdrawn_log = logged(&seeded.pool, "unfollow").await;
    assert_eq!(withdrawn_log.len(), 1, "and its own entry");
    assert_eq!(
        withdrawn_log[0].0, "@root made @plain unfollow @lockedy",
        "Pleroma's sentence"
    );

    // Re-following an accepted pair changes nothing and still reports ok;
    // a second unfollow is the same no-op with its second entry.
    let again = TestRequest::post(FOLLOW)
        .bearer(&token)
        .json(&json!({ "follower": "plain", "followed": "lockedy" }))
        .send(&seeded.app)
        .await;
    common::assert_answer(&again, StatusCode::OK, "the re-follow");
    let idempotent = TestRequest::post(UNFOLLOW)
        .bearer(&token)
        .json(&json!({ "follower": "plain", "followed": "lockedy" }))
        .send(&seeded.app)
        .await;
    common::assert_answer(&idempotent, StatusCode::OK, "the repeat unfollow");
    assert_eq!(
        logged(&seeded.pool, "unfollow").await.len(),
        2,
        "logged again"
    );
}

/// The follow pair refuses what it cannot force: a name that answers to
/// nobody, a remote FOLLOWER (this instance cannot order a mirror) and
/// following yourself — the model's own 422 rides through.
#[tokio::test]
async fn the_admin_follow_pair_refuses_the_impossible() {
    let db = common::test_db().await;
    let seeded = seed(&db).await;
    let token = write_token(&seeded).await;

    let miss = TestRequest::post(FOLLOW)
        .bearer(&token)
        .json(&json!({ "follower": "plain", "followed": "ghost" }))
        .send(&seeded.app)
        .await;
    common::assert_answer(
        &miss,
        StatusCode::NOT_FOUND,
        "a name that answers to nobody",
    );

    let mirror_first = TestRequest::post(FOLLOW)
        .bearer(&token)
        .json(&json!({ "follower": "faraway@remote.example", "followed": "plain" }))
        .send(&seeded.app)
        .await;
    common::assert_answer(&mirror_first, StatusCode::NOT_FOUND, "a remote follower");

    let selfish = TestRequest::post(FOLLOW)
        .bearer(&token)
        .json(&json!({ "follower": "plain", "followed": "plain" }))
        .send(&seeded.app)
        .await;
    common::assert_answer(
        &selfish,
        StatusCode::UNPROCESSABLE_ENTITY,
        "the self-follow",
    );
    common::assert_error(
        &selfish,
        "Can not follow yourself",
        "the model's own wording",
    );

    let bare = TestRequest::post(FOLLOW)
        .bearer(&token)
        .json(&json!({ "follower": "plain" }))
        .send(&seeded.app)
        .await;
    common::assert_answer(&bare, StatusCode::UNPROCESSABLE_ENTITY, "the half body");
    assert!(
        logged(&seeded.pool, "follow").await.is_empty(),
        "and no refusal left an entry behind"
    );
}

/// One maintenance route: the verb, the path, the body it carries and the
/// status its success answer has.
struct Route {
    /// The request verb.
    verb: &'static str,
    /// The full path.
    path: String,
    /// The JSON body, `None` for the GETs.
    body: Option<Value>,
    /// The status an accepted request answers.
    status: StatusCode,
}

/// The routes of this ticket's section, each with a body that passes its
/// checks against the seeded fixture.
fn maintenance_routes() -> Vec<Route> {
    vec![
        Route {
            verb: "GET",
            path: issued_reset("plain"),
            body: None,
            status: StatusCode::OK,
        },
        Route {
            verb: "PATCH",
            path: FORCE_RESET.to_owned(),
            body: Some(json!({"nicknames": []})),
            status: StatusCode::NO_CONTENT,
        },
        Route {
            verb: "PATCH",
            path: CONFIRM.to_owned(),
            body: Some(json!({"nicknames": []})),
            status: StatusCode::NO_CONTENT,
        },
        Route {
            verb: "PATCH",
            path: RESEND.to_owned(),
            body: Some(json!({"nicknames": []})),
            status: StatusCode::NO_CONTENT,
        },
        Route {
            verb: "PUT",
            path: DISABLE_MFA.to_owned(),
            body: Some(json!({"nickname": "plain"})),
            status: StatusCode::OK,
        },
        Route {
            verb: "POST",
            path: FOLLOW.to_owned(),
            body: Some(json!({"follower": "plain", "followed": "root"})),
            status: StatusCode::OK,
        },
        Route {
            verb: "POST",
            path: UNFOLLOW.to_owned(),
            body: Some(json!({"follower": "plain", "followed": "root"})),
            status: StatusCode::OK,
        },
        Route {
            verb: "PUT",
            path: TAG.to_owned(),
            body: Some(json!({"nicknames": [], "tags": []})),
            status: StatusCode::NO_CONTENT,
        },
        Route {
            verb: "DELETE",
            path: TAG.to_owned(),
            body: Some(json!({"nicknames": [], "tags": []})),
            status: StatusCode::NO_CONTENT,
        },
    ]
}

/// The routes of the profile-tag pair, the permission-group writes and the
/// suggest pair, each answered the way the seeded fixture allows. They ride
/// the same gate as the maintenance routes: a write scope, then a write.
fn role_routes() -> Vec<Route> {
    vec![
        Route {
            verb: "POST",
            path: group_of("plain", "moderator"),
            body: None,
            status: StatusCode::OK,
        },
        Route {
            verb: "DELETE",
            path: group_of("plain", "moderator"),
            body: None,
            status: StatusCode::OK,
        },
        Route {
            verb: "POST",
            path: group_bulk("admin"),
            body: Some(json!({"nicknames": []})),
            status: StatusCode::OK,
        },
        Route {
            verb: "DELETE",
            path: group_bulk("admin"),
            body: Some(json!({"nicknames": []})),
            status: StatusCode::OK,
        },
        Route {
            verb: "PATCH",
            path: SUGGEST.to_owned(),
            body: Some(json!({"nicknames": []})),
            status: StatusCode::OK,
        },
        Route {
            verb: "PATCH",
            path: UNSUGGEST.to_owned(),
            body: Some(json!({"nicknames": []})),
            status: StatusCode::OK,
        },
    ]
}

/// Everything in `maintenance_routes` plus this ticket's write routes.
fn write_routes() -> Vec<Route> {
    let mut routes = maintenance_routes();
    routes.extend(role_routes());
    routes
}

/// The credentials pair joins the set for the ROLE gate: both endpoints
/// answer 401/403 by role and scope before anything else. So do the two
/// permission-group reads, which are this ticket's only GETs.
fn credentials_routes() -> Vec<Route> {
    vec![
        Route {
            verb: "GET",
            path: credentials("plain"),
            body: None,
            status: StatusCode::OK,
        },
        Route {
            verb: "PATCH",
            path: credentials("plain"),
            body: Some(json!({})),
            status: StatusCode::OK,
        },
        Route {
            verb: "GET",
            path: group("plain"),
            body: None,
            status: StatusCode::OK,
        },
        Route {
            verb: "GET",
            path: group_of("plain", "moderator"),
            body: None,
            status: StatusCode::OK,
        },
    ]
}

/// The write class's own share of the pair: the PATCH — the GET reads
/// through the read class and must not be granted by a write token.
fn credentials_patch() -> Route {
    Route {
        verb: "PATCH",
        path: credentials("plain"),
        body: Some(json!({})),
        status: StatusCode::OK,
    }
}

/// A [`TestRequest`] for one route; `token` is `None` for the anonymous
/// request the gate must refuse with 401.
fn request(route: &Route, token: Option<&str>) -> TestRequest {
    let base = match route.verb {
        "GET" => TestRequest::get(&route.path),
        "PUT" => TestRequest::put(&route.path),
        "POST" => TestRequest::post(&route.path),
        "DELETE" => TestRequest::delete(&route.path),
        _ => TestRequest::patch(&route.path),
    };
    let with_body = match route.body.as_ref() {
        Some(body) => base.json(body),
        None => base,
    };
    match token {
        Some(bearer) => with_body.bearer(bearer),
        None => with_body,
    }
}

/// None of the maintenance routes answers without an admin: no token 401
/// "Invalid credentials", an unknown token 401 "The access token is
/// invalid", an app-only token 403 naming the missing user, and any
/// non-admin — the moderator included — 403 `{"error":"User is not an
/// admin."}`.
#[tokio::test]
async fn only_admins_reach_the_maintenance_routes() {
    let db = common::test_db().await;
    let seeded = seed(&db).await;
    let staff = common::create_local_user_with(&seeded.pool, "moddy3", |user| NewUser {
        is_moderator: Flag::new(true),
        ..user
    })
    .await;
    let moderator = mint(&seeded.pool, &staff, "admin:write:accounts").await;
    let outsider = common::create_local_user(&seeded.pool, "gawker2").await;
    let regular = mint(&seeded.pool, &outsider, "admin:write").await;
    let app_only = common::create_app_only_token(&seeded.pool, "admin:write:accounts").await;

    for route in write_routes().into_iter().chain(credentials_routes()) {
        let anonymous = request(&route, None).send(&seeded.app).await;
        common::assert_answer(&anonymous, StatusCode::UNAUTHORIZED, "no token");
        common::assert_error(&anonymous, NO_TOKEN, "no token");

        let forged = request(&route, Some("no-such-token"))
            .send(&seeded.app)
            .await;
        common::assert_answer(&forged, StatusCode::UNAUTHORIZED, "an unknown token");
        common::assert_error(&forged, INVALID_TOKEN, "an unknown token");

        let unbound = request(&route, Some(&app_only)).send(&seeded.app).await;
        common::assert_answer(&unbound, StatusCode::FORBIDDEN, "an app-only token");
        common::assert_error(&unbound, USER_REQUIRED, "an app-only token");

        let moddy = request(&route, Some(&moderator)).send(&seeded.app).await;
        common::assert_answer(&moddy, StatusCode::FORBIDDEN, "a moderator");
        common::assert_error(&moddy, NOT_ADMIN, "a moderator");

        let gawker = request(&route, Some(&regular)).send(&seeded.app).await;
        common::assert_answer(&gawker, StatusCode::FORBIDDEN, "a regular user");
        common::assert_error(&gawker, NOT_ADMIN, "a regular user");
    }
}

/// The class half of the gate, both ways: the write scopes run every
/// maintenance route (the issued mint included, Pleroma's `write:accounts`
/// pair), the read scope is refused the whole set with the write class's
/// 403, and the credentials READ is refused to a write-only token with the
/// read class's — the classes never grant each other.
#[tokio::test]
async fn the_scope_class_sorts_the_maintenance_from_the_reads() {
    let db = common::test_db().await;
    let seeded = seed(&db).await;
    for scopes in ["admin:write:accounts", "admin:write"] {
        let token = mint(&seeded.pool, &seeded.root, scopes).await;
        for route in write_routes().into_iter().chain([credentials_patch()]) {
            let response = request(&route, Some(&token)).send(&seeded.app).await;
            common::assert_answer(
                &response,
                route.status,
                &format!("{scopes} grants {} {}", route.verb, route.path),
            );
        }
    }

    let write_only = mint(&seeded.pool, &seeded.root, "admin:write:accounts").await;
    let wrong_way = TestRequest::get(&credentials("plain"))
        .bearer(&write_only)
        .send(&seeded.app)
        .await;
    common::assert_answer(
        &wrong_way,
        StatusCode::FORBIDDEN,
        "the write class must not read credentials",
    );
    common::assert_error(&wrong_way, NO_ADMIN_READ, "in the read class's words");

    for route in write_routes() {
        let refused = request(&route, Some(&seeded.token)).send(&seeded.app).await;
        common::assert_answer(
            &refused,
            StatusCode::FORBIDDEN,
            &format!("the read scope must not run {} {}", route.verb, route.path),
        );
        common::assert_error(&refused, NO_ADMIN_WRITE, "in the write class's words");
    }
}

// ===== WBE-197: the profile tags, the permission groups, the pool =====

/// The stored `tags` array of one account.
async fn tags_of(pool: &PgPool, account: &User) -> Vec<String> {
    sqlx::query_scalar::<_, Vec<String>>("SELECT tags FROM users WHERE id = $1")
        .bind(account.id)
        .fetch_one(pool)
        .await
        .expect("the tags read back")
}

/// A `PUT` of `path` as `token`, with the JSON `body` when one is given.
async fn put_json(
    app: &Router,
    token: &str,
    path: &str,
    body: Option<&Value>,
) -> common::TestResponse {
    let base = TestRequest::put(path);
    let with_body = match body {
        Some(value) => base.json(value),
        None => base,
    };
    with_body.bearer(token).send(app).await
}

/// A `DELETE` of `path` as `token`, with the JSON `body` when one is given.
async fn delete_json(
    app: &Router,
    token: &str,
    path: &str,
    body: Option<&Value>,
) -> common::TestResponse {
    let base = TestRequest::delete(path);
    let with_body = match body {
        Some(value) => base.json(value),
        None => base,
    };
    with_body.bearer(token).send(app).await
}

/// A `POST` of `path` as `token`, with the JSON `body` when one is given.
async fn post_json(
    app: &Router,
    token: &str,
    path: &str,
    body: Option<&Value>,
) -> common::TestResponse {
    let base = TestRequest::post(path);
    let with_body = match body {
        Some(value) => base.json(value),
        None => base,
    };
    with_body.bearer(token).send(app).await
}

/// The nicknames one answer's `users` array carries, in order.
fn answered_users(body: &Value) -> Vec<&str> {
    body["users"]
        .as_array()
        .expect("the users array")
        .iter()
        .filter_map(|found| found["nickname"].as_str())
        .collect()
}

/// The ids one `/api/v1/suggestions` answer carries, in order.
fn suggested_ids(body: &Value) -> Vec<String> {
    body.as_array()
        .expect("the suggestion array")
        .iter()
        .filter_map(|found| found["id"].as_str())
        .map(str::to_owned)
        .collect()
}

/// `PUT /users/tag` appends the submitted tags to every named account —
/// lowercased and deduped Pleroma-style, stored tags and their order kept —
/// and the write is logged whether or not it changed anything.
#[tokio::test]
async fn the_tag_write_adds_the_submitted_tags() {
    let db = common::test_db().await;
    let seeded = seed(&db).await;
    let token = write_token(&seeded).await;
    let plain = plain(&seeded.pool).await;
    let beeper = account_named(&seeded.pool, "beeper").await;
    let body = json!({"nicknames": ["plain", "beeper"], "tags": ["staff", "VIP", "staff"]});

    let added = put_json(&seeded.app, &token, TAG, Some(&body)).await;
    assert_no_content(&added, "the tag write");
    assert_eq!(
        tags_of(&seeded.pool, &plain).await,
        vec!["staff", "vip"],
        "the submission is lowercased, its duplicate dropped, the order kept"
    );
    assert_eq!(
        tags_of(&seeded.pool, &beeper).await,
        vec!["bots", "staff", "vip"],
        "the stored tag leads and the new ones follow"
    );

    let again = put_json(&seeded.app, &token, TAG, Some(&body)).await;
    assert_no_content(&again, "the repeated tag write");
    assert_eq!(
        tags_of(&seeded.pool, &plain).await,
        vec!["staff", "vip"],
        "tagging the held tags again changes nothing"
    );

    let shown = read(&seeded.app, &seeded.token, &show("plain")).await;
    assert_eq!(
        shown["tags"],
        json!(["staff", "vip"]),
        "and the admin entity reads the new list back"
    );

    let entries = logged(&seeded.pool, "tag").await;
    assert_eq!(
        entries.len(),
        2,
        "one entry per tag write, even the one that changed nothing"
    );
    assert_eq!(
        entries[0].0, "@root added tags: staff, VIP, staff to users: @plain, @beeper",
        "the sentence names the accounts found and the tags as they were spelled"
    );
    assert_eq!(
        entries[0].1["tags"],
        json!(["staff", "VIP", "staff"]),
        "the data map carries the raw submission, Pleroma-style"
    );
}

/// `DELETE /users/tag` takes exactly the named tags back out of the named
/// accounts, leaves every other tag and account alone, is idempotent, and
/// lands its own sentence.
#[tokio::test]
async fn the_untag_write_takes_only_the_named_tags() {
    let db = common::test_db().await;
    let seeded = seed(&db).await;
    let token = write_token(&seeded).await;
    let plain = plain(&seeded.pool).await;
    let beeper = account_named(&seeded.pool, "beeper").await;
    let setup = put_json(
        &seeded.app,
        &token,
        TAG,
        Some(&json!({"nicknames": ["plain", "beeper"], "tags": ["staff", "vip"]})),
    )
    .await;
    assert_no_content(&setup, "the write this test undoes");

    let held = json!({"nicknames": ["plain"], "tags": ["staff"]});
    let removed = delete_json(&seeded.app, &token, TAG, Some(&held)).await;
    assert_no_content(&removed, "the untag write");
    assert_eq!(
        tags_of(&seeded.pool, &plain).await,
        vec!["vip"],
        "only the named tag went"
    );
    assert_eq!(
        tags_of(&seeded.pool, &beeper).await,
        vec!["bots", "staff", "vip"],
        "and the account nobody named kept every tag"
    );

    let again = delete_json(&seeded.app, &token, TAG, Some(&held)).await;
    assert_no_content(&again, "the repeated untag");
    assert_eq!(
        tags_of(&seeded.pool, &plain).await,
        vec!["vip"],
        "removing an absent tag is no error and no change"
    );

    let entries = logged(&seeded.pool, "untag").await;
    assert_eq!(
        entries[0].0, "@root removed tags: staff from users: @plain",
        "and the pair's own sentence"
    );
}

/// Both tag routes read their parameters from the query string and from a
/// form body as well as from JSON, an MRF tag is an ordinary accepted tag,
/// and a request that states only one of the two lists is the 422.
#[tokio::test]
async fn the_tag_routes_read_their_parameters_from_every_spelling() {
    let db = common::test_db().await;
    let seeded = seed(&db).await;
    let token = write_token(&seeded).await;
    let plain = plain(&seeded.pool).await;

    let via_query = put_json(
        &seeded.app,
        &token,
        &format!("{TAG}?nicknames[]=plain&tags[]=mrf_tag%3Asandbox&tags[]=ops"),
        None,
    )
    .await;
    assert_no_content(&via_query, "the query-string tag write");
    assert_eq!(
        tags_of(&seeded.pool, &plain).await,
        vec!["mrf_tag:sandbox", "ops"],
        "a known MRF tag lands as the switch spells it, and the list came from the query"
    );

    let missing_tags = delete_json(
        &seeded.app,
        &token,
        TAG,
        Some(&json!({"nicknames": ["plain"]})),
    )
    .await;
    common::assert_answer(&missing_tags, StatusCode::UNPROCESSABLE_ENTITY, "no `tags`");
    let via_form = TestRequest::put(TAG)
        .bearer(&token)
        .form(&[("nicknames[]", "plain"), ("tags[]", "OPS")])
        .send(&seeded.app)
        .await;
    assert_no_content(&via_form, "the form-body tag write");
    assert_eq!(
        tags_of(&seeded.pool, &plain).await,
        vec!["mrf_tag:sandbox", "ops"],
        "the bracketed form key nests, and the fold leaves `ops` as it already stood"
    );

    let half = put_json(&seeded.app, &token, TAG, Some(&json!({"tags": ["x"]}))).await;
    common::assert_answer(
        &half,
        StatusCode::UNPROCESSABLE_ENTITY,
        "a tag write with no nicknames",
    );
    assert!(
        half.text().contains("missing field `nicknames`"),
        "and the 422 names the key it needs: {}",
        half.text()
    );
    let bare = put_json(&seeded.app, &token, TAG, Some(&json!({}))).await;
    common::assert_answer(
        &bare,
        StatusCode::UNPROCESSABLE_ENTITY,
        "a tag write with neither list",
    );
    let bare_delete = delete_json(&seeded.app, &token, TAG, Some(&json!({"nicknames": []}))).await;
    common::assert_answer(
        &bare_delete,
        StatusCode::UNPROCESSABLE_ENTITY,
        "an untag with no tags",
    );
    assert_eq!(
        logged(&seeded.pool, "tag").await.len(),
        2,
        "the two accepted writes above logged, and the three refusals logged nothing"
    );
    assert!(
        logged(&seeded.pool, "untag").await.is_empty(),
        "and no refused request left an entry behind"
    );
}

/// The tag routes skip the handles nobody answers to — inside the same 204
/// — and a tag list naming nothing writes nothing while the entry still
/// names every account that was found.
#[tokio::test]
async fn the_tag_routes_skip_what_they_cannot_resolve() {
    let db = common::test_db().await;
    let seeded = seed(&db).await;
    let token = write_token(&seeded).await;
    let plain = plain(&seeded.pool).await;

    let ghosted = put_json(
        &seeded.app,
        &token,
        TAG,
        Some(&json!({"nicknames": ["ghost", "plain"], "tags": ["staff"]})),
    )
    .await;
    assert_no_content(&ghosted, "a list with one unknown handle");
    assert_eq!(
        tags_of(&seeded.pool, &plain).await,
        vec!["staff"],
        "the known one was written"
    );

    let empty = delete_json(
        &seeded.app,
        &token,
        TAG,
        Some(&json!({"nicknames": ["plain", "ghost"], "tags": []})),
    )
    .await;
    assert_no_content(&empty, "a tag list with nothing in it");
    assert_eq!(
        tags_of(&seeded.pool, &plain).await,
        vec!["staff"],
        "and nothing was subtracted"
    );
    let entries = logged(&seeded.pool, "untag").await;
    assert_eq!(entries.len(), 1, "the write logged all the same");
    assert_eq!(
        entries[0].0, "@root removed tags:  from users: @plain",
        "Pleroma's clause spells the empty tag list as it reads it"
    );
    assert_eq!(
        entries[0].1["nicknames"],
        json!(["plain"]),
        "the unknown handle is no subject"
    );
}

/// Both permission-group reads answer the same two-key object for any
/// account — an admin, a moderator, a plain member, a remote mirror, a row
/// named by its id — and refuse a group off the two and a name nobody
/// answers to with their 404s.
#[tokio::test]
async fn the_permission_group_reads_answer_both_flags() {
    let db = common::test_db().await;
    let seeded = seed(&db).await;
    let cases: &[(&str, Value)] = &[
        ("root", json!({ "is_moderator": false, "is_admin": true })),
        ("moddy", json!({ "is_moderator": true, "is_admin": false })),
        ("plain", json!({ "is_moderator": false, "is_admin": false })),
        (
            "faraway@remote.example",
            json!({ "is_moderator": false, "is_admin": false }),
        ),
    ];
    for (nickname, expected) in cases {
        let bare = read(&seeded.app, &seeded.token, &group(nickname)).await;
        assert_eq!(&bare, expected, "the group-less read of {nickname}");
        assert_eq!(
            bare.as_object().expect("an object").len(),
            2,
            "exactly the two flags"
        );
        let named = read(&seeded.app, &seeded.token, &group_of(nickname, "moderator")).await;
        assert_eq!(&named, expected, "and the named group answers the same");
        let admin = read(&seeded.app, &seeded.token, &group_of(nickname, "admin")).await;
        assert_eq!(&admin, expected, "for either group");
    }

    let by_id = read(
        &seeded.app,
        &seeded.token,
        &group(&seeded.root.id.to_string()),
    )
    .await;
    assert_eq!(
        by_id["is_admin"],
        json!(true),
        "the segment resolves as an id like the show's"
    );

    for verb in ["GET", "POST", "DELETE"] {
        let path = group_of("plain", "owner");
        let bearer = if verb == "GET" {
            seeded.token.clone()
        } else {
            write_token(&seeded).await
        };
        let bogus = match verb {
            "GET" => {
                TestRequest::get(&path)
                    .bearer(&bearer)
                    .send(&seeded.app)
                    .await
            }
            "POST" => post_json(&seeded.app, &bearer, &path, None).await,
            _ => delete_json(&seeded.app, &bearer, &path, None).await,
        };
        common::assert_answer(
            &bogus,
            StatusCode::NOT_FOUND,
            &format!("a group off the two, over {verb}"),
        );
        common::assert_error(&bogus, NO_SUCH_GROUP, "Pleroma's wording");
    }
    let miss = TestRequest::get(&group("ghost"))
        .bearer(&seeded.token)
        .send(&seeded.app)
        .await;
    common::assert_answer(&miss, StatusCode::NOT_FOUND, "a name nobody answers to");
    common::assert_error(&miss, NOT_FOUND, "the module's 404");

    let write_only = write_token(&seeded).await;
    let refused = TestRequest::get(&group("plain"))
        .bearer(&write_only)
        .send(&seeded.app)
        .await;
    common::assert_answer(
        &refused,
        StatusCode::FORBIDDEN,
        "the write class must not read",
    );
    common::assert_error(&refused, NO_ADMIN_READ, "in the read class's words");
}

/// The single account's grant and revoke: the one-key answer of the group
/// that moved, the flag in storage, the roles in the entity, the granted
/// account reading the admin API it could not reach before, and one
/// `grant` / `revoke` entry per write.
#[tokio::test]
async fn a_group_grant_lands_and_a_revoke_takes_it_back() {
    let db = common::test_db().await;
    let seeded = seed(&db).await;
    let token = write_token(&seeded).await;
    let plain = plain(&seeded.pool).await;

    let before = TestRequest::get(USERS)
        .bearer(&mint(&seeded.pool, &plain, "admin:read:accounts").await)
        .send(&seeded.app)
        .await;
    common::assert_answer(
        &before,
        StatusCode::FORBIDDEN,
        "the account is no admin yet",
    );

    let granted = post_json(&seeded.app, &token, &group_of("plain", "admin"), None).await;
    common::assert_answer(&granted, StatusCode::OK, "the grant");
    assert_eq!(
        granted.json_value(),
        json!({ "is_admin": true }),
        "one key, the group that moved"
    );
    assert!(
        flag_of(
            &seeded.pool,
            "SELECT is_admin FROM users WHERE id = $1",
            &plain
        )
        .await,
        "the column moved"
    );
    assert!(
        !flag_of(
            &seeded.pool,
            "SELECT is_moderator FROM users WHERE id = $1",
            &plain
        )
        .await,
        "and the other group stayed"
    );

    let now_admin = TestRequest::get(USERS)
        .bearer(&mint(&seeded.pool, &plain, "admin:read:accounts").await)
        .send(&seeded.app)
        .await;
    common::assert_answer(&now_admin, StatusCode::OK, "the grant took effect at once");

    let shown = read(&seeded.app, &seeded.token, &show("plain")).await;
    assert_eq!(
        shown["roles"],
        json!({ "admin": true, "moderator": false }),
        "and the admin entity says so"
    );

    let revoked = delete_json(&seeded.app, &token, &group_of("plain", "admin"), None).await;
    common::assert_answer(&revoked, StatusCode::OK, "the revoke");
    assert_eq!(
        revoked.json_value(),
        json!({ "is_admin": false }),
        "the same one key, cleared"
    );
    assert!(
        !flag_of(
            &seeded.pool,
            "SELECT is_admin FROM users WHERE id = $1",
            &plain
        )
        .await,
        "and the column went back"
    );

    let (grant, _) = one_entry(&seeded.pool, "grant").await;
    assert_eq!(
        grant, "@root made @plain admin",
        "one entry per write, Pleroma's sentence with the group it named"
    );
    let (revoke, _) = one_entry(&seeded.pool, "revoke").await;
    assert_eq!(
        revoke, "@root revoked admin role from @plain",
        "and the revoke's own wording"
    );
}

/// The other group of the pair writes under its own key, touches nothing
/// else on the row, and names its permission in the logged data map; a
/// handle nobody answers to is the same 404 the rest of the module gives.
#[tokio::test]
async fn the_moderator_group_writes_under_its_own_key() {
    let db = common::test_db().await;
    let seeded = seed(&db).await;
    let token = write_token(&seeded).await;
    let plain = plain(&seeded.pool).await;

    let modded = post_json(&seeded.app, &token, &group_of("plain", "moderator"), None).await;
    assert_eq!(
        modded.json_value(),
        json!({ "is_moderator": true }),
        "the other group answers under its own key"
    );

    let revoked = delete_json(&seeded.app, &token, &group_of("plain", "moderator"), None).await;
    assert_eq!(
        revoked.json_value(),
        json!({ "is_moderator": false }),
        "the same one key, cleared"
    );
    assert_eq!(
        tags_of(&seeded.pool, &plain).await,
        Vec::<String>::new(),
        "a role write touches no other column"
    );
    assert!(
        !flag_of(
            &seeded.pool,
            "SELECT is_admin FROM users WHERE id = $1",
            &plain
        )
        .await,
        "and the group it was not asked for stays where it stood"
    );

    let grants = logged(&seeded.pool, "grant").await;
    assert_eq!(
        grants
            .iter()
            .map(|(message, _)| message.as_str())
            .collect::<Vec<_>>(),
        vec!["@root made @plain moderator"],
        "the entry names the group that moved"
    );
    assert_eq!(
        grants[0].1["permission"],
        json!("moderator"),
        "the data map carries the permission"
    );
    assert_eq!(
        grants[0].1["subject"]
            .as_array()
            .expect("the subject list")
            .iter()
            .filter_map(|found| found["nickname"].as_str())
            .collect::<Vec<_>>(),
        vec!["plain"],
        "and the subject map names the account"
    );

    for path in [group_of("ghost", "admin"), group_of("ghost", "moderator")] {
        let miss = if path.ends_with("admin") {
            post_json(&seeded.app, &token, &path, None).await
        } else {
            delete_json(&seeded.app, &token, &path, None).await
        };
        common::assert_answer(&miss, StatusCode::NOT_FOUND, "a name nobody answers to");
        common::assert_error(&miss, NOT_FOUND, "the module's 404");
    }
}

/// A role is this instance's own to give: a remote mirror is the 400 both
/// the grant and the revoke answer, on its handle and on its id, and
/// nothing is written or logged.
#[tokio::test]
async fn no_group_is_set_for_a_remote_account() {
    let db = common::test_db().await;
    let seeded = seed(&db).await;
    let token = write_token(&seeded).await;

    for path in [
        group_of("faraway@remote.example", "admin"),
        group_of(&seeded.mirror.id.to_string(), "moderator"),
    ] {
        let granted = post_json(&seeded.app, &token, &path, None).await;
        common::assert_answer(&granted, StatusCode::BAD_REQUEST, "a mirror grant");
        common::assert_error(&granted, REMOTE_ROLE, "the pinned wording");
        let revoked = delete_json(&seeded.app, &token, &path, None).await;
        common::assert_answer(&revoked, StatusCode::BAD_REQUEST, "a mirror revoke");
        common::assert_error(&revoked, REMOTE_ROLE, "the same words");
    }
    assert!(
        logged(&seeded.pool, "grant").await.is_empty()
            && logged(&seeded.pool, "revoke").await.is_empty(),
        "and no refusal logged anything"
    );
    assert!(
        !flag_of(
            &seeded.pool,
            "SELECT is_admin FROM users WHERE id = $1",
            &seeded.mirror
        )
        .await,
        "the mirror holds nothing"
    );

    let bulk = post_json(
        &seeded.app,
        &token,
        &group_bulk("admin"),
        Some(&json!({"nicknames": ["faraway@remote.example", "plain"]})),
    )
    .await;
    common::assert_answer(
        &bulk,
        StatusCode::OK,
        "a bulk grant with a mirror on the list",
    );
    assert_eq!(
        bulk.json_value(),
        json!({ "is_admin": true }),
        "the list write answers the group either way"
    );
    assert!(
        !flag_of(
            &seeded.pool,
            "SELECT is_admin FROM users WHERE id = $1",
            &seeded.mirror
        )
        .await,
        "the mirror was skipped, not written"
    );
    let plain = plain(&seeded.pool).await;
    assert!(
        flag_of(
            &seeded.pool,
            "SELECT is_admin FROM users WHERE id = $1",
            &plain
        )
        .await,
        "and the local account was written"
    );
    let (grant, subjects) = one_entry(&seeded.pool, "grant").await;
    assert_eq!(
        grant, "@root made @plain admin",
        "the entry names only who moved"
    );
    assert_eq!(subjects, vec!["plain"], "and its subject map says so");
}

/// The self-protection: neither group can be revoked from the acting admin,
/// and the bulk revoke leaves the caller's row alone instead of refusing
/// the whole list — which is the divergence from Pleroma the ticket pins.
#[tokio::test]
async fn an_admin_cannot_revoke_their_own_group() {
    let db = common::test_db().await;
    let seeded = seed(&db).await;
    let token = write_token(&seeded).await;

    for name in ["admin", "moderator"] {
        let refused = delete_json(&seeded.app, &token, &group_of("root", name), None).await;
        common::assert_answer(
            &refused,
            StatusCode::FORBIDDEN,
            &format!("the self-revoke of {name}"),
        );
        common::assert_error(&refused, SELF_REVOKE, "Pleroma's wording");
    }
    let plain = plain(&seeded.pool).await;
    let _granted = post_json(&seeded.app, &token, &group_of("plain", "admin"), None).await;
    let mixed = delete_json(
        &seeded.app,
        &token,
        &group_bulk("admin"),
        Some(&json!({"nicknames": ["root", "plain"]})),
    )
    .await;
    common::assert_answer(
        &mixed,
        StatusCode::OK,
        "the bulk revoke that names the caller",
    );
    assert_eq!(
        mixed.json_value(),
        json!({ "is_admin": false }),
        "Pleroma's update map, as the ticket pins it"
    );
    assert!(
        flag_of(
            &seeded.pool,
            "SELECT is_admin FROM users WHERE id = $1",
            &seeded.root
        )
        .await,
        "the caller's own grant stands"
    );
    assert!(
        !flag_of(
            &seeded.pool,
            "SELECT is_admin FROM users WHERE id = $1",
            &plain
        )
        .await,
        "and everybody else named lost it"
    );
    let (revoke, subjects) = one_entry(&seeded.pool, "revoke").await;
    assert_eq!(
        revoke, "@root revoked admin role from @plain",
        "one entry, naming only the account that moved"
    );
    assert_eq!(subjects, vec!["plain"], "the caller is no subject");
    assert_eq!(
        logged(&seeded.pool, "revoke").await.len(),
        1,
        "and the two refused self-revokes logged nothing"
    );
}

/// The nickname-list grant: everyone named and local joins the group, the
/// unknown handles are skipped inside the same success, and one entry names
/// the whole moved list in the submitted order.
#[tokio::test]
async fn the_bulk_group_grant_writes_to_everyone_named() {
    let db = common::test_db().await;
    let seeded = seed(&db).await;
    let token = write_token(&seeded).await;
    let plain = plain(&seeded.pool).await;
    let moddy = account_named(&seeded.pool, "moddy").await;
    let names = json!({"nicknames": ["plain", "moddy", "ghost"]});

    let granted = post_json(&seeded.app, &token, &group_bulk("moderator"), Some(&names)).await;
    common::assert_answer(&granted, StatusCode::OK, "the bulk grant");
    assert_eq!(
        granted.json_value(),
        json!({ "is_moderator": true }),
        "the group's key, the state it was written to"
    );
    for account in [&plain, &moddy] {
        assert!(
            flag_of(
                &seeded.pool,
                "SELECT is_moderator FROM users WHERE id = $1",
                account
            )
            .await,
            "{} joined the group",
            account.nickname
        );
    }
    let (grant, subjects) = one_entry(&seeded.pool, "grant").await;
    assert_eq!(
        grant, "@root made @plain, @moddy moderator",
        "one entry for the whole list, in the submitted order"
    );
    assert_eq!(
        subjects,
        vec!["plain", "moddy"],
        "the unknown name is not a subject"
    );
}

/// The revoke half of the pair takes the group back the same way — the list
/// may come over the query string too — and a route that states no list or a
/// group off the two writes nothing at all.
#[tokio::test]
async fn the_bulk_group_revoke_takes_the_group_back() {
    let db = common::test_db().await;
    let seeded = seed(&db).await;
    let token = write_token(&seeded).await;
    let plain = plain(&seeded.pool).await;
    let moddy = account_named(&seeded.pool, "moddy").await;
    let names = json!({"nicknames": ["plain", "moddy"]});
    let setup = post_json(&seeded.app, &token, &group_bulk("moderator"), Some(&names)).await;
    common::assert_answer(&setup, StatusCode::OK, "the grant this test undoes");

    let revoked = delete_json(
        &seeded.app,
        &token,
        &format!(
            "{}?nicknames[]=plain&nicknames[]=moddy",
            group_bulk("moderator")
        ),
        None,
    )
    .await;
    common::assert_answer(
        &revoked,
        StatusCode::OK,
        "the bulk revoke over the query string",
    );
    assert_eq!(
        revoked.json_value(),
        json!({ "is_moderator": false }),
        "the same answer, cleared"
    );
    for account in [&plain, &moddy] {
        assert!(
            !flag_of(
                &seeded.pool,
                "SELECT is_moderator FROM users WHERE id = $1",
                account
            )
            .await,
            "{} left the group",
            account.nickname
        );
    }
    let (revoke, _) = one_entry(&seeded.pool, "revoke").await;
    assert_eq!(
        revoke, "@root revoked moderator role from @plain, @moddy",
        "and the revoke names both"
    );

    let bare = post_json(&seeded.app, &token, &group_bulk("admin"), None).await;
    common::assert_answer(
        &bare,
        StatusCode::UNPROCESSABLE_ENTITY,
        "no nicknames at all",
    );
    let bogus = post_json(
        &seeded.app,
        &token,
        &group_bulk("owner"),
        Some(&json!({"nicknames": []})),
    )
    .await;
    common::assert_answer(&bogus, StatusCode::NOT_FOUND, "a group off the two");
    common::assert_error(&bogus, NO_SUCH_GROUP, "Pleroma's wording");
    assert!(
        !flag_of(
            &seeded.pool,
            "SELECT is_admin FROM users WHERE id = $1",
            &seeded.actor
        )
        .await,
        "and the refused writes granted nobody — the instance actor included"
    );
}

/// `PATCH /users/suggest` sets the flag on every account it resolved and
/// answers the moved rows as admin entities under the one `users` key — the
/// order the request named, the miss gone — and lands Pleroma's
/// `add_suggestion` sentence.
#[tokio::test]
async fn suggesting_users_answers_the_moved_accounts() {
    let db = common::test_db().await;
    let seeded = seed(&db).await;
    let token = write_token(&seeded).await;
    let body = json!({"nicknames": ["plain", "suspended", "ghost", "beeper"]});

    let suggested = patch_json(&seeded.app, &token, SUGGEST, &body).await;
    common::assert_answer(&suggested, StatusCode::OK, "the suggest write");
    let answered = suggested.json_value();
    assert_eq!(
        answered
            .as_object()
            .expect("an object")
            .keys()
            .collect::<Vec<_>>(),
        vec!["users"],
        "the one key Pleroma's index.json clause renders"
    );
    assert_eq!(
        answered_users(&answered),
        vec!["plain", "suspended", "beeper"],
        "the accounts that were found, in the order the request named them, the miss gone"
    );
    for entry in answered["users"].as_array().expect("the users array") {
        assert_eq!(
            entry["is_suggested"],
            json!(true),
            "each answer carries the flag the write set: {entry}"
        );
        assert!(
            entry["roles"].is_object() && entry["created_at"].is_string(),
            "and the answer is the admin entity, not a bare account: {entry}"
        );
    }

    let (added, added_subjects) = one_entry(&seeded.pool, "add_suggestion").await;
    assert_eq!(
        added, "@root added suggested users: @plain, @suspended, @beeper",
        "Pleroma's add_suggestion sentence"
    );
    assert_eq!(
        added_subjects,
        vec!["plain", "suspended", "beeper"],
        "and its subject list"
    );
}

/// The listings follow the flag both ways: `PATCH /users/suggest` puts the
/// live local picks into `/api/v1/suggestions` and leaves the deactivated
/// one out of it though its row is set, and `PATCH /users/unsuggest` empties
/// the pool again while answering the rows as they now stand.
#[tokio::test]
async fn the_suggestion_pool_follows_the_flag_both_ways() {
    let db = common::test_db().await;
    let seeded = seed(&db).await;
    let token = write_token(&seeded).await;
    let viewer = common::create_local_user(&seeded.pool, "watcher").await;
    let watching = mint(&seeded.pool, &viewer, "read").await;
    let names = json!({"nicknames": ["plain", "suspended", "beeper"]});
    let suggested = patch_json(&seeded.app, &token, SUGGEST, &names).await;
    common::assert_answer(&suggested, StatusCode::OK, "the suggest write");

    let pool = suggested_ids(&read(&seeded.app, &watching, "/api/v1/suggestions").await);
    let plain = plain(&seeded.pool).await;
    let beeper = account_named(&seeded.pool, "beeper").await;
    let suspended = account_named(&seeded.pool, "suspended").await;
    assert!(
        pool.contains(&plain.id.to_string()) && pool.contains(&beeper.id.to_string()),
        "both live local picks are in the listing the pool answers: {pool:?}"
    );
    assert!(
        !pool.contains(&suspended.id.to_string()),
        "the flagged-but-deactivated account is not, even though its flag is set"
    );
    assert!(
        flag_of(
            &seeded.pool,
            "SELECT is_suggested FROM users WHERE id = $1",
            &suspended
        )
        .await,
        "and the write did reach its row"
    );

    let unsuggested = patch_json(
        &seeded.app,
        &token,
        UNSUGGEST,
        &json!({"nicknames": ["plain", "beeper"]}),
    )
    .await;
    common::assert_answer(&unsuggested, StatusCode::OK, "the unsuggest write");
    assert_eq!(
        answered_users(&unsuggested.json_value()),
        vec!["plain", "beeper"],
        "the same pair of accounts, now leaving"
    );
    assert_eq!(
        unsuggested.json_value()["users"][0]["is_suggested"],
        json!(false),
        "answered as the row stands after the write"
    );
    let emptied = suggested_ids(&read(&seeded.app, &watching, "/api/v1/suggestions").await);
    assert_eq!(
        emptied,
        Vec::<String>::new(),
        "the pool the viewer sees is empty again"
    );

    let (removed, _) = one_entry(&seeded.pool, "remove_suggestion").await;
    assert_eq!(
        removed, "@root removed suggested users: @plain, @beeper",
        "and the pair's other sentence"
    );
}

/// The suggest pair writes nothing it was not asked to: a body that states no
/// list is the 422, the read scope is the write class's 403, and neither
/// leaves an entry behind.
#[tokio::test]
async fn a_suggest_write_refuses_what_it_cannot_write() {
    let db = common::test_db().await;
    let seeded = seed(&db).await;
    let token = write_token(&seeded).await;
    let body = json!({"nicknames": ["plain"]});

    let bare = patch_json(&seeded.app, &token, SUGGEST, &json!({})).await;
    common::assert_answer(
        &bare,
        StatusCode::UNPROCESSABLE_ENTITY,
        "a body with no list",
    );
    assert!(
        bare.text().contains("nicknames"),
        "and the 422 names the key it needs: {}",
        bare.text()
    );

    let refused = patch_json(&seeded.app, &seeded.token, UNSUGGEST, &body).await;
    common::assert_answer(
        &refused,
        StatusCode::FORBIDDEN,
        "the read scope must not write",
    );
    common::assert_error(&refused, NO_ADMIN_WRITE, "in the write class's words");
    assert!(
        logged(&seeded.pool, "add_suggestion").await.is_empty()
            && logged(&seeded.pool, "remove_suggestion").await.is_empty(),
        "and the refused writes left no entry behind"
    );
}

/// The account the shared fixture seeded, re-read from behind the row.
async fn account_named(pool: &PgPool, nickname: &str) -> User {
    wired_be::models::user::get_by_nickname(pool, nickname)
        .await
        .expect("the account reads back")
        .expect("the fixture seeded it")
}

// ===== WBE-196: the account lifecycle — create, delete, activation, approval =====

/// The bulk activation PATCH route.
const ACTIVATE: &str = "/api/v1/pleroma/admin/users/activate";
/// The bulk deactivation PATCH route.
const DEACTIVATE: &str = "/api/v1/pleroma/admin/users/deactivate";
/// The bulk approval PATCH route.
const APPROVE: &str = "/api/v1/pleroma/admin/users/approve";
/// A password every lifecycle fixture and created account uses, over the
/// test config's minimum length.
const PASSWORD: &str = "a long enough pass";
/// The 403 body of every bearer request a deactivated account makes.
const DISABLED: &str = "Your account is currently disabled";

/// The toggle route of one account.
fn toggle_path(nickname: &str) -> String {
    format!("{USERS}/{nickname}/toggle_activation")
}

/// A `users` body creating one account per `(nickname, email, password)`
/// triple, in the order stated.
fn create_body(items: &[(&str, &str, &str)]) -> Value {
    json!({
        "users": items
            .iter()
            .map(|(nickname, email, password)| json!({
                "nickname": nickname,
                "email": email,
                "password": password,
            }))
            .collect::<Vec<Value>>(),
    })
}

/// The OAuth `password` grant body for `username`, over the app's
/// credentials.
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

/// The three lifecycle flags one account carries: activation, confirmation
/// and the staff gate.
async fn lifecycle_flags(pool: &PgPool, nickname: &str) -> (bool, bool, bool) {
    sqlx::query_as::<_, (bool, bool, bool)>(
        "SELECT is_active, email_confirmed, approval_pending FROM users WHERE nickname = $1",
    )
    .bind(nickname)
    .fetch_one(pool)
    .await
    .expect("the flags read back")
}

/// Whether any account answers to `nickname`.
async fn exists(pool: &PgPool, nickname: &str) -> bool {
    sqlx::query_scalar::<_, bool>("SELECT EXISTS (SELECT 1 FROM users WHERE nickname = $1)")
        .bind(nickname)
        .fetch_one(pool)
        .await
        .expect("the existence read runs")
}

/// The arguments of every job on `queue` of `kind`, oldest first.
async fn jobs_of(pool: &PgPool, queue: &str, kind: &str) -> Vec<Value> {
    sqlx::query_as::<_, (Value,)>(
        "SELECT args FROM jobs WHERE queue = $1 AND kind = $2 ORDER BY inserted_at",
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

/// How many live bearer tokens `account` holds.
async fn token_count(pool: &PgPool, account: &User) -> i64 {
    sqlx::query_scalar::<_, i64>("SELECT count(*) FROM oauth_tokens WHERE user_id = $1")
        .bind(account.id)
        .fetch_one(pool)
        .await
        .expect("the token count reads back")
}

/// Whether `account` carries a deletion stamp.
async fn deletion_stamped(pool: &PgPool, account: &User) -> bool {
    sqlx::query_scalar::<_, bool>("SELECT deleted_at IS NOT NULL FROM users WHERE id = $1")
        .bind(account.id)
        .fetch_one(pool)
        .await
        .expect("the stamp reads back")
}

/// The admin-created account is active, confirmed and approved on an
/// instance that demands confirmation AND approval: the registration
/// queues no letter for it, its row clears the staff gate the mode sets,
/// it signs in at once, and the batch logs Pleroma's one `create` entry
/// with its plural `subjects` key.
#[tokio::test]
async fn admin_created_accounts_bypass_every_gate() {
    let db = common::test_db().await;
    let pool = db.pool().clone();
    let mut config = mail_config();
    config.registrations_approval_required = wired_be::config::Toggle::new(true);
    config.account_activation_required = wired_be::config::Toggle::new(true);
    let app = common::test_app_with_config(pool.clone(), config);
    let root = common::create_local_user_with(&pool, "root", |user| NewUser {
        is_admin: Flag::new(true),
        email: Some("root@example.org".to_owned()),
        ..user
    })
    .await;
    let token = mint(&pool, &root, "admin:write:accounts").await;

    let response = post_json(
        &app,
        &token,
        USERS,
        Some(&create_body(&[
            ("fresh", "fresh@example.org", PASSWORD),
            ("other", "other@example.org", PASSWORD),
        ])),
    )
    .await;
    common::assert_answer(&response, StatusCode::OK, "the bulk create");
    assert_eq!(
        response.json_value(),
        json!([
            {"code": 200, "type": "success",
             "data": {"email": "fresh@example.org", "nickname": "fresh"}},
            {"code": 200, "type": "success",
             "data": {"email": "other@example.org", "nickname": "other"}},
        ]),
        "one success entry per item, in the order the request stated them"
    );

    for nickname in ["fresh", "other"] {
        let (active, confirmed, pending) = lifecycle_flags(&pool, nickname).await;
        assert!(
            active && confirmed && !pending,
            "{nickname} is active, confirmed and approved whatever the modes: {active} {confirmed} {pending}"
        );
    }
    assert!(
        jobs_of(&pool, "mailer", "mailer.send").await.is_empty(),
        "activation mode queued no confirmation letter for an admin-created account"
    );

    let client = common::create_app(&pool, "read").await;
    let login = TestRequest::post("/oauth/token")
        .json(&grant_body(
            &client.client_id,
            &client.client_secret,
            "fresh",
        ))
        .send(&app)
        .await;
    common::assert_answer(
        &login,
        StatusCode::OK,
        "the created account signs straight in",
    );
    assert!(
        !login.json_value()["access_token"].is_null(),
        "and the grant hands out its credential"
    );

    let entries = logged(&pool, "create").await;
    assert_eq!(entries.len(), 1, "one create entry for the whole batch");
    assert_eq!(
        entries[0].0, "@root created users: @fresh, @other",
        "Pleroma's create sentence"
    );
    let named: Vec<&str> = entries[0].1["subjects"]
        .as_array()
        .expect("the plural subjects key Pleroma's clause reads")
        .iter()
        .filter_map(|found| found["nickname"].as_str())
        .collect();
    assert_eq!(named, vec!["fresh", "other"], "both created accounts");
    assert!(
        entries[0].1.get("subject").is_none(),
        "and the entry keeps Pleroma's spelling of the key"
    );
}

/// A mixed batch answers per item, and stays per item: the one valid
/// account is stored and logged while the duplicate, the malformed
/// address and the short password each report their own 400 refusal —
/// where Pleroma rolls the whole batch back into one 409, the ticket pins
/// the always-200 per-item answer.
#[tokio::test]
async fn the_create_answers_every_item_for_itself() {
    let db = common::test_db().await;
    let seeded = seed(&db).await;
    let token = write_token(&seeded).await;

    let response = post_json(
        &seeded.app,
        &token,
        USERS,
        Some(&create_body(&[
            ("goodone", "goodone@example.org", PASSWORD),
            ("plain", "dup@example.org", PASSWORD),
            ("wrong", "not-an-email", PASSWORD),
            ("shorty", "shorty@example.org", "pw"),
        ])),
    )
    .await;
    common::assert_answer(&response, StatusCode::OK, "the mixed batch");
    let items = response.json_value();
    let items = items.as_array().expect("four items");
    assert_eq!(items.len(), 4, "one entry per submitted item");

    assert_eq!(items[0]["code"], 200, "the valid item stored");
    assert_eq!(items[0]["type"], "success");
    for (index, needle, who) in [
        (1, "has already been taken", "the duplicate handle"),
        (2, "has invalid format", "the malformed address"),
        (3, "should be at least", "the too-short password"),
    ] {
        assert_eq!(items[index]["code"], 400, "{who} reports the pinned code");
        assert_eq!(items[index]["type"], "error", "{who} is an error item");
        assert!(
            items[index]["error"]
                .as_str()
                .unwrap_or_default()
                .contains(needle),
            "{who} says why: {}",
            items[index]["error"]
        );
    }
    assert_eq!(
        items[1]["data"]["nickname"], "plain",
        "the refusal names what was submitted"
    );
    assert_eq!(
        items[2]["data"]["email"], "not-an-email",
        "handle and address alike"
    );

    assert!(
        exists(&seeded.pool, "goodone").await,
        "the good item is stored"
    );
    let (active, confirmed, pending) = lifecycle_flags(&seeded.pool, "goodone").await;
    assert!(
        active && confirmed && !pending,
        "on the default open instance the bypass is the registration's own plain state"
    );
    assert!(
        !exists(&seeded.pool, "wrong").await,
        "the refused items created nothing"
    );
    assert!(!exists(&seeded.pool, "shorty").await, "every one of them");

    let entries = logged(&seeded.pool, "create").await;
    assert_eq!(entries.len(), 1, "one entry for the batch");
    assert_eq!(
        entries[0].0, "@root created users: @goodone",
        "naming only the created"
    );
}

/// The fifty-account cap refuses the oversized batch whole, before any
/// item is created and with no entry logged.
#[tokio::test]
async fn the_create_caps_the_batch_at_fifty() {
    let db = common::test_db().await;
    let seeded = seed(&db).await;
    let token = write_token(&seeded).await;
    let before = sqlx::query_scalar::<_, i64>("SELECT count(*) FROM users")
        .fetch_one(&seeded.pool)
        .await
        .expect("the count reads");

    let users: Vec<Value> = (0..51)
        .map(|index| {
            json!({
                "nickname": format!("batch{index}"),
                "email": format!("batch{index}@example.org"),
                "password": PASSWORD,
            })
        })
        .collect();
    let response = post_json(&seeded.app, &token, USERS, Some(&json!({ "users": users }))).await;
    common::assert_answer(&response, StatusCode::UNPROCESSABLE_ENTITY, "a batch of 51");
    assert!(
        response.text().contains("50"),
        "and the 422 states the cap: {}",
        response.text()
    );

    let after = sqlx::query_scalar::<_, i64>("SELECT count(*) FROM users")
        .fetch_one(&seeded.pool)
        .await
        .expect("the count reads");
    assert_eq!(after, before, "nothing was created");
    assert!(
        logged(&seeded.pool, "create").await.is_empty(),
        "and the refused batch left no entry"
    );
}

/// The admin's delete starts each named account's own deletion exactly as
/// `POST /api/pleroma/delete_account` starts it — marked, revoked, swept
/// on the `background` queue — answers the started nicknames, ignores the
/// caller's own account and the misses, and the `nicknames[]` arrives in
/// the body OR the query string.
#[tokio::test]
async fn admin_delete_starts_the_sweep_of_every_other_account() {
    let db = common::test_db().await;
    let seeded = seed(&db).await;
    let token = write_token(&seeded).await;
    let plain = account_named(&seeded.pool, "plain").await;
    let _plain_token = mint(&seeded.pool, &plain, "read").await;

    let response = TestRequest::delete(USERS)
        .bearer(&token)
        .json(&json!({"nicknames": ["plain", "ghost", "root"]}))
        .send(&seeded.app)
        .await;
    common::assert_answer(&response, StatusCode::OK, "the delete");
    assert_eq!(
        response.json_value(),
        json!(["plain"]),
        "the started nicknames: the miss skipped, the caller ignored"
    );

    let sweeps = jobs_of(&seeded.pool, "background", "users.delete").await;
    assert_eq!(sweeps.len(), 1, "one sweep queued");
    assert_eq!(
        sweeps[0]["user_id"],
        plain.id.to_string(),
        "and it names plain's row, on the queue account deletion uses"
    );
    assert!(
        deletion_stamped(&seeded.pool, &plain).await,
        "the account carries its deletion stamp"
    );
    let (active, _, _) = lifecycle_flags(&seeded.pool, "plain").await;
    assert!(!active, "and it is inactive");
    assert_eq!(
        token_count(&seeded.pool, &plain).await,
        0,
        "every token went"
    );
    let root = account_named(&seeded.pool, "root").await;
    assert!(
        !deletion_stamped(&seeded.pool, &root).await && token_count(&seeded.pool, &root).await > 0,
        "the caller's own account stands untouched, tokens included"
    );

    // The same call through the query string, bracketed.
    let again = TestRequest::delete(&format!("{USERS}?nicknames[]=beeper&nicknames[]=ghost"))
        .bearer(&token)
        .send(&seeded.app)
        .await;
    common::assert_answer(&again, StatusCode::OK, "the query-string delete");
    assert_eq!(again.json_value(), json!(["beeper"]), "one started name");
    assert_eq!(
        jobs_of(&seeded.pool, "background", "users.delete")
            .await
            .len(),
        2,
        "and its own sweep job"
    );

    let entries = logged(&seeded.pool, "delete").await;
    let messages: Vec<&str> = entries
        .iter()
        .map(|(message, _)| message.as_str())
        .collect();
    assert_eq!(
        messages,
        vec![
            "@root deleted users: @plain",
            "@root deleted users: @beeper"
        ],
        "Pleroma's delete sentence, once per request"
    );
}

/// The `password` grant for `username`, sent.
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

/// Deactivation blocks the account — its bearer requests with the auth
/// gate's own 403, its login with the password grant's disabled refusal —
/// while revoking no token; the caller's own row and the unknown handle
/// are skipped inside the same success, and the moved account answers as
/// the admin entity with Pleroma's `deactivate` entry behind it.
#[tokio::test]
async fn deactivation_blocks_the_account_and_skips_the_caller() {
    let db = common::test_db().await;
    let pool = db.pool().clone();
    let app = common::test_app(pool.clone());
    let root = common::create_local_user_with(&pool, "root", |user| NewUser {
        is_admin: Flag::new(true),
        ..user
    })
    .await;
    let sleepy = common::create_user_with_password(&pool, "sleepy", PASSWORD).await;
    let sleepy_token = mint(&pool, &sleepy, "read").await;
    let client = common::create_app(&pool, "read").await;
    let token = mint(&pool, &root, "admin:write:accounts").await;

    let off = patch_json(
        &app,
        &token,
        DEACTIVATE,
        &json!({"nicknames": ["sleepy", "root", "ghost"]}),
    )
    .await;
    common::assert_answer(&off, StatusCode::OK, "the deactivation");
    let body = off.json_value();
    assert_eq!(
        answered_users(&body),
        vec!["sleepy"],
        "the moved account only: the ghost is skipped inside the same 200"
    );
    assert_eq!(
        body["users"][0]["is_active"], false,
        "answered as deactivated"
    );
    let (sleeping, _, _) = lifecycle_flags(&pool, "sleepy").await;
    assert!(!sleeping, "the flag moved");
    let (root_active, _, _) = lifecycle_flags(&pool, "root").await;
    assert!(
        root_active,
        "the caller cannot deactivate itself, even by name"
    );
    assert_eq!(
        token_count(&pool, &sleepy).await,
        1,
        "deactivation revokes no token"
    );

    let denied = TestRequest::get("/api/v1/accounts/verify_credentials")
        .bearer(&sleepy_token)
        .send(&app)
        .await;
    common::assert_answer(&denied, StatusCode::FORBIDDEN, "the bearer gate's own 403");
    common::assert_error(&denied, DISABLED, "in the auth gate's words");

    let refused = login(&app, &client.client_id, &client.client_secret, "sleepy").await;
    common::assert_answer(&refused, StatusCode::FORBIDDEN, "the deactivated login");
    assert_eq!(
        refused.json_value()["identifier"],
        "account_is_disabled",
        "with the password grant's own reason code"
    );

    let (message, subjects) = one_entry(&pool, "deactivate").await;
    assert_eq!(
        message, "@root deactivated users: @sleepy",
        "Pleroma's sentence"
    );
    assert_eq!(
        subjects,
        vec!["sleepy"],
        "the caller's own name left the subject list"
    );
}

/// Activation is the same write with the flag set: it answers the entity
/// active, logs the `activate` entry, and the account that was blocked
/// signs in and calls the API again — the deactivation's un-revoked token
/// works once more, the flag being the whole gate.
#[tokio::test]
async fn activation_frees_the_account_it_releases() {
    let db = common::test_db().await;
    let pool = db.pool().clone();
    let app = common::test_app(pool.clone());
    let root = common::create_local_user_with(&pool, "root", |user| NewUser {
        is_admin: Flag::new(true),
        ..user
    })
    .await;
    let sleepy = common::create_user_with_password(&pool, "sleepy", PASSWORD).await;
    let sleepy_token = mint(&pool, &sleepy, "read").await;
    let client = common::create_app(&pool, "read").await;
    let token = mint(&pool, &root, "admin:write:accounts").await;
    let _off = patch_json(&app, &token, DEACTIVATE, &json!({"nicknames": ["sleepy"]})).await;

    let on = patch_json(&app, &token, ACTIVATE, &json!({"nicknames": ["sleepy"]})).await;
    common::assert_answer(&on, StatusCode::OK, "the activation");
    assert_eq!(
        on.json_value()["users"][0]["is_active"],
        true,
        "answered as active"
    );
    let granted = login(&app, &client.client_id, &client.client_secret, "sleepy").await;
    common::assert_answer(&granted, StatusCode::OK, "the freed account signs in again");

    let verify = TestRequest::get("/api/v1/accounts/verify_credentials")
        .bearer(&sleepy_token)
        .send(&app)
        .await;
    common::assert_answer(
        &verify,
        StatusCode::OK,
        "and the token the deactivation left behind works again",
    );

    let (message, subjects) = one_entry(&pool, "activate").await;
    assert_eq!(
        message, "@root activated users: @sleepy",
        "Pleroma's sentence"
    );
    assert_eq!(subjects, vec!["sleepy"]);
}

/// The single-account toggle flips the flag both ways — by nickname and,
/// like the show route, by id — answers that account's own admin entity
/// carrying the new state, logs the move under the bulk pair's actions,
/// and sends the miss and the wrong-scoped token to 404 and 403 without
/// touching the row.
#[tokio::test]
async fn toggle_flips_one_account_and_logs_the_move() {
    let db = common::test_db().await;
    let seeded = seed(&db).await;
    let token = write_token(&seeded).await;
    let suspended = account_named(&seeded.pool, "suspended").await;

    let on = TestRequest::patch(&toggle_path("suspended"))
        .bearer(&token)
        .send(&seeded.app)
        .await;
    common::assert_answer(&on, StatusCode::OK, "the wake-up");
    let entity = on.json_value();
    assert_eq!(
        entity["id"],
        suspended.id.to_string(),
        "the one account's entity"
    );
    assert_eq!(entity["nickname"], "suspended");
    assert_eq!(entity["is_active"], true, "carrying the state it moved to");

    let off = TestRequest::patch(&toggle_path(&suspended.id.to_string()))
        .bearer(&token)
        .send(&seeded.app)
        .await;
    common::assert_answer(&off, StatusCode::OK, "the toggle by id, back to sleep");
    assert_eq!(
        off.json_value()["is_active"],
        false,
        "the id reaches the same row"
    );

    let (on_message, on_subjects) = one_entry(&seeded.pool, "activate").await;
    assert_eq!(
        on_message, "@root activated users: @suspended",
        "Pleroma logs the toggle's activation as an activate"
    );
    assert_eq!(on_subjects, vec!["suspended"]);
    let (off_message, off_subjects) = one_entry(&seeded.pool, "deactivate").await;
    assert_eq!(
        off_message, "@root deactivated users: @suspended",
        "and the deactivation likewise"
    );
    assert_eq!(off_subjects, vec!["suspended"]);

    let miss = TestRequest::patch(&toggle_path("ghost"))
        .bearer(&token)
        .send(&seeded.app)
        .await;
    common::assert_answer(&miss, StatusCode::NOT_FOUND, "no account named ghost");
    common::assert_error(&miss, NOT_FOUND, "the module's 404");

    let wrong_scope = TestRequest::patch(&toggle_path("plain"))
        .bearer(&seeded.token)
        .send(&seeded.app)
        .await;
    common::assert_answer(
        &wrong_scope,
        StatusCode::FORBIDDEN,
        "the read scope must not write",
    );
    common::assert_error(&wrong_scope, NO_ADMIN_WRITE, "in the write class's words");
    let (still_active, _, _) = lifecycle_flags(&seeded.pool, "plain").await;
    assert!(still_active, "and the refused toggle wrote nothing");
}

/// Approval clears the staff gate for the pending accounts, mails each
/// one that holds an address the approval letter, mints nothing, answers
/// every named account that exists — and leaves the already-approved one
/// unwritten and unmailed when the request runs a second time.
#[tokio::test]
async fn approve_opens_the_pending_accounts_and_mails_them() {
    let db = common::test_db().await;
    let seeded = seed(&db).await;
    let app = common::test_app_with_config(seeded.pool.clone(), mail_config());
    let token = write_token(&seeded).await;
    let _waiting = common::create_local_user_with(&seeded.pool, "waiting", |user| NewUser {
        approval_pending: Flag::new(true),
        email: Some("waiting@example.org".to_owned()),
        ..user
    })
    .await;

    let response = patch_json(
        &app,
        &token,
        APPROVE,
        &json!({"nicknames": ["waiting", "newbie", "plain", "ghost"]}),
    )
    .await;
    common::assert_answer(&response, StatusCode::OK, "the approval");
    let body = response.json_value();
    assert_eq!(
        answered_users(&body),
        vec!["waiting", "newbie", "plain"],
        "every named account that exists, in order, the miss skipped"
    );
    for index in 0..3 {
        assert_eq!(
            body["users"][index]["is_approved"], true,
            "answered as approved, moved now or before"
        );
    }

    for nickname in ["waiting", "newbie"] {
        let (_, _, pending) = lifecycle_flags(&seeded.pool, nickname).await;
        assert!(!pending, "{nickname} is off the approval list");
    }
    let waiting = account_named(&seeded.pool, "waiting").await;
    assert_eq!(
        token_count(&seeded.pool, &waiting).await,
        0,
        "and the approval minted no credential"
    );

    let letters = jobs_of(&seeded.pool, "mailer", "mailer.send").await;
    assert_eq!(
        letters.len(),
        1,
        "one letter: plain was not approved now, and newbie has no address"
    );
    assert_eq!(
        letters[0]["to"],
        json!("waiting@example.org"),
        "to the newly approved"
    );
    assert!(
        letters[0]["subject"]
            .as_str()
            .unwrap_or_default()
            .contains("approved"),
        "the approval letter: {}",
        letters[0]["subject"]
    );

    let (message, subjects) = one_entry(&seeded.pool, "approve").await;
    assert_eq!(
        message, "@root approved users: @waiting, @newbie, @plain",
        "Pleroma's approve sentence, naming every resolved subject"
    );
    assert_eq!(subjects, vec!["waiting", "newbie", "plain"], "moved or not");
}

/// Approving an already-approved account changes nothing and mails
/// nothing — Pleroma's `approve(%User{is_approved: true})` clause hands
/// the user back untouched — yet the answer still carries its entity and
/// the write still logs its entry.
#[tokio::test]
async fn approving_an_already_approved_account_sends_no_letter() {
    let db = common::test_db().await;
    let seeded = seed(&db).await;
    let app = common::test_app_with_config(seeded.pool.clone(), mail_config());
    let token = write_token(&seeded).await;

    let again = patch_json(&app, &token, APPROVE, &json!({"nicknames": ["plain"]})).await;
    common::assert_answer(&again, StatusCode::OK, "approval of the already approved");
    assert_eq!(
        answered_users(&again.json_value()),
        vec!["plain"],
        "it answers unchanged"
    );
    assert!(
        jobs_of(&seeded.pool, "mailer", "mailer.send")
            .await
            .is_empty(),
        "no letter for an account nobody just approved"
    );
    let (message, subjects) = one_entry(&seeded.pool, "approve").await;
    assert_eq!(
        message, "@root approved users: @plain",
        "and still the entry"
    );
    assert_eq!(subjects, vec!["plain"]);
}

/// One gated lifecycle route: its name for the assertions, the HTTP verb,
/// the path, and the JSON body it needs (`None` for the body-less toggle).
type LifecycleRoute<'a> = (&'a str, &'a str, &'a str, Option<&'a Value>);

/// One expected refusal: who presents which credential (no bearer at all
/// for `None`) and the status and message the gate answers.
type Refusal<'a> = (&'a str, Option<&'a str>, StatusCode, &'a str);

/// Sends one lifecycle request, asserting the gate refused it with the
/// pinned status and message before anything ran.
async fn refuse(app: &Router, route: LifecycleRoute<'_>, refusal: Refusal<'_>) {
    let (what, verb, path, body) = route;
    let (who, token, status, message) = refusal;
    let base = match verb {
        "POST" => TestRequest::post(path),
        "DELETE" => TestRequest::delete(path),
        _ => TestRequest::patch(path),
    };
    let with_body = match body {
        Some(value) => base.json(value),
        None => base,
    };
    let with_auth = match token {
        Some(bearer) => with_body.bearer(bearer),
        None => with_body,
    };
    let response = with_auth.send(app).await;
    common::assert_answer(&response, status, &format!("{what} from {who}"));
    common::assert_error(&response, message, &format!("{what} from {who}"));
}

/// Every lifecycle route stands behind the same gate as the rest of the
/// module: no token is the 401, a NON-admin holding `admin:write:accounts`
/// is the 403 "User is not an admin.", and an admin token of the wrong
/// scope or the wrong area is the 403 naming `admin:write` — nobody
/// outside the accounts gate moves an account, and nothing lands while
/// the gate is closed.
#[tokio::test]
async fn the_lifecycle_writes_guard_their_gate() {
    let db = common::test_db().await;
    let seeded = seed(&db).await;
    let member = common::create_local_user_with(&seeded.pool, "member", |user| user).await;
    let member_token = mint(&seeded.pool, &member, "admin:write:accounts").await;
    let other_area = mint(&seeded.pool, &seeded.root, "admin:write:reports").await;
    let names = json!({"nicknames": ["plain"]});
    let gate_body = create_body(&[("gate1", "gate1@example.org", PASSWORD)]);
    let toggle = toggle_path("plain");

    let routes: [LifecycleRoute<'_>; 6] = [
        ("POST create", "POST", USERS, Some(&gate_body)),
        ("DELETE users", "DELETE", USERS, Some(&names)),
        ("PATCH activate", "PATCH", ACTIVATE, Some(&names)),
        ("PATCH deactivate", "PATCH", DEACTIVATE, Some(&names)),
        ("PATCH approve", "PATCH", APPROVE, Some(&names)),
        ("PATCH toggle", "PATCH", &toggle, None),
    ];
    let refusals: [Refusal<'_>; 4] = [
        ("no token at all", None, StatusCode::UNAUTHORIZED, NO_TOKEN),
        (
            "a NON-admin holding the exact scope",
            Some(member_token.as_str()),
            StatusCode::FORBIDDEN,
            NOT_ADMIN,
        ),
        (
            "the admin's read scope",
            Some(seeded.token.as_str()),
            StatusCode::FORBIDDEN,
            NO_ADMIN_WRITE,
        ),
        (
            "the admin's other write area",
            Some(other_area.as_str()),
            StatusCode::FORBIDDEN,
            NO_ADMIN_WRITE,
        ),
    ];

    for route in routes {
        for refusal in refusals {
            refuse(&seeded.app, route, refusal).await;
        }
    }

    assert!(
        !exists(&seeded.pool, "gate1").await,
        "and every refused request wrote nothing"
    );
    let (plain_active, _, plain_pending) = lifecycle_flags(&seeded.pool, "plain").await;
    assert!(
        plain_active && !plain_pending,
        "plain's row stands as the fixture left it"
    );
    for action in ["create", "delete", "activate", "deactivate", "approve"] {
        assert!(
            logged(&seeded.pool, action).await.is_empty(),
            "the closed gate logged no {action} entry"
        );
    }
    assert!(
        jobs_of(&seeded.pool, "background", "users.delete")
            .await
            .is_empty(),
        "and queued no sweep"
    );
}
