//! The streaming WebSocket: `GET /api/v1/streaming`.
//!
//! The reference is Pleroma's `Pleroma.Web.MastodonAPI.WebsocketHandler`
//! (`lib/pleroma/web/mastodon_api/websocket_handler.ex`) for the handshake,
//! the client messages and the heartbeat, and `Pleroma.Web.Streamer`
//! (`lib/pleroma/web/streamer.ex`) for the stream rules and the per-reader
//! filter. Where the ticket pins a wording it wins (decision 16): the 401
//! body, the two error frames and the 30 s ping are the ticket's.
//!
//! One connection is:
//!
//! - **a reader of the hub**. Every connection takes a
//!   [`Hub::subscribe`](crate::streaming::hub::Hub::subscribe) receiver and
//!   forwards the events it may see as Mastodon's text frame,
//!   `{"stream":["user:<reader uuid>"],"event":"update","payload":"<json
//!   string>"}`. An event passes when [`stream_matches`] finds one of the
//!   connection's subscriptions on it, its [`Audience`] covers the socket's
//!   reader (a `Users` event reaches those users only), a `local_only` event
//!   has a reader at all — an anonymous socket never sees one — and the
//!   reader's own `Filters` say nothing against it. An event that carries a
//!   [`render`](crate::streaming::hub::StreamEvent::render) hint instead of
//!   a payload has its entity rendered for this connection's OWN viewer at
//!   send time — one hub event, each socket its own view, the way Pleroma's
//!   `Streamer` renders in the socket process: a hinted status through
//!   [`render_status`], a hinted notification through the notifications
//!   endpoint's builder, a hinted chat through the chats endpoint's builder
//!   and a hinted conversation through the conversations endpoint's one.
//!   A hint the socket cannot answer for its reader — no row, not visible,
//!   a deactivated actor, or text one of the reader's irreversible word
//!   filters matches — drops the event rather than sending an
//!   entity-less frame. The socket's viewer is its authenticated [`User`]
//!   row, loaded once when the socket opens or authenticates.
//! - **its own subscription set**, opened at the handshake by `stream` (plus
//!   `tag` / `list` / `instance`) and changed afterwards by the `subscribe`
//!   and `unsubscribe` frames, capped at [`MAX_SUBSCRIPTIONS`].
//! - **an authentication context**. `access_token` — or, the way the Mastodon
//!   web client sends it, the first `Sec-WebSocket-Protocol` value, which is
//!   echoed back as the chosen subprotocol — names a user. A token that does
//!   not resolve rejects the upgrade with 401
//!   `{"error":"Invalid access token"}`; a public stream needs no token at
//!   all. `{"type":"pleroma:authenticate"}` gives an anonymous socket a user
//!   without reconnecting.
//! - **a heartbeat**. A WebSocket Ping every [`PING_INTERVAL`] (what
//!   [`Config::streaming_ping_interval_secs`] defaults to), and a socket that
//!   has not answered the ping it was last sent is closed.
//!
//! The producing side — who publishes what on which stream — is
//! [`crate::streaming::hub`]; this module only ever reads that bus.
//!
//! [`Config::streaming_ping_interval_secs`]: crate::config::Config::streaming_ping_interval_secs

use std::{
    collections::{HashMap, HashSet},
    convert::Infallible,
    sync::Arc,
    time::Duration,
};

use axum::{
    Router,
    extract::{
        FromRequestParts, Query, State,
        ws::{Message, WebSocket, WebSocketUpgrade},
    },
    http::{HeaderMap, HeaderValue, header::SEC_WEBSOCKET_PROTOCOL, request::Parts},
    response::Response,
    routing::get,
};
use bytes::Bytes;
use serde::{Deserialize, Serialize};
use serde_json::{Map, Value};
use sqlx::PgPool;
use tokio::{
    sync::broadcast,
    time::{Interval, interval_at},
};
use tracing::{debug, warn};
use url::Url;
use uuid::Uuid;

use crate::{
    api::{chats, conversations, notifications},
    auth,
    error::{AppError, AppResult},
    models::{
        block, chat, conversation, conversation_mute, domain_block, filter, hashtag, list, mute,
        notification, status, status_visibility,
        user::{self, User},
    },
    oauth::scopes::{READ, READ_LISTS, READ_NOTIFICATIONS, READ_STATUSES, Scopes},
    services::render_status::{ViewerMode, render_status},
    state::AppState,
    streaming::hub::{Audience, RenderSpec, StreamEvent, stream_matches},
};

/// The heartbeat period of one streaming socket: Pleroma's `@tick`
/// (`lib/pleroma/web/mastodon_api/websocket_handler.ex`).
///
/// It is what [`Config::streaming_ping_interval_secs`] defaults to, and a test
/// keeps the two spans equal.
///
/// [`Config::streaming_ping_interval_secs`]: crate::config::Config::streaming_ping_interval_secs
pub const PING_INTERVAL: Duration = Duration::from_secs(30);
/// How long a connection's `Filters` stay in force before they are read
/// again.
///
/// The ticket's "query once per connection, refreshed every 5 minutes", so a
/// block or a mute lands on a live socket within this window.
pub const FILTER_TTL: Duration = Duration::from_secs(300);
/// How many streams one socket may hold at once (the ticket's limit, which is
/// Mastodon's `MAX_STREAMS_PER_CONNECTION`).
pub const MAX_SUBSCRIPTIONS: usize = 50;
/// 401 body for a presented token that names no live user, and for a private
/// stream on a socket that has no token at all (the ticket's wording).
const INVALID_TOKEN: &str = "Invalid access token";
/// The frame a `subscribe` / `unsubscribe` naming a stream this endpoint does
/// not know (or missing the parameter it needs) answers with.
const UNKNOWN_STREAM_FRAME: &str =
    r#"{"error":"bad_request","error_description":"unknown stream"}"#;
/// The frame a subscription this socket may not make answers with: the stream
/// is refused and the connection goes on holding what it already has.
const UNAUTHORIZED_FRAME: &str = r#"{"error":"unauthorized"}"#;
/// The frame the subscription past [`MAX_SUBSCRIPTIONS`] answers with, refused
/// the same way an unknown stream is.
const FULL_FRAME: &str = r#"{"error":"bad_request","error_description":"too many subscriptions"}"#;
/// 400 body for a request on the streaming path that is not a WebSocket
/// upgrade: Cowboy's own wording, which is what Pleroma answers here.
const NOT_AN_UPGRADE: &str = "Expected a WebSocket upgrade";
/// The query parameter and message field naming the stream to watch.
const STREAM_FIELD: &str = "stream";
/// The query parameter a bearer token may arrive in.
const ACCESS_TOKEN_FIELD: &str = "access_token";
/// The `subscribe` message's `type`.
const SUBSCRIBE: &str = "subscribe";
/// The `unsubscribe` message's `type`.
const UNSUBSCRIBE: &str = "unsubscribe";
/// The message that gives an anonymous socket a user (Pleroma's
/// `pleroma:authenticate`).
const AUTHENTICATE: &str = "pleroma:authenticate";
/// The public timeline, watched by anyone.
const PUBLIC: &str = "public";
/// The local public timeline.
const PUBLIC_LOCAL: &str = "public:local";
/// The public timeline with media attached.
const PUBLIC_MEDIA: &str = "public:media";
/// The local public timeline with media attached.
const PUBLIC_LOCAL_MEDIA: &str = "public:local:media";
/// The public timeline of remote instances, narrowed by `instance`.
const PUBLIC_REMOTE: &str = "public:remote";
/// The remote media timeline, narrowed by `instance`.
const PUBLIC_REMOTE_MEDIA: &str = "public:remote:media";
/// One hashtag's statuses (`tag` names it).
const HASHTAG: &str = "hashtag";
/// One hashtag's local statuses (`tag` names it).
const HASHTAG_LOCAL: &str = "hashtag:local";
/// The reader's own timeline; the socket holds it as the hub's
/// `user:<reader uuid>`.
const USER: &str = "user";
/// The reader's notifications; the socket holds it as the hub's
/// `user:notification:<reader uuid>`.
const USER_NOTIFICATION: &str = "user:notification";
/// The reader's chats (Pleroma's `user:pleroma_chat`); the socket holds it
/// as the hub's `user:pleroma_chat:<reader uuid>`.
const USER_PLEROMA_CHAT: &str = "user:pleroma_chat";
/// The reader's direct messages; the socket holds it as the hub's
/// `direct:<reader uuid>`.
const DIRECT: &str = "direct";
/// One of the reader's lists (`list` names it).
const LIST: &str = "list";

/// The `/api/v1/streaming` router: the WebSocket endpoint and the health
/// probe Mastodon's streaming server answers on the same prefix. Merged into
/// [`crate::api::v1`].
pub fn router() -> Router<AppState> {
    Router::new()
        .route("/streaming", get(connect))
        .route("/streaming/health", get(health))
}

/// `GET /api/v1/streaming/health` — `200` with the text body `OK`, the probe
/// Mastodon's clients poll before opening a socket.
async fn health() -> &'static str {
    "OK"
}

/// The upgrade request's [`WebSocketUpgrade`], or `None` when the request is
/// not an upgrade at all.
///
/// This is an extractor of its own because axum implements neither
/// `Option<WebSocketUpgrade>` nor a hook for the rejection: the endpoint wants
/// a JSON 400 of its own for a plain `GET` on the path, and axum's rejection
/// is a plain-text one, while decision 8 says every failure of this API is
/// `{"error": …}`.
#[derive(Debug)]
struct Upgrade(Option<WebSocketUpgrade>);

impl FromRequestParts<AppState> for Upgrade {
    /// Nothing rejects: a request that cannot be upgraded is the `None` case.
    type Rejection = Infallible;

    async fn from_request_parts(
        parts: &mut Parts,
        state: &AppState,
    ) -> Result<Self, Self::Rejection> {
        Ok(Self(
            WebSocketUpgrade::from_request_parts(parts, state)
                .await
                .ok(),
        ))
    }
}

/// `GET /api/v1/streaming` — the WebSocket upgrade.
///
/// The credential and the initial subscription are settled *before* the
/// upgrade, so a socket that may not be opened is refused as an ordinary JSON
/// error instead of being opened and then closed: 401
/// `{"error":"Invalid access token"}` for a token that names no live user and
/// for a private stream on an anonymous socket, 400 for a stream name this
/// endpoint does not know, and 400 with [`NOT_AN_UPGRADE`] for a request that
/// is no upgrade at all.
///
/// A `Sec-WebSocket-Protocol` the client offered is echoed back as the chosen
/// subprotocol, which is what the Mastodon web client needs to accept the
/// handshake it smuggled its token into.
async fn connect(
    headers: HeaderMap,
    Query(params): Query<HashMap<String, String>>,
    Upgrade(upgrade): Upgrade,
    State(state): State<AppState>,
) -> AppResult<Response> {
    let mut upgrade = upgrade.ok_or_else(|| AppError::BadRequest(NOT_AN_UPGRADE.to_owned()))?;
    if let Some(protocol) = requested_protocol(&headers) {
        upgrade.set_selected_protocol(protocol);
    }
    let credential = match presented_token(&headers, &params) {
        None => None,
        Some(raw) => Some(
            auth::credential(&state, &raw)
                .await?
                .ok_or_else(|| AppError::Unauthorized(INVALID_TOKEN.to_owned()))?,
        ),
    };
    let mut connection = Connection::new(state, credential).await?;
    connection.open_initial(&params).await?;
    // The hub subscription is taken *here*, not in the socket task: the
    // broadcast bus delivers to whoever is subscribed when an event is
    // published, and the upgrade task starts some time after this handler has
    // already answered `101`. Taking the receiver now means an event published
    // the moment the client sees the handshake is buffered for it rather than
    // lost.
    let events = connection.state.hub.subscribe();
    Ok(upgrade.on_upgrade(move |socket| run(socket, connection, events)))
}

/// The first value of the request's `Sec-WebSocket-Protocol` header, split
/// the way `Plug.Conn.Utils.list/1` splits it in Pleroma: on commas, trimmed,
/// empty pieces dropped. This is the value the Mastodon web client puts its
/// bearer token in and the value echoed back as the chosen subprotocol.
fn requested_protocol(headers: &HeaderMap) -> Option<HeaderValue> {
    let offered = headers.get(SEC_WEBSOCKET_PROTOCOL)?.to_str().ok()?;
    let first = offered
        .split(',')
        .map(str::trim)
        .find(|protocol| !protocol.is_empty())?;
    HeaderValue::from_str(first).ok()
}

/// The bearer token the request presents: the first `Sec-WebSocket-Protocol`
/// value first, then the `access_token` query parameter — Pleroma's order in
/// `find_access_token` (`lib/pleroma/web/mastodon_api/websocket_handler.ex`),
/// where the subprotocol is the Mastodon web client's carrier and the
/// parameter the fallback for clients that name their subprotocols something
/// else. An empty value on either side presents no token.
fn presented_token(headers: &HeaderMap, params: &HashMap<String, String>) -> Option<String> {
    let from_protocol =
        || requested_protocol(headers).and_then(|value| value.to_str().ok().map(str::to_owned));
    let from_query = || {
        params
            .get(ACCESS_TOKEN_FIELD)
            .filter(|token| !token.is_empty())
            .cloned()
    };
    from_protocol().or_else(from_query)
}

/// What the socket loop is asked to do with the connection after one input.
#[derive(Debug, PartialEq, Eq)]
enum Flow {
    /// The connection lives on.
    Keep,
    /// The connection is over; stop serving it.
    Stop,
}

/// One input the streaming socket loop waits for.
enum Input {
    /// A frame from the client, or the end of its stream.
    Client(Option<Result<Message, axum::Error>>),
    /// An event from the hub, or the receiver's own report.
    Bus(Result<Arc<StreamEvent>, broadcast::error::RecvError>),
    /// The heartbeat period elapsed.
    Ping,
    /// The suppression sets went stale.
    Refresh,
}

/// One streaming connection: who it streams for, what it watches, and the two
/// timers that keep it honest.
#[derive(Debug)]
struct Connection {
    /// The shared state: the hub to read, and the pool the reader's
    /// suppression sets are (re-)read from.
    state: AppState,
    /// The `users` row of the user the socket authenticated as, read ONCE
    /// when the socket opens (or authenticates later) and held for its
    /// life: every send-time render is this viewer's own view; `None`
    /// while the socket watches public streams only.
    reader: Option<Arc<User>>,
    /// The scopes that user's token granted — empty for an anonymous socket,
    /// which is what turns every private stream into a
    /// [`Refusal::Unauthorized`] subscription.
    scopes: Scopes,
    /// The hub stream names this socket holds, in the order they were opened,
    /// capped at [`MAX_SUBSCRIPTIONS`].
    subscriptions: Vec<String>,
    /// What this reader does not want to see.
    filters: Filters,
    /// Whether the last heartbeat ping is still unanswered.
    awaiting_pong: bool,
    /// The heartbeat timer, armed at [`heartbeat_period`].
    pings: Interval,
    /// The filter-refresh timer, armed at [`FILTER_TTL`].
    refreshes: Interval,
}

impl Connection {
    /// A connection reading `state`'s hub as `credential`'s user (anonymous
    /// when `None`): no subscriptions yet, both timers armed, and the reader's
    /// suppression sets loaded.
    ///
    /// # Errors
    ///
    /// Whatever [`Filters::load`] fails on.
    async fn new(state: AppState, credential: Option<auth::AuthContext>) -> AppResult<Self> {
        let (reader, scopes) = credential.map_or_else(
            || (None, Scopes::parse("")),
            |found| (found.user.map(Arc::new), Scopes::parse(&found.token.scopes)),
        );
        let filters = Filters::load(&state.db, reader.as_ref().map(|user| user.id)).await?;
        let now = tokio::time::Instant::now();
        let ping = heartbeat_period(&state);
        Ok(Self {
            state,
            reader,
            scopes,
            subscriptions: Vec::new(),
            filters,
            awaiting_pong: false,
            pings: interval_at(now + ping, ping),
            refreshes: interval_at(now + FILTER_TTL, FILTER_TTL),
        })
    }

    /// Opens the subscription the upgrade request's `stream` parameter names.
    /// No `stream` is no subscription: the socket is open, authenticated and
    /// free to `subscribe` later — Pleroma's `get_topic(nil, …)` is the same
    /// no-topic pass.
    ///
    /// # Errors
    ///
    /// [`AppError::BadRequest`] for a stream name this endpoint does not
    /// know, [`AppError::Unauthorized`] for a private stream this socket may
    /// not watch.
    async fn open_initial(&mut self, params: &HashMap<String, String>) -> AppResult<()> {
        let Some(name) = params.get(STREAM_FIELD) else {
            return Ok(());
        };
        let topic = self
            .topic_for(name, &StreamArgs::from_query(params))
            .await
            .map_err(Refusal::into_error)?;
        self.subscriptions.push(topic);
        Ok(())
    }

    /// Serves one text frame from the client and answers with the frames it
    /// owes: the ticket's error JSON for a refused subscription, and nothing
    /// for one that succeeded (Pleroma's `pleroma:respond` ack is not part of
    /// what this endpoint pins).
    async fn on_client_text(&mut self, text: &str) -> Vec<String> {
        let Ok(message) = serde_json::from_str::<ClientMessage>(text) else {
            debug!("streaming client sent a frame that is no known message");
            return Vec::new();
        };
        match message.kind.as_str() {
            SUBSCRIBE => self.on_subscribe(&message).await,
            UNSUBSCRIBE => self.on_unsubscribe(&message).await,
            AUTHENTICATE => self.on_authenticate(&message).await,
            other => {
                debug!(kind = other, "ignoring an unknown streaming message");
                Vec::new()
            }
        }
    }

    /// The `subscribe` message: the stream is resolved first, and only one
    /// this socket may watch joins its subscriptions.
    async fn on_subscribe(&mut self, message: &ClientMessage) -> Vec<String> {
        let Some(name) = message.stream.as_deref() else {
            return vec![UNKNOWN_STREAM_FRAME.to_owned()];
        };
        match self.topic_for(name, &message.args()).await {
            Err(refusal) => vec![refusal.frame().to_owned()],
            Ok(topic) if self.subscriptions.contains(&topic) => Vec::new(),
            Ok(_) if self.subscriptions.len() >= MAX_SUBSCRIPTIONS => vec![FULL_FRAME.to_owned()],
            Ok(topic) => {
                self.subscriptions.push(topic);
                Vec::new()
            }
        }
    }

    /// The `unsubscribe` message: the same resolution, and the stream leaves
    /// the subscription set. A stream the socket never subscribed to changes
    /// nothing and answers nothing.
    async fn on_unsubscribe(&mut self, message: &ClientMessage) -> Vec<String> {
        let Some(name) = message.stream.as_deref() else {
            return vec![UNKNOWN_STREAM_FRAME.to_owned()];
        };
        match self.topic_for(name, &message.args()).await {
            Err(refusal) => vec![refusal.frame().to_owned()],
            Ok(topic) => {
                self.subscriptions.retain(|held| *held != topic);
                Vec::new()
            }
        }
    }

    /// The `pleroma:authenticate` message: an anonymous socket takes the
    /// token's user, scopes and suppression sets. Every other case — no token,
    /// a token that names nobody, a socket that already has a user — answers
    /// [`UNAUTHORIZED_FRAME`] and changes nothing.
    async fn on_authenticate(&mut self, message: &ClientMessage) -> Vec<String> {
        let refused = vec![UNAUTHORIZED_FRAME.to_owned()];
        if self.reader.is_some() {
            return refused;
        }
        let Some(raw) = message.token.as_deref().filter(|token| !token.is_empty()) else {
            return refused;
        };
        match auth::credential(&self.state, raw).await {
            Ok(Some(found)) => match found.user {
                Some(reader) => {
                    self.adopt(reader, &found.token.scopes).await;
                    Vec::new()
                }
                None => refused,
            },
            Ok(None) => refused,
            Err(err) => {
                warn!(%err, "streaming authenticate could not read the token");
                refused
            }
        }
    }

    /// Takes an authenticated reader over from an anonymous socket: the user
    /// row, its token's scopes, and the suppression sets that go with them.
    /// A failed read leaves the (empty) sets in place rather than dropping
    /// the connection; the next refresh retries.
    async fn adopt(&mut self, reader: User, scopes: &str) {
        self.scopes = Scopes::parse(scopes);
        let reader_id = reader.id;
        self.reader = Some(Arc::new(reader));
        match Filters::load(&self.state.db, Some(reader_id)).await {
            Ok(filters) => self.filters = filters,
            Err(err) => warn!(%err, "streaming filters could not be loaded on authenticate"),
        }
    }

    /// The hub stream name the requested `name` expands to for this socket.
    ///
    /// The client keeps sending the bare `user`, `user:notification`,
    /// `user:pleroma_chat` and `direct`; what the socket holds is the
    /// canonical per-reader hub name with the reader's uuid suffixed onto
    /// it (`user:<reader uuid>`, `direct:<reader uuid>`, …), the way
    /// Pleroma registers a socket on a `user:<id>` topic. The event's
    /// [`Audience`] stays the second gate: a matching stream name alone
    /// does not deliver to a reader it does not cover. A hashtag and a
    /// list are suffixed with what they are about, exactly as Pleroma's
    /// topics are (`hashtag:<tag>`, `list:<id>`).
    async fn topic_for(&self, name: &str, args: &StreamArgs) -> Result<String, Refusal> {
        if is_user_stream(name) {
            return self.user_topic(name);
        }
        if name == LIST {
            return self.list_topic(args).await;
        }
        public_topic(name, args).ok_or(Refusal::Unknown)
    }

    /// A private user stream: the socket needs a user whose token grants one
    /// of [`PRIVATE_SCOPES`] (Pleroma's `get_topic` `@user_streams` clause is
    /// the same user-bound-token match with one scope per stream; the ticket
    /// lists the three accepted scopes together, and `read` covers them all),
    /// and the topic it holds is the reader's own: the requested `name` with
    /// the reader's uuid suffixed onto it.
    fn user_topic(&self, name: &str) -> Result<String, Refusal> {
        let Some(reader) = self
            .reader_id()
            .filter(|_| grants(&self.scopes, &PRIVATE_SCOPES))
        else {
            return Err(Refusal::Unauthorized);
        };
        Ok(format!("{name}:{reader}"))
    }

    /// A list stream: one of [`PRIVATE_SCOPES`] or [`LIST_SCOPES`], and a list
    /// the reader does not own is the `:bad_topic` Pleroma's
    /// `get_topic("list", …)` answers too.
    async fn list_topic(&self, args: &StreamArgs) -> Result<String, Refusal> {
        let refused = Err(Refusal::Unauthorized);
        let Some(reader) = self.reader_id() else {
            return refused;
        };
        if !(grants(&self.scopes, &PRIVATE_SCOPES) || grants(&self.scopes, &LIST_SCOPES)) {
            return refused;
        }
        let Some(id) = args.list.as_deref().and_then(parse_uuid) else {
            return Err(Refusal::Unknown);
        };
        match list::get_owned(&self.state.db, reader, id).await {
            Ok(Some(_)) => Ok(format!("{LIST}:{id}")),
            Ok(None) => Err(Refusal::Unknown),
            Err(err) => {
                warn!(%err, "streaming list check failed");
                refused
            }
        }
    }

    /// The id of this socket's authenticated reader, `None` while it is
    /// anonymous.
    fn reader_id(&self) -> Option<Uuid> {
        self.reader.as_ref().map(|reader| reader.id)
    }

    /// This socket's viewer row for the send-time renders: its own
    /// authenticated reader, or nobody — the anonymous view — while the
    /// socket has no reader. An authenticated socket never renders in
    /// another reader's view: the viewer is the one loaded at the
    /// handshake, not a row re-read per event.
    fn viewer(&self) -> Option<&User> {
        self.reader.as_ref().map(Arc::as_ref)
    }

    /// The frame to write for `event`, `None` when this connection may not
    /// see it: no subscribed stream, an audience it is outside, a
    /// `local_only` event with no reader to be local for, a reader whose
    /// filters suppress the entity, or a [`RenderSpec`] hint this
    /// socket cannot answer for its own viewer.
    ///
    /// An event that carries its entity as `payload` is written as it is.
    /// An event that carries a [`RenderSpec`] hint instead — what
    /// [`status_created`] and [`status_edited`] publish — has that entity
    /// rendered for this connection's OWN viewer here, the way Pleroma's
    /// `Streamer` renders in the socket process: one hub event, each
    /// socket its own view. A rendered entity then replaces the frame's
    /// `payload` (Mastodon's string-in-envelope rule), and the reader's
    /// `Filters` judge it: relationship suppression weighs the rendered
    /// payload as it weighs any other, and a rendered entity whose text
    /// matches one of the reader's active irreversible word filters is
    /// dropped rather than shown.
    ///
    /// [`status_created`]: crate::services::streaming::status_created
    /// [`status_edited`]: crate::services::streaming::status_edited
    async fn frame_for(&self, event: &StreamEvent) -> Option<String> {
        if !stream_matches(&self.subscriptions, event)
            || !self.audience_allows(event)
            || !self.local_allows(event)
        {
            return None;
        }
        let payload = self.payload_for(event).await?;
        if self.filters.suppresses(payload.as_deref()) {
            return None;
        }
        let frame = Frame {
            stream: event.streams.as_slice(),
            event: event.event.as_str(),
            payload: payload.as_deref(),
        };
        match serde_json::to_string(&frame) {
            Ok(text) => Some(text),
            Err(err) => {
                warn!(%err, "streaming frame could not be serialized");
                None
            }
        }
    }

    /// The payload to write for `event`: the event's own body when it
    /// carries one (the pre-serialized path, unchanged), the hinted entity
    /// rendered for this socket's viewer when it carries a [`RenderSpec`]
    /// hint without a body, and no body for an event that carries neither.
    /// The outer `None` is the drop signal that [`frame_for`] reads: a
    /// hint the socket cannot answer for its reader has no entity to show,
    /// and Mastodon's `update` *is* its entity — silence beats an
    /// entity-less frame.
    async fn payload_for(&self, event: &StreamEvent) -> Option<Option<String>> {
        if event.payload.is_some() {
            return Some(event.payload.clone());
        }
        let Some(render) = event.render.as_ref() else {
            return Some(None);
        };
        let payload = self.render_hint(render).await?;
        if self.filters.suppresses_words(&payload) {
            debug!("a streamed render hint matches the reader's irreversible word filters");
            return None;
        }
        Some(Some(payload))
    }

    /// The entity JSON a [`RenderSpec`] hint answers to for this socket's
    /// viewer: each kind built by its own endpoint's render path, so one
    /// hub event becomes every reader's own view. `None` — the drop
    /// signal — when this socket cannot be shown the entity at all.
    async fn render_hint(&self, render: &RenderSpec) -> Option<String> {
        match render {
            RenderSpec::Status(id) => self.render_hinted_status(*id).await,
            RenderSpec::Notification(id) => self.render_hinted_notification(*id).await,
            RenderSpec::Chat(id) => self.render_hinted_chat(*id).await,
            RenderSpec::Conversation(id) => self.render_hinted_conversation(*id).await,
        }
    }

    /// The Status entity JSON of the hinted status `id`, rendered for this
    /// socket's viewer. `None` — the drop signal — when the row is gone or
    /// unreadable (a status deleted before this socket caught up reads as
    /// gone: [`status::get_by_id`] hides tombstones), when
    /// [`status_visibility::can_view`] says no for this viewer or the
    /// author's account is deactivated, or when the renderer has no view
    /// of it at all.
    ///
    /// [`status::get_by_id`]: crate::models::status::get_by_id
    async fn render_hinted_status(&self, id: Uuid) -> Option<String> {
        let row = self.hinted_status_row(id).await?;
        if !self.status_admits_reader(&row, id).await {
            return None;
        }
        let viewer = self.viewer();
        match render_status(&self.state, ViewerMode::User(viewer), &row).await {
            Ok(Some(entity)) => entity_json(&entity, id),
            Ok(None) => {
                debug!(%id, "a streaming render hint has no renderable view");
                None
            }
            Err(err) => {
                warn!(%err, %id, "a streaming render hint could not be rendered");
                None
            }
        }
    }

    /// The live row the status hint names, `None` when it is gone (the
    /// delete beat this socket to it) or unreadable (a log line).
    async fn hinted_status_row(&self, id: Uuid) -> Option<status::Status> {
        match status::get_by_id(&self.state.db, id).await {
            Ok(Some(row)) => Some(row),
            Ok(None) => {
                debug!(%id, "a streaming render hint names no live status");
                None
            }
            Err(err) => {
                warn!(%err, %id, "a streaming render hint could not be read");
                None
            }
        }
    }

    /// Whether this socket's viewer may see the hinted status row at all:
    /// the shared visibility rules (the anonymous socket gets the
    /// anonymous view, a private post reaches its author and the followers
    /// `can_view` admits — and nobody else), and an author whose account
    /// is deactivated answers nothing to anybody.
    async fn status_admits_reader(&self, row: &status::Status, id: Uuid) -> bool {
        let viewer = self.viewer();
        match status_visibility::can_view(&self.state.db, row, viewer).await {
            Ok(true) => {}
            Ok(false) => {
                debug!(%id, "a streaming render hint is not visible to its reader");
                return false;
            }
            Err(err) => {
                warn!(%err, %id, "a streaming render hint could not be judged");
                return false;
            }
        }
        match user::get_by_id(&self.state.db, row.user_id).await {
            Ok(Some(author)) => {
                let active = author.is_active.get();
                if !active {
                    debug!(%id, "the author of a streaming render hint is deactivated");
                }
                active
            }
            Ok(None) => {
                debug!(%id, "a streaming render hint names a status with no author");
                false
            }
            Err(err) => {
                warn!(%err, %id, "the author of a streaming render hint could not be read");
                false
            }
        }
    }

    /// The Notification entity JSON of the hinted notification `id`, built
    /// the way [`crate::api::notifications`] renders one and for this
    /// socket's reader only: `None` — the drop signal — when the socket
    /// has no viewer, the row is not a live notification addressed to
    /// this reader (a foreign, unknown or dismissed row, or one whose
    /// actor is deactivated, all of which
    /// [`notification::get_visible`] answers `None` for), the actor left
    /// no user row, or any read or render fails.
    async fn render_hinted_notification(&self, id: Uuid) -> Option<String> {
        let Some(viewer) = self.viewer() else {
            debug!(%id, "a streaming notification hint reaches a readerless socket");
            return None;
        };
        let row = match notification::get_visible(&self.state.db, viewer.id, id).await {
            Ok(Some(row)) => row,
            Ok(None) => {
                debug!(%id, "a streaming notification hint is no live notification of the reader's");
                return None;
            }
            Err(err) => {
                warn!(%err, %id, "a streaming notification hint could not be read");
                return None;
            }
        };
        let rows = [row];
        // Like the endpoint's single-notification read, a hinted row keeps
        // its full render: `filters: &[]` is that pass's no-op spelling.
        let entities =
            match notifications::render_notifications(&self.state, viewer, &rows, &[]).await {
                Ok(entities) => entities,
                Err(err) => {
                    warn!(%err, %id, "a streaming notification hint could not be rendered");
                    return None;
                }
            };
        let Some(entity) = entities.into_iter().next() else {
            debug!(%id, "a streaming notification hint has no renderable actor");
            return None;
        };
        entity_json(&entity, id)
    }

    /// The Chat entity JSON of the hinted chat `id` (the reader's own row
    /// of it, the way the chats endpoints address a chat), built by
    /// [`crate::api::chats`]' entity builder. `None` — the drop signal —
    /// when the socket has no viewer, no chat of this reader's carries
    /// that id, or the other participant is gone or deactivated.
    async fn render_hinted_chat(&self, id: Uuid) -> Option<String> {
        let Some(reader) = self.reader_id() else {
            debug!(%id, "a streaming chat hint reaches a readerless socket");
            return None;
        };
        let row = match chat::get_owned(&self.state.db, reader, id).await {
            Ok(Some(row)) => row,
            Ok(None) => {
                debug!(%id, "a streaming chat hint names no chat of the reader's");
                return None;
            }
            Err(err) => {
                warn!(%err, %id, "a streaming chat hint could not be read");
                return None;
            }
        };
        match user::get_by_id(&self.state.db, row.recipient_id).await {
            Ok(Some(recipient)) if recipient.is_active.get() => {}
            Ok(Some(_)) => {
                debug!(%id, "the other participant of a streaming chat hint is deactivated");
                return None;
            }
            Ok(None) => {
                debug!(%id, "a streaming chat hint names a chat with no recipient");
                return None;
            }
            Err(err) => {
                warn!(%err, %id, "the other participant of a streaming chat hint could not be read");
                return None;
            }
        }
        match chats::chat_entity(&self.state, &row).await {
            Ok(entity) => entity_json(&entity, id),
            Err(err) => {
                warn!(%err, %id, "a streaming chat hint could not be rendered");
                None
            }
        }
    }

    /// The Conversation entity JSON of the hinted participation `id` (the
    /// reader's own row of the direct-message thread, the way the
    /// conversations endpoints address a conversation), built by
    /// [`crate::api::conversations`]' entity builder. `None` — the drop
    /// signal — when the socket has no viewer or no participation of this
    /// reader carries that id: [`conversation::get_participation`] answers
    /// `None` for a foreign id and for no id at all, so another reader's
    /// conversation never renders here.
    async fn render_hinted_conversation(&self, id: Uuid) -> Option<String> {
        let Some(viewer) = self.viewer() else {
            debug!(%id, "a streaming conversation hint reaches a readerless socket");
            return None;
        };
        let row = match conversation::get_participation(&self.state.db, viewer.id, id).await {
            Ok(Some(row)) => row,
            Ok(None) => {
                debug!(%id, "a streaming conversation hint names no participation of the reader's");
                return None;
            }
            Err(err) => {
                warn!(%err, %id, "a streaming conversation hint could not be read");
                return None;
            }
        };
        let rows = [row];
        // Like the endpoint's single-participation reads, a hinted row goes
        // through the page renderer on its own; that builder answers one
        // entity per row it is handed.
        match conversations::conversation_entities(&self.state, viewer, &rows).await {
            Ok(entities) => entities
                .into_iter()
                .next()
                .and_then(|entity| entity_json(&entity, id)),
            Err(err) => {
                warn!(%err, %id, "a streaming conversation hint could not be rendered");
                None
            }
        }
    }

    /// Whether the event's audience covers this connection's reader: a public
    /// event reaches every socket, an `Everyone` event only an authenticated
    /// one, and a `Users` event only the users it names.
    fn audience_allows(&self, event: &StreamEvent) -> bool {
        match &event.audience {
            Audience::Public => true,
            Audience::Everyone => self.reader.is_some(),
            Audience::Users(ids) => self.reader_id().is_some_and(|reader| ids.contains(&reader)),
        }
    }

    /// Whether the event's `local_only` flag lets this connection see it at
    /// all: the flag marks an event about this instance's local users and
    /// entities, and a socket with no authenticated reader is not one of the
    /// local viewers it may reach — whatever its audience or stream names say.
    const fn local_allows(&self, event: &StreamEvent) -> bool {
        !event.local_only || self.reader.is_some()
    }
}

/// The heartbeat period this instance streams with:
/// [`Config::streaming_ping_interval_secs`], whose default is
/// [`PING_INTERVAL`]. A configured `0` is read as one second: a
/// zero-length period would spin the socket loop.
fn heartbeat_period(state: &AppState) -> Duration {
    let configured = state.config.streaming_ping_interval_secs;
    let secs = u64::try_from(configured).unwrap_or(PING_INTERVAL.as_secs());
    Duration::from_secs(secs.max(1))
}

/// The scopes the ticket accepts for one of the private streams — `user`,
/// `user:notification`, `user:pleroma_chat`, `direct` and `list`: any one of
/// them, and through [`Scopes::contains`] a plain `read` token covers them all.
const PRIVATE_SCOPES: [&str; 3] = [READ, READ_STATUSES, READ_NOTIFICATIONS];
/// The scope [`LIST`] additionally accepts: Pleroma's `read:lists` on its
/// `get_topic("list", …)` clause, beside the ticket's private-set list.
const LIST_SCOPES: [&str; 1] = [READ_LISTS];

/// Whether `granted` covers any scope of `allowed` (through the Mastodon
/// hierarchy [`Scopes::contains`] implements).
fn grants(granted: &Scopes, allowed: &[&'static str]) -> bool {
    allowed.iter().any(|scope| granted.contains(scope))
}

/// Whether `name` is one of the private user streams, the streams that need a
/// user and a read scope: Pleroma's `@user_streams` list.
fn is_user_stream(name: &str) -> bool {
    matches!(name, USER | USER_NOTIFICATION | USER_PLEROMA_CHAT | DIRECT)
}

/// The hub stream name a public stream expands to, `None` when `name` names
/// no public stream or lacks the parameter it needs. A remote stream is
/// narrowed to `public:remote:<instance>` when the client sent `instance` and
/// stays `public:remote` when it did not (Pleroma requires the parameter; the
/// ticket calls it optional), and a hashtag stream is
/// `hashtag:<normalized tag>`.
fn public_topic(name: &str, args: &StreamArgs) -> Option<String> {
    match name {
        PUBLIC | PUBLIC_LOCAL | PUBLIC_MEDIA | PUBLIC_LOCAL_MEDIA => Some(name.to_owned()),
        PUBLIC_REMOTE | PUBLIC_REMOTE_MEDIA => args
            .instance
            .as_deref()
            .map(|host| format!("{name}:{}", host.trim().to_lowercase()))
            .or_else(|| Some(name.to_owned())),
        HASHTAG | HASHTAG_LOCAL => args
            .tag
            .as_deref()
            .and_then(hashtag::normalize_name)
            .map(|tag| format!("{name}:{tag}")),
        _ => None,
    }
}

/// Why one requested stream cannot be watched: the two refusals Pleroma's
/// `get_topic` answers (`:bad_topic` and `:unauthorized`) and the two frames
/// the ticket pins for them.
#[derive(Debug, PartialEq, Eq)]
enum Refusal {
    /// No stream of that name, or a parameter it needs is missing or unusable
    /// (Pleroma's `:bad_topic`).
    Unknown,
    /// The stream exists and this socket may not watch it (Pleroma's
    /// `:unauthorized`).
    Unauthorized,
}

impl Refusal {
    /// The text frame a message-level refusal is answered with.
    const fn frame(self) -> &'static str {
        match self {
            Self::Unknown => UNKNOWN_STREAM_FRAME,
            Self::Unauthorized => UNAUTHORIZED_FRAME,
        }
    }

    /// The HTTP error an upgrade-time refusal is answered with: an upgrade is
    /// no frame, so the same two refusals become the 400 and the 401.
    fn into_error(self) -> AppError {
        match self {
            Self::Unknown => AppError::BadRequest(UNKNOWN_STREAM_FRAME.to_owned()),
            Self::Unauthorized => AppError::Unauthorized(INVALID_TOKEN.to_owned()),
        }
    }
}

/// The parameters that qualify a stream name — `tag`, `list` and `instance` —
/// read from the handshake query string or from a client message: the same
/// three keys in both places, as they are in the one `params` map Pleroma's
/// socket handler is handed.
#[derive(Debug, Default)]
struct StreamArgs {
    /// The hashtag of a `hashtag` or `hashtag:local` stream.
    tag: Option<String>,
    /// The list id of a `list` stream.
    list: Option<String>,
    /// The host of a `public:remote` or `public:remote:media` stream.
    instance: Option<String>,
}

impl StreamArgs {
    /// The qualifying parameters of the handshake query string; an empty
    /// value is no value at all.
    fn from_query(params: &HashMap<String, String>) -> Self {
        let take = |key: &str| params.get(key).filter(|value| !value.is_empty()).cloned();
        Self {
            tag: take("tag"),
            list: take("list"),
            instance: take("instance"),
        }
    }

    /// The qualifying parameters of one client message.
    fn of(message: &ClientMessage) -> Self {
        Self {
            tag: message.tag.clone(),
            list: message.list.clone(),
            instance: message.instance.clone(),
        }
    }
}

/// One text frame the client sent: a `subscribe` / `unsubscribe` of a stream,
/// or a `pleroma:authenticate` with a token. Keys the message does not need
/// are ignored, and so is a frame that is none of these — Pleroma logs those
/// and keeps the connection.
#[derive(Debug, Deserialize)]
struct ClientMessage {
    /// The message kind: `subscribe`, `unsubscribe` or `pleroma:authenticate`.
    #[serde(rename = "type")]
    kind: String,
    /// The stream the message names.
    #[serde(default)]
    stream: Option<String>,
    /// The `tag` of a hashtag stream.
    #[serde(default)]
    tag: Option<String>,
    /// The `list` of a list stream.
    #[serde(default)]
    list: Option<String>,
    /// The `instance` of a remote stream.
    #[serde(default)]
    instance: Option<String>,
    /// The token of a `pleroma:authenticate` message.
    #[serde(default)]
    token: Option<String>,
}

impl ClientMessage {
    /// The qualifying parameters this message carries.
    fn args(&self) -> StreamArgs {
        StreamArgs::of(self)
    }
}

/// One frame written to the socket, in the shape Mastodon's streaming clients
/// read: the streams the event belongs to, the event name, and the entity as a
/// JSON *string* the client parses again.
#[derive(Debug, Serialize)]
struct Frame<'event> {
    /// The event's own stream names, as published (Mastodon's `stream` array).
    stream: &'event [String],
    /// `update`, `delete`, `notification`, `status.update`, …
    event: &'event str,
    /// The entity as its JSON text; a `delete` event carries the deleted
    /// status's id here. Omitted for the events that carry no body at all.
    #[serde(skip_serializing_if = "Option::is_none")]
    payload: Option<&'event str>,
}

/// What a reader does not want to see, read once when the socket opens and
/// refreshed every [`FILTER_TTL`]: the accounts it blocked and muted, the
/// domains it blocked, the threads it muted, and its active irreversible
/// word filters in the streaming contexts.
///
/// Pleroma recomputes the same sets per event in `Streamer.filtered_by_user?`
/// (`lib/pleroma/web/streamer.ex`) against its cache; one set per connection
/// is the same answer for a fraction of the queries, and a change lands on a
/// live socket one refresh later at the latest.
#[derive(Debug, Default)]
struct Filters {
    /// Accounts the reader blocked.
    blocked: HashSet<Uuid>,
    /// Accounts the reader muted.
    muted: HashSet<Uuid>,
    /// Hosts the reader blocked, in [`domain_block::normalize_domain`]'s
    /// canonical spelling.
    domains: HashSet<String>,
    /// Threads the reader muted (`conversation_mutes`).
    threads: HashSet<Uuid>,
    /// The reader's active, irreversible word filters in the streaming
    /// contexts ([`STREAMING_CONTEXTS`]), verbatim [`filter::Filter`] rows
    /// for [`filter::matches`] to run.
    words: Vec<filter::Filter>,
}

/// The filter contexts streaming answers: `home` for the reader's own,
/// direct and list timelines, `notifications` for the notification stream
/// and `public` for the public, media, remote and hashtag timelines. A
/// filter active in any one of them judges every entity a socket renders;
/// the `thread` and `account` contexts belong to the HTTP reads that name
/// them, not to a stream.
const STREAMING_CONTEXTS: [&str; 3] = ["home", "notifications", "public"];

impl Filters {
    /// The suppression sets of `reader`, read in five queries. An anonymous
    /// reader suppresses nothing and asks the database nothing.
    ///
    /// # Errors
    ///
    /// Whatever any of the five reads fails on.
    async fn load(pool: &PgPool, reader: Option<Uuid>) -> AppResult<Self> {
        let Some(reader) = reader else {
            return Ok(Self::default());
        };
        let blocked = block::blocked_ids(pool, reader).await?;
        let muted = mute::muted_ids(pool, reader).await?;
        let domains = domain_block::blocked_domains(pool, reader).await?;
        let threads = conversation_mute::muted_ids(pool, reader).await?;
        let words = word_filters(pool, reader).await?;
        Ok(Self {
            blocked: blocked.into_iter().collect(),
            muted: muted.into_iter().collect(),
            domains: domains.into_iter().collect(),
            threads: threads.into_iter().collect(),
            words,
        })
    }

    /// Whether the reader has no relationship suppression at all, so no
    /// payload needs reading for [`Filters::suppresses`]. The word filters
    /// are a separate gate with a guard of their own.
    fn is_empty(&self) -> bool {
        self.blocked.is_empty()
            && self.muted.is_empty()
            && self.domains.is_empty()
            && self.threads.is_empty()
    }

    /// Whether the payload one frame would carry is something this reader
    /// suppresses: an entity authored by a blocked or muted account, posted
    /// from a blocked domain, or part of a muted thread. A payload that
    /// names none of those — a `delete`, whose payload is only a status id,
    /// or a body-less event — is never suppressed: there is nothing to
    /// object to. The payload judged is the one [`frame_for`] resolved, so
    /// a render-hinted event is weighed by its rendered entity too.
    fn suppresses(&self, payload: Option<&str>) -> bool {
        if self.is_empty() {
            return false;
        }
        let facts = Facts::of(payload);
        self.suppressed_author(&facts)
            || facts.hosts.iter().any(|host| self.domains.contains(host))
            || facts
                .threads
                .iter()
                .any(|thread| self.threads.contains(thread))
    }

    /// Whether any author of the entity is blocked or muted. A reblog is
    /// judged by both its own account and the one it boosted, which is what
    /// Pleroma's `filtered_by_user?` asks of `item` and its `parent`.
    fn suppressed_author(&self, facts: &Facts) -> bool {
        facts
            .authors
            .iter()
            .any(|author| self.blocked.contains(author) || self.muted.contains(author))
    }

    /// Whether the text a rendered entity carries matches one of the
    /// reader's active irreversible word filters — the streaming drop for
    /// a phrase the reader refuses to see even once. Only rendered
    /// entities are weighed: a pre-serialized payload event stays with
    /// [`Filters::suppresses`], the suppression it has always had.
    fn suppresses_words(&self, payload: &str) -> bool {
        if self.words.is_empty() {
            return false;
        }
        let texts = entity_texts(payload);
        texts
            .iter()
            .any(|text| self.words.iter().any(|word| filter::matches(word, text)))
    }
}

/// The reader's active word filters that apply to streaming: the
/// [`filter::list_active`] rows that are irreversible (a warn filter only
/// tells the client to cover the post, it never drops it) and name at
/// least one of the [`STREAMING_CONTEXTS`].
async fn word_filters(pool: &PgPool, reader: Uuid) -> AppResult<Vec<filter::Filter>> {
    let active = filter::list_active(pool, reader).await?;
    Ok(active
        .into_iter()
        .filter(|row| {
            row.irreversible
                && row
                    .context
                    .iter()
                    .any(|context| STREAMING_CONTEXTS.contains(&context.as_str()))
        })
        .collect())
}

/// The serialized entity JSON of a rendered hint, `None` with a log when
/// it would not serialize.
fn entity_json<T: Serialize>(entity: &T, id: Uuid) -> Option<String> {
    match serde_json::to_string(entity) {
        Ok(text) => Some(text),
        Err(err) => {
            warn!(%err, %id, "a streamed entity could not be serialized");
            None
        }
    }
}

/// The filterable text a rendered entity payload states: every
/// `content` and `spoiler_text` string it carries, nested entities
/// included — a reblog's original, a notification's status, a chat's last
/// message. A payload that is no JSON object, or holds neither key,
/// states no text and matches nothing.
fn entity_texts(payload: &str) -> Vec<String> {
    let Ok(value) = serde_json::from_str::<Value>(payload) else {
        debug!("streaming render payload is no JSON");
        return Vec::new();
    };
    let mut texts = Vec::new();
    collect_texts(&value, &mut texts);
    texts
}

/// Records the `content` / `spoiler_text` strings of `value`, walking
/// every nested object and array so one pass reaches the whole entity.
fn collect_texts(value: &Value, texts: &mut Vec<String>) {
    match value {
        Value::Object(map) => {
            for key in ["content", "spoiler_text"] {
                if let Some(text) = map.get(key).and_then(Value::as_str) {
                    texts.push(text.to_owned());
                }
            }
            for nested in map.values() {
                collect_texts(nested, texts);
            }
        }
        Value::Array(items) => {
            for item in items {
                collect_texts(item, texts);
            }
        }
        Value::Null | Value::Bool(_) | Value::Number(_) | Value::String(_) => {}
    }
}

/// What one event's payload says about the entity it carries, as far as a
/// reader's [`Filters`] can read it: the accounts that wrote it, the hosts
/// those accounts live on, and the threads they belong to.
///
/// The hub carries no authorship of its own, so all of it is parsed out of the
/// entity JSON, whose shape is the Status entity's: `account` (and a reblog's
/// own `account`) name the authors, `acct` then `url` name the host, and
/// `conversation_id` name the thread — the column this schema keys a thread on
/// and the `pleroma.conversation_id` key of Pleroma's Status entity
/// (`lib/pleroma/web/mastodon_api/views/status_view.ex`). A payload that is no
/// JSON object names nothing and suppresses nothing.
#[derive(Debug, Default)]
struct Facts {
    /// The author account ids: the status's own first, a reblog's second.
    authors: Vec<Uuid>,
    /// The hosts of those same accounts; empty for local authors, whose bare
    /// `acct` names no domain.
    hosts: Vec<String>,
    /// The threads the entity belongs to: the status's and the reblog's.
    threads: Vec<Uuid>,
}

impl Facts {
    /// The facts one frame payload states.
    fn of(payload: Option<&str>) -> Self {
        let mut facts = Self::default();
        let Some(payload) = payload else {
            return facts;
        };
        match serde_json::from_str::<Value>(payload) {
            Ok(Value::Object(entity)) => facts.note_entity(&entity),
            Ok(_scalar_or_list) => debug!("streaming payload is no entity object"),
            Err(err) => debug!(%err, "streaming payload is no JSON"),
        }
        facts
    }

    /// Records what one status-shaped object says — its author account and the
    /// thread it belongs to — and, when it is a reblog, what the boosted
    /// status says too. Pleroma's `filtered_by_user?` asks the same pair of
    /// `item` and its `parent`.
    fn note_entity(&mut self, entity: &Map<String, Value>) {
        self.note_account(entity.get("account"));
        self.threads.extend(thread_of(entity));
        let pleroma = entity.get("pleroma").and_then(Value::as_object);
        self.threads.extend(pleroma.and_then(thread_of));
        if let Some(Value::Object(reblog)) = entity.get("reblog") {
            self.note_entity(reblog);
        }
    }

    /// Records the author and the host of one `account` object, if the value
    /// is one.
    fn note_account(&mut self, account: Option<&Value>) {
        let Some(Value::Object(account)) = account else {
            return;
        };
        if let Some(id) = account.get("id").and_then(value_uuid) {
            self.authors.push(id);
        }
        self.hosts.extend(account_host(account));
    }
}

/// The thread `entity` names: `conversation_id`, a uuid string (decision 4).
fn thread_of(entity: &Map<String, Value>) -> Option<Uuid> {
    entity.get("conversation_id").and_then(value_uuid)
}

/// The host an account object names: the domain of its `acct` handle
/// (`user@host`, decision 7's shape for a remote account) and, when the handle
/// carries no domain, the authority of its profile `url`. A remote account
/// reports its own instance either way; a local one reports this instance,
/// which a reader suppresses only by blocking it.
fn account_host(account: &Map<String, Value>) -> Option<String> {
    let handle = account
        .get("acct")
        .and_then(Value::as_str)
        .and_then(|acct| acct.rsplit_once('@').map(|(_, host)| host));
    let url = account
        .get("url")
        .and_then(Value::as_str)
        .and_then(|url| Url::parse(url).ok())
        .and_then(|parsed| parsed.host_str().map(str::to_owned));
    domain_block::normalize_domain(handle.or(url.as_deref())?)
}

/// A uuid out of a JSON value: the hyphenated string every id in this API is
/// (decision 4).
fn value_uuid(value: &Value) -> Option<Uuid> {
    let text = value.as_str()?;
    parse_uuid(text)
}

/// A uuid out of a query parameter or a message field.
fn parse_uuid(text: &str) -> Option<Uuid> {
    Uuid::parse_str(text).ok()
}

/// Serves `socket` until either peer ends the connection.
///
/// One task owns the socket and waits on four things at once: the client's
/// frames, the hub's `events` (taken by [`connect`], before the handshake was
/// answered), the heartbeat and the filter refresh. Every write to the socket
/// happens here, which is why the socket never needs splitting into halves.
async fn run(
    mut socket: WebSocket,
    mut connection: Connection,
    mut events: broadcast::Receiver<Arc<StreamEvent>>,
) {
    debug!(
        subscriptions = connection.subscriptions.len(),
        authenticated = connection.reader.is_some(),
        "streaming connection opened"
    );
    loop {
        let input = tokio::select! {
            received = socket.recv() => Input::Client(received),
            event = events.recv() => Input::Bus(event),
            _ = connection.pings.tick() => Input::Ping,
            _ = connection.refreshes.tick() => Input::Refresh,
        };
        if serve_one(&mut socket, &mut connection, input).await == Flow::Stop {
            break;
        }
    }
    debug!("streaming connection closed");
}

/// Handles one [`Input`] and reports whether the connection lives on.
async fn serve_one(socket: &mut WebSocket, connection: &mut Connection, input: Input) -> Flow {
    match input {
        Input::Client(received) => from_client(socket, connection, received).await,
        Input::Bus(received) => from_hub(socket, connection, received).await,
        Input::Ping => heartbeat(socket, connection).await,
        Input::Refresh => refresh_filters(connection).await,
    }
}

/// A frame from the client: any frame at all is proof the socket answers its
/// pings, a text frame may change the connection, and the end of the stream
/// ends it.
async fn from_client(
    socket: &mut WebSocket,
    connection: &mut Connection,
    received: Option<Result<Message, axum::Error>>,
) -> Flow {
    let Some(message) = received else {
        return Flow::Stop;
    };
    connection.awaiting_pong = false;
    match message {
        Ok(Message::Text(text)) => {
            for frame in connection.on_client_text(text.as_str()).await {
                if write(socket, Message::Text(frame.into())).await == Flow::Stop {
                    return Flow::Stop;
                }
            }
            Flow::Keep
        }
        Ok(Message::Close(_)) => Flow::Stop,
        // An inbound `Ping` is answered by the websocket layer itself; a
        // `Pong`, a binary frame or a protocol-level frame carries nothing
        // this endpoint acts on. Their only meaning is that the client lives.
        Ok(Message::Ping(_) | Message::Pong(_) | Message::Binary(_)) => Flow::Keep,
        Err(err) => {
            debug!(%err, "streaming socket failed");
            Flow::Stop
        }
    }
}

/// An event from the hub: written when this connection may see it, dropped
/// silently when it may not. A lagged receiver skips what fell out of the
/// buffer and keeps streaming.
async fn from_hub(
    socket: &mut WebSocket,
    connection: &Connection,
    received: Result<Arc<StreamEvent>, broadcast::error::RecvError>,
) -> Flow {
    match received {
        Ok(event) => match connection.frame_for(&event).await {
            None => Flow::Keep,
            Some(frame) => write(socket, Message::Text(frame.into())).await,
        },
        Err(broadcast::error::RecvError::Lagged(skipped)) => {
            warn!(skipped, "streaming connection fell behind the hub");
            Flow::Keep
        }
        Err(broadcast::error::RecvError::Closed) => Flow::Stop,
    }
}

/// The heartbeat tick: a socket with a ping still outstanding never answered
/// the last one and is closed, every other socket gets the next ping.
async fn heartbeat(socket: &mut WebSocket, connection: &mut Connection) -> Flow {
    if connection.awaiting_pong {
        debug!("closing a streaming socket that never answered a ping");
        let _closed = socket.send(Message::Close(None)).await;
        return Flow::Stop;
    }
    connection.awaiting_pong = true;
    write(socket, Message::Ping(Bytes::new())).await
}

/// Reads the reader's suppression sets again.
async fn refresh_filters(connection: &mut Connection) -> Flow {
    let Some(reader) = connection.reader_id() else {
        return Flow::Keep;
    };
    match Filters::load(&connection.state.db, Some(reader)).await {
        Ok(filters) => connection.filters = filters,
        Err(err) => warn!(%err, "streaming filters could not be refreshed; the old set stays"),
    }
    Flow::Keep
}

/// Writes one frame to the socket; a failed write ends the connection.
async fn write(socket: &mut WebSocket, message: Message) -> Flow {
    match socket.send(message).await {
        Ok(()) => Flow::Keep,
        Err(err) => {
            debug!(%err, "streaming write failed");
            Flow::Stop
        }
    }
}

#[cfg(test)]
mod tests {
    use axum::{http::StatusCode, response::IntoResponse as _};

    use super::*;
    use crate::{
        config::{Config, DEFAULT_STREAMING_PING_INTERVAL_SECS, TEST_BASE_URL},
        http_client::HttpClient,
        models::{
            follow,
            list::NewList,
            status::{NewStatus, Visibility},
            user::{Flag, NewUser, User, insert},
        },
        streaming::hub::Hub,
        uploads::UploaderBackend,
    };

    /// An event on `streams` carrying the entity `payload`.
    fn event(streams: &[&str], payload: &str) -> StreamEvent {
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
    fn status_payload(author: Uuid, acct: &str, thread: Uuid) -> String {
        format!(
            r#"{{"id":"status-1","account":{{"id":"{author}","acct":"{acct}","url":"https://{acct}/users/x"}},"conversation_id":"{thread}"}}"#
        )
    }

    /// The header map a `Sec-WebSocket-Protocol: offered` request carries.
    fn protocol_headers(offered: &str) -> HeaderMap {
        let mut headers = HeaderMap::new();
        let value = HeaderValue::from_str(offered).expect("test subprotocol is header-safe");
        let previous = headers.insert(SEC_WEBSOCKET_PROTOCOL, value);
        assert!(previous.is_none(), "a fresh map holds no header");
        headers
    }

    /// The query parameter map `pairs` describes.
    fn query(pairs: &[(&str, &str)]) -> HashMap<String, String> {
        pairs
            .iter()
            .map(|(key, value)| ((*key).to_owned(), (*value).to_owned()))
            .collect()
    }

    /// The [`AppState`] a connection test runs on: `pool` and the fixed test
    /// config, with a hub of its own.
    fn app_state(pool: &PgPool) -> AppState {
        app_state_with(pool, Config::for_tests())
    }

    /// [`app_state`] over `config`, for the tests that read a setting.
    fn app_state_with(pool: &PgPool, config: Config) -> AppState {
        let config = Arc::new(config);
        AppState {
            db: pool.clone(),
            hub: Hub::new(pool),
            config: Arc::clone(&config),
            http_client: HttpClient::from(config.as_ref()),
            uploader: UploaderBackend::from(config.as_ref()),
            instance_stats_cache: Arc::default(),
            mrf_config_cache: Arc::default(),
            translation_languages_cache: Arc::default(),
            rate_limiter: Arc::new(crate::rate_limit::RateLimiter::from_config(config.as_ref())),
            mailer: Arc::new(crate::mailer::MemoryTransport::new()),
        }
    }

    /// A migrated database slot and the [`AppState`] built on it. The slot is
    /// returned alongside, so a test keeps its lease until it ends.
    async fn state() -> (crate::test_db::TestDb, AppState) {
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
    async fn account(state: &AppState, nickname: &str) -> Arc<User> {
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
    fn one<T>(value: T) -> HashSet<T>
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
    async fn mute_author(state: &AppState, reader: &User, author: &User) {
        let _mute = mute::mute(&state.db, reader, author, mute::MuteOptions::default())
            .await
            .expect("the mute fixture writes");
    }

    /// Blocks `author` for `reader`.
    ///
    /// # Panics
    ///
    /// If the write fails.
    async fn block_author(state: &AppState, reader: &User, author: &User) {
        let _block = block::block(&state.db, &state.config.base_url, reader, author)
            .await
            .expect("the block fixture writes");
    }

    /// A connection over `state` with `streams` already subscribed and nothing
    /// else suppressed: the shape every delivery test asserts on.
    async fn watching(state: &AppState, streams: &[&str]) -> Connection {
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
    async fn inactive_account(state: &AppState, nickname: &str) -> User {
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
    async fn for_reader(state: &AppState, reader: &Arc<User>, streams: &[&str]) -> Connection {
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
    fn frame_payload(frame: &str) -> Value {
        let envelope: Value = serde_json::from_str(frame).expect("the frame is JSON");
        serde_json::from_str(
            envelope["payload"]
                .as_str()
                .expect("the frame carries a payload string"),
        )
        .expect("the payload is the entity JSON")
    }

    #[test]
    fn the_config_default_and_the_heartbeat_constant_agree() {
        assert_eq!(
            PING_INTERVAL.as_secs(),
            u64::try_from(DEFAULT_STREAMING_PING_INTERVAL_SECS).expect("a positive default"),
            "PING_INTERVAL and STREAMING_PING_INTERVAL_SECS' default are one span"
        );
    }

    #[tokio::test]
    async fn a_configured_heartbeat_is_the_socket_loops_period() {
        let pool = sqlx::postgres::PgPoolOptions::new()
            .connect_lazy("postgres://localhost/unused")
            .expect("a lazy pool needs no database");
        let mut tuned = Config::for_tests();
        tuned.streaming_ping_interval_secs = 3;
        let mut zero = Config::for_tests();
        zero.streaming_ping_interval_secs = 0;

        assert_eq!(
            heartbeat_period(&app_state_with(&pool, tuned)),
            Duration::from_secs(3),
            "the configured span is what the socket pings on"
        );
        assert_eq!(
            heartbeat_period(&app_state_with(&pool, zero)),
            Duration::from_secs(1),
            "a zero period would spin the socket loop, so it is read as one second"
        );
    }

    #[test]
    fn a_public_stream_needs_no_token_and_keeps_its_name() {
        for name in [PUBLIC, PUBLIC_LOCAL, PUBLIC_MEDIA, PUBLIC_LOCAL_MEDIA] {
            assert_eq!(
                public_topic(name, &StreamArgs::default()).as_deref(),
                Some(name),
                "`{name}` is its own stream name"
            );
        }
    }

    #[test]
    fn a_remote_stream_is_narrowed_by_instance_when_one_is_given() {
        assert_eq!(
            public_topic(PUBLIC_REMOTE, &StreamArgs::default()).as_deref(),
            Some("public:remote"),
            "no instance keeps the plain stream"
        );
        let args = StreamArgs {
            instance: Some(" Remote.Example ".to_owned()),
            ..StreamArgs::default()
        };
        assert_eq!(
            public_topic(PUBLIC_REMOTE_MEDIA, &args).as_deref(),
            Some("public:remote:media:remote.example"),
            "the host is trimmed and lowercased, the way a domain is stored"
        );
    }

    #[test]
    fn a_hashtag_stream_needs_a_usable_tag() {
        let args = StreamArgs {
            tag: Some("#Rust".to_owned()),
            ..StreamArgs::default()
        };
        assert_eq!(
            public_topic(HASHTAG, &args).as_deref(),
            Some("hashtag:rust"),
            "the tag is normalized onto the end of the name"
        );
        assert_eq!(
            public_topic(HASHTAG_LOCAL, &args).as_deref(),
            Some("hashtag:local:rust"),
            "and so it is on the local flavour"
        );
        assert_eq!(
            public_topic(HASHTAG, &StreamArgs::default()),
            None,
            "a hashtag with no tag names no stream"
        );
    }

    #[test]
    fn only_the_known_streams_resolve_at_all() {
        assert_eq!(public_topic("nonsense", &StreamArgs::default()), None);
        assert_eq!(
            public_topic(USER, &StreamArgs::default()),
            None,
            "a user stream is never a public topic"
        );
        for name in [USER, USER_NOTIFICATION, USER_PLEROMA_CHAT, DIRECT] {
            assert!(is_user_stream(name), "`{name}` is a private user stream");
        }
        assert!(
            !is_user_stream(PUBLIC) && !is_user_stream(LIST),
            "the public streams and `list` are classified elsewhere"
        );
    }

    #[tokio::test]
    async fn a_socket_opens_the_stream_the_query_string_names() {
        let (_db, state) = state().await;
        let mut connection = Connection::new(state, None).await.unwrap();

        connection
            .open_initial(&query(&[("stream", "hashtag"), ("tag", "Rust")]))
            .await
            .unwrap();

        assert_eq!(
            connection.subscriptions,
            vec!["hashtag:rust".to_owned()],
            "the handshake subscription is expanded like a message one"
        );
    }

    #[tokio::test]
    async fn no_stream_parameter_is_no_subscription() {
        let (_db, state) = state().await;
        let mut connection = Connection::new(state, None).await.unwrap();

        connection.open_initial(&query(&[])).await.unwrap();

        assert!(
            connection.subscriptions.is_empty(),
            "an open socket with nothing to watch waits for a message"
        );
    }

    #[tokio::test]
    async fn an_unknown_initial_stream_is_a_400_and_a_private_one_a_401() {
        let (_db, state) = state().await;
        for (name, status) in [
            ("nonsense", StatusCode::BAD_REQUEST),
            (USER, StatusCode::UNAUTHORIZED),
        ] {
            let mut connection = Connection::new(state.clone(), None).await.unwrap();

            let err = connection
                .open_initial(&query(&[(STREAM_FIELD, name)]))
                .await
                .expect_err("neither stream opens anonymously");

            assert_eq!(
                err.into_response().status(),
                status,
                "`{name}` at the handshake is refused as {status}"
            );
        }
    }

    #[tokio::test]
    async fn subscribing_then_unsubscribing_moves_one_stream_in_and_out() {
        let (_db, state) = state().await;
        let mut connection = Connection::new(state, None).await.unwrap();

        let subscribed = connection
            .on_client_text(r#"{"type":"subscribe","stream":"hashtag","tag":"x"}"#)
            .await;
        let twice = connection
            .on_client_text(r#"{"type":"subscribe","stream":"hashtag","tag":"x"}"#)
            .await;
        let unknown = connection
            .on_client_text(r#"{"type":"subscribe","stream":"nonsense"}"#)
            .await;
        let refused = connection
            .on_client_text(r#"{"type":"subscribe","stream":"user"}"#)
            .await;

        assert!(
            subscribed.is_empty(),
            "an accepted subscribe answers nothing"
        );
        assert_eq!(
            connection.subscriptions,
            vec!["hashtag:x".to_owned()],
            "and the stream is held"
        );
        assert!(twice.is_empty(), "a repeat is accepted too");
        assert_eq!(
            connection.subscriptions.len(),
            1,
            "and adds nothing a second time"
        );
        assert_eq!(
            unknown,
            vec![UNKNOWN_STREAM_FRAME.to_owned()],
            "an unknown stream is the ticket's bad_request frame"
        );
        assert_eq!(
            refused,
            vec![UNAUTHORIZED_FRAME.to_owned()],
            "a private stream on an anonymous socket is the ticket's unauthorized frame"
        );
        assert_eq!(connection.subscriptions.len(), 1, "and neither subscribed");

        let unsubscribed = connection
            .on_client_text(r#"{"type":"unsubscribe","stream":"hashtag","tag":"x"}"#)
            .await;
        assert!(unsubscribed.is_empty(), "a known unsubscribe is accepted");
        assert!(
            connection.subscriptions.is_empty(),
            "and the stream is gone"
        );

        let never = connection
            .on_client_text(r#"{"type":"unsubscribe","stream":"hashtag","tag":"x"}"#)
            .await;
        assert!(
            never.is_empty(),
            "leaving a stream never held changes nothing"
        );
    }

    #[tokio::test]
    async fn the_fifty_first_subscription_is_refused() {
        let (_db, state) = state().await;
        let mut connection = Connection::new(state, None).await.unwrap();

        for index in 0..MAX_SUBSCRIPTIONS {
            let replies = connection
                .on_client_text(&format!(
                    r#"{{"type":"subscribe","stream":"hashtag","tag":"tag{index}"}}"#
                ))
                .await;
            assert!(replies.is_empty(), "stream {index} fits under the cap");
        }

        let over = connection
            .on_client_text(r#"{"type":"subscribe","stream":"hashtag","tag":"one-too-many"}"#)
            .await;

        assert_eq!(
            connection.subscriptions.len(),
            MAX_SUBSCRIPTIONS,
            "the refused subscription changed nothing"
        );
        assert_eq!(over, vec![FULL_FRAME.to_owned()], "and answered the limit");
    }

    #[tokio::test]
    async fn a_frame_that_is_no_message_is_ignored() {
        let (_db, state) = state().await;
        let mut connection = Connection::new(state, None).await.unwrap();

        let junk = connection.on_client_text("hello").await;
        let unknown_type = connection
            .on_client_text(r#"{"type":"nonsense","stream":"public"}"#)
            .await;

        assert!(
            junk.is_empty() && unknown_type.is_empty(),
            "nothing is written back"
        );
        assert!(
            connection.subscriptions.is_empty(),
            "and nothing about the connection changed"
        );
    }

    #[tokio::test]
    async fn a_public_event_is_rendered_the_mastodon_way() {
        let (_db, state) = state().await;
        let connection = watching(&state, &[PUBLIC]).await;

        let frame = connection
            .frame_for(&event(&["public"], r#"{"id":"7"}"#))
            .await
            .expect("a matching public event is rendered");

        assert_eq!(
            frame, r#"{"stream":["public"],"event":"update","payload":"{\"id\":\"7\"}"}"#,
            "the payload stays a JSON string inside the envelope, as Mastodon sends it"
        );
    }

    #[tokio::test]
    async fn a_delete_event_carries_the_status_id_as_its_payload() {
        let (_db, state) = state().await;
        let connection = watching(&state, &[PUBLIC]).await;
        let id = Uuid::now_v7().to_string();
        let deleted = StreamEvent {
            event: "delete".to_owned(),
            payload: Some(id.clone()),
            ..event(&["public"], "")
        };

        let frame = connection
            .frame_for(&deleted)
            .await
            .expect("a delete on a held stream is rendered");

        assert_eq!(
            frame,
            format!(r#"{{"stream":["public"],"event":"delete","payload":"{id}"}}"#),
            "its payload is the deleted status's id, as a plain string"
        );
    }

    #[tokio::test]
    async fn an_event_on_another_stream_is_not_rendered() {
        let (_db, state) = state().await;
        let connection = watching(&state, &["hashtag:rust"]).await;

        assert!(
            connection
                .frame_for(&event(&["public"], "{}"))
                .await
                .is_none(),
            "a public event does not reach a hashtag subscription"
        );
        assert!(
            connection
                .frame_for(&event(&["hashtag", "hashtag:rust"], "{}"))
                .await
                .is_some(),
            "the hashtag's own event does, on any of its streams"
        );
    }

    #[tokio::test]
    async fn a_body_less_event_carries_no_payload_key() {
        let (_db, state) = state().await;
        let connection = watching(&state, &[PUBLIC]).await;
        let filters_changed = StreamEvent {
            event: "filters_changed".to_owned(),
            payload: None,
            ..event(&["public"], "")
        };

        let frame = connection
            .frame_for(&filters_changed)
            .await
            .expect("an event with no body is still delivered");

        assert_eq!(
            frame, r#"{"stream":["public"],"event":"filters_changed"}"#,
            "and Mastodon's envelope simply omits the payload it has none of"
        );
    }

    #[tokio::test]
    async fn the_owner_socket_forwards_a_bare_filters_changed_frame() {
        let (_db, state) = state().await;
        let reader = account(&state, "filters-changed-reader").await;
        let stranger = account(&state, "filters-changed-stranger").await;
        let own_stream = format!("{USER}:{}", reader.id);
        let other_stream = format!("{USER}:{}", stranger.id);
        let owner = for_reader(&state, &reader, &[own_stream.as_str()]).await;
        let holding_stranger = for_reader(&state, &stranger, &[own_stream.as_str()]).await;
        let anonymous = watching(&state, &[own_stream.as_str()]).await;
        // The event `services::streaming::filters_changed` publishes.
        let filters_changed = StreamEvent {
            streams: vec![own_stream.clone()],
            event: "filters_changed".to_owned(),
            payload: None,
            audience: Audience::Users(vec![reader.id]),
            render: None,
            local_only: false,
        };

        assert_eq!(
            owner.frame_for(&filters_changed).await,
            Some(format!(
                r#"{{"stream":["{own_stream}"],"event":"filters_changed"}}"#
            )),
            "the owner's socket writes the bare envelope, no payload key"
        );
        assert!(
            holding_stranger.frame_for(&filters_changed).await.is_none(),
            "a reader holding the stream name but outside the audience never sees it: {other_stream}"
        );
        assert!(
            anonymous.frame_for(&filters_changed).await.is_none(),
            "and neither does an anonymous socket"
        );
    }

    #[tokio::test]
    async fn audience_decides_who_sees_an_event() {
        let (_db, state) = state().await;
        let reader = account(&state, "stream-reader").await;
        let other = Uuid::now_v7();
        let own_stream = format!("{USER}:{}", reader.id);
        let other_stream = format!("{USER}:{other}");
        let mut mine = watching(&state, &[own_stream.as_str()]).await;
        mine.reader = Some(Arc::clone(&reader));
        let on = event(&[own_stream.as_str()], "{}");

        let for_me = StreamEvent {
            audience: Audience::Users(vec![reader.id]),
            ..on.clone()
        };
        let for_other = StreamEvent {
            audience: Audience::Users(vec![other]),
            ..on.clone()
        };
        let everyone = StreamEvent {
            audience: Audience::Everyone,
            ..on.clone()
        };
        let on_other_stream = event(&[other_stream.as_str()], "{}");

        assert!(
            mine.frame_for(&for_me).await.is_some(),
            "its own user sees it"
        );
        assert!(
            mine.frame_for(&for_other).await.is_none(),
            "the audience stays the second gate even on a held stream name"
        );
        assert!(
            mine.frame_for(&everyone).await.is_some(),
            "an Everyone event reaches an authenticated socket"
        );
        assert!(
            mine.frame_for(&on_other_stream).await.is_none(),
            "another reader's uuid-named stream never matches"
        );

        let anonymous = watching(&state, &[own_stream.as_str()]).await;
        assert!(
            anonymous.frame_for(&for_me).await.is_none(),
            "an anonymous socket never receives a Users event"
        );
        assert!(
            anonymous.frame_for(&everyone).await.is_none(),
            "nor an Everyone one"
        );
        assert!(
            anonymous.frame_for(&on).await.is_some(),
            "but a Public event is public"
        );
    }

    #[tokio::test]
    async fn a_local_only_event_never_reaches_an_anonymous_socket() {
        let (_db, state) = state().await;
        let reader = account(&state, "local-gate-reader").await;
        let anonymous = watching(&state, &[PUBLIC]).await;
        let mut member = watching(&state, &[PUBLIC]).await;
        member.reader = Some(Arc::clone(&reader));
        let local_public = StreamEvent {
            local_only: true,
            ..event(&[PUBLIC], "{}")
        };

        assert!(
            anonymous.frame_for(&local_public).await.is_none(),
            "a local_only event is dropped for a readerless socket, whatever its audience"
        );
        let local_everyone = StreamEvent {
            audience: Audience::Everyone,
            ..local_public.clone()
        };
        assert!(
            anonymous.frame_for(&local_everyone).await.is_none(),
            "Everyone plus local_only means authenticated local viewers only"
        );
        assert!(
            member.frame_for(&local_everyone).await.is_some(),
            "and an authenticated socket is exactly such a viewer"
        );
        let not_local = StreamEvent {
            local_only: false,
            ..local_public
        };
        assert!(
            anonymous.frame_for(&not_local).await.is_some(),
            "without the flag the same event streams on as before"
        );
    }

    #[tokio::test]
    async fn a_user_stream_is_held_under_the_readers_uuid() {
        let (_db, state) = state().await;
        let reader = account(&state, "uuid-naming-reader").await;
        let mut connection = Connection::new(state, None).await.unwrap();
        connection.reader = Some(Arc::clone(&reader));
        connection.scopes = Scopes::parse(READ);

        for name in [USER, USER_NOTIFICATION, USER_PLEROMA_CHAT, DIRECT] {
            let topic = connection
                .topic_for(name, &StreamArgs::default())
                .await
                .expect("a reader with read watches its own streams");
            assert_eq!(
                topic,
                format!("{name}:{}", reader.id),
                "the client's bare `{name}` is held as the canonical hub name"
            );
        }

        let subscribed = connection
            .on_client_text(r#"{"type":"subscribe","stream":"user"}"#)
            .await;
        assert_eq!(
            connection.subscriptions,
            vec![format!("{USER}:{}", reader.id)],
            "the bare subscribe is answered nothing and holds the suffixed name"
        );
        assert!(subscribed.is_empty(), "and the frame itself is accepted");

        let unsubscribed = connection
            .on_client_text(r#"{"type":"unsubscribe","stream":"user"}"#)
            .await;
        assert!(unsubscribed.is_empty(), "and the bare name lets go of it");
        assert!(
            connection.subscriptions.is_empty(),
            "the suffixed name left with it"
        );
    }

    #[tokio::test]
    async fn a_reader_who_muted_the_author_never_sees_the_status() {
        let (_db, state) = state().await;
        let reader = account(&state, "suppressing-reader").await;
        let author = account(&state, "suppressed-author").await;
        let thread = Uuid::now_v7();
        let body = status_payload(author.id, "quiet", thread);
        mute_author(&state, &reader, &author).await;
        let mut connection = Connection::new(state.clone(), None)
            .await
            .expect("the connection loads");
        connection.reader = Some(Arc::clone(&reader));
        connection.filters = Filters::load(&state.db, Some(reader.id))
            .await
            .expect("the filters load");
        connection.subscriptions = vec![PUBLIC.to_owned()];

        assert!(
            connection
                .frame_for(&event(&["public"], &body))
                .await
                .is_none(),
            "a muted author is suppressed, on a public stream too"
        );
        assert!(
            connection
                .frame_for(&event(
                    &["public"],
                    &status_payload(Uuid::now_v7(), "loud", thread)
                ))
                .await
                .is_some(),
            "and somebody else's status still arrives"
        );
    }

    #[tokio::test]
    async fn the_suppression_sets_are_read_from_the_five_tables() {
        let (_db, state) = state().await;
        let reader = account(&state, "filter-reader").await;
        let blocked = account(&state, "filter-blocked").await;
        let muted = account(&state, "filter-muted").await;
        let thread = Uuid::now_v7();
        block_author(&state, &reader, &blocked).await;
        mute_author(&state, &reader, &muted).await;
        domain_block::block_domain(&state.db, reader.id, "spam.example")
            .await
            .expect("the domain block fixture writes");
        conversation_mute::mute(&state.db, reader.id, thread)
            .await
            .expect("the thread mute fixture writes");

        let filters = Filters::load(&state.db, Some(reader.id))
            .await
            .expect("the five reads run");

        assert_eq!(
            filters.blocked,
            one(blocked.id),
            "the blocked account is in"
        );
        assert!(filters.muted.contains(&muted.id), "so is the muted one");
        assert_eq!(
            filters.domains,
            one("spam.example".to_owned()),
            "and the blocked domain"
        );
        assert!(filters.threads.contains(&thread), "and the muted thread");

        let body = status_payload(muted.id, "muted@spam.example", thread);
        assert!(
            filters.suppresses(Some(body.as_str())),
            "one entity can be suppressed on three counts at once"
        );
        assert!(
            Filters::load(&state.db, None).await.unwrap().is_empty(),
            "and an anonymous reader suppresses nothing"
        );
    }

    /// The fifth read keeps only what streaming hides outright: an
    /// irreversible filter of a streaming context. A warn filter stays out
    /// (it covers a post, it never drops one) and so does an irreversible
    /// filter of a non-streaming context.
    #[tokio::test]
    async fn the_word_filter_read_keeps_irreversible_streaming_rows() {
        let (_db, state) = state().await;
        let reader = account(&state, "wordfilter-loader").await;
        seed_word_filter(&state, &reader, "spoilers", "home", true).await;
        seed_word_filter(&state, &reader, "maybe", "public", false).await;
        seed_word_filter(&state, &reader, "profile-only", "account", true).await;
        let _expired = filter::create(
            &state.db,
            reader.id,
            &filter::NewFilter {
                phrase: "past-due".to_owned(),
                context: vec!["home".to_owned()],
                irreversible: true,
                expires_at: Some(chrono::Utc::now() - chrono::Duration::days(1)),
                ..filter::NewFilter::default()
            },
        )
        .await
        .expect("the expired filter fixture creates");

        let filters = Filters::load(&state.db, Some(reader.id))
            .await
            .expect("the five reads run");

        assert_eq!(
            filters
                .words
                .iter()
                .map(|row| row.phrase.as_str())
                .collect::<Vec<&str>>(),
            ["spoilers"],
            "the irreversible streaming row, newest-first as [`filter::list_active`] reads it"
        );
        let anonymous = Filters::load(&state.db, None)
            .await
            .expect("the anonymous load runs no read");
        assert!(
            anonymous.words.is_empty(),
            "word filters are a reader's own, and the anonymous reader has none"
        );
    }

    /// A stored word filter of `reader`'s: `phrase` in the single
    /// `context`, hidden outright per `irreversible`.
    ///
    /// # Panics
    ///
    /// If the insert fails.
    async fn seed_word_filter(
        state: &AppState,
        reader: &User,
        phrase: &str,
        context: &str,
        irreversible: bool,
    ) {
        let _created = filter::create(
            &state.db,
            reader.id,
            &filter::NewFilter {
                phrase: phrase.to_owned(),
                context: vec![context.to_owned()],
                irreversible,
                ..filter::NewFilter::default()
            },
        )
        .await
        .expect("the word filter fixture creates");
    }

    #[test]
    fn a_reblog_is_judged_by_both_authors() {
        let booster = Uuid::now_v7();
        let original = Uuid::now_v7();
        let thread = Uuid::now_v7();
        let body = format!(
            r#"{{"account":{{"id":"{booster}","acct":"booster"}},"reblog":{{"account":{{"id":"{original}","acct":"original"}},"conversation_id":"{thread}"}}}}"#
        );
        let facts = Facts::of(Some(body.as_str()));

        assert_eq!(
            facts.authors,
            vec![booster, original],
            "the boost and what it boosted are both read"
        );
        let blocking_the_original = Filters {
            blocked: one(original),
            ..Filters::default()
        };
        assert!(
            blocking_the_original.suppresses(Some(body.as_str())),
            "and blocking either one suppresses the reblog"
        );
    }

    #[test]
    fn a_delete_event_names_nobody_and_is_never_suppressed() {
        let body = format!("\"{}\"", Uuid::now_v7());
        let filters = Filters {
            muted: one(Uuid::now_v7()),
            ..Filters::default()
        };

        let facts = Facts::of(Some(body.as_str()));

        assert!(
            facts.authors.is_empty() && facts.hosts.is_empty() && facts.threads.is_empty(),
            "a payload that is no entity object states no facts"
        );
        assert!(
            !filters.suppresses(Some(body.as_str())),
            "so there is nothing to object to and the delete goes out"
        );
    }

    #[test]
    fn an_account_object_names_its_host_by_acct_then_url() {
        let remote: Value = serde_json::from_str(
            r#"{"acct":"user@Remote.Example","url":"https://other.example/@user"}"#,
        )
        .expect("test fixture JSON");
        let local: Value =
            serde_json::from_str(r#"{"acct":"user","url":"https://test.example/users/user"}"#)
                .expect("test fixture JSON");
        let by_url: Value = serde_json::from_str(r#"{"url":"https://Fallback.Example/@user"}"#)
            .expect("fixture JSON");

        assert_eq!(
            account_host(remote.as_object().expect("object")).as_deref(),
            Some("remote.example"),
            "the acct handle speaks first, lowercased"
        );
        assert_eq!(
            account_host(local.as_object().expect("object")).as_deref(),
            Some("test.example"),
            "a local account reports this instance through its profile url"
        );
        assert_eq!(
            account_host(by_url.as_object().expect("object")).as_deref(),
            Some("fallback.example"),
            "and a handle without a domain falls back to the profile url"
        );
    }

    #[test]
    fn the_thread_is_read_from_either_spelling() {
        let id = Uuid::now_v7();
        let column: Value = serde_json::from_str(&format!(r#"{{"conversation_id":"{id}"}}"#))
            .expect("fixture JSON");
        let entity: Value =
            serde_json::from_str(&format!(r#"{{"pleroma":{{"conversation_id":"{id}"}}}}"#))
                .expect("fixture JSON");
        let mut facts = Facts::default();

        facts.note_entity(column.as_object().expect("object"));
        facts.note_entity(entity.as_object().expect("object"));

        assert_eq!(
            facts.threads,
            vec![id, id],
            "the status column and the pleroma extension both name the thread"
        );
    }

    #[test]
    fn the_token_comes_from_the_subprotocol_or_the_query_parameter() {
        let headers = protocol_headers("tok3n, chat");
        assert_eq!(
            presented_token(&headers, &query(&[])).as_deref(),
            Some("tok3n"),
            "the first subprotocol value is the token"
        );
        assert_eq!(
            presented_token(&headers, &query(&[("access_token", "query-token")])).as_deref(),
            Some("tok3n"),
            "and it wins over the parameter, as in Pleroma's find_access_token"
        );
        assert_eq!(
            presented_token(
                &HeaderMap::new(),
                &query(&[("access_token", "query-token")])
            )
            .as_deref(),
            Some("query-token"),
            "with no subprotocol offered the parameter speaks"
        );
        assert_eq!(
            requested_protocol(&headers)
                .and_then(|value| value.to_str().ok().map(str::to_owned))
                .as_deref(),
            Some("tok3n"),
            "and that same value is what the server echoes back"
        );
        assert_eq!(
            presented_token(&HeaderMap::new(), &query(&[("access_token", "")])),
            None,
            "an empty parameter presents no token"
        );
        assert_eq!(
            presented_token(&protocol_headers(" , chat"), &query(&[])).as_deref(),
            Some("chat"),
            "an empty first offer is skipped, the next value is the token"
        );
        assert_eq!(
            requested_protocol(&protocol_headers(" , ")),
            None,
            "an offer of nothing names no protocol"
        );
    }

    #[tokio::test]
    async fn the_health_probe_answers_ok_as_text() {
        let body = health().await;

        assert_eq!(body, "OK", "the probe's body is exactly OK");
    }

    #[test]
    fn a_refusal_is_the_ticket_frame_or_the_ticket_http_error() {
        assert_eq!(Refusal::Unknown.frame(), UNKNOWN_STREAM_FRAME);
        assert_eq!(Refusal::Unauthorized.frame(), UNAUTHORIZED_FRAME);
        assert_eq!(
            Refusal::Unknown.into_error().into_response().status(),
            StatusCode::BAD_REQUEST,
            "an unknown stream at the handshake is a 400"
        );
        assert_eq!(
            Refusal::Unauthorized.into_error().into_response().status(),
            StatusCode::UNAUTHORIZED,
            "and a private stream without a token the 401 the ticket pins"
        );
    }

    #[tokio::test]
    async fn a_list_stream_needs_a_list_the_reader_owns_and_a_read_scope() {
        let (_db, state) = state().await;
        let reader = account(&state, "list-owner").await;
        let mine = list::create(
            &state.db,
            reader.id,
            &NewList {
                title: "Reading".to_owned(),
                ..NewList::default()
            },
        )
        .await
        .expect("the list fixture writes");
        let mut connection = Connection::new(state.clone(), None).await.unwrap();
        connection.reader = Some(Arc::clone(&reader));
        let args = StreamArgs {
            list: Some(mine.id.to_string()),
            ..StreamArgs::default()
        };

        connection.scopes = Scopes::parse(READ_STATUSES);
        let topic = connection
            .topic_for(LIST, &args)
            .await
            .expect("read:statuses is a read scope");
        assert_eq!(
            topic,
            format!("list:{}", mine.id),
            "an owned list is a topic"
        );

        connection.scopes = Scopes::parse(READ_LISTS);
        assert!(
            connection
                .topic_for(LIST, &args)
                .await
                .is_ok_and(|name| name == topic),
            "and so is read:lists, Pleroma's other grant here"
        );

        connection.scopes = Scopes::parse("write");
        assert_eq!(
            connection.topic_for(LIST, &args).await,
            Err(Refusal::Unauthorized),
            "a token with no read scope may not watch it"
        );

        connection.scopes = Scopes::parse(READ);
        let stranger = StreamArgs {
            list: Some(Uuid::now_v7().to_string()),
            ..StreamArgs::default()
        };
        assert_eq!(
            connection.topic_for(LIST, &stranger).await,
            Err(Refusal::Unknown),
            "a list the reader does not own is Pleroma's :bad_topic"
        );
        let junk = StreamArgs {
            list: Some("not-a-uuid".to_owned()),
            ..StreamArgs::default()
        };
        assert_eq!(
            connection.topic_for(LIST, &junk).await,
            Err(Refusal::Unknown),
            "and so is a parameter that names no list at all"
        );
    }

    #[tokio::test]
    async fn an_anonymous_socket_authenticates_and_then_watches_its_own_stream() {
        let (_db, state) = state().await;
        let reader = account(&state, "late-reader").await;
        let muted = account(&state, "late-suppressed").await;
        block_author(&state, &reader, &muted).await;
        let token = token_for(&state, &reader, READ).await;
        let mut connection = Connection::new(state.clone(), None).await.unwrap();

        let refused = connection
            .on_client_text(r#"{"type":"pleroma:authenticate","token":"nope"}"#)
            .await;
        let accepted = connection
            .on_client_text(&format!(
                r#"{{"type":"pleroma:authenticate","token":"{token}"}}"#
            ))
            .await;
        let again = connection
            .on_client_text(&format!(
                r#"{{"type":"pleroma:authenticate","token":"{token}"}}"#
            ))
            .await;

        assert_eq!(
            refused,
            vec![UNAUTHORIZED_FRAME.to_owned()],
            "a token that names nobody is refused"
        );
        assert!(accepted.is_empty(), "a live user token is taken up");
        assert_eq!(
            again,
            vec![UNAUTHORIZED_FRAME.to_owned()],
            "and a socket that has a user already is not authenticated twice"
        );
        assert_eq!(
            connection.reader_id(),
            Some(reader.id),
            "the socket now streams as its reader"
        );

        let subscribed = connection
            .on_client_text(r#"{"type":"subscribe","stream":"user"}"#)
            .await;
        assert!(
            subscribed.is_empty(),
            "its own stream opens without a reconnect"
        );
        assert_eq!(
            connection.subscriptions,
            vec![format!("{USER}:{}", reader.id)],
            "and is held under the reader's uuid"
        );
        connection.filters = Filters::load(&state.db, connection.reader_id())
            .await
            .expect("the filters load");
        let body = status_payload(muted.id, "late-suppressed", Uuid::now_v7());
        let own_stream = format!("{USER}:{}", reader.id);
        assert!(
            connection
                .frame_for(&StreamEvent {
                    audience: Audience::Users(vec![reader.id]),
                    ..event(&[own_stream.as_str()], &body)
                })
                .await
                .is_none(),
            "and the filters the new reader brought apply to what it sees"
        );
    }

    /// The render hint of a live status arrives at the socket as an
    /// `update` carrying the rendered entity: the hint is the hub's
    /// envelope, the entity the socket's own view of it.
    #[tokio::test]
    async fn a_status_render_hint_arrives_as_the_rendered_entity() {
        let (_db, state) = state().await;
        let alice = account(&state, "hint-render-alice").await;
        let row = status::insert(
            &state.db,
            TEST_BASE_URL,
            &NewStatus::local(&alice, "rendered at the socket"),
        )
        .await
        .expect("the status fixture inserts");
        let connection = watching(&state, &[PUBLIC]).await;

        let frame = connection
            .frame_for(&hinted_update(row.id))
            .await
            .expect("a hinted update is rendered for the socket");

        let envelope: Value = serde_json::from_str(&frame).expect("the frame is JSON");
        assert_eq!(
            envelope["event"], "update",
            "the event name rides the frame as published: {frame}"
        );
        let payload: Value = serde_json::from_str(
            envelope["payload"]
                .as_str()
                .expect("the rendered update carries a payload string"),
        )
        .expect("the payload is the entity JSON");
        assert_eq!(
            payload["id"],
            Value::from(row.id.to_string()),
            "the payload is the hinted status's entity"
        );
        assert_eq!(
            payload["content"],
            Value::from("<p>rendered at the socket</p>"),
            "rendered from the live row"
        );
    }

    /// A render hint that no longer names a renderable status — the row
    /// tombstone between the publish and this socket's read — drops the
    /// frame: Mastodon's `update` is its entity, and an entity-less one is
    /// noise a client cannot use.
    #[tokio::test]
    async fn a_render_hint_whose_status_is_gone_drops_the_frame() {
        let (_db, state) = state().await;
        let alice = account(&state, "hint-gone-alice").await;
        let row = status::insert(
            &state.db,
            TEST_BASE_URL,
            &NewStatus::local(&alice, "doomed before delivery"),
        )
        .await
        .expect("the status fixture inserts");
        let _tombstone = status::soft_delete(&state.db, row.id)
            .await
            .expect("the delete fixture runs");
        let connection = watching(&state, &[PUBLIC]).await;

        assert!(
            connection.frame_for(&hinted_update(row.id)).await.is_none(),
            "the tombstoned hint delivers nothing"
        );
        assert!(
            connection
                .frame_for(&hinted_update(Uuid::now_v7()))
                .await
                .is_none(),
            "and so does a hint that never named a row"
        );
    }

    /// A notification hint that names no live notification of the
    /// socket's reader — here, a readerless socket, which can never be
    /// the addressee — drops the event: the hint asked for an entity this
    /// viewer cannot be shown, and a body-less `notification` would be
    /// the client parsing `null`.
    #[tokio::test]
    async fn a_render_hint_that_cannot_be_answered_drops_the_event() {
        let (_db, state) = state().await;
        let connection = watching(&state, &[USER_NOTIFICATION, DIRECT]).await;
        let hinted = StreamEvent {
            streams: vec![USER_NOTIFICATION.to_owned()],
            event: "notification".to_owned(),
            payload: None,
            audience: Audience::Public,
            render: Some(RenderSpec::Notification(Uuid::now_v7())),
            local_only: false,
        };

        assert!(
            connection.frame_for(&hinted).await.is_none(),
            "a hint no viewer can be answered for is dropped, not pushed body-less"
        );
        let hinted_chat = StreamEvent {
            render: Some(RenderSpec::Chat(Uuid::now_v7())),
            ..hinted
        };
        assert!(
            connection.frame_for(&hinted_chat).await.is_none(),
            "and the same for a chat hint on a readerless socket"
        );
        let hinted_conversation = StreamEvent {
            streams: vec![DIRECT.to_owned()],
            event: "conversation".to_owned(),
            render: Some(RenderSpec::Conversation(Uuid::now_v7())),
            ..hinted_chat
        };
        assert!(
            connection.frame_for(&hinted_conversation).await.is_none(),
            "and the same for a conversation hint, which no participation of nobody's answers"
        );
    }

    /// The reader's filters judge the rendered payload: an update hinted
    /// from a muted account never reaches the muter's socket.
    #[tokio::test]
    async fn a_rendered_update_is_judged_by_the_readers_filters() {
        let (_db, state) = state().await;
        let reader = account(&state, "hint-filter-reader").await;
        let author = account(&state, "hint-filter-author").await;
        let row = status::insert(
            &state.db,
            TEST_BASE_URL,
            &NewStatus::local(&author, "from a muted account"),
        )
        .await
        .expect("the status fixture inserts");
        mute_author(&state, &reader, &author).await;
        let mut connection = watching(&state, &[PUBLIC]).await;
        connection.reader = Some(Arc::clone(&reader));
        connection.filters = Filters::load(&state.db, Some(reader.id))
            .await
            .expect("the filters load");

        assert!(
            connection.frame_for(&hinted_update(row.id)).await.is_none(),
            "the rendered payload carries the authorship the filters read"
        );
    }

    /// The ticket's core promise, per viewer: a hinted PRIVATE status
    /// renders for a follower of the author, and the same hint dropped on
    /// a stranger's socket — stream name and audience matched for them,
    /// but the send-time [`status_visibility::can_view`] re-judgment says
    /// no. The pre-serialized path could never tell these viewers apart.
    #[tokio::test]
    async fn a_private_status_hint_renders_for_a_follower_and_not_for_a_stranger() {
        let (_db, state) = state().await;
        let author = account(&state, "private-hint-author").await;
        let follower = account(&state, "private-hint-follower").await;
        let stranger = account(&state, "private-hint-stranger").await;
        let _relation = follow::follow(&state.db, TEST_BASE_URL, &follower, &author)
            .await
            .expect("the follow fixture writes");
        let row = status::insert(
            &state.db,
            TEST_BASE_URL,
            &NewStatus {
                visibility: Visibility::Private,
                ..NewStatus::local(&author, "followers only")
            },
        )
        .await
        .expect("the status fixture inserts");

        let own_stream = format!("{USER}:{}", follower.id);
        let follower_connection = for_reader(&state, &follower, &[&own_stream]).await;
        let frame = follower_connection
            .frame_for(&StreamEvent {
                streams: vec![own_stream.clone()],
                audience: Audience::Users(vec![follower.id]),
                ..hinted_update(row.id)
            })
            .await
            .expect("the hinted private status renders for a follower");
        let entity = frame_payload(&frame);
        assert_eq!(
            entity["id"],
            Value::from(row.id.to_string()),
            "the payload is the hinted status's entity"
        );
        assert_eq!(
            entity["content"],
            Value::from("<p>followers only</p>"),
            "rendered for the socket's own viewer"
        );

        let other_stream = format!("{USER}:{}", stranger.id);
        let stranger_connection = for_reader(&state, &stranger, &[&other_stream]).await;
        assert!(
            stranger_connection
                .frame_for(&StreamEvent {
                    streams: vec![other_stream],
                    audience: Audience::Users(vec![stranger.id]),
                    ..hinted_update(row.id)
                })
                .await
                .is_none(),
            "the same hint on a non-follower's stream drops at the visibility gate"
        );
    }

    /// An active irreversible word filter of a streaming context drops the
    /// rendered event; the same phrase as a warn filter (not irreversible)
    /// or outside the streaming contexts leaves it on the wire.
    #[tokio::test]
    async fn an_irreversible_word_filter_drops_a_rendered_status() {
        let (_db, state) = state().await;
        let author = account(&state, "wordfilter-author").await;
        let row = status::insert(
            &state.db,
            TEST_BASE_URL,
            &NewStatus::local(&author, "a plain post about spoilers"),
        )
        .await
        .expect("the status fixture inserts");
        let warner = phrase_filter(&state, "wordfilter-warner", "spoilers", false, "home").await;
        let hider = phrase_filter(&state, "wordfilter-hider", "spoilers", true, "home").await;
        let outsider =
            phrase_filter(&state, "wordfilter-outsider", "spoilers", true, "account").await;

        let aloud = for_reader(&state, &warner, &[PUBLIC]).await;
        assert!(
            aloud.frame_for(&hinted_update(row.id)).await.is_some(),
            "a warn filter never drops the event; the client covers it"
        );
        let silent = for_reader(&state, &hider, &[PUBLIC]).await;
        assert!(
            silent.frame_for(&hinted_update(row.id)).await.is_none(),
            "the irreversible streaming filter matches the rendered content and drops"
        );
        let standing = for_reader(&state, &outsider, &[PUBLIC]).await;
        assert!(
            standing.frame_for(&hinted_update(row.id)).await.is_some(),
            "a filter outside the streaming contexts judges no stream"
        );
    }

    /// The word filter judges every rendered kind the same way: the text
    /// of the notification's own status falls, and its recipient can never
    /// be the socket's anonymous nobody.
    #[tokio::test]
    async fn an_irreversible_word_filter_drops_a_rendered_notification() {
        let (_db, state) = state().await;
        let actor = account(&state, "wordfilter-note-actor").await;
        let recipient = phrase_filter(
            &state,
            "wordfilter-note-recipient",
            "spoilers",
            true,
            "home",
        )
        .await;
        let row = status::insert(
            &state.db,
            TEST_BASE_URL,
            &NewStatus::local(&actor, "a post full of spoilers"),
        )
        .await
        .expect("the status fixture inserts");
        let note = notification::create(
            &state.db,
            &notification::NewNotification {
                user_id: recipient.id,
                from_account_id: actor.id,
                notification_type: "favourite",
                status_id: Some(row.id),
                emoji: None,
                report_id: None,
                chat_message_id: None,
            },
        )
        .await
        .expect("the notification fixture writes");
        let own_stream = format!("{USER_NOTIFICATION}:{}", recipient.id);
        let connection = for_reader(&state, &recipient, &[&own_stream]).await;

        assert!(
            connection
                .frame_for(&notification_hint(&own_stream, recipient.id, note.id))
                .await
                .is_none(),
            "the notification's embedded status text matches the reader's hidden phrase"
        );
    }

    /// The viewer is the row the socket took at authenticate: the reader's
    /// profile changes afterwards ride past the live socket, which keeps
    /// rendering in the view it loaded. (Suppression sets are a separate
    /// question: they refresh every [`FILTER_TTL`].)
    #[tokio::test]
    async fn the_socket_holds_its_viewer_from_the_handshake() {
        let (_db, state) = state().await;
        let reader = account(&state, "frozen-viewer").await;
        let author = account(&state, "frozen-viewer-author").await;
        let row = status::insert(
            &state.db,
            TEST_BASE_URL,
            &NewStatus::local(&author, "a public post"),
        )
        .await
        .expect("the status fixture inserts");
        let mut connection = watching(&state, &[PUBLIC]).await;
        connection.reader = Some(Arc::clone(&reader));

        let _renamed = sqlx::query("UPDATE users SET name = 'Renamed Later' WHERE id = $1")
            .bind(reader.id)
            .execute(&state.db)
            .await
            .expect("the rename fixture runs");

        let frame = connection
            .frame_for(&hinted_update(row.id))
            .await
            .expect("the public hint still renders for the viewer");
        let entity = frame_payload(&frame);
        assert_eq!(
            entity["content"], "<p>a public post</p>",
            "the hint rendered through the socket's held viewer"
        );
        let held = connection
            .viewer()
            .expect("the socket holds its viewer from the handshake");
        assert!(
            held.name.is_empty(),
            "the rename never reached the connection: it renders from the row it loaded, \
             not a live re-read per event"
        );
    }

    /// A hint whose status author has been deactivated since the publish
    /// answers nothing to anybody.
    #[tokio::test]
    async fn a_status_hint_of_a_deactivated_author_drops() {
        let (_db, state) = state().await;
        let ghost = inactive_account(&state, "deactivated-hint-author").await;
        let row = status::insert(
            &state.db,
            TEST_BASE_URL,
            &NewStatus::local(&ghost, "heard from the grave"),
        )
        .await
        .expect("the status fixture inserts");
        let connection = watching(&state, &[PUBLIC]).await;

        assert!(
            connection.frame_for(&hinted_update(row.id)).await.is_none(),
            "the send-time render drops the deactivated author's status"
        );
    }

    /// A notification hint arrives at its recipient's socket as the full
    /// notification entity — actor expanded, the named status rendered,
    /// the mute flag the endpoint's own relationship read answers — the
    /// same shape `GET /api/v1/notifications` gives.
    #[tokio::test]
    async fn a_notification_hint_renders_the_entity_for_its_recipient() {
        let (_db, state) = state().await;
        let actor = account(&state, "notification-hint-actor").await;
        let recipient = account(&state, "notification-hint-recipient").await;
        let row = status::insert(
            &state.db,
            TEST_BASE_URL,
            &NewStatus::local(&actor, "the liked post"),
        )
        .await
        .expect("the status fixture inserts");
        let note = notification::create(
            &state.db,
            &notification::NewNotification {
                user_id: recipient.id,
                from_account_id: actor.id,
                notification_type: "favourite",
                status_id: Some(row.id),
                emoji: None,
                report_id: None,
                chat_message_id: None,
            },
        )
        .await
        .expect("the notification fixture writes");
        let own_stream = format!("{USER_NOTIFICATION}:{}", recipient.id);
        let connection = for_reader(&state, &recipient, &[&own_stream]).await;

        let frame = connection
            .frame_for(&notification_hint(&own_stream, recipient.id, note.id))
            .await
            .expect("the recipient's socket renders the notification");

        let entity = frame_payload(&frame);
        assert_eq!(entity["id"], note.id.to_string(), "the notification's id");
        assert_eq!(entity["type"], "favourite", "and its stored type");
        assert_eq!(
            entity["account"]["id"],
            actor.id.to_string(),
            "the actor expanded into an account"
        );
        assert_eq!(
            entity["status"]["id"],
            row.id.to_string(),
            "the named status rendered into the entity"
        );
        assert_eq!(
            entity["pleroma"]["is_muted"], false,
            "the recipient does not mute the actor"
        );
    }

    /// A notification hint is the recipient's alone: another reader's
    /// socket and the recipient's own socket after a dismissal both lose
    /// the event at the `get_visible` gate.
    #[tokio::test]
    async fn a_notification_hint_is_dropped_for_everybody_but_its_recipient() {
        let (_db, state) = state().await;
        let actor = account(&state, "notification-drop-actor").await;
        let recipient = account(&state, "notification-drop-recipient").await;
        let stranger = account(&state, "notification-drop-stranger").await;
        let note = notification::create(
            &state.db,
            &notification::NewNotification {
                user_id: recipient.id,
                from_account_id: actor.id,
                notification_type: "follow",
                status_id: None,
                emoji: None,
                report_id: None,
                chat_message_id: None,
            },
        )
        .await
        .expect("the notification fixture writes");

        let theirs = format!("{USER_NOTIFICATION}:{}", stranger.id);
        let stranger_connection = for_reader(&state, &stranger, &[&theirs]).await;
        assert!(
            stranger_connection
                .frame_for(&notification_hint(&theirs, stranger.id, note.id))
                .await
                .is_none(),
            "a notification addressed to somebody else renders on no other socket"
        );

        let own_stream = format!("{USER_NOTIFICATION}:{}", recipient.id);
        let _dismissed = notification::dismiss(&state.db, recipient.id, note.id)
            .await
            .expect("the dismissal fixture runs");
        let recipient_connection = for_reader(&state, &recipient, &[&own_stream]).await;
        assert!(
            recipient_connection
                .frame_for(&notification_hint(&own_stream, recipient.id, note.id))
                .await
                .is_none(),
            "and the recipient loses the row once it is dismissed"
        );
    }

    /// A chat hint renders the reader's own chat through the chats
    /// endpoint's builder: the peer as account, the live unread count and
    /// the newest message.
    #[tokio::test]
    async fn a_chat_hint_renders_the_chat_for_its_participant() {
        let (_db, state) = state().await;
        let reader = account(&state, "chat-hint-reader").await;
        let sender = account(&state, "chat-hint-sender").await;
        let chat_row = chat::get_or_create(&state.db, &reader, &sender)
            .await
            .expect("the chat fixture opens");
        let mut tx = state
            .db
            .begin()
            .await
            .expect("the message fixture takes a transaction");
        let _sent = chat::add_message(
            &mut tx,
            &state.config.base_url,
            &sender,
            &reader,
            &chat::NewChatMessage::text("hello in here"),
        )
        .await
        .expect("the message fixture sends");
        tx.commit().await.expect("the message fixture commits");
        let own_stream = format!("{USER_PLEROMA_CHAT}:{}", reader.id);
        let connection = for_reader(&state, &reader, &[&own_stream]).await;

        let frame = connection
            .frame_for(&chat_hint(&own_stream, reader.id, chat_row.id))
            .await
            .expect("the participant's socket renders their chat");

        let entity = frame_payload(&frame);
        assert_eq!(
            entity["id"],
            chat_row.id.to_string(),
            "the reader's own chat row"
        );
        assert_eq!(
            entity["account"]["id"],
            sender.id.to_string(),
            "the peer as the chat's account"
        );
        assert_eq!(entity["unread"], 1, "the unread count read at send time");
        assert_eq!(
            entity["last_message"]["content"], "<p>hello in here</p>",
            "the newest message rendered into the entity"
        );
    }

    /// A chat hint answers only for its participant, and only while the
    /// peer is a live account: the other side's chat id, no id at all and
    /// a deactivated peer all drop the event.
    #[tokio::test]
    async fn a_chat_hint_is_dropped_for_strangers_and_a_deactivated_peer() {
        let (_db, state) = state().await;
        let reader = account(&state, "chat-drop-reader").await;
        let stranger = account(&state, "chat-drop-stranger").await;
        let peer = account(&state, "chat-drop-peer").await;
        let chat_row = chat::get_or_create(&state.db, &reader, &peer)
            .await
            .expect("the chat fixture opens");

        let theirs = format!("{USER_PLEROMA_CHAT}:{}", stranger.id);
        let stranger_connection = for_reader(&state, &stranger, &[&theirs]).await;
        assert!(
            stranger_connection
                .frame_for(&chat_hint(&theirs, stranger.id, chat_row.id))
                .await
                .is_none(),
            "one participant's chat id means nothing to another"
        );
        assert!(
            stranger_connection
                .frame_for(&chat_hint(&theirs, stranger.id, Uuid::now_v7()))
                .await
                .is_none(),
            "and so does a hint that names no chat at all"
        );

        let _deactivated = sqlx::query("UPDATE users SET is_active = false WHERE id = $1")
            .bind(peer.id)
            .execute(&state.db)
            .await
            .expect("the deactivation fixture runs");
        let own_stream = format!("{USER_PLEROMA_CHAT}:{}", reader.id);
        let reader_connection = for_reader(&state, &reader, &[&own_stream]).await;
        assert!(
            reader_connection
                .frame_for(&chat_hint(&own_stream, reader.id, chat_row.id))
                .await
                .is_none(),
            "a deactivated peer answers nothing to the participant's socket"
        );
    }

    /// A conversation hint renders the reader's own participation through
    /// the conversations endpoint's builder: the thread's other
    /// participant as account, the row's unread flag and the newest status
    /// as this viewer sees it.
    #[tokio::test]
    async fn a_conversation_hint_renders_the_conversation_for_its_participant() {
        let (_db, state) = state().await;
        let reader = account(&state, "conversation-hint-reader").await;
        let author = account(&state, "conversation-hint-author").await;
        let post = dm(&state, &author, &[&reader], "only between us").await;
        let own = participation_id(&state, &reader, &post).await;
        let own_stream = format!("{DIRECT}:{}", reader.id);
        let connection = for_reader(&state, &reader, &[&own_stream]).await;

        let frame = connection
            .frame_for(&conversation_hint(&own_stream, reader.id, own))
            .await
            .expect("the participant's socket renders their conversation");

        let entity = frame_payload(&frame);
        assert_eq!(
            entity["id"],
            own.to_string(),
            "the reader's own participation, never the thread's id"
        );
        assert_eq!(
            entity["accounts"][0]["id"],
            author.id.to_string(),
            "the other participant as the viewer sees them"
        );
        assert_eq!(
            entity["unread"], true,
            "the side that has not answered reads unread"
        );
        assert_eq!(
            entity["last_status"]["content"], "<p>only between us</p>",
            "the thread's newest status rendered into the entity"
        );
    }

    /// A conversation hint answers only for its own participant: another
    /// reader's participation id, and an id nobody holds at all, both drop
    /// the event — the same indistinguishable miss the conversations
    /// endpoints answer with a 404.
    #[tokio::test]
    async fn a_conversation_hint_is_dropped_for_a_stranger_and_a_miss() {
        let (_db, state) = state().await;
        let reader = account(&state, "conversation-drop-reader").await;
        let stranger = account(&state, "conversation-drop-stranger").await;
        let author = account(&state, "conversation-drop-author").await;
        let post = dm(&state, &author, &[&reader], "not for you").await;
        let own = participation_id(&state, &reader, &post).await;

        let theirs = format!("{DIRECT}:{}", stranger.id);
        let stranger_connection = for_reader(&state, &stranger, &[&theirs]).await;
        assert!(
            stranger_connection
                .frame_for(&conversation_hint(&theirs, stranger.id, own))
                .await
                .is_none(),
            "one participant's conversation id means nothing to another"
        );
        assert!(
            stranger_connection
                .frame_for(&conversation_hint(&theirs, stranger.id, Uuid::now_v7()))
                .await
                .is_none(),
            "and so does a hint that names no participation at all"
        );
    }

    /// A `notification` event that carries only the notification render
    /// hint, for `reader`'s stream — the shape a notification producer
    /// will publish.
    fn notification_hint(stream: &str, reader: Uuid, id: Uuid) -> StreamEvent {
        StreamEvent {
            streams: vec![stream.to_owned()],
            event: "notification".to_owned(),
            payload: None,
            audience: Audience::Users(vec![reader]),
            render: Some(RenderSpec::Notification(id)),
            local_only: false,
        }
    }

    /// A `pleroma:chat_update` event that carries only the chat render
    /// hint, for `reader`'s chat stream.
    fn chat_hint(stream: &str, reader: Uuid, id: Uuid) -> StreamEvent {
        StreamEvent {
            streams: vec![stream.to_owned()],
            event: "pleroma:chat_update".to_owned(),
            payload: None,
            audience: Audience::Users(vec![reader]),
            render: Some(RenderSpec::Chat(id)),
            local_only: false,
        }
    }

    /// A `conversation` event that carries only the conversation render
    /// hint, for `reader`'s direct stream — the shape a conversation
    /// producer will publish.
    fn conversation_hint(stream: &str, reader: Uuid, id: Uuid) -> StreamEvent {
        StreamEvent {
            streams: vec![stream.to_owned()],
            event: "conversation".to_owned(),
            payload: None,
            audience: Audience::Users(vec![reader]),
            render: Some(RenderSpec::Conversation(id)),
            local_only: false,
        }
    }

    /// `author`'s `direct` status addressed to everyone in `mentioned`,
    /// with the create hook's participation write run over it: the state a
    /// posted DM leaves behind, and the only way a participation row comes
    /// to exist.
    ///
    /// # Panics
    ///
    /// If either fixture write fails.
    async fn dm(
        state: &AppState,
        author: &User,
        mentioned: &[&User],
        text: &str,
    ) -> status::Status {
        let ids: Vec<Uuid> = mentioned.iter().map(|who| who.id).collect();
        let post = status::insert(
            &state.db,
            TEST_BASE_URL,
            &NewStatus {
                visibility: Visibility::Direct,
                mentioned_user_ids: ids,
                ..NewStatus::local(author, text)
            },
        )
        .await
        .expect("the direct fixture inserts");
        conversation::participate_for_status(&state.db, &post)
            .await
            .expect("the participation hook runs");
        post
    }

    /// `reader`'s own participation id in the thread `post` was stored
    /// into — the id a conversation hint names.
    ///
    /// # Panics
    ///
    /// If either fixture read fails, or the hook left `post` unthreaded or
    /// `reader` out of the thread.
    async fn participation_id(state: &AppState, reader: &User, post: &status::Status) -> Uuid {
        let thread = status::get_by_id(&state.db, post.id)
            .await
            .expect("the fixture post reads")
            .expect("the fixture post is live")
            .direct_conversation_id
            .expect("the participation hook threaded the fixture");
        *conversation::participation_ids(&state.db, reader.id, &[thread])
            .await
            .expect("the participations read")
            .get(&thread)
            .expect("the reader is a participant of the fixture thread")
    }

    /// `nickname`'s user with one active word filter for `phrase`:
    /// irreversible per `irreversible`, in the single context `context`.
    ///
    /// # Panics
    ///
    /// If either fixture write fails.
    async fn phrase_filter(
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

    /// The word filter reads the whole rendered entity, not just its top
    /// level: the status's own content and spoiler, a reblog's original,
    /// a notification's status and a chat's last message all state text.
    #[test]
    fn the_filterable_text_reaches_every_nested_entity() {
        let payload = r#"{"id":"1","content":"outer","spoiler_text":"subject",
             "reblog":{"content":"inner"},
             "pleroma":{"content":{"text/plain":"raw"},"conversation_id":"c"},
             "status":{"spoiler_text":"nested subject"},
             "last_message":{"content":"chat text"},
             "media_attachments":[{"description":"not filterable"}]}"#;

        let mut texts = entity_texts(payload);
        texts.sort();

        assert_eq!(
            texts,
            ["chat text", "inner", "nested subject", "outer", "subject"],
            "content and spoiler_text at every depth, and nothing else"
        );
    }

    #[test]
    fn a_payload_with_no_entity_states_no_text() {
        assert!(
            entity_texts("\"just-a-status-id\"").is_empty(),
            "a delete's id payload matches no phrase"
        );
        assert!(
            entity_texts("not json at all").is_empty(),
            "and neither does a body that is no JSON"
        );
    }

    /// An `update` event that carries only a status render hint: the shape
    /// [`crate::services::streaming::status_created`] publishes.
    fn hinted_update(id: Uuid) -> StreamEvent {
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
    async fn token_for(state: &AppState, reader: &User, scopes: &str) -> String {
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
}
