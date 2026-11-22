//! The routes, response bodies and request helpers every interaction
//! scenario shares, plus the helpers that read one status back through
//! every surface that renders it.

use axum::{Router, http::StatusCode};
use serde_json::{Value, json};
use sqlx::PgPool;
use wired_be::{models::user::User, state::AppState};

use crate::common;

/// The status create/read/delete route every scenario starts from.
pub(super) const STATUSES: &str = "/api/v1/statuses";
/// The public timeline read.
pub(super) const PUBLIC: &str = "/api/v1/timelines/public";
/// The home timeline read.
pub(super) const HOME: &str = "/api/v1/timelines/home";
/// The caller's own favourites list.
pub(super) const FAVOURITES: &str = "/api/v1/favourites";
/// The caller's own bookmarks list.
pub(super) const BOOKMARKS: &str = "/api/v1/bookmarks";
/// The Pleroma bookmark-folders route the filing scenario creates through.
pub(super) const FOLDERS: &str = "/api/v1/pleroma/bookmark_folders";
/// The account route the follow write and the profile reads live under.
pub(super) const ACCOUNTS: &str = "/api/v1/accounts";
/// The Pleroma status route the reaction writes and reads live under.
pub(super) const PLEROMA: &str = "/api/v1/pleroma/statuses";
/// The polls route the vote scenario casts its ballot through.
pub(super) const POLLS: &str = "/api/v1/polls";
/// The scopes every scenario caller carries: `read write` covers every
/// sub-scope the ten journeys touch (decision 9's scope tree).
pub(super) const SCOPES: &str = "read write";
/// The shared 404 body of the status gate.
pub(super) const NOT_FOUND: &str = "Record not found";
/// The 401 body `AuthUser` renders when the request carries no token.
pub(super) const NO_TOKEN: &str = "Invalid credentials";
/// The 401 body for a token that does not resolve.
pub(super) const BAD_TOKEN: &str = "The access token is invalid";
/// The 403 body for an app-only token at a route that needs a user.
pub(super) const USER_REQUIRED: &str = "This API requires an authenticated user";
/// The 403 wording of the `write:statuses` gate (the delete's own).
pub(super) const NO_WRITE: &str = "Insufficient permissions: write:statuses.";
/// The 422 body for a voter's second ballot.
pub(super) const SECOND_BALLOT: &str = "You have already voted on this poll";
/// An id that names no stored row.
pub(super) const UNKNOWN_ID: &str = "01934567-89ab-7cde-f012-3456789abcde";
/// One unicode emoji, as it is stored and as the API names it.
pub(super) const THUMB: &str = "👍";
/// [`THUMB`] percent-encoded as a path segment, as a client sends it.
pub(super) const THUMB_SEGMENT: &str = "%F0%9F%91%8D";
/// A second unicode emoji, for the multi-emoji roll-up.
pub(super) const PARTY: &str = "🎉";
/// [`PARTY`] percent-encoded as a path segment.
pub(super) const PARTY_SEGMENT: &str = "%F0%9F%8E%89";

/// A local user named `nickname` with a bearer token granting `scopes`.
pub(super) async fn actor(pool: &PgPool, nickname: &str, scopes: &str) -> (User, String) {
    let created = common::create_local_user(pool, nickname).await;
    let token = common::create_app_and_token(pool, &created, scopes).await;
    (created, token)
}

/// A local user named `nickname` who may do everything the scenarios do.
pub(super) async fn user(pool: &PgPool, nickname: &str) -> (User, String) {
    actor(pool, nickname, SCOPES).await
}

/// The router and the [`AppState`] behind it over `pool`: requests ride
/// the router, [`common::run_all_jobs`] drains on the state, and the
/// config is the fixed [`common::test_config`] one.
pub(super) fn app_and_state(pool: PgPool) -> (Router, AppState) {
    let state = common::test_app_state(pool, common::test_config());
    (wired_be::app(state.clone()), state)
}

/// The router with a pin limit of `limit`, for the scenario that pins two
/// posts: the shipped default is Pleroma's one.
pub(super) fn app_with_pin_limit(pool: &PgPool, limit: usize) -> Router {
    let mut config = common::test_config();
    config.max_pinned_statuses = limit;
    common::test_app_with_config(pool.clone(), config)
}

/// `GET`s `path`, with `token` when the scenario presents one.
pub(super) async fn get(app: &Router, token: Option<&str>, path: &str) -> common::TestResponse {
    let request = common::TestRequest::get(path);
    match token {
        Some(found) => request.bearer(found).send(app).await,
        None => request.send(app).await,
    }
}

/// `POST /api/v1/statuses` as `token` with the JSON `body`, answered raw
/// for the shape probes.
pub(super) async fn create(app: &Router, token: &str, body: Value) -> common::TestResponse {
    common::TestRequest::post(STATUSES)
        .bearer(token)
        .json(&body)
        .send(app)
        .await
}

/// A create asserted 200 and answered as the status entity it wrote.
pub(super) async fn created(app: &Router, token: &str, body: Value) -> Value {
    let response = create(app, token, body).await;
    common::assert_answer(&response, StatusCode::OK, "the fixture post creates");
    response.json_value()
}

/// A plain public post of `text` by `token`, answered as its entity.
pub(super) async fn post(app: &Router, token: &str, text: &str) -> Value {
    created(app, token, json!({"status": text})).await
}

/// A body-less `POST` status action (`…/favourite`, `…/mute`, …) as
/// `token`, answered raw for the guard probes.
pub(super) async fn action(app: &Router, token: &str, path: &str) -> common::TestResponse {
    common::TestRequest::post(path)
        .bearer(token)
        .send(app)
        .await
}

/// An action asserted 200 and answered as the entity it returns.
pub(super) async fn acted(app: &Router, token: &str, path: &str) -> Value {
    let response = action(app, token, path).await;
    common::assert_answer(&response, StatusCode::OK, "the status action stores");
    response.json_value()
}

/// A `POST` of the JSON `body` to `path` as `token`, asserted 200 and
/// answered: the folder and filing writes, which carry parameters.
pub(super) async fn posted(app: &Router, token: &str, path: &str, body: Value) -> Value {
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
pub(super) async fn reacted(app: &Router, token: &str, path: &str) -> Value {
    let response = common::TestRequest::put(path).bearer(token).send(app).await;
    common::assert_answer(&response, StatusCode::OK, "the reaction stores");
    response.json_value()
}

/// A `DELETE` action (`…/reactions/{emoji}`) asserted 200 and answered as
/// the entity it returns.
pub(super) async fn withdrawn(app: &Router, token: &str, path: &str) -> Value {
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
pub(super) async fn shown(app: &Router, token: Option<&str>, id: &str) -> Value {
    let response = get(app, token, &format!("{STATUSES}/{id}")).await;
    common::assert_answer(&response, StatusCode::OK, "the status reads back");
    response.json_value()
}

/// A status the gate no longer answers: the shared 404 body, for `who`.
pub(super) async fn assert_gone(app: &Router, token: Option<&str>, id: &str, who: &str) {
    let response = get(app, token, &format!("{STATUSES}/{id}")).await;
    common::assert_answer(&response, StatusCode::NOT_FOUND, who);
    common::assert_error(&response, NOT_FOUND, who);
}

/// `POST /api/v1/accounts/{target}/follow` as `token`: the follow of an
/// open local account, granted on the spot — which is what puts the
/// followee's posts on the caller's home timeline.
pub(super) async fn follows(app: &Router, token: &str, target: &str) {
    let related = acted(app, token, &format!("{ACCOUNTS}/{target}/follow")).await;
    assert_eq!(
        related["following"],
        json!(true),
        "the relationship answers granted: {related}"
    );
}

/// The ids of a rendered page, in answer order.
pub(super) fn ids_of(body: &Value) -> Vec<String> {
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
pub(super) fn entity_id(entity: &Value) -> String {
    entity["id"]
        .as_str()
        .expect("the entity carries its id")
        .to_owned()
}

/// The row of a rendered page that carries `wanted`, for `who`.
pub(super) fn row_of<'a>(page: &'a Value, wanted: &str, who: &str) -> &'a Value {
    page.as_array()
        .expect("a page is a JSON array")
        .iter()
        .find(|row| row["id"].as_str() == Some(wanted))
        .unwrap_or_else(|| panic!("the {who} page has no row {wanted}: {page}"))
}

/// The value at the key path `path` of an entity (`&["pleroma",
/// "thread_muted"]`), `null` when any leg is missing.
pub(super) fn pick(entity: &Value, path: &[&str]) -> Value {
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
pub(super) async fn representations(app: &Router, token: Option<&str>, id: &str) -> Vec<Value> {
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
pub(super) fn assert_every(seen: &[Value], path: &[&str], want: &Value, who: &str) {
    let label = path.join(".");
    for (index, entity) in seen.iter().enumerate() {
        assert_eq!(
            pick(entity, path),
            *want,
            "{label} of representation {index}: {who}"
        );
    }
}
