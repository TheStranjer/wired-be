//! The fixtures every streaming child's unit tests share: the state and the
//! database slot a connection is built on, the accounts and the relationship
//! writes the suppression tests seed, the connections the delivery tests
//! assert on, and the event, payload and frame shapes they read.
//!
//! Only compiled for tests; the fixtures that belong to one child alone — the
//! per-kind render hints — sit with that child's own tests.

use std::{
    collections::{HashMap, HashSet},
    sync::Arc,
};

use axum::http::{HeaderMap, HeaderValue, header::SEC_WEBSOCKET_PROTOCOL};
use serde_json::Value;
use sqlx::PgPool;
use uuid::Uuid;

use crate::{
    api::streaming::{connection::Connection, filters::Filters, names::PUBLIC},
    config::Config,
    models::{
        block, filter, mute,
        user::{Flag, NewUser, User, insert},
    },
    state::AppState,
    streaming::hub::{Audience, RenderSpec, StreamEvent},
    test_support::app_state,
};

/// An event on `streams` carrying the entity `payload`.
pub(super) fn event(streams: &[&str], payload: &str) -> StreamEvent {
    StreamEvent {
        streams: streams.iter().copied().map(String::from).collect(),
        event: "update".to_owned(),
        payload: Some(payload.to_owned()),
        audience: Audience::Public,
        render: None,
        local_only: false,
    }
}

/// The payload of a status authored by `author` (a uuid) as `acct`, in the
/// thread `thread`.
pub(super) fn status_payload(author: Uuid, acct: &str, thread: Uuid) -> String {
    format!(
        r#"{{"id":"status-1","account":{{"id":"{author}","acct":"{acct}","url":"https://{acct}/users/x"}},"conversation_id":"{thread}"}}"#
    )
}

/// The header map a `Sec-WebSocket-Protocol: offered` request carries.
pub(super) fn protocol_headers(offered: &str) -> HeaderMap {
    let mut headers = HeaderMap::new();
    let value = HeaderValue::from_str(offered).expect("test subprotocol is header-safe");
    let previous = headers.insert(SEC_WEBSOCKET_PROTOCOL, value);
    assert!(previous.is_none(), "a fresh map holds no header");
    headers
}

/// The query parameter map `pairs` describes.
pub(super) fn query(pairs: &[(&str, &str)]) -> HashMap<String, String> {
    pairs
        .iter()
        .map(|(key, value)| ((*key).to_owned(), (*value).to_owned()))
        .collect()
}

/// The [`AppState`] over `config`, for the tests that read a setting.
pub(super) fn state_with_config(pool: &PgPool, config: Config) -> AppState {
    AppState::for_tests(pool.clone(), config)
}

/// A migrated database slot and the [`AppState`] built on it. The slot is
/// returned alongside, so a test keeps its lease until it ends.
pub(super) async fn state() -> (crate::test_db::TestDb, AppState) {
    let db = crate::test_db::test_db().await.expect("test database slot");
    let state = app_state(db.pool());
    (db, state)
}

/// The id of a fresh local account named `nickname`.
///
/// A connection holds its viewer in an `Arc`, which `User` is not
/// (`User` is a wide row and is never cloned by the serving code), so
/// the fixtures hand out the shared handle too: `&Arc<User>` coerces
/// to `&User` wherever a model call wants the row.
///
/// # Panics
///
/// If the insert fails, e.g. because the nickname is taken.
pub(super) async fn account(state: &AppState, nickname: &str) -> Arc<User> {
    Arc::new(
        insert(
            &state.db,
            &NewUser::local(nickname, &format!("https://test.example/users/{nickname}")),
        )
        .await
        .expect("user fixture inserts"),
    )
}

/// The one-element set `value` is, for building a [`Filters`] by hand.
pub(super) fn one<T>(value: T) -> HashSet<T>
where
    T: Eq + std::hash::Hash,
{
    std::iter::once(value).collect()
}

/// Mutes `author` for `reader`, with Mastodon's default options.
///
/// # Panics
///
/// If the write fails.
pub(super) async fn mute_author(state: &AppState, reader: &User, author: &User) {
    let _mute = mute::mute(&state.db, reader, author, mute::MuteOptions::default())
        .await
        .expect("the mute fixture writes");
}

/// Blocks `author` for `reader`.
///
/// # Panics
///
/// If the write fails.
pub(super) async fn block_author(state: &AppState, reader: &User, author: &User) {
    let _block = block::block(&state.db, &state.config.base_url, reader, author)
        .await
        .expect("the block fixture writes");
}

/// A connection over `state` with `streams` already subscribed and nothing
/// else suppressed: the shape every delivery test asserts on.
pub(super) async fn watching(state: &AppState, streams: &[&str]) -> Connection {
    let mut connection = Connection::new(state.clone(), None)
        .await
        .expect("an anonymous connection loads no filters");
    connection.subscriptions = streams.iter().copied().map(String::from).collect();
    connection
}

/// The id of a fresh local account named `nickname` whose user is
/// deactivated.
///
/// # Panics
///
/// If the insert fails, e.g. because the nickname is taken.
pub(super) async fn inactive_account(state: &AppState, nickname: &str) -> User {
    insert(
        &state.db,
        &NewUser {
            is_active: Flag::new(false),
            ..NewUser::local(nickname, &format!("https://test.example/users/{nickname}"))
        },
    )
    .await
    .expect("the deactivated user fixture inserts")
}

/// A connection streaming for `reader` as its own viewer, with the
/// suppression sets read now and `streams` held.
pub(super) async fn for_reader(
    state: &AppState,
    reader: &Arc<User>,
    streams: &[&str],
) -> Connection {
    let mut connection = watching(state, streams).await;
    let reader_id = reader.id;
    connection.reader = Some(Arc::clone(reader));
    connection.filters = Filters::load(&state.db, Some(reader_id))
        .await
        .expect("the filters load");
    connection
}

/// The entity JSON a frame's Mastodon payload string carries.
///
/// # Panics
///
/// If the frame is no JSON envelope or its payload is no entity JSON.
pub(super) fn frame_payload(frame: &str) -> Value {
    let envelope: Value = serde_json::from_str(frame).expect("the frame is JSON");
    serde_json::from_str(
        envelope["payload"]
            .as_str()
            .expect("the frame carries a payload string"),
    )
    .expect("the payload is the entity JSON")
}

/// A stored word filter of `reader`'s: `phrase` in the single
/// `context`, hidden outright per `irreversible`.
///
/// # Panics
///
/// If the insert fails.
pub(super) async fn seed_word_filter(
    state: &AppState,
    reader: &User,
    phrase: &str,
    context: &str,
    irreversible: bool,
) {
    let _created = filter::phrase::create_phrase(
        &state.db,
        reader.id,
        &filter::phrase::NewPhrase {
            phrase: phrase.to_owned(),
            context: vec![context.to_owned()],
            irreversible,
            ..filter::phrase::NewPhrase::default()
        },
    )
    .await
    .expect("the word filter fixture creates");
}

/// `nickname`'s user with one active word filter for `phrase`:
/// irreversible per `irreversible`, in the single context `context`.
///
/// # Panics
///
/// If either fixture write fails.
pub(super) async fn phrase_filter(
    state: &AppState,
    nickname: &str,
    phrase: &str,
    irreversible: bool,
    context: &str,
) -> Arc<User> {
    let reader = account(state, nickname).await;
    seed_word_filter(state, &reader, phrase, context, irreversible).await;
    reader
}

/// The `update` event that carries only a status render hint: the shape
/// [`crate::services::streaming::status_created`] publishes.
pub(super) fn hinted_update(id: Uuid) -> StreamEvent {
    StreamEvent {
        streams: vec![PUBLIC.to_owned()],
        event: "update".to_owned(),
        payload: None,
        audience: Audience::Public,
        render: Some(RenderSpec::Status(id)),
        local_only: false,
    }
}

/// A bearer token string minted for `reader` granting `scopes`.
pub(super) async fn token_for(state: &AppState, reader: &User, scopes: &str) -> String {
    use crate::models::{
        oauth_app::{NewOauthApp, create as create_app},
        oauth_token::{NewToken, create_token},
    };

    let app = create_app(
        &state.db,
        &NewOauthApp {
            client_name: "streaming test client".to_owned(),
            redirect_uris: "https://client.example/callback".to_owned(),
            scopes: scopes.to_owned(),
            website: None,
            user_id: Some(reader.id),
        },
    )
    .await
    .expect("app fixture inserts");
    create_token(
        &state.db,
        &NewToken {
            user_id: Some(reader.id),
            app_id: app.id,
            scopes: scopes.to_owned(),
            ttl: None,
        },
    )
    .await
    .expect("token fixture inserts")
    .token
}

/// The single-frame view of [`Connection::frames_for`] most delivery tests
/// read: the one frame an event was written as, `None` when it was dropped.
pub(super) trait FrameFor {
    /// The only frame `event` is written as.
    ///
    /// # Panics
    ///
    /// If the event is written as more than one frame.
    async fn frame_for(&self, event: &StreamEvent) -> Option<String>;
}

impl FrameFor for Connection {
    async fn frame_for(&self, event: &StreamEvent) -> Option<String> {
        let mut frames = self.frames_for(event).await;
        assert!(frames.len() <= 1, "one frame expected, got {frames:?}");
        frames.pop()
    }
}
