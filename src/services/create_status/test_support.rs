//! Test-only fixtures shared by the module's children: the state and users
//! the database tests build on, the JSON-to-params reader, the plain
//! [`check_body`] runners, the idempotency and preview row counters, and
//! the reply, media and poll body builders the themed children post with.

use axum::{
    http::StatusCode,
    response::{IntoResponse as _, Response},
};
use serde_json::{Value, json};
use sqlx::PgPool;
use uuid::Uuid;

use super::{Checked, CreateStatusParams, PollRequest, check_body, create_status};
use crate::{
    config::{Config, TEST_BASE_URL},
    entities::status::Status,
    error::AppError,
    models::{
        media_attachment::{self, NewMediaAttachment},
        status::{self, NewStatus},
        user::{NewUser, User, insert as insert_user, local_ap_id},
    },
    state::AppState,
};

/// The instance origin the fixtures post under.
pub(super) const BASE: &str = TEST_BASE_URL;

/// The state under test: `pool` and the test config, with the
/// collaborators the service never touches.
pub(super) fn state(pool: &PgPool) -> AppState {
    AppState::for_tests(pool.clone(), Config::for_tests())
}

/// A local user named `nickname`.
pub(super) async fn user(pool: &PgPool, nickname: &str) -> User {
    insert_user(
        pool,
        &NewUser::local(nickname, &local_ap_id(BASE, nickname)),
    )
    .await
    .expect("the fixture user inserts")
}

/// A body carrying `text` as its only parameter.
pub(super) fn posted(text: &str) -> CreateStatusParams {
    CreateStatusParams {
        status: Some(text.to_owned()),
        ..CreateStatusParams::default()
    }
}

/// A body posting `text` beside the minimum two-option poll, for the
/// persistence and key-race tests.
pub(super) fn polled(text: &str) -> CreateStatusParams {
    CreateStatusParams {
        poll: Some(PollRequest {
            options: vec!["a".to_owned(), "b".to_owned()],
            expires_in: None,
            multiple: None,
            hide_totals: None,
        }),
        ..posted(text)
    }
}

/// The status and the `{"error": …}` body one refusal renders: what the
/// client sees, read off the response rather than the variant.
pub(super) async fn wire(err: AppError) -> (StatusCode, Value) {
    let response: Response = err.into_response();
    let status = response.status();
    let bytes = axum::body::to_bytes(response.into_body(), 1_024)
        .await
        .expect("an error body is collectable");
    (
        status,
        serde_json::from_slice(&bytes).expect("errors are JSON"),
    )
}

/// The one body every validation refusal owes: 422, `{"error": message}`.
pub(super) fn refused(answer: &(StatusCode, Value), message: &str) {
    assert_eq!(
        answer.0,
        StatusCode::UNPROCESSABLE_ENTITY,
        "the status a refusal answers with"
    );
    assert_eq!(
        answer.1,
        json!({ "error": message }),
        "the body that refusal carries"
    );
}

/// A plain post through the service, as `author` with no application.
pub(super) async fn created(state: &AppState, author: &User, params: CreateStatusParams) -> Status {
    create_status(state, author, None, params)
        .await
        .expect("the post creates")
}

/// The stored row of a rendered entity, live and unabridged.
pub(super) async fn stored(pool: &PgPool, entity: &Status) -> status::Status {
    let id = Uuid::parse_str(&entity.id).expect("the entity carries its id");
    status::get_by_id(pool, id)
        .await
        .unwrap()
        .expect("the created status is stored live")
}

/// The parameters a JSON body deserializes to: the same
/// `into_params::<CreateStatusParams>` call an [`ApiBody`] makes, so these
/// tests see a body exactly as the endpoint will.
///
/// [`ApiBody`]: crate::extract::ApiBody
pub(super) fn params(value: Value) -> CreateStatusParams {
    crate::extract::into_params::<CreateStatusParams>(value)
        .expect("every body in these tests fits the parameter type")
}

/// The reading of `body` by [`check_body`] under the stock test
/// configuration and an author who defaults to `public` and to not
/// marking posts sensitive.
pub(super) fn accepted(body: &CreateStatusParams) -> Checked<'_> {
    check_body(&Config::for_tests(), "public", false, body)
        .unwrap_or_else(|err| panic!("this body should be accepted: {err}"))
}

/// The 422 message [`check_body`] answers for `body` under the same
/// configuration and defaults.
pub(super) fn rejection(body: &CreateStatusParams) -> String {
    match check_body(&Config::for_tests(), "public", false, body) {
        Err(err) => err.to_string(),
        Ok(checked) => panic!("this body should be refused, got `{}`", checked.text),
    }
}

/// How many `polls` rows the database holds: one per stored question,
/// none for a plain create, a refused body or a preview.
pub(super) async fn poll_rows(pool: &PgPool) -> i64 {
    sqlx::query_scalar::<_, i64>("SELECT count(*) FROM polls")
        .fetch_one(pool)
        .await
        .expect("the poll count reads")
}

/// How many `idempotency_keys` rows the database holds: one per create
/// that claimed a key, none for a replay or a preview.
pub(super) async fn key_rows(pool: &PgPool) -> i64 {
    sqlx::query_scalar::<_, i64>("SELECT count(*) FROM idempotency_keys")
        .fetch_one(pool)
        .await
        .expect("the count reads")
}

/// The ids of every live status in the database, sorted: what a replay
/// leaves untouched and a fresh create grows by exactly one.
pub(super) async fn live_ids(pool: &PgPool) -> Vec<Uuid> {
    sqlx::query_scalar::<_, Uuid>("SELECT id FROM statuses WHERE deleted_at IS NULL ORDER BY id")
        .fetch_all(pool)
        .await
        .expect("the ids read")
}

/// The `status_mentions` links of one stored status, in position order.
pub(super) async fn mention_ids(pool: &PgPool, status_id: Uuid) -> Vec<Uuid> {
    sqlx::query_as::<_, (Uuid,)>(
        "SELECT user_id FROM status_mentions WHERE status_id = $1 ORDER BY position",
    )
    .bind(status_id)
    .fetch_all(pool)
    .await
    .expect("the mention links read")
    .into_iter()
    .map(|link| link.0)
    .collect()
}

/// The live row behind a status id, for the counters and links a reply
/// or a quote moved on the row it names.
pub(super) async fn parent_row(pool: &PgPool, id: Uuid) -> status::Status {
    status::get_by_id(pool, id)
        .await
        .unwrap()
        .expect("the parent is stored live")
}

/// A body answering the status `parent_id`, with `text` as its post.
pub(super) fn replying(text: &str, parent_id: Uuid) -> CreateStatusParams {
    CreateStatusParams {
        in_reply_to_id: Some(parent_id.to_string()),
        ..posted(text)
    }
}

/// The `{"error": …}` body of the shared 404, plus its status code:
/// what an unanswerable reply parent or quote original owes the client.
pub(super) async fn missed(err: AppError) {
    let (code, body) = wire(err).await;
    assert_eq!(code, StatusCode::NOT_FOUND, "the parent miss is a 404");
    assert_eq!(
        body,
        json!({ "error": "Record not found" }),
        "the shared body"
    );
}

/// A plain stored post written straight through the model — the fixture
/// parent a service test replies to.
pub(super) async fn plain_post(pool: &PgPool, author: &User, text: &str) -> status::Status {
    status::insert(pool, BASE, &NewStatus::local(author, text))
        .await
        .expect("the fixture post inserts")
}

/// The media body: one unattached `image/png` upload by `owner`, as the
/// row a `media_ids` entry names.
pub(super) async fn upload(pool: &PgPool, owner: &User) -> media_attachment::MediaAttachment {
    media_attachment::insert(
        pool,
        &NewMediaAttachment {
            user_id: Some(owner.id),
            url: format!("{BASE}/media/fixture.png"),
            content_type: "image/png".to_owned(),
            ..NewMediaAttachment::default()
        },
    )
    .await
    .expect("a fixture upload inserts")
}

/// A post carrying `text` and `ids` as its `media_ids`, the ids as the
/// client spells them.
pub(super) fn with_media(text: &str, ids: &[&str]) -> CreateStatusParams {
    CreateStatusParams {
        status: Some(text.to_owned()),
        media_ids: Some(ids.iter().map(|id| (*id).to_owned()).collect()),
        ..CreateStatusParams::default()
    }
}

/// The stored media row, for the claims and rollbacks a test inspects.
pub(super) async fn media_row(pool: &PgPool, id: Uuid) -> media_attachment::MediaAttachment {
    media_attachment::get_by_id(pool, id)
        .await
        .unwrap()
        .expect("the fixture upload is stored")
}

/// The ids an entity's `media_attachments` answers, in answer order.
pub(super) fn attachment_ids(entity: &Status) -> Vec<String> {
    entity
        .media_attachments
        .iter()
        .map(|attachment| attachment.id.clone())
        .collect()
}
