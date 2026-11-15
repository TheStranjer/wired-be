//! The admin users endpoints over HTTP (`/api/v1/pleroma/admin/users`,
//! WBE-195): Pleroma's `AdminAPI.UserController` `index` and `show`, the
//! eight `filters`, the `query`/`name`/`email`/`tags[]`/`actor_types[]`
//! criteria, the `page`/`page_size` window with its `count`, the admin
//! account entity, and the `admin:read:accounts` gate in front of all of it.
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
