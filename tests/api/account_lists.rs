//! Integration tests for the account-listing endpoints (WBE-124): the
//! profile directory (`GET /api/v1/directory`, no auth required, with its
//! `offset`/`limit`/`order`/`local` parameters), the staff suggestions
//! (`GET /api/v1/suggestions` as bare accounts and `GET /api/v2/suggestions`
//! source-wrapped, both excluding the viewer and everyone they follow,
//! block, mute or have dismissed through
//! `DELETE /api/v1/suggestions/{account_id}`), the familiar-followers batch
//! (`GET /api/v1/accounts/familiar_followers`) and Pleroma's birthday list
//! (`GET /api/v1/pleroma/birthdays`: today's UTC parts by default, `day` and
//! `month` to name another day, and a leap day that matches nothing else).
//! Auth, scope, not-found, validation and page-size failures are pinned for
//! every route.

#![cfg(test)]

use axum::{Router, http::StatusCode};
use chrono::{Datelike as _, NaiveDate, Utc};
use serde_json::{Value, json};
use sqlx::PgPool;
use uuid::Uuid;
use wired_be::{
    config::TEST_BASE_URL,
    models::{
        block, follow,
        mute::{self, MuteOptions},
        suggestion,
        user::{Flag, User},
    },
};

use crate::common;

/// The instance origin the fixtures live on (`Config::for_tests`).
const BASE: &str = TEST_BASE_URL;
/// The 401 body `AuthUser` renders when no token was presented at all.
const NO_TOKEN: &str = "Invalid credentials";
/// The 401 body for a token that does not resolve.
const INVALID_TOKEN: &str = "The access token is invalid";
/// The 403 body an app-only token gets from the user-requiring extractor.
const USER_REQUIRED: &str = "This API requires an authenticated user";
/// The 403 body of a listing whose token lacks the `read` root scope.
const NO_READ: &str = "Insufficient permissions: read.";
/// The 403 body of a read whose token lacks `read:accounts`.
const NO_READ_ACCOUNTS: &str = "Insufficient permissions: read:accounts.";
/// The 403 body of a read whose token lacks `read:follows`.
const NO_READ_FOLLOWS: &str = "Insufficient permissions: read:follows.";
/// The 403 body of the dismissal whose token lacks `write:accounts`.
const NO_WRITE_ACCOUNTS: &str = "Insufficient permissions: write:accounts.";
/// The 404 body the dismissal route answers for an account it cannot find.
const NOT_FOUND: &str = "Record not found";
/// The 404 body of a path no area claims.
const NOT_IMPLEMENTED: &str = "Not implemented";
/// The 400 body of an unreadable month.
const BAD_MONTH: &str = "Invalid parameter: month";
/// The 400 body of an unreadable day.
const BAD_DAY: &str = "Invalid parameter: day";
/// The familiar-followers batch path, without its parameters.
const FAMILIAR_PATH: &str = "/api/v1/accounts/familiar_followers";
/// Pleroma's birthday list path.
const BIRTHDAYS_PATH: &str = "/api/v1/pleroma/birthdays";

/// A local user named `nickname` plus a bearer token granting `scopes`.
async fn fixture(pool: &PgPool, nickname: &str, scopes: &str) -> (User, String) {
    let created = common::create_local_user(pool, nickname).await;
    let token = common::create_app_and_token(pool, &created, scopes).await;
    (created, token)
}

/// A local user named `nickname` with `birthday` set to `year`/`month`/`day`
/// and shown to strangers — the account the birthday list is built from.
async fn born(pool: &PgPool, nickname: &str, year: i32, month: u32, day: u32) -> User {
    common::create_local_user_with(pool, nickname, move |mut new| {
        new.birthday = NaiveDate::from_ymd_opt(year, month, day);
        new.show_birthday = Flag::new(true);
        new
    })
    .await
}

/// A discoverable local user named `nickname` whose newest status was posted
/// `days_ago` days before now (`None`: never posted).
async fn discoverable(pool: &PgPool, nickname: &str, days_ago: Option<i64>) -> User {
    common::create_local_user_with(pool, nickname, move |mut new| {
        new.discoverable = Flag::new(true);
        new.last_status_at = days_ago.map(|days| Utc::now() - chrono::Duration::days(days));
        new
    })
    .await
}

/// A local user named `nickname` an admin put in the suggestion pool.
async fn suggested(pool: &PgPool, nickname: &str) -> User {
    common::create_local_user_with(pool, nickname, |mut new| {
        new.is_suggested = Flag::new(true);
        new
    })
    .await
}

/// A `GET` of `path` with `token` (`None` anonymous).
async fn get(app: &Router, token: Option<&str>, path: &str) -> common::TestResponse {
    let request = common::TestRequest::get(path);
    match token {
        Some(found) => request.bearer(found).send(app).await,
        None => request.send(app).await,
    }
}

/// A `DELETE` of `path` with `token`.
async fn delete(app: &Router, token: &str, path: &str) -> common::TestResponse {
    common::TestRequest::delete(path)
        .bearer(token)
        .send(app)
        .await
}

/// The `id` of every entity of an array response, in order: the accounts of
/// a listing, and the entries of a batch alike.
fn listed_ids(body: &Value) -> Vec<String> {
    body.as_array()
        .expect("the answer is an array")
        .iter()
        .map(|entry| {
            entry["id"]
                .as_str()
                .expect("an id on every entry")
                .to_owned()
        })
        .collect()
}

/// The `account.id` of every v2 suggestion entry, in order.
fn wrapped_ids(body: &Value) -> Vec<String> {
    body.as_array()
        .expect("the v2 listing is an array")
        .iter()
        .map(|entry| {
            entry["account"]["id"]
                .as_str()
                .expect("a wrapped account id")
                .to_owned()
        })
        .collect()
}

/// The ids of `users`, newest first: the order `ORDER BY id DESC` answers,
/// which for `UUIDv7`s minted inside one millisecond is their byte order and
/// not their insert order (decision 4).
fn newest_first(ids: impl IntoIterator<Item = Uuid>) -> Vec<String> {
    let mut names: Vec<String> = ids.into_iter().map(|id| id.to_string()).collect();
    names.sort_by(|left, right| right.cmp(left));
    names
}

/// Records `follower`'s accepted follow of the open local account `target`.
async fn seed_follow(pool: &PgPool, follower: &User, target: &User) {
    let row = follow::follow(pool, BASE, follower, target)
        .await
        .expect("the fixture follow records");
    assert_eq!(
        row.state,
        follow::FollowState::Accepted,
        "an open local account answers on the spot"
    );
}

/// Deactivates `target`, the state a closed account is in.
async fn deactivate(pool: &PgPool, target: &User) {
    let gone = sqlx::query("UPDATE users SET is_active = false WHERE id = $1")
        .bind(target.id)
        .execute(pool)
        .await
        .expect("the fixture deactivates");
    assert_eq!(gone.rows_affected(), 1, "the row was there to close");
}

/// Opt `target` into discovery, the profile setting the directory reads and
/// no fixture endpoint in this suite writes.
async fn make_discoverable(pool: &PgPool, target: &User) {
    let changed = sqlx::query("UPDATE users SET discoverable = true WHERE id = $1")
        .bind(target.id)
        .execute(pool)
        .await
        .expect("the fixture opts the account into discovery");
    assert_eq!(changed.rows_affected(), 1, "the row was there to flag");
}

/// Moves `target`'s `inserted_at` back `days` days, so an `order=new`
/// expectation does not depend on how fast the fixture loop ran.
async fn backdate(pool: &PgPool, target: &User, days: i64) {
    let moved = sqlx::query(
        "UPDATE users SET inserted_at = now() - $1::bigint * interval '1 day' WHERE id = $2",
    )
    .bind(days)
    .bind(target.id)
    .execute(pool)
    .await
    .expect("the fixture backdates the account");
    assert_eq!(moved.rows_affected(), 1, "the row was there to move");
}

/// The directory's answer to `query`, asserted to be a plain 200.
async fn directory(app: &Router, query: &str) -> Value {
    let answer = get(app, None, &format!("/api/v1/directory{query}")).await;
    common::assert_answer(&answer, StatusCode::OK, "the anonymous directory");
    answer.json_value()
}

/// The batch path asking about every id in `ids`, spelled the way Mastodon
/// clients spell `id[]`.
fn familiar_path(ids: &[String]) -> String {
    let joined: Vec<String> = ids.iter().map(|id| format!("id%5B%5D={id}")).collect();
    format!("{FAMILIAR_PATH}?{}", joined.join("&"))
}

/// `GET /api/v1/directory` needs no token and answers the discoverable,
/// active accounts: newest activity first, the never-posted ones last, and
/// nothing that opted out or closed down.
#[tokio::test]
async fn directory_lists_discoverable_accounts_without_auth() {
    let db = common::test_db().await;
    let pool = db.pool().clone();
    let newest = discoverable(&pool, "newest", Some(1)).await;
    let older = discoverable(&pool, "older", Some(5)).await;
    let quiet = discoverable(&pool, "quiet", None).await;
    let private = common::create_local_user(&pool, "private").await;
    let closed = discoverable(&pool, "closed", Some(0)).await;
    deactivate(&pool, &closed).await;
    let app = common::test_app(pool);

    let listed = directory(&app, "").await;
    assert_eq!(
        listed_ids(&listed),
        vec![
            newest.id.to_string(),
            older.id.to_string(),
            quiet.id.to_string()
        ],
        "newest status first, the account that never posted last"
    );
    assert!(
        !listed_ids(&listed).contains(&private.id.to_string()),
        "an account that never opted into discovery is not listed"
    );
    assert!(
        !listed_ids(&listed).contains(&closed.id.to_string()),
        "and neither is a deactivated one"
    );
}

/// One directory entry is an `Account` entity: the stored profile, rendered
/// for an anonymous viewer, so `pleroma.relationship` is the empty object.
#[tokio::test]
async fn directory_answers_account_entities() {
    let db = common::test_db().await;
    let pool = db.pool().clone();
    let alice = common::create_local_user_with(&pool, "alice", |mut new| {
        new.discoverable = Flag::new(true);
        new.name = "Alice".to_owned();
        new
    })
    .await;
    let app = common::test_app(pool);

    let listed = directory(&app, "").await;
    let entries = listed.as_array().expect("the listing is an array");
    assert_eq!(entries.len(), 1, "the one discoverable account");
    let account = &entries[0];
    assert_eq!(account["id"], alice.id.to_string(), "the id");
    assert_eq!(account["username"], "alice", "the handle");
    assert_eq!(account["acct"], "alice", "and the local acct");
    assert_eq!(account["display_name"], "Alice", "the display name");
    assert_eq!(account["url"], format!("{BASE}/users/alice"), "the url");
    assert_eq!(
        account["pleroma"]["relationship"],
        json!({}),
        "an anonymous request has no relationship to embed"
    );
}

/// The `order` and `local` parameters steer the listing: `new` sorts on the
/// account itself, `local=true` drops the remote mirrors, and any other
/// spelling keeps Pleroma's default of local accounts plus mirrors.
#[tokio::test]
async fn directory_order_and_local_flags_narrow_the_listing() {
    let db = common::test_db().await;
    let pool = db.pool().clone();
    let veteran = discoverable(&pool, "veteran", Some(1)).await;
    let fresh = discoverable(&pool, "fresh", Some(9)).await;
    backdate(&pool, &veteran, 30).await;
    backdate(&pool, &fresh, 1).await;
    let mirror = common::create_remote_user(&pool, "mirror", "remote.example").await;
    make_discoverable(&pool, &mirror).await;
    let app = common::test_app(pool);
    let by_activity = vec![
        veteran.id.to_string(),
        fresh.id.to_string(),
        mirror.id.to_string(),
    ];

    assert_eq!(
        listed_ids(&directory(&app, "").await),
        by_activity,
        "with no `order` the activity sort runs, and it lists the mirror too"
    );
    assert_eq!(
        listed_ids(&directory(&app, "?order=bogus").await),
        by_activity,
        "an unrecognised spelling is the default order"
    );
    assert_eq!(
        listed_ids(&directory(&app, "?order=new").await),
        vec![
            mirror.id.to_string(),
            fresh.id.to_string(),
            veteran.id.to_string()
        ],
        "`new` puts the newest account first, ignoring last activity: the \
         fresh mirror, then the year-old veteran"
    );
    assert_eq!(
        listed_ids(&directory(&app, "?local=true").await),
        vec![veteran.id.to_string(), fresh.id.to_string()],
        "`local=true` answers this instance's own accounts only"
    );
    assert_eq!(
        listed_ids(&directory(&app, "?local=false").await),
        by_activity,
        "and `local=false` is the default: mirrors stay in"
    );
}

/// The directory pages by `offset` and honours `limit`, so walking the same
/// order twice covers every account exactly once.
#[tokio::test]
async fn directory_pages_by_limit_and_offset() {
    let db = common::test_db().await;
    let pool = db.pool().clone();
    let mut expected = Vec::new();
    for (number, nickname) in ["one", "two", "three"].iter().enumerate() {
        let days_ago = i64::try_from(number).expect("a small index") + 1;
        let posted = discoverable(&pool, nickname, Some(days_ago)).await;
        expected.push(posted.id.to_string());
    }
    let app = common::test_app(pool);

    assert_eq!(
        listed_ids(&directory(&app, "?limit=2").await),
        expected[..2],
        "`limit=2` answers the head of the listing"
    );
    assert_eq!(
        listed_ids(&directory(&app, "?limit=2&offset=2").await),
        expected[2..],
        "and `offset=2` picks up where it stopped"
    );
    assert_eq!(
        listed_ids(&directory(&app, "?offset=99").await),
        Vec::<String>::new(),
        "an offset past the end answers nothing"
    );
    assert_eq!(
        listed_ids(&directory(&app, "?limit=-3").await),
        expected,
        "an unusable limit falls back to the default page of 40"
    );
}

/// The directory never requires a token, but it never ignores a bad one
/// either: an invalid credential is the extractor's 401, and a valid one
/// only decides whose view the entities render as.
#[tokio::test]
async fn directory_accepts_an_anonymous_request_and_refuses_a_bad_token() {
    let db = common::test_db().await;
    let pool = db.pool().clone();
    let (_, token) = fixture(&pool, "watcher", "read").await;
    let listed = discoverable(&pool, "listed", Some(1)).await;
    let app = common::test_app(pool);
    let open = get(&app, None, "/api/v1/directory").await;
    common::assert_answer(&open, StatusCode::OK, "the directory with no token");
    assert_eq!(
        listed_ids(&open.json_value()),
        vec![listed.id.to_string()],
        "the anonymous listing is the listing"
    );

    let with_token = get(&app, Some(&token), "/api/v1/directory").await;
    common::assert_answer(
        &with_token,
        StatusCode::OK,
        "a token the directory does not need",
    );
    assert_eq!(
        listed_ids(&with_token.json_value()),
        listed_ids(&open.json_value()),
        "a viewer who is not in the listing sees the same accounts"
    );
    common::assert_all_refused(
        &[get(&app, Some("not-a-token"), "/api/v1/directory").await],
        StatusCode::UNAUTHORIZED,
        INVALID_TOKEN,
        "a bad token on an open listing is refused, not ignored",
    );
}

/// Both suggestion versions answer the staff pool — suggested, local, active
/// accounts — v1 as bare `Account` entities and v2 with each account wrapped
/// in the source object it came from.
#[tokio::test]
async fn suggestions_answer_the_staff_pool_in_both_shapes() {
    let db = common::test_db().await;
    let pool = db.pool().clone();
    let (alice, token) = fixture(&pool, "alice", "read").await;
    let older = suggested(&pool, "older").await;
    let newer = suggested(&pool, "newer").await;
    let expected = newest_first([older.id, newer.id]);
    let plain = common::create_local_user(&pool, "plain").await;
    let mirror = suggested(&pool, "mirror").await;
    let unlocal = sqlx::query("UPDATE users SET local = false WHERE id = $1")
        .bind(mirror.id)
        .execute(&pool)
        .await
        .expect("the fixture becomes a mirror");
    assert_eq!(
        unlocal.rows_affected(),
        1,
        "the flagged pick is now a remote mirror"
    );
    let closed = suggested(&pool, "closed").await;
    deactivate(&pool, &closed).await;
    let app = common::test_app(pool);

    let v1 = get(&app, Some(&token), "/api/v1/suggestions").await;
    common::assert_answer(&v1, StatusCode::OK, "the v1 listing");
    let v1_body = v1.json_value();
    assert_eq!(
        listed_ids(&v1_body),
        expected,
        "the pool, newest account first, as plain accounts"
    );
    let mut names: Vec<&str> = v1_body
        .as_array()
        .expect("the v1 listing is an array")
        .iter()
        .filter_map(|account| account["username"].as_str())
        .collect();
    names.sort_unstable();
    assert_eq!(
        names,
        vec!["newer", "older"],
        "every entry is a plain account entity, rendered from the pool"
    );

    let v2 = get(&app, Some(&token), "/api/v2/suggestions").await;
    common::assert_answer(&v2, StatusCode::OK, "the v2 listing");
    let v2_body = v2.json_value();
    assert_eq!(wrapped_ids(&v2_body), expected, "the same two accounts");
    for entry in v2_body.as_array().expect("the v2 listing is an array") {
        assert_eq!(entry["source"], "staff", "every suggestion is staff-made");
        assert_eq!(
            entry["sources"],
            json!(["staff"]),
            "v2 spells the source as a list too"
        );
        assert!(
            entry["account"]["id"].is_string() && entry["account"]["username"].is_string(),
            "and the account rides inside the wrapper: {entry}"
        );
    }
    for absent in [plain.id, mirror.id, closed.id, alice.id] {
        assert!(
            !listed_ids(&v1_body).contains(&absent.to_string()),
            "{absent} is in nobody's pool: an unflagged, non-local, \
             deactivated or self account is out"
        );
    }
}

/// The pool is the viewer's own: nobody they follow, block, mute or have
/// dismissed shows up, and one viewer's state changes nobody else's list.
#[tokio::test]
async fn suggestions_exclude_every_relationship_the_viewer_has() {
    let db = common::test_db().await;
    let pool = db.pool().clone();
    let (alice, alice_token) = fixture(&pool, "alice", "read").await;
    let (_, carol_token) = fixture(&pool, "carol", "read").await;
    let followed = suggested(&pool, "followed").await;
    let blocked = suggested(&pool, "blocked").await;
    let muted = suggested(&pool, "muted").await;
    let dismissed = suggested(&pool, "dismissed").await;
    let kept = suggested(&pool, "kept").await;

    seed_follow(&pool, &alice, &followed).await;
    let block_row = block::block(&pool, BASE, &alice, &blocked)
        .await
        .expect("the fixture block stores");
    assert_eq!(
        block_row.blocked_id, blocked.id,
        "the block is alice's on `blocked`"
    );
    let mute_row = mute::mute(&pool, &alice, &muted, MuteOptions::default())
        .await
        .expect("the fixture mute stores");
    assert_eq!(
        mute_row.muted_id, muted.id,
        "the mute is alice's on `muted`"
    );
    suggestion::dismiss(&pool, alice.id, dismissed.id)
        .await
        .expect("the fixture dismissal stores");
    let app = common::test_app(pool);

    let mine = get(&app, Some(&alice_token), "/api/v1/suggestions")
        .await
        .json_value();
    assert_eq!(
        listed_ids(&mine),
        vec![kept.id.to_string()],
        "only the account alice has no relationship with is left"
    );
    let theirs = get(&app, Some(&carol_token), "/api/v1/suggestions")
        .await
        .json_value();
    assert_eq!(
        listed_ids(&theirs).len(),
        5,
        "carol sees the whole pool: alice's relationships are her own business"
    );
}

/// `limit` bounds the suggestion page on both versions, and an unreadable
/// `limit` falls back to the default 40 rather than failing.
#[tokio::test]
async fn suggestions_honour_the_limit_on_both_versions() {
    let db = common::test_db().await;
    let pool = db.pool().clone();
    let (_, token) = fixture(&pool, "alice", "read").await;
    let mut picks = Vec::new();
    for nickname in ["one", "two", "three"] {
        let pick = suggested(&pool, nickname).await;
        picks.push(pick.id);
    }
    let expected = newest_first(picks);
    let app = common::test_app(pool);

    let capped = get(&app, Some(&token), "/api/v1/suggestions?limit=2")
        .await
        .json_value();
    assert_eq!(
        listed_ids(&capped),
        expected[..2],
        "`limit=2` answers the head of the pool"
    );
    let capped_v2 = get(&app, Some(&token), "/api/v2/suggestions?limit=2")
        .await
        .json_value();
    assert_eq!(
        wrapped_ids(&capped_v2),
        expected[..2],
        "v2 pages the same head of the pool"
    );
    let whole = get(&app, Some(&token), "/api/v1/suggestions?limit=nope")
        .await
        .json_value();
    assert_eq!(
        listed_ids(&whole),
        expected,
        "an unreadable limit is the default page of 40, which holds all three"
    );
}

/// The suggestion routes need a user token that grants `read` (both
/// versions) or `write:accounts` (the dismissal); every other credential is
/// refused the way Pleroma's plugs refuse it.
#[tokio::test]
async fn suggestion_routes_demand_their_scope() {
    let db = common::test_db().await;
    let pool = db.pool().clone();
    let (alice, _) = fixture(&pool, "alice", "read").await;
    let narrow = common::create_app_and_token(&pool, &alice, "read:statuses").await;
    let write_only = common::create_app_and_token(&pool, &alice, "write:accounts").await;
    let read_only = common::create_app_and_token(&pool, &alice, "read:accounts").await;
    let app_only = common::create_app_only_token(&pool, "read").await;
    let app = common::test_app(pool);
    let dismiss_path = format!("/api/v1/suggestions/{id}", id = alice.id);

    common::assert_all_refused(
        &[
            get(&app, None, "/api/v1/suggestions").await,
            get(&app, None, "/api/v2/suggestions").await,
            delete(&app, "", &dismiss_path).await,
        ],
        StatusCode::UNAUTHORIZED,
        NO_TOKEN,
        "the suggestion routes present no credential at all",
    );
    common::assert_all_refused(
        &[
            get(&app, Some("nope"), "/api/v1/suggestions").await,
            get(&app, Some("nope"), "/api/v2/suggestions").await,
            delete(&app, "nope", &dismiss_path).await,
        ],
        StatusCode::UNAUTHORIZED,
        INVALID_TOKEN,
        "and a credential that does not resolve, rather than proceeding anonymously",
    );
    common::assert_all_refused(
        &[
            get(&app, Some(&app_only), "/api/v1/suggestions").await,
            get(&app, Some(&app_only), "/api/v2/suggestions").await,
            delete(&app, &app_only, &dismiss_path).await,
        ],
        StatusCode::FORBIDDEN,
        USER_REQUIRED,
        "the suggestion routes on a credential that names no user",
    );
    common::assert_all_refused(
        &[
            get(&app, Some(&narrow), "/api/v1/suggestions").await,
            get(&app, Some(&narrow), "/api/v2/suggestions").await,
        ],
        StatusCode::FORBIDDEN,
        NO_READ,
        "a `read:statuses` token does not grant the `read` root scope",
    );
    common::assert_all_refused(
        &[
            get(&app, Some(&write_only), "/api/v1/suggestions").await,
            get(&app, Some(&write_only), "/api/v2/suggestions").await,
        ],
        StatusCode::FORBIDDEN,
        NO_READ,
        "and a write-only token cannot read the pool",
    );
    common::assert_all_refused(
        &[
            delete(&app, &narrow, &dismiss_path).await,
            delete(&app, &read_only, &dismiss_path).await,
        ],
        StatusCode::FORBIDDEN,
        NO_WRITE_ACCOUNTS,
        "the dismissal needs `write:accounts`",
    );
}

/// `DELETE /api/v1/suggestions/{account_id}` records the dismissal and
/// answers the empty object; it is per-viewer, idempotent, and never clears
/// the account's own flag, because the pool is the admins' to curate.
#[tokio::test]
async fn dismissing_a_suggestion_is_per_viewer_and_keeps_the_flag() {
    let db = common::test_db().await;
    let pool = db.pool().clone();
    let (alice, alice_token) = fixture(&pool, "alice", "read write").await;
    let (_, carol_token) = fixture(&pool, "carol", "read").await;
    let pick = suggested(&pool, "pick").await;
    let app = common::test_app(pool.clone());
    let path = format!("/api/v1/suggestions/{id}", id = pick.id);

    let answer = delete(&app, &alice_token, &path).await;
    common::assert_answer(&answer, StatusCode::OK, "the dismissal");
    assert_eq!(answer.text(), "{}", "the answer is the empty JSON object");
    assert_eq!(
        suggestion::dismissed_ids(&pool, alice.id).await.unwrap(),
        vec![pick.id],
        "the pair is what got stored"
    );

    let gone = get(&app, Some(&alice_token), "/api/v1/suggestions")
        .await
        .json_value();
    assert_eq!(gone, json!([]), "alice stops seeing the account");
    let still = get(&app, Some(&carol_token), "/api/v1/suggestions")
        .await
        .json_value();
    assert_eq!(
        listed_ids(&still),
        vec![pick.id.to_string()],
        "and carol keeps seeing it, because she dismissed nothing"
    );

    let again = delete(&app, &alice_token, &path).await;
    common::assert_answer(&again, StatusCode::OK, "dismissing twice");
    assert_eq!(again.text(), "{}", "with the same answer");
    let rows: i64 = sqlx::query_scalar("SELECT count(*) FROM dismissed_suggestions")
        .fetch_one(&pool)
        .await
        .expect("the count reads");
    assert_eq!(rows, 1, "and still one row");

    let stored: bool = sqlx::query_scalar("SELECT is_suggested FROM users WHERE id = $1")
        .bind(pick.id)
        .fetch_one(&pool)
        .await
        .expect("the flag reads back");
    assert!(stored, "the account stays in the pool itself");
}

/// The dismissal resolves its account like every other route keyed on an id:
/// a malformed id, an unknown one and a deactivated account are all 404, and
/// a refusal records nothing.
#[tokio::test]
async fn dismissing_an_unknown_account_is_not_found() {
    let db = common::test_db().await;
    let pool = db.pool().clone();
    let (_, token) = fixture(&pool, "alice", "read write").await;
    let closed = suggested(&pool, "closed").await;
    deactivate(&pool, &closed).await;
    let app = common::test_app(pool.clone());

    for path in [
        "/api/v1/suggestions/not-an-id".to_owned(),
        format!("/api/v1/suggestions/{id}", id = Uuid::now_v7()),
        format!("/api/v1/suggestions/{id}", id = closed.id),
    ] {
        common::assert_all_refused(
            &[delete(&app, &token, &path).await],
            StatusCode::NOT_FOUND,
            NOT_FOUND,
            &path,
        );
    }
    let rows: i64 = sqlx::query_scalar("SELECT count(*) FROM dismissed_suggestions")
        .fetch_one(&pool)
        .await
        .expect("the count reads");
    assert_eq!(rows, 0, "and no refusal recorded a dismissal");
}

/// `GET /api/v1/accounts/familiar_followers` answers one entry per *known*
/// requested id, in the order asked, skipping the values that are not ids
/// here; an entry holds the viewer's follows that follow that account.
#[tokio::test]
async fn familiar_followers_answers_one_entry_per_known_id() {
    let db = common::test_db().await;
    let pool = db.pool().clone();
    let (alice, token) = fixture(&pool, "alice", "read:follows").await;
    let target = common::create_local_user(&pool, "target").await;
    let other = common::create_local_user(&pool, "other").await;
    let friend = common::create_local_user(&pool, "friend").await;
    let stranger = common::create_local_user(&pool, "stranger").await;
    seed_follow(&pool, &alice, &friend).await;
    seed_follow(&pool, &friend, &target).await;
    seed_follow(&pool, &stranger, &target).await;
    seed_follow(&pool, &friend, &other).await;
    let app = common::test_app(pool);

    let answer = get(
        &app,
        Some(&token),
        &familiar_path(&[
            target.id.to_string(),
            "not-an-id".to_owned(),
            Uuid::now_v7().to_string(),
            other.id.to_string(),
        ]),
    )
    .await;
    common::assert_answer(&answer, StatusCode::OK, "the familiar batch");
    let body = answer.json_value();
    assert_eq!(
        listed_ids(&body),
        vec![target.id.to_string(), other.id.to_string()],
        "the two known ids in the order asked, the junk and the unknown one skipped"
    );
    assert_eq!(
        listed_ids(&body[0]["accounts"]),
        vec![friend.id.to_string()],
        "the one account alice follows that follows the target, and not \
         the stranger who follows it too"
    );
    assert_eq!(
        listed_ids(&body[1]["accounts"]),
        vec![friend.id.to_string()],
        "friend follows both, so both entries name them"
    );

    let empty = get(&app, Some(&token), FAMILIAR_PATH).await;
    common::assert_answer(&empty, StatusCode::OK, "a batch with no ids");
    assert_eq!(
        empty.json_value(),
        json!([]),
        "an absent `id` is the empty array, never an error"
    );
    let single = get(
        &app,
        Some(&token),
        &format!("{FAMILIAR_PATH}?id={}", target.id),
    )
    .await
    .json_value();
    assert_eq!(
        listed_ids(&single),
        vec![target.id.to_string()],
        "a lone `id=` is read too, Pleroma's List.wrap"
    );
}

/// The two privacy rules of the batch: an account that hides its followers
/// answers the empty set to everyone but itself, and an account that hides
/// its own follows is never named as a familiar follower.
#[tokio::test]
async fn familiar_followers_respects_the_hidden_collections() {
    let db = common::test_db().await;
    let pool = db.pool().clone();
    let (alice, alice_token) = fixture(&pool, "alice", "read:follows").await;
    let hidden = common::create_local_user_with(&pool, "hidden", |mut new| {
        new.hide_followers = Flag::new(true);
        new
    })
    .await;
    let private = common::create_local_user_with(&pool, "private", |mut new| {
        new.hide_follows = Flag::new(true);
        new
    })
    .await;
    let open = common::create_local_user(&pool, "open").await;
    seed_follow(&pool, &alice, &private).await;
    seed_follow(&pool, &private, &hidden).await;
    seed_follow(&pool, &alice, &open).await;
    seed_follow(&pool, &open, &hidden).await;
    // The target follows both of them too, so both are its own familiar
    // followers and only the collection flags tell the two views apart.
    seed_follow(&pool, &hidden, &private).await;
    seed_follow(&pool, &hidden, &open).await;
    let hidden_token = common::create_app_and_token(&pool, &hidden, "read:follows").await;
    let app = common::test_app(pool);
    let path = familiar_path(&[hidden.id.to_string()]);

    let stranger = get(&app, Some(&alice_token), &path).await.json_value();
    assert_eq!(
        listed_ids(&stranger[0]["accounts"]),
        Vec::<String>::new(),
        "a target that hides its followers answers nobody else"
    );

    let own = get(&app, Some(&hidden_token), &path).await.json_value();
    assert_eq!(
        listed_ids(&own[0]["accounts"]),
        vec![open.id.to_string()],
        "asked about themself the target sees its followers, and the one of \
         them that hides its follows is still never named"
    );
}

/// A familiar set is capped at twenty: the batch answers twenty of a
/// twenty-one-strong crowd, never all of it.
#[tokio::test]
async fn familiar_followers_caps_the_set_at_twenty() {
    let db = common::test_db().await;
    let pool = db.pool().clone();
    let (alice, token) = fixture(&pool, "alice", "read:follows").await;
    let target = common::create_local_user(&pool, "target").await;
    for number in 0..21 {
        let friend = common::create_local_user(&pool, &format!("friend-{number}")).await;
        seed_follow(&pool, &alice, &friend).await;
        seed_follow(&pool, &friend, &target).await;
    }
    let app = common::test_app(pool);

    let body = get(&app, Some(&token), &familiar_path(&[target.id.to_string()]))
        .await
        .json_value();
    assert_eq!(
        body[0]["accounts"]
            .as_array()
            .expect("an account array")
            .len(),
        20,
        "twenty of the twenty-one familiar followers"
    );
}

/// The batch is a `read:follows` endpoint behind `AuthUser`: no token and an
/// invalid one are 401s, a token that cannot read follows is the scope 403,
/// an app-only token is the user-required 403, and the `read` root passes.
#[tokio::test]
async fn familiar_followers_demands_read_follows() {
    let db = common::test_db().await;
    let pool = db.pool().clone();
    let (alice, _) = fixture(&pool, "alice", "read").await;
    let narrow = common::create_app_and_token(&pool, &alice, "read:accounts").await;
    let broad = common::create_app_and_token(&pool, &alice, "read").await;
    let app_only = common::create_app_only_token(&pool, "read:follows").await;
    let app = common::test_app(pool);
    let path = format!("{FAMILIAR_PATH}?id={}", Uuid::now_v7());

    common::assert_answer(
        &get(&app, None, &path).await,
        StatusCode::UNAUTHORIZED,
        "the batch without a token",
    );
    common::assert_all_refused(
        &[get(&app, Some("nope"), &path).await],
        StatusCode::UNAUTHORIZED,
        INVALID_TOKEN,
        "and on one that does not resolve",
    );
    common::assert_all_refused(
        &[get(&app, Some(&narrow), &path).await],
        StatusCode::FORBIDDEN,
        NO_READ_FOLLOWS,
        "a token that cannot read follows",
    );
    common::assert_all_refused(
        &[get(&app, Some(&app_only), &path).await],
        StatusCode::FORBIDDEN,
        USER_REQUIRED,
        "an app-only token names no viewer to read as",
    );
    common::assert_answer(
        &get(&app, Some(&broad), &path).await,
        StatusCode::OK,
        "the `read` root scope covers `read:follows`",
    );
}

/// `GET /api/v1/pleroma/birthdays` answers the viewer and the accounts of
/// their accepted follows whose birthday falls on the asked-for day, and
/// only when they chose to show it.
#[tokio::test]
async fn birthdays_list_the_viewer_and_their_follows_on_the_day() {
    let db = common::test_db().await;
    let pool = db.pool().clone();
    let alice = born(&pool, "alice", 1990, 5, 17).await;
    let token = common::create_app_and_token(&pool, &alice, "read:accounts").await;
    let same_day = born(&pool, "same-day", 1988, 5, 17).await;
    let other_day = born(&pool, "other-day", 1988, 5, 18).await;
    let undisclosed = common::create_local_user_with(&pool, "undisclosed", |mut new| {
        new.birthday = NaiveDate::from_ymd_opt(1988, 5, 17);
        new
    })
    .await;
    let unfollowed = born(&pool, "unfollowed", 1988, 5, 17).await;
    let closed = born(&pool, "closed", 1988, 5, 17).await;
    deactivate(&pool, &closed).await;
    for known in [&same_day, &other_day, &undisclosed] {
        seed_follow(&pool, &alice, known).await;
    }
    let expected = newest_first([alice.id, same_day.id]);
    let app = common::test_app(pool);

    let answer = get(
        &app,
        Some(&token),
        &format!("{BIRTHDAYS_PATH}?month=5&day=17"),
    )
    .await;
    common::assert_answer(&answer, StatusCode::OK, "the birthday listing");
    let body = answer.json_value();
    assert_eq!(
        listed_ids(&body),
        expected,
        "the viewer and the one follow born that day, newest account first"
    );
    for absent in [other_day.id, undisclosed.id, unfollowed.id, closed.id] {
        assert!(
            !listed_ids(&body).contains(&absent.to_string()),
            "{absent} is out: another day, an undisclosed birthday, nobody \
             the viewer follows, and a closed account"
        );
    }
}

/// With no parameters the list asks about today's UTC date, and a `day` or
/// `month` on its own replaces only the part it names.
#[tokio::test]
async fn birthdays_default_to_today_in_utc() {
    let db = common::test_db().await;
    let pool = db.pool().clone();
    let (alice, token) = fixture(&pool, "alice", "read:accounts").await;
    let today = Utc::now().date_naive();
    let year = today.year();
    let other_day = if today.day() == 1 { 2 } else { today.day() - 1 };
    let party = born(&pool, "party", year, today.month(), today.day()).await;
    let shifted = born(&pool, "shifted", year, today.month(), other_day).await;
    seed_follow(&pool, &alice, &party).await;
    seed_follow(&pool, &alice, &shifted).await;
    let app = common::test_app(pool);

    let now = get(&app, Some(&token), BIRTHDAYS_PATH).await.json_value();
    assert_eq!(
        listed_ids(&now),
        vec![party.id.to_string()],
        "today's birthday is on the list with no parameters at all, and the \
         viewer is absent because alice named no birthday"
    );
    let wrong_month = if today.month() == 1 {
        12
    } else {
        today.month() - 1
    };
    let moved = get(
        &app,
        Some(&token),
        &format!("{BIRTHDAYS_PATH}?month={wrong_month}"),
    )
    .await
    .json_value();
    assert_eq!(
        listed_ids(&moved),
        Vec::<String>::new(),
        "`month` alone keeps today's day and asks about another month"
    );
    let by_day = get(
        &app,
        Some(&token),
        &format!("{BIRTHDAYS_PATH}?day={other_day}"),
    )
    .await
    .json_value();
    assert_eq!(
        listed_ids(&by_day),
        vec![shifted.id.to_string()],
        "and `day` alone asks about another day of this month"
    );
}

/// A `29 February` birthday matches `month=2&day=29` and nothing else: no
/// other year, month or day ever finds it.
#[tokio::test]
async fn birthdays_match_a_leap_day_only_on_the_leap_day() {
    let db = common::test_db().await;
    let pool = db.pool().clone();
    let (alice, token) = fixture(&pool, "alice", "read:accounts").await;
    let leaper = born(&pool, "leaper", 2000, 2, 29).await;
    assert!(
        leaper.birthday.is_some(),
        "2000 was a leap year, so the fixture birthday was stored"
    );
    seed_follow(&pool, &alice, &leaper).await;
    let app = common::test_app(pool);

    let on = get(
        &app,
        Some(&token),
        &format!("{BIRTHDAYS_PATH}?month=2&day=29"),
    )
    .await
    .json_value();
    assert_eq!(
        listed_ids(&on),
        vec![leaper.id.to_string()],
        "2/29 finds the account born on it"
    );
    for query in ["month=2&day=28", "month=3&day=29", "day=29", "month=2"] {
        let miss = get(&app, Some(&token), &format!("{BIRTHDAYS_PATH}?{query}"))
            .await
            .json_value();
        assert!(
            !listed_ids(&miss).contains(&leaper.id.to_string()),
            "`{query}` does not find the leap-day account"
        );
    }
}

/// The birthday route needs a user token granting `read:accounts`, and its
/// two parameters are checked before anything is read.
#[tokio::test]
async fn birthdays_demand_read_accounts_and_valid_parts() {
    let db = common::test_db().await;
    let pool = db.pool().clone();
    let (alice, token) = fixture(&pool, "alice", "read:accounts").await;
    let narrow = common::create_app_and_token(&pool, &alice, "read:follows").await;
    let app_only = common::create_app_only_token(&pool, "read:accounts").await;
    let app = common::test_app(pool);

    common::assert_answer(
        &get(&app, None, BIRTHDAYS_PATH).await,
        StatusCode::UNAUTHORIZED,
        "the birthday list without a token",
    );
    common::assert_all_refused(
        &[get(&app, Some("nope"), BIRTHDAYS_PATH).await],
        StatusCode::UNAUTHORIZED,
        INVALID_TOKEN,
        "and on one that does not resolve",
    );
    common::assert_all_refused(
        &[get(&app, Some(&narrow), BIRTHDAYS_PATH).await],
        StatusCode::FORBIDDEN,
        NO_READ_ACCOUNTS,
        "a token that cannot read accounts",
    );
    common::assert_all_refused(
        &[get(&app, Some(&app_only), BIRTHDAYS_PATH).await],
        StatusCode::FORBIDDEN,
        USER_REQUIRED,
        "an app-only token names no viewer to read for",
    );
    common::assert_all_refused(
        &[
            get(&app, Some(&token), &format!("{BIRTHDAYS_PATH}?month=13")).await,
            get(&app, Some(&token), &format!("{BIRTHDAYS_PATH}?month=0")).await,
            get(&app, Some(&token), &format!("{BIRTHDAYS_PATH}?month=x")).await,
        ],
        StatusCode::BAD_REQUEST,
        BAD_MONTH,
        "a month that is not a month",
    );
    common::assert_all_refused(
        &[
            get(&app, Some(&token), &format!("{BIRTHDAYS_PATH}?day=32")).await,
            get(&app, Some(&token), &format!("{BIRTHDAYS_PATH}?day=-1")).await,
        ],
        StatusCode::BAD_REQUEST,
        BAD_DAY,
        "a day that is not a day",
    );
    common::assert_answer(
        &get(
            &app,
            Some(&token),
            &format!("{BIRTHDAYS_PATH}?month=2&day=29"),
        )
        .await,
        StatusCode::OK,
        "the leap day is a fine thing to ask about",
    );
}

/// The area's five routes answer where the ticket puts them, and no other
/// path joins it: an unrouted sibling still reaches the app-wide fallback.
#[tokio::test]
async fn the_account_listing_routes_are_the_tickets() {
    let db = common::test_db().await;
    let pool = db.pool().clone();
    let (_, token) = fixture(&pool, "alice", "read write").await;
    let app = common::test_app(pool);

    for path in [
        "/api/v1/directory",
        "/api/v1/suggestions",
        "/api/v2/suggestions",
        FAMILIAR_PATH,
        BIRTHDAYS_PATH,
    ] {
        let answer = get(&app, Some(&token), path).await;
        assert_ne!(
            answer.text(),
            json!({ "error": NOT_IMPLEMENTED }).to_string(),
            "`{path}` is routed, not the fallback: {}",
            answer.text()
        );
    }
    common::assert_all_refused(
        &[
            get(&app, Some(&token), "/api/v1/directories").await,
            get(&app, Some(&token), "/api/v2/directory").await,
            get(&app, Some(&token), "/api/v1/pleroma/birthday").await,
            get(&app, Some(&token), "/api/v2/suggestions/not-an-id").await,
        ],
        StatusCode::NOT_FOUND,
        NOT_IMPLEMENTED,
        "a path this area does not claim",
    );
}
