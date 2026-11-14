//! End-to-end interaction scenarios (WBE-223 ticket scenario 2): ten
//! complete journeys across the client API, each driven only through the
//! test router and asserted through every surface that renders a status —
//! the direct `GET /api/v1/statuses/{id}`, the home timeline and the
//! public timeline — so the Mastodon counts, the viewer flags
//! (`favourited`, `reblogged`, `bookmarked`, `pinned`, `muted`,
//! `pleroma.thread_muted`), the reaction arrays and the rest of the
//! `pleroma` fields agree on every page, for every viewer, all the way
//! through the stack.
//!
//! The scenarios: favourite/unfavourite (`GET /api/v1/favourites`);
//! reblog/undo (nested `reblog`, once on a follower's home); bookmark +
//! Pleroma bookmark folder (round-tripped through `GET
//! /api/v1/bookmarks`); pin/unpin (the profile's `?pinned=true` page and
//! its pin order); thread mute/unmute (`POST …/mute`); emoji reactions
//! (the `count` inside `pleroma.emoji_reactions` is the reaction count,
//! Pleroma 2.10's spelling, which keeps the object's `reaction_count`
//! off the wire); a poll vote and its locked second ballot; an edit and
//! the history/source that keep it readable; a `statuses.expire` fire
//! drained through [`common::run_all_jobs`] after the job's `run_at` is
//! backdated, which must take the status off every surface; and a delete
//! — its 401/403/404 guards, the boost that falls with it, and the
//! counters left settled.
//!
//! The per-endpoint refusals are pinned by the area files
//! (`favourites.rs`, `reblogs.rs`, …); this module owns the cross-surface
//! coherence, like `e2e_scheduled.rs` owns the queue seam.

#![cfg(test)]

use axum::{Router, http::StatusCode};
use serde_json::{Value, json};
use sqlx::PgPool;
use wired_be::{jobs::statuses::KIND_EXPIRE, models::user::User, state::AppState};

use crate::common;

/// The status create/read/delete route every scenario starts from.
const STATUSES: &str = "/api/v1/statuses";
/// The public timeline read.
const PUBLIC: &str = "/api/v1/timelines/public";
/// The home timeline read.
const HOME: &str = "/api/v1/timelines/home";
/// The caller's own favourites list.
const FAVOURITES: &str = "/api/v1/favourites";
/// The caller's own bookmarks list.
const BOOKMARKS: &str = "/api/v1/bookmarks";
/// The Pleroma bookmark-folders route the filing scenario creates through.
const FOLDERS: &str = "/api/v1/pleroma/bookmark_folders";
/// The account route the follow write and the profile reads live under.
const ACCOUNTS: &str = "/api/v1/accounts";
/// The Pleroma status route the reaction writes and reads live under.
const PLEROMA: &str = "/api/v1/pleroma/statuses";
/// The polls route the vote scenario casts its ballot through.
const POLLS: &str = "/api/v1/polls";
/// The scopes every scenario caller carries: `read write` covers every
/// sub-scope the ten journeys touch (decision 9's scope tree).
const SCOPES: &str = "read write";
/// The shared 404 body of the status gate.
const NOT_FOUND: &str = "Record not found";
/// The 401 body `AuthUser` renders when the request carries no token.
const NO_TOKEN: &str = "Invalid credentials";
/// The 401 body for a token that does not resolve.
const BAD_TOKEN: &str = "The access token is invalid";
/// The 403 body for an app-only token at a route that needs a user.
const USER_REQUIRED: &str = "This API requires an authenticated user";
/// The 403 wording of the `write:statuses` gate (the delete's own).
const NO_WRITE: &str = "Insufficient permissions: write:statuses.";
/// The 422 body for a voter's second ballot.
const SECOND_BALLOT: &str = "You have already voted on this poll";
/// An id that names no stored row.
const UNKNOWN_ID: &str = "01934567-89ab-7cde-f012-3456789abcde";
/// One unicode emoji, as it is stored and as the API names it.
const THUMB: &str = "👍";
/// [`THUMB`] percent-encoded as a path segment, as a client sends it.
const THUMB_SEGMENT: &str = "%F0%9F%91%8D";
/// A second unicode emoji, for the multi-emoji roll-up.
const PARTY: &str = "🎉";
/// [`PARTY`] percent-encoded as a path segment.
const PARTY_SEGMENT: &str = "%F0%9F%8E%89";

/// A local user named `nickname` with a bearer token granting `scopes`.
async fn actor(pool: &PgPool, nickname: &str, scopes: &str) -> (User, String) {
    let created = common::create_local_user(pool, nickname).await;
    let token = common::create_app_and_token(pool, &created, scopes).await;
    (created, token)
}

/// A local user named `nickname` who may do everything the scenarios do.
async fn user(pool: &PgPool, nickname: &str) -> (User, String) {
    actor(pool, nickname, SCOPES).await
}

/// The router and the [`AppState`] behind it over `pool`: requests ride
/// the router, [`common::run_all_jobs`] drains on the state, and the
/// config is the fixed [`common::test_config`] one.
fn app_and_state(pool: PgPool) -> (Router, AppState) {
    let state = common::test_app_state(pool, common::test_config());
    (wired_be::app(state.clone()), state)
}

/// The router with a pin limit of `limit`, for the scenario that pins two
/// posts: the shipped default is Pleroma's one.
fn app_with_pin_limit(pool: &PgPool, limit: usize) -> Router {
    let mut config = common::test_config();
    config.max_pinned_statuses = limit;
    common::test_app_with_config(pool.clone(), config)
}

/// `GET`s `path`, with `token` when the scenario presents one.
async fn get(app: &Router, token: Option<&str>, path: &str) -> common::TestResponse {
    let request = common::TestRequest::get(path);
    match token {
        Some(found) => request.bearer(found).send(app).await,
        None => request.send(app).await,
    }
}

/// `POST /api/v1/statuses` as `token` with the JSON `body`, answered raw
/// for the shape probes.
async fn create(app: &Router, token: &str, body: Value) -> common::TestResponse {
    common::TestRequest::post(STATUSES)
        .bearer(token)
        .json(&body)
        .send(app)
        .await
}

/// A create asserted 200 and answered as the status entity it wrote.
async fn created(app: &Router, token: &str, body: Value) -> Value {
    let response = create(app, token, body).await;
    common::assert_answer(&response, StatusCode::OK, "the fixture post creates");
    response.json_value()
}

/// A plain public post of `text` by `token`, answered as its entity.
async fn post(app: &Router, token: &str, text: &str) -> Value {
    created(app, token, json!({"status": text})).await
}

/// A body-less `POST` status action (`…/favourite`, `…/mute`, …) as
/// `token`, answered raw for the guard probes.
async fn action(app: &Router, token: &str, path: &str) -> common::TestResponse {
    common::TestRequest::post(path)
        .bearer(token)
        .send(app)
        .await
}

/// An action asserted 200 and answered as the entity it returns.
async fn acted(app: &Router, token: &str, path: &str) -> Value {
    let response = action(app, token, path).await;
    common::assert_answer(&response, StatusCode::OK, "the status action stores");
    response.json_value()
}

/// A `POST` of the JSON `body` to `path` as `token`, asserted 200 and
/// answered: the folder and filing writes, which carry parameters.
async fn posted(app: &Router, token: &str, path: &str, body: Value) -> Value {
    let response = common::TestRequest::post(path)
        .bearer(token)
        .json(&body)
        .send(app)
        .await;
    common::assert_answer(&response, StatusCode::OK, "the parameterized write stores");
    response.json_value()
}

/// A `PUT` status action (`…/reactions/{emoji}`) as `token`, asserted 200
/// and answered as the entity it returns.
async fn reacted(app: &Router, token: &str, path: &str) -> Value {
    let response = common::TestRequest::put(path).bearer(token).send(app).await;
    common::assert_answer(&response, StatusCode::OK, "the reaction stores");
    response.json_value()
}

/// A `DELETE` action (`…/reactions/{emoji}`) asserted 200 and answered as
/// the entity it returns.
async fn withdrawn(app: &Router, token: &str, path: &str) -> Value {
    let response = common::TestRequest::delete(path)
        .bearer(token)
        .send(app)
        .await;
    common::assert_answer(
        &response,
        StatusCode::OK,
        "the withdrawal answers the status",
    );
    response.json_value()
}

/// `GET /api/v1/statuses/{id}` asserted 200 and answered as JSON.
async fn shown(app: &Router, token: Option<&str>, id: &str) -> Value {
    let response = get(app, token, &format!("{STATUSES}/{id}")).await;
    common::assert_answer(&response, StatusCode::OK, "the status reads back");
    response.json_value()
}

/// A status the gate no longer answers: the shared 404 body, for `who`.
async fn assert_gone(app: &Router, token: Option<&str>, id: &str, who: &str) {
    let response = get(app, token, &format!("{STATUSES}/{id}")).await;
    common::assert_answer(&response, StatusCode::NOT_FOUND, who);
    common::assert_error(&response, NOT_FOUND, who);
}

/// `POST /api/v1/accounts/{target}/follow` as `token`: the follow of an
/// open local account, granted on the spot — which is what puts the
/// followee's posts on the caller's home timeline.
async fn follows(app: &Router, token: &str, target: &str) {
    let related = acted(app, token, &format!("{ACCOUNTS}/{target}/follow")).await;
    assert_eq!(
        related["following"],
        json!(true),
        "the relationship answers granted: {related}"
    );
}

/// The ids of a rendered page, in answer order.
fn ids_of(body: &Value) -> Vec<String> {
    body.as_array()
        .expect("a page is a JSON array")
        .iter()
        .map(|status| {
            status["id"]
                .as_str()
                .expect("every Status entity carries its id")
                .to_owned()
        })
        .collect()
}

/// The `id` of a rendered entity, as its JSON spells it.
fn entity_id(entity: &Value) -> String {
    entity["id"]
        .as_str()
        .expect("the entity carries its id")
        .to_owned()
}

/// The row of a rendered page that carries `wanted`, for `who`.
fn row_of<'a>(page: &'a Value, wanted: &str, who: &str) -> &'a Value {
    page.as_array()
        .expect("a page is a JSON array")
        .iter()
        .find(|row| row["id"].as_str() == Some(wanted))
        .unwrap_or_else(|| panic!("the {who} page has no row {wanted}: {page}"))
}

/// The value at the key path `path` of an entity (`&["pleroma",
/// "thread_muted"]`), `null` when any leg is missing.
fn pick(entity: &Value, path: &[&str]) -> Value {
    path.iter()
        .try_fold(entity, |node, key| node.get(*key))
        .cloned()
        .unwrap_or(Value::Null)
}

/// Every representation of one status a scenario reads back: the direct
/// `GET /api/v1/statuses/{id}` plus the row of the home timeline and the
/// row of the public timeline, each rendered as `token`'s viewer sees it.
/// `None` asks for the two an anonymous request reaches — home has no
/// anonymous page.
async fn representations(app: &Router, token: Option<&str>, id: &str) -> Vec<Value> {
    let mut seen = vec![shown(app, token, id).await];
    if let Some(found) = token {
        let home = get(app, Some(found), HOME).await.json_value();
        seen.push(row_of(&home, id, "home").clone());
    }
    let public = get(app, token, PUBLIC).await.json_value();
    seen.push(row_of(&public, id, "public").clone());
    seen
}

/// The value at `path` is `want` on every representation of the status,
/// naming the reading `who`.
fn assert_every(seen: &[Value], path: &[&str], want: &Value, who: &str) {
    let label = path.join(".");
    for (index, entity) in seen.iter().enumerate() {
        assert_eq!(
            pick(entity, path),
            *want,
            "{label} of representation {index}: {who}"
        );
    }
}

// ────────────────────  Scenario (1): favourite  ────────────────────

/// A like steps `favourited` and `favourites_count` on every
/// representation, lands in `GET /api/v1/favourites`, and an unlike walks
/// the whole set back down.
#[tokio::test]
async fn a_favourite_moves_the_count_the_flag_and_the_list() {
    let db = common::test_db().await;
    let pool = db.pool().clone();
    let app = common::test_app(pool.clone());
    let (alice, alice_token) = user(&pool, "alice").await;
    let (_bob, bob_token) = user(&pool, "bob").await;
    let id = entity_id(&post(&app, &alice_token, "favourite me").await);
    follows(&app, &bob_token, &alice.id.to_string()).await;
    let (on, off) = (json!(true), json!(false));
    let (one, zero) = (json!(1), json!(0));

    assert_every(
        &representations(&app, Some(&bob_token), &id).await,
        &["favourited"],
        &off,
        "the like before it is liked",
    );

    let liked = acted(&app, &bob_token, &format!("{STATUSES}/{id}/favourite")).await;
    assert_eq!(liked["favourited"], on, "the write flags the like");
    assert_eq!(liked["favourites_count"], one, "and counts it once");
    let again = acted(&app, &bob_token, &format!("{STATUSES}/{id}/favourite")).await;
    assert_eq!(
        again["favourites_count"], one,
        "a repeat never counts twice"
    );

    let mine = representations(&app, Some(&bob_token), &id).await;
    assert_every(&mine, &["favourited"], &on, "the liker's reads");
    assert_every(&mine, &["favourites_count"], &one, "the liker's reads");
    let strangers = representations(&app, None, &id).await;
    assert_every(&strangers, &["favourited"], &off, "a stranger's reads");
    assert_every(&strangers, &["favourites_count"], &one, "the public count");

    let listed = get(&app, Some(&bob_token), FAVOURITES).await.json_value();
    assert_eq!(
        ids_of(&listed),
        vec![id.clone()],
        "the favourites list round-trips the like"
    );
    assert_eq!(
        row_of(&listed, &id, "favourites")["favourited"],
        on,
        "listed as the liker's own"
    );

    let unlike = acted(&app, &bob_token, &format!("{STATUSES}/{id}/unfavourite")).await;
    assert_eq!(unlike["favourited"], off, "the withdrawal clears the flag");
    assert_eq!(unlike["favourites_count"], zero, "and steps the count back");
    assert_every(
        &representations(&app, Some(&bob_token), &id).await,
        &["favourites_count"],
        &zero,
        "every read agrees the unlike",
    );
    let emptied = get(&app, Some(&bob_token), FAVOURITES).await.json_value();
    assert!(
        ids_of(&emptied).is_empty(),
        "the favourites list empties with it"
    );
}

// ─────────────────────  Scenario (2): reblog  ─────────────────────

/// A boost nests the original, steps `reblogs_count` and `reblogged` on
/// every representation, and rides the follower's home timeline exactly
/// once; the undo walks it all back and takes the boost row down.
#[tokio::test]
async fn a_reblog_nests_the_original_and_rides_the_follower_home_once() {
    let db = common::test_db().await;
    let pool = db.pool().clone();
    let app = common::test_app(pool.clone());
    let (alice, alice_token) = user(&pool, "alice").await;
    let (bob, bob_token) = user(&pool, "bob").await;
    let (_carol, carol_token) = user(&pool, "carol").await;
    let id = entity_id(&post(&app, &alice_token, "boost me").await);
    follows(&app, &bob_token, &alice.id.to_string()).await;
    follows(&app, &carol_token, &bob.id.to_string()).await;

    let boost = acted(&app, &bob_token, &format!("{STATUSES}/{id}/reblog")).await;
    let boost_id = entity_id(&boost);
    assert_ne!(boost_id, id, "the answer is the boost, not the post");
    assert_eq!(
        boost["content"], "",
        "the boost row carries no text of its own"
    );
    assert_eq!(boost["reblog"]["id"], id, "the original nests inside");
    assert_eq!(
        boost["reblog"]["reblogs_count"],
        json!(1),
        "and counts the boost"
    );
    assert_eq!(
        boost["reblogged"],
        json!(true),
        "one's own boost flags itself"
    );

    let boosted = representations(&app, Some(&bob_token), &id).await;
    assert_every(
        &boosted,
        &["reblogs_count"],
        &json!(1),
        "the booster's reads",
    );
    assert_every(
        &boosted,
        &["reblogged"],
        &json!(true),
        "the booster's reads",
    );
    let strangers = representations(&app, None, &id).await;
    assert_every(
        &strangers,
        &["reblogs_count"],
        &json!(1),
        "the count every reader sees",
    );
    assert_every(
        &strangers,
        &["reblogged"],
        &json!(false),
        "nobody else boosted it",
    );

    let home = get(&app, Some(&carol_token), HOME).await.json_value();
    assert_eq!(
        ids_of(&home),
        vec![boost_id.clone()],
        "the follower's home carries the boost exactly once"
    );
    assert_eq!(
        row_of(&home, &boost_id, "the follower's home")["reblog"]["id"],
        id,
        "boosting the post it names"
    );

    let undo = acted(&app, &bob_token, &format!("{STATUSES}/{id}/unreblog")).await;
    assert_eq!(undo["id"], id, "the withdrawal answers the original");
    assert_eq!(undo["reblogs_count"], json!(0), "its count steps back");
    assert_eq!(undo["reblogged"], json!(false), "and the flag clears");
    assert_gone(&app, Some(&bob_token), &boost_id, "the withdrawn boost").await;
    let settled = get(&app, Some(&carol_token), HOME).await.json_value();
    assert!(
        ids_of(&settled).is_empty(),
        "the boost left the home with it"
    );
}

// ──────────────────  Scenario (3): bookmark + folder  ──────────────────

/// A Pleroma folder created over HTTP files a bookmark through the
/// bookmark write; the filing renders as `pleroma.bookmark_folder` on
/// every representation and round-trips through `GET /api/v1/bookmarks`
/// and its `folder_id` filter.
#[tokio::test]
async fn a_filed_bookmark_round_trips_the_folder_and_the_list() {
    let db = common::test_db().await;
    let pool = db.pool().clone();
    let app = common::test_app(pool.clone());
    let (alice, alice_token) = user(&pool, "alice").await;
    let (_bob, bob_token) = user(&pool, "bob").await;
    let id = entity_id(&post(&app, &alice_token, "save this").await);
    follows(&app, &bob_token, &alice.id.to_string()).await;
    let folder_id = create_folder(&app, &bob_token, "Reading list").await;

    let filed = posted(
        &app,
        &bob_token,
        &format!("{STATUSES}/{id}/bookmark"),
        json!({"folder_id": folder_id}),
    )
    .await;
    assert_eq!(
        filed["bookmarked"],
        json!(true),
        "the write flags the bookmark"
    );
    assert_eq!(
        filed["pleroma"]["bookmark_folder"],
        json!(folder_id),
        "and files it in the folder"
    );

    assert_filing(
        &app,
        Some(&bob_token),
        &id,
        &json!(folder_id.clone()),
        "the bookmarker",
    )
    .await;
    assert_filing(&app, None, &id, &Value::Null, "a stranger").await;

    assert_bookmarks(&app, &bob_token, "", std::slice::from_ref(&id)).await;
    let listed = bookmarks_page(&app, &bob_token, "").await;
    assert_eq!(
        row_of(&listed, &id, "bookmarks")["pleroma"]["bookmark_folder"],
        json!(folder_id),
        "listed as filed"
    );
    assert_bookmarks(
        &app,
        &bob_token,
        &format!("folder_id={folder_id}"),
        std::slice::from_ref(&id),
    )
    .await;
    assert_bookmarks(&app, &bob_token, &format!("folder_id={UNKNOWN_ID}"), &[]).await;

    let freed = acted(&app, &bob_token, &format!("{STATUSES}/{id}/unbookmark")).await;
    assert_eq!(
        freed["bookmarked"],
        json!(false),
        "the withdrawal clears the flag"
    );
    assert_eq!(
        freed["pleroma"]["bookmark_folder"],
        Value::Null,
        "and the filing"
    );
    assert_bookmarks(&app, &bob_token, "", &[]).await;
}

/// A Pleroma bookmark folder named `name`, created over HTTP and shown
/// back on the folder list; answers its id.
async fn create_folder(app: &Router, token: &str, name: &str) -> String {
    let folder = posted(app, token, FOLDERS, json!({"name": name})).await;
    assert_eq!(folder["name"], json!(name), "the folder answers its name");
    let folder_id = entity_id(&folder);
    let shelves = get(app, Some(token), FOLDERS).await.json_value();
    assert_eq!(
        ids_of(&shelves),
        vec![folder_id.clone()],
        "the folder list holds it, in creation order"
    );
    folder_id
}

/// The `GET /api/v1/bookmarks` page narrowed by `query` (empty: the whole
/// list), asserted 200 and answered as JSON.
async fn bookmarks_page(app: &Router, token: &str, query: &str) -> Value {
    let path = if query.is_empty() {
        BOOKMARKS.to_owned()
    } else {
        format!("{BOOKMARKS}?{query}")
    };
    let response = get(app, Some(token), &path).await;
    common::assert_answer(&response, StatusCode::OK, "the bookmarks list reads");
    response.json_value()
}

/// The bookmarks page for `query` answers exactly the ids in `want`.
async fn assert_bookmarks(app: &Router, token: &str, query: &str, want: &[String]) {
    assert_eq!(
        ids_of(&bookmarks_page(app, token, query).await),
        want,
        "the bookmarks list answers"
    );
}

/// The bookmark pair every representation of `id` answers as the reader
/// behind `token`: `bookmarked` is the folder's presence and
/// `pleroma.bookmark_folder` is `folder_id` — which is why the two can
/// never disagree, the case named by `who`.
async fn assert_filing(app: &Router, token: Option<&str>, id: &str, folder_id: &Value, who: &str) {
    let seen = representations(app, token, id).await;
    let flag = json!(!folder_id.is_null());
    assert_every(&seen, &["bookmarked"], &flag, who);
    assert_every(&seen, &["pleroma", "bookmark_folder"], folder_id, who);
}

// ────────────────────────  Scenario (4): pin  ────────────────────────

/// Pins flag the owner's every read with `pinned` and
/// `pleroma.pinned_at`, drive the profile's `?pinned=true` page in
/// newest-pin-first order, and an unpin walks the flag and the page back.
#[tokio::test]
async fn pins_flag_the_owner_and_order_the_profile() {
    let db = common::test_db().await;
    let pool = db.pool().clone();
    let app = app_with_pin_limit(&pool, 5);
    let (alice, alice_token) = user(&pool, "alice").await;
    let account_id = alice.id.to_string();
    let first = entity_id(&post(&app, &alice_token, "pin me first").await);
    let second = entity_id(&post(&app, &alice_token, "and me second").await);

    let pinned = acted(&app, &alice_token, &format!("{STATUSES}/{first}/pin")).await;
    assert_eq!(pinned["pinned"], json!(true), "the pin flags the answer");
    assert!(
        pinned["pleroma"]["pinned_at"].is_string(),
        "and stamps it: {pinned}"
    );
    let second_pin = acted(&app, &alice_token, &format!("{STATUSES}/{second}/pin")).await;
    assert_eq!(
        second_pin["pinned"],
        json!(true),
        "the second pin flags too"
    );

    let page = get(
        &app,
        Some(&alice_token),
        &format!("{ACCOUNTS}/{account_id}/statuses?pinned=true"),
    )
    .await
    .json_value();
    assert_eq!(
        ids_of(&page),
        vec![second.clone(), first.clone()],
        "the featured page answers newest pin first"
    );

    let owners = representations(&app, Some(&alice_token), &first).await;
    assert_every(&owners, &["pinned"], &json!(true), "the owner's reads");
    let strangers = representations(&app, None, &first).await;
    assert_every(
        &strangers,
        &["pinned"],
        &json!(false),
        "a stranger sees nobody's pin",
    );
    assert_every(
        &strangers,
        &["pleroma", "pinned_at"],
        &Value::Null,
        "and none of its stamp",
    );

    let released = acted(&app, &alice_token, &format!("{STATUSES}/{first}/unpin")).await;
    assert_eq!(
        released["pinned"],
        json!(false),
        "the unpin clears the flag"
    );
    assert_eq!(
        released["pleroma"]["pinned_at"],
        Value::Null,
        "and its stamp"
    );
    let rest = get(
        &app,
        Some(&alice_token),
        &format!("{ACCOUNTS}/{}/statuses?pinned=true", alice.id),
    )
    .await
    .json_value();
    assert_eq!(
        ids_of(&rest),
        vec![second],
        "the page keeps the standing pin alone"
    );
}

// ─────────────────────  Scenario (5): thread mute  ─────────────────────

/// A thread mute flags the muting viewer's every read of the whole
/// conversation with `muted` and `pleroma.thread_muted`, hides nothing
/// from anybody else, and the unmute clears the pair.
#[tokio::test]
async fn a_thread_mute_flags_the_whole_thread_until_the_unmute() {
    let db = common::test_db().await;
    let pool = db.pool().clone();
    let app = common::test_app(pool.clone());
    let (_alice, alice_token) = user(&pool, "alice").await;
    let id = entity_id(&post(&app, &alice_token, "a thread to mute").await);
    let answer = entity_id(
        &created(
            &app,
            &alice_token,
            json!({"status": "and its reply", "in_reply_to_id": id}),
        )
        .await,
    );

    let muted = acted(&app, &alice_token, &format!("{STATUSES}/{id}/mute")).await;
    assert_eq!(muted["muted"], json!(true), "the write flags the answer");
    assert_eq!(
        muted["pleroma"]["thread_muted"],
        json!(true),
        "on both halves of the pair"
    );

    let own = representations(&app, Some(&alice_token), &id).await;
    assert_every(&own, &["muted"], &json!(true), "the muter's reads");
    assert_every(
        &own,
        &["pleroma", "thread_muted"],
        &json!(true),
        "the muter's reads, both halves",
    );
    let thread = representations(&app, Some(&alice_token), &answer).await;
    assert_every(
        &thread,
        &["pleroma", "thread_muted"],
        &json!(true),
        "the mute covers the whole conversation",
    );
    let strangers = representations(&app, None, &id).await;
    assert_every(
        &strangers,
        &["muted"],
        &json!(false),
        "one muter is nobody else's",
    );
    assert_every(
        &strangers,
        &["pleroma", "thread_muted"],
        &json!(false),
        "on either half of the pair",
    );

    let released = acted(&app, &alice_token, &format!("{STATUSES}/{id}/unmute")).await;
    assert_eq!(
        released["muted"],
        json!(false),
        "the unmute clears the flag"
    );
    assert_eq!(
        released["pleroma"]["thread_muted"],
        json!(false),
        "and the extension with it",
    );
    assert_every(
        &representations(&app, Some(&alice_token), &id).await,
        &["muted"],
        &json!(false),
        "every read agrees the thread is open",
    );
}

// ────────────────────  Scenario (6): emoji reactions  ────────────────────

/// The `count` entries of `pleroma.emoji_reactions` are this instance's
/// reaction counts (Pleroma 2.10's spelling, whose `reaction_count`
/// stays inside the activity object): adding an emoji counts on every
/// representation and on the reactions endpoint, and withdrawing clears
/// both again.
#[tokio::test]
async fn reactions_step_the_rollup_and_the_reactions_endpoint() {
    let db = common::test_db().await;
    let pool = db.pool().clone();
    let app = common::test_app(pool.clone());
    let (alice, alice_token) = user(&pool, "alice").await;
    let (bob, bob_token) = user(&pool, "bob").await;
    let id = entity_id(&post(&app, &alice_token, "react to me").await);
    let bob_id = bob.id.to_string();
    follows(&app, &bob_token, &alice.id.to_string()).await;

    let thumb_on = reacted(
        &app,
        &bob_token,
        &format!("{PLEROMA}/{id}/reactions/{THUMB_SEGMENT}"),
    )
    .await;
    assert_eq!(
        thumb_on["pleroma"]["emoji_reactions"],
        json!([rollup(THUMB, true, &bob_id)]),
        "the write answers the whole roll-up"
    );
    let repeat = reacted(
        &app,
        &bob_token,
        &format!("{PLEROMA}/{id}/reactions/{THUMB_SEGMENT}"),
    )
    .await;
    assert_eq!(
        repeat["pleroma"]["emoji_reactions"], thumb_on["pleroma"]["emoji_reactions"],
        "a repeat restates one reaction"
    );
    let partied = reacted(
        &app,
        &bob_token,
        &format!("{PLEROMA}/{id}/reactions/{PARTY_SEGMENT}"),
    )
    .await;
    let mine = json!([rollup(THUMB, true, &bob_id), rollup(PARTY, true, &bob_id)]);
    assert_eq!(
        pick(&partied, &["pleroma", "emoji_reactions"]),
        mine,
        "the second emoji rolls up beside the first"
    );

    let views = representations(&app, Some(&bob_token), &id).await;
    assert_every(
        &views,
        &["pleroma", "emoji_reactions"],
        &mine,
        "the reactor's reads",
    );
    let theirs = json!([rollup(THUMB, false, &bob_id), rollup(PARTY, false, &bob_id)]);
    let strangers = representations(&app, None, &id).await;
    assert_every(
        &strangers,
        &["pleroma", "emoji_reactions"],
        &theirs,
        "a stranger counts without being one",
    );
    assert_listed(&app, &bob_token, &id, &bob_id).await;
    assert_withdrawals(&app, &bob_token, &id, &bob_id).await;
}

/// One `pleroma.emoji_reactions` entry: `emoji` counted once, flagged for
/// the viewer by `me`, with `reactor` as its only listed account.
fn rollup(emoji: &str, me: bool, reactor: &str) -> Value {
    json!({
        "name": emoji,
        "count": 1,
        "me": me,
        "url": null,
        "account_ids": [reactor],
    })
}

/// The reactions endpoint read for status `id`: two entries in reaction
/// order, each counting its single reactor and carrying them as
/// [`Account`] entities.
///
/// [`Account`]: wired_be::entities::account::Account
async fn assert_listed(app: &Router, token: &str, id: &str, reactor: &str) {
    let listed = get(app, Some(token), &format!("{PLEROMA}/{id}/reactions"))
        .await
        .json_value();
    let entries = listed.as_array().expect("the reactions read is an array");
    assert_eq!(
        entries.len(),
        2,
        "one entry per emoji, first reaction first"
    );
    assert_eq!(entries[0]["name"], json!(THUMB), "the thumbs-up came first");
    assert_eq!(entries[0]["count"], json!(1), "each counting its reactor");
    assert_eq!(
        entries[0]["accounts"][0]["id"],
        json!(reactor),
        "and naming them as accounts"
    );
}

/// Withdrawing both reactions in turn: the first withdrawal leaves the
/// other emoji standing, the last empties the write's answer, the
/// reactions endpoint and every read representation.
async fn assert_withdrawals(app: &Router, token: &str, id: &str, reactor: &str) {
    let thumbed_off = withdrawn(
        app,
        token,
        &format!("{PLEROMA}/{id}/reactions/{THUMB_SEGMENT}"),
    )
    .await;
    assert_eq!(
        thumbed_off["pleroma"]["emoji_reactions"],
        json!([rollup(PARTY, true, reactor)]),
        "the withdrawal leaves the other emoji standing"
    );
    let last_off = withdrawn(
        app,
        token,
        &format!("{PLEROMA}/{id}/reactions/{PARTY_SEGMENT}"),
    )
    .await;
    assert_eq!(
        last_off["pleroma"]["emoji_reactions"],
        json!([]),
        "and the last one empties the roll-up"
    );
    let empty = get(app, Some(token), &format!("{PLEROMA}/{id}/reactions"))
        .await
        .json_value();
    assert_eq!(empty, json!([]), "the reactions read empties with it");
    assert_every(
        &representations(app, Some(token), id).await,
        &["pleroma", "emoji_reactions"],
        &json!([]),
        "every read agrees the post is unreacted",
    );
}

// ──────────────────────  Scenario (7): poll vote  ──────────────────────

/// A ballot through `POST /api/v1/polls/{id}/votes` moves the tallies the
/// status entity renders on every representation, shows the voter their
/// own marks and nobody else's, and the second ballot of one voter is the
/// already-voted 422 that counted nothing.
#[tokio::test]
async fn a_poll_ballot_moves_the_tallies_and_locks_a_second_vote() {
    let db = common::test_db().await;
    let pool = db.pool().clone();
    let app = common::test_app(pool.clone());
    let (alice, alice_token) = user(&pool, "alice").await;
    let (_bob, bob_token) = user(&pool, "bob").await;
    let created = created(
        &app,
        &alice_token,
        json!({"status": "tea or coffee?", "poll": {"options": ["tea", "coffee"]}}),
    )
    .await;
    let id = entity_id(&created);
    let poll_id = pick(&created, &["poll", "id"])
        .as_str()
        .expect("a poll create answers the poll entity")
        .to_owned();
    assert_eq!(
        created["poll"]["votes_count"],
        json!(0),
        "the stand opens at zero"
    );
    follows(&app, &bob_token, &alice.id.to_string()).await;

    let ballot = posted(
        &app,
        &bob_token,
        &format!("{POLLS}/{poll_id}/votes"),
        json!({"choices": [1]}),
    )
    .await;
    assert_ballot(&ballot, &poll_id);

    let one = json!(1);
    let mine = representations(&app, Some(&bob_token), &id).await;
    assert_every(
        &mine,
        &["poll", "votes_count"],
        &one,
        "the tallies ride every read",
    );
    assert_every(
        &mine,
        &["poll", "voted"],
        &json!(true),
        "and the voter sees their ballot",
    );
    let strangers = representations(&app, None, &id).await;
    assert_every(
        &strangers,
        &["poll", "votes_count"],
        &one,
        "the public tallies",
    );
    assert_every(
        &strangers,
        &["poll", "own_votes"],
        &json!([]),
        "which name nobody else's marks",
    );

    let second = common::TestRequest::post(&format!("{POLLS}/{poll_id}/votes"))
        .bearer(&bob_token)
        .json(&json!({"choices": [0]}))
        .send(&app)
        .await;
    common::assert_answer(&second, StatusCode::UNPROCESSABLE_ENTITY, "a second ballot");
    common::assert_error(&second, SECOND_BALLOT, "the already-voted refusal");
    let stand = shown(&app, Some(&bob_token), &id).await;
    assert_eq!(
        pick(&stand, &["poll", "votes_count"]),
        one,
        "the refusal counted nothing"
    );
    assert_eq!(
        pick(&stand, &["poll", "own_votes"]),
        json!([1]),
        "and rewrote nothing"
    );
}

/// The ballot's answer: the poll's own entity, `voted` for its voter with
/// position `1` marked, and the moved tallies on the marked option alone.
fn assert_ballot(stand: &Value, poll_id: &str) {
    assert_eq!(
        stand["id"],
        json!(poll_id),
        "the answer is the poll's entity"
    );
    assert_eq!(stand["voted"], json!(true), "flagged as cast");
    assert_eq!(stand["own_votes"], json!([1]), "with the voter's mark");
    assert_eq!(stand["votes_count"], json!(1), "and the moved roll-up");
    let options = stand["options"]
        .as_array()
        .expect("the poll answers its options");
    assert_eq!(options.len(), 2, "one entry per option");
    assert_eq!(
        options[1]["votes_count"],
        json!(1),
        "only the marked option gained"
    );
    assert_eq!(
        options[0]["votes_count"],
        json!(0),
        "and the other stayed at zero"
    );
}

// ─────────────────────────  Scenario (8): edit  ─────────────────────────

/// A `PUT /api/v1/statuses/{id}` rewrite rides every representation with
/// its new content and `edited_at` stamp, and the two reads behind it
/// keep the story: `…/history` answers the superseded text beside the
/// current one, and `…/source` the live status's raw source.
#[tokio::test]
async fn an_edit_rewrites_the_post_and_the_history_keeps_the_old_text() {
    let db = common::test_db().await;
    let pool = db.pool().clone();
    let app = common::test_app(pool.clone());
    let (alice, alice_token) = user(&pool, "alice").await;
    let (_bob, bob_token) = user(&pool, "bob").await;
    let id = entity_id(&post(&app, &alice_token, "original words").await);
    follows(&app, &bob_token, &alice.id.to_string()).await;

    let response = common::TestRequest::put(&format!("{STATUSES}/{id}"))
        .bearer(&alice_token)
        .json(&json!({"status": "corrected words"}))
        .send(&app)
        .await;
    common::assert_answer(&response, StatusCode::OK, "the author edits");
    let edited = response.json_value();
    assert_eq!(
        edited["content"],
        json!("<p>corrected words</p>"),
        "the write answers the rewrite"
    );
    assert!(
        edited["edited_at"].is_string(),
        "stamped as edited: {edited}"
    );

    let rewritten = representations(&app, Some(&bob_token), &id).await;
    assert_every(
        &rewritten,
        &["content"],
        &json!("<p>corrected words</p>"),
        "the rewrite rides every read",
    );

    let history = get(&app, Some(&bob_token), &format!("{STATUSES}/{id}/history"))
        .await
        .json_value();
    let items = history.as_array().expect("the history is an array");
    assert_eq!(
        items.len(),
        2,
        "the superseded version plus the current one"
    );
    assert_eq!(
        items[0]["content"],
        json!("<p>original words</p>"),
        "history keeps the old text first"
    );
    assert_eq!(
        items[1]["content"],
        json!("<p>corrected words</p>"),
        "and answers the current one last"
    );

    let source = get(&app, Some(&alice_token), &format!("{STATUSES}/{id}/source"))
        .await
        .json_value();
    assert_eq!(source["id"], json!(id), "the source names the post");
    assert_eq!(
        source["text"],
        json!("corrected words"),
        "as the author's newest raw text"
    );
    assert_eq!(
        source["content_type"],
        json!("text/plain"),
        "in its stored type"
    );
}

// ──────────────────────────  Scenario (9): expire  ──────────────────────────

/// Moves the queued [`KIND_EXPIRE`] fires an hour into the past, so the
/// next claim round takes them — the ticket's exact `UPDATE`, rows only.
///
/// # Panics
///
/// If the update hits anything but exactly one row.
async fn backdate_expire_jobs(pool: &PgPool) {
    let hit = sqlx::query("UPDATE jobs SET run_at = now() - interval '1 minute' WHERE kind = $1")
        .bind(KIND_EXPIRE)
        .execute(pool)
        .await
        .expect("the backdate update runs");
    assert_eq!(
        hit.rows_affected(),
        1,
        "the expiring post queued exactly one fire"
    );
}

/// The states of the [`KIND_EXPIRE`] job rows, in insertion order.
async fn expire_job_states(pool: &PgPool) -> Vec<String> {
    sqlx::query_scalar::<_, String>("SELECT state FROM jobs WHERE kind = $1 ORDER BY id")
        .bind(KIND_EXPIRE)
        .fetch_all(pool)
        .await
        .expect("the job rows read")
}

/// The `statuses_count` the account entity answers for `account`.
async fn statuses_count(app: &Router, account: &str) -> i64 {
    let found = get(app, None, &format!("{ACCOUNTS}/{account}"))
        .await
        .json_value();
    found["statuses_count"]
        .as_i64()
        .expect("the account entity counts its statuses")
}

/// A post carrying `expires_in` renders its expiry, rides every surface
/// until its [`KIND_EXPIRE`] fire drains through the queue harness, and
/// then is gone: the shared 404 for author and stranger alike, off the
/// public timeline and every home that carried it — while its unexpired
/// neighbour stays put.
#[tokio::test]
async fn an_expiring_status_leaves_every_surface_when_its_fire_drains() {
    let db = common::test_db().await;
    let pool = db.pool().clone();
    let (app, state) = app_and_state(pool.clone());
    let (alice, alice_token) = user(&pool, "alice").await;
    let (_bob, bob_token) = user(&pool, "bob").await;
    follows(&app, &bob_token, &alice.id.to_string()).await;

    let doomed = created(
        &app,
        &alice_token,
        json!({"status": "here today", "expires_in": 3600}),
    )
    .await;
    let doomed_id = entity_id(&doomed);
    assert!(
        doomed["pleroma"]["expires_at"].is_string(),
        "the answer states the expiry: {doomed}"
    );
    let keeper = entity_id(&post(&app, &alice_token, "and stays").await);

    let riding = get(&app, None, PUBLIC).await.json_value();
    assert_eq!(
        ids_of(&riding),
        vec![keeper.clone(), doomed_id.clone()],
        "both posts ride the public timeline"
    );

    backdate_expire_jobs(&pool).await;
    assert_eq!(
        common::run_all_jobs(&state).await,
        3,
        "the drain ran both creates' card fetches and the backdated fire"
    );
    assert_eq!(
        expire_job_states(&pool).await,
        vec!["completed".to_owned()],
        "the fire reached its terminal state"
    );

    assert_gone(&app, Some(&alice_token), &doomed_id, "the expired status").await;
    assert_gone(&app, None, &doomed_id, "the expired status, anonymously").await;
    let public = get(&app, None, PUBLIC).await.json_value();
    assert_eq!(
        ids_of(&public),
        vec![keeper.clone()],
        "the expired post is off the public timeline"
    );
    for viewer in [&alice_token, &bob_token] {
        let home = get(&app, Some(viewer.as_str()), HOME).await.json_value();
        assert_eq!(
            ids_of(&home),
            vec![keeper.clone()],
            "and off every home that carried it"
        );
    }
    assert_eq!(
        statuses_count(&app, &alice.id.to_string()).await,
        1,
        "the author stops counting the expired post"
    );
    common::assert_answer(
        &get(&app, None, &format!("{STATUSES}/{keeper}")).await,
        StatusCode::OK,
        "the unexpired neighbour stands",
    );
}

// ─────────────────────────  Scenario (10): delete  ─────────────────────────

/// The delete route's refusal stack on status `id`: no token and a dead
/// token are the 401 arms, an app-only credential and a scope-less user
/// token are the 403 arms, a stranger's 403 keeps the `"Record not
/// found"` body, and an id that names nothing is the shared 404.
async fn assert_delete_guards(
    app: &Router,
    id: &str,
    stranger: &str,
    reader: &str,
    app_only: &str,
) {
    let path = format!("{STATUSES}/{id}");
    let anonymous = common::TestRequest::delete(&path).send(app).await;
    common::assert_answer(
        &anonymous,
        StatusCode::UNAUTHORIZED,
        "a delete with no token",
    );
    common::assert_error(&anonymous, NO_TOKEN, "the delete with no token");

    let ghost = common::TestRequest::delete(&path)
        .bearer("tok3n-that-exists-not")
        .send(app)
        .await;
    common::assert_answer(
        &ghost,
        StatusCode::UNAUTHORIZED,
        "a delete with a dead token",
    );
    common::assert_error(&ghost, BAD_TOKEN, "the delete with a dead token");

    let client = common::TestRequest::delete(&path)
        .bearer(app_only)
        .send(app)
        .await;
    common::assert_answer(&client, StatusCode::FORBIDDEN, "an app-only delete");
    common::assert_error(&client, USER_REQUIRED, "the app-only delete");

    let scopeless = common::TestRequest::delete(&path)
        .bearer(reader)
        .send(app)
        .await;
    common::assert_answer(&scopeless, StatusCode::FORBIDDEN, "a scope-less delete");
    common::assert_error(&scopeless, NO_WRITE, "the token without write:statuses");

    let foreign = common::TestRequest::delete(&path)
        .bearer(stranger)
        .send(app)
        .await;
    common::assert_answer(&foreign, StatusCode::FORBIDDEN, "a stranger's delete");
    common::assert_error(
        &foreign,
        NOT_FOUND,
        "the ownership refusal keeps the shared body",
    );

    let missing = common::TestRequest::delete(&format!("{STATUSES}/{UNKNOWN_ID}"))
        .bearer(stranger)
        .send(app)
        .await;
    common::assert_answer(
        &missing,
        StatusCode::NOT_FOUND,
        "a delete of an id that names nothing",
    );
    common::assert_error(&missing, NOT_FOUND, "the gate's shared 404");
}

/// The delete guards answer before the write does; the author's own
/// delete then answers the gone status with its raw text, takes the
/// boost of it down as a tombstone nobody can read, clears the
/// timelines that carried both, and leaves every counter settled.
#[tokio::test]
async fn a_delete_guards_answers_and_takes_the_boost_with_it() {
    let db = common::test_db().await;
    let pool = db.pool().clone();
    let app = common::test_app(pool.clone());
    let (alice, alice_token) = user(&pool, "alice").await;
    let (bob, bob_token) = user(&pool, "bob").await;
    let (_carol, carol_token) = user(&pool, "carol").await;
    let reader = actor(&pool, "reader", "read").await.1;
    let app_only = common::create_app_only_token(&pool, SCOPES).await;
    let id = entity_id(&post(&app, &alice_token, "boost me then lose me").await);

    assert_delete_guards(&app, &id, &carol_token, &reader, &app_only).await;

    follows(&app, &carol_token, &bob.id.to_string()).await;
    let boost = entity_id(&acted(&app, &bob_token, &format!("{STATUSES}/{id}/reblog")).await);
    let riding = get(&app, Some(&carol_token), HOME).await.json_value();
    assert_eq!(
        ids_of(&riding),
        vec![boost.clone()],
        "the boost rides carol's home once"
    );

    let response = common::TestRequest::delete(&format!("{STATUSES}/{id}"))
        .bearer(&alice_token)
        .send(&app)
        .await;
    common::assert_answer(&response, StatusCode::OK, "the author's delete");
    let body = response.json_value();
    assert_eq!(body["id"], json!(id), "the answer is the status as it was");
    assert_eq!(
        body["text"],
        json!("boost me then lose me"),
        "with its raw source text beside it"
    );

    assert_gone(&app, Some(&alice_token), &id, "the deleted status").await;
    assert_gone(
        &app,
        Some(&bob_token),
        &boost,
        "the boost the delete took down",
    )
    .await;
    let public = get(&app, None, PUBLIC).await.json_value();
    assert!(
        ids_of(&public).is_empty(),
        "both left the public timeline: {public}"
    );
    let settled = get(&app, Some(&carol_token), HOME).await.json_value();
    assert!(
        ids_of(&settled).is_empty(),
        "and the follower's home with them"
    );
    assert_eq!(
        statuses_count(&app, &alice.id.to_string()).await,
        0,
        "the author counts nothing"
    );
    assert_eq!(
        statuses_count(&app, &bob.id.to_string()).await,
        0,
        "and neither does the booster"
    );
}
