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

/// The reset-token route of one account.
fn issued_reset(nickname: &str) -> String {
    format!("{USERS}/{nickname}/password_reset")
}

/// The forced-reset PATCH route.
const FORCE_RESET: &str = "/api/v1/pleroma/admin/users/password_reset";
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
/// The completion form the issued link points at.
const RESET_FORM: &str = "/api/v1/pleroma/password_reset";
/// The 403 body of a token that grants neither `admin:write` nor
/// `admin:write:accounts`.
const NO_ADMIN_WRITE: &str = "Insufficient permissions: admin:write.";
/// The 403 `error` of the password grant refused to a forced-reset account.
const RESET_REQUIRED: &str = "Password reset is required";

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
    ]
}

/// The credentials pair joins the set for the ROLE gate: both endpoints
/// answer 401/403 by role and scope before anything else.
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

    for route in maintenance_routes().into_iter().chain(credentials_routes()) {
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
        for route in maintenance_routes()
            .into_iter()
            .chain([credentials_patch()])
        {
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

    for route in maintenance_routes() {
        let refused = request(&route, Some(&seeded.token)).send(&seeded.app).await;
        common::assert_answer(
            &refused,
            StatusCode::FORBIDDEN,
            &format!("the read scope must not run {} {}", route.verb, route.path),
        );
        common::assert_error(&refused, NO_ADMIN_WRITE, "in the write class's words");
    }
}
