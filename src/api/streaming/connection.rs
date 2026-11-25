//! One streaming connection: who it streams for, what it watches, and the two
//! timers that keep it honest.
//!
//! This is the state a socket holds, its own [`Flow`] and [`Input`] vocabulary,
//! and the single inherent implementation of [`Connection`]: the constructors
//! and the two entry points the socket loop calls — the client text frame and
//! the frame an event is rendered into. The handling those two delegate to
//! lives in the children beside this file: `subscriptions` for the client's
//! `subscribe` / `unsubscribe` / `pleroma:authenticate` messages, `delivery`
//! for the gate an event passes before it is written, and the four
//! `*_hints` modules for the entity a render hint answers to. The crate denies
//! splitting a type's inherent blocks across modules
//! (`multiple_inherent_impl`), so the children extend the connection with free
//! functions over it rather than with further `impl` blocks.

use std::{collections::HashMap, sync::Arc};

use axum::extract::ws::Message;
use tokio::{
    sync::broadcast,
    time::{Interval, interval_at},
};
use tracing::{debug, warn};
use uuid::Uuid;

use crate::{
    api::timelines::timeline_area,
    auth,
    error::AppResult,
    models::{list, user::User},
    oauth::scopes::{READ_LISTS, Scopes},
    state::AppState,
    streaming::hub::{StreamEvent, matched_streams, public_stream_name},
};

use super::{
    args::{ClientMessage, Frame, Refusal, StreamArgs},
    facts::parse_uuid,
    filters::Filters,
    names::{
        AUTHENTICATE, FILTER_TTL, LIST, PUBLIC_LOCAL, PUBLIC_LOCAL_MEDIA, STREAM_FIELD, SUBSCRIBE,
        UNSUBSCRIBE, is_user_stream, public_topic, required_scope,
    },
    run::heartbeat_period,
};

use self::{
    delivery::{audience_allows, local_allows, payload_for},
    subscriptions::{on_authenticate, on_subscribe, on_unsubscribe},
};

mod chat_hints;
mod conversation_hints;
mod delivery;
mod notification_hints;
mod status_hints;
mod subscriptions;

/// What the socket loop is asked to do with the connection after one input.
#[derive(Debug, PartialEq, Eq)]
pub(super) enum Flow {
    /// The connection lives on.
    Keep,
    /// The connection is over; stop serving it.
    Stop,
}

/// One input the streaming socket loop waits for.
pub(super) enum Input {
    /// A frame from the client, or the end of its stream.
    Client(Option<Result<Message, axum::Error>>),
    /// An event from the hub, or the receiver's own report.
    Bus(Result<Arc<StreamEvent>, broadcast::error::RecvError>),
    /// The heartbeat period elapsed.
    Ping,
    /// The refresh period elapsed: the credential is re-checked and the
    /// suppression sets are read again.
    Refresh,
}

/// One streaming connection: who it streams for, what it watches, and the two
/// timers that keep it honest.
#[derive(Debug)]
pub(super) struct Connection {
    /// The shared state: the hub to read, and the pool the reader's
    /// suppression sets are (re-)read from.
    pub(super) state: AppState,
    /// The `users` row of the user the socket authenticated as, read ONCE
    /// when the socket opens (or authenticates later) and held for its
    /// life: every send-time render is this viewer's own view; `None`
    /// while the socket watches public streams only.
    pub(super) reader: Option<Arc<User>>,
    /// The scopes that user's token granted — empty for an anonymous socket,
    /// which is what turns every private stream into a
    /// [`Refusal::Unauthorized`] subscription.
    pub(super) scopes: Scopes,
    /// The id of the `oauth_tokens` row the socket authenticated with, `None`
    /// while it is anonymous. The refresh tick re-reads this row, and its
    /// user, so a revoked or expired token (or a deactivated account) closes
    /// the socket instead of streaming on from a credential cached at the
    /// handshake.
    pub(super) token_id: Option<Uuid>,
    /// The hub stream names this socket holds, in the order they were opened,
    /// capped at [`MAX_SUBSCRIPTIONS`].
    ///
    /// [`MAX_SUBSCRIPTIONS`]: super::names::MAX_SUBSCRIPTIONS
    pub(super) subscriptions: Vec<String>,
    /// What this reader does not want to see.
    pub(super) filters: Filters,
    /// Whether the last heartbeat ping is still unanswered.
    pub(super) awaiting_pong: bool,
    /// The heartbeat timer, armed at [`heartbeat_period`].
    pub(super) pings: Interval,
    /// The filter-refresh timer, armed at [`FILTER_TTL`].
    pub(super) refreshes: Interval,
}

impl Connection {
    /// A connection reading `state`'s hub as `credential`'s user (anonymous
    /// when `None`): no subscriptions yet, both timers armed, and the reader's
    /// suppression sets loaded.
    ///
    /// # Errors
    ///
    /// Whatever [`Filters::load`] fails on.
    pub(super) async fn new(
        state: AppState,
        credential: Option<auth::AuthContext>,
    ) -> AppResult<Self> {
        let (reader, scopes, token_id) = credential.map_or_else(
            || (None, Scopes::parse(""), None),
            |found| {
                (
                    found.user.map(Arc::new),
                    Scopes::parse(&found.token.scopes),
                    Some(found.token.id),
                )
            },
        );
        let filters = Filters::load(&state.db, reader.as_ref().map(|user| user.id)).await?;
        let now = tokio::time::Instant::now();
        let ping = heartbeat_period(&state);
        Ok(Self {
            state,
            reader,
            scopes,
            token_id,
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
    ///
    /// [`AppError::BadRequest`]: crate::error::AppError::BadRequest
    /// [`AppError::Unauthorized`]: crate::error::AppError::Unauthorized
    pub(super) async fn open_initial(&mut self, params: &HashMap<String, String>) -> AppResult<()> {
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
    pub(super) async fn on_client_text(&mut self, text: &str) -> Vec<String> {
        let Ok(message) = serde_json::from_str::<ClientMessage>(text) else {
            debug!("streaming client sent a frame that is no known message");
            return Vec::new();
        };
        match message.kind.as_str() {
            SUBSCRIBE => on_subscribe(self, &message).await,
            UNSUBSCRIBE => on_unsubscribe(self, &message).await,
            AUTHENTICATE => on_authenticate(self, &message).await,
            other => {
                debug!(kind = other, "ignoring an unknown streaming message");
                Vec::new()
            }
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
    ///
    /// An anonymous socket may not watch a public stream whose
    /// `RESTRICT_UNAUTHENTICATED` area the instance lists (Pleroma's
    /// `can_access_stream`): `public:local` and `public:local:media` fall
    /// under `timelines_local`, every other public stream under
    /// `timelines_federated`, as [`timeline_area`] chooses for the REST
    /// timeline too.
    ///
    /// [`Audience`]: crate::streaming::hub::Audience
    async fn topic_for(&self, name: &str, args: &StreamArgs) -> Result<String, Refusal> {
        if is_user_stream(name) {
            return self.user_topic(name);
        }
        if name == LIST {
            return self.list_topic(args).await;
        }
        let topic = public_topic(name, args).ok_or(Refusal::Unknown)?;
        if self.reader.is_none()
            && self.state.config.restricts(timeline_area(matches!(
                name,
                PUBLIC_LOCAL | PUBLIC_LOCAL_MEDIA
            )))
        {
            return Err(Refusal::Unauthorized);
        }
        Ok(topic)
    }

    /// A private user stream: the socket needs a user whose token grants the
    /// stream's own [`required_scope`] (Pleroma's `get_topic` `@user_streams`
    /// clause; `read` covers them all), and the topic it holds is the
    /// reader's own: the requested `name` with the reader's uuid suffixed
    /// onto it.
    fn user_topic(&self, name: &str) -> Result<String, Refusal> {
        let Some(reader) = self
            .reader_id()
            .filter(|_| self.scopes.contains(required_scope(name)))
        else {
            return Err(Refusal::Unauthorized);
        };
        Ok(format!("{name}:{reader}"))
    }

    /// A list stream: `read:lists` (or `read`), and a list the reader does
    /// not own is the `:bad_topic` Pleroma's `get_topic("list", …)` answers
    /// too.
    async fn list_topic(&self, args: &StreamArgs) -> Result<String, Refusal> {
        let refused = Err(Refusal::Unauthorized);
        let Some(reader) = self.reader_id() else {
            return refused;
        };
        if !self.scopes.contains(READ_LISTS) {
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
    pub(super) fn reader_id(&self) -> Option<Uuid> {
        self.reader.as_ref().map(|reader| reader.id)
    }

    /// This socket's viewer row for the send-time renders: its own
    /// authenticated reader, or nobody — the anonymous view — while the
    /// socket has no reader. An authenticated socket never renders in
    /// another reader's view: the viewer is the one loaded at the
    /// handshake, not a row re-read per event.
    pub(super) fn viewer(&self) -> Option<&User> {
        self.reader.as_ref().map(Arc::as_ref)
    }

    /// The frames to write for `event`: one per subscription of this socket
    /// the event belongs to, each naming only that subscription in its
    /// public form (`["user"]`, `["hashtag","rust"]`, …, see
    /// [`public_stream_name`]) — never the event's internal routing list.
    /// Empty when this connection may not see the event: no subscribed
    /// stream, an audience it is outside, a `local_only` event with no
    /// reader to be local for, a reader whose filters suppress the entity,
    /// or a [`RenderSpec`] hint this socket cannot answer for its own
    /// viewer.
    ///
    /// An event that carries its entity as `payload` is written as it is.
    /// An event that carries a [`RenderSpec`] hint instead — what
    /// [`status_created`] and [`status_edited`] publish — has that entity
    /// rendered for this connection's OWN viewer here, the way Pleroma's
    /// `Streamer` renders in the socket process: one hub event, each
    /// socket its own view. A rendered entity then replaces the frame's
    /// `payload` (Mastodon's string-in-envelope rule), and the reader's
    /// `Filters` judge it: relationship suppression weighs the rendered
    /// payload as it weighs any other, and the reader's phrase filters
    /// (`services::filter`) judge the rendered status in the context of
    /// the stream it arrived on — an irreversible match is dropped, a warn
    /// match rides the entity's `filtered` array.
    ///
    /// [`status_created`]: crate::services::streaming::status_created
    /// [`status_edited`]: crate::services::streaming::status_edited
    /// [`RenderSpec`]: crate::streaming::hub::RenderSpec
    pub(super) async fn frames_for(&self, event: &StreamEvent) -> Vec<String> {
        let matched = matched_streams(&self.subscriptions, event);
        if matched.is_empty() || !audience_allows(self, event) || !local_allows(self, event) {
            return Vec::new();
        }
        let Some(payload) = payload_for(self, event).await else {
            return Vec::new();
        };
        if self.filters.suppresses(payload.as_deref()) {
            return Vec::new();
        }
        let mut frames = Vec::with_capacity(matched.len());
        for internal in matched {
            let frame = Frame {
                stream: public_stream_name(internal),
                event: event.event.as_str(),
                payload: payload.as_deref(),
            };
            match serde_json::to_string(&frame) {
                Ok(text) => frames.push(text),
                Err(err) => warn!(%err, "streaming frame could not be serialized"),
            }
        }
        frames
    }
}

#[cfg(test)]
mod tests {
    use axum::{http::StatusCode, response::IntoResponse as _};

    use crate::{
        api::streaming::{
            names::{
                DIRECT, HASHTAG, PUBLIC, PUBLIC_MEDIA, PUBLIC_REMOTE, PUBLIC_REMOTE_MEDIA, USER,
                USER_NOTIFICATION, USER_PLEROMA_CHAT,
            },
            test_support::{account, query, state, state_with_config},
        },
        config::Config,
        models::list::NewList,
        oauth::scopes::{READ, READ_NOTIFICATIONS, READ_STATUSES},
    };

    use super::*;

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
    async fn each_user_stream_checks_its_own_scope() {
        let (_db, state) = state().await;
        let reader = account(&state, "per-stream-scope-reader").await;
        let mut connection = Connection::new(state, None).await.unwrap();
        connection.reader = Some(Arc::clone(&reader));
        let streams = [USER, USER_NOTIFICATION, USER_PLEROMA_CHAT, DIRECT];

        for (scope, allowed) in [
            (READ_NOTIFICATIONS, vec![USER_NOTIFICATION]),
            (READ_STATUSES, vec![USER, USER_PLEROMA_CHAT, DIRECT]),
            (READ, streams.to_vec()),
            (READ_LISTS, Vec::new()),
        ] {
            connection.scopes = Scopes::parse(scope);
            assert_user_streams(&connection, &reader, scope, &allowed).await;
        }
    }

    /// Asserts that `connection` admits exactly the user streams in `allowed`
    /// and refuses the rest, `scope` being what its token grants.
    async fn assert_user_streams(
        connection: &Connection,
        reader: &User,
        scope: &str,
        allowed: &[&str],
    ) {
        for name in [USER, USER_NOTIFICATION, USER_PLEROMA_CHAT, DIRECT] {
            let expected = allowed
                .contains(&name)
                .then(|| format!("{name}:{}", reader.id))
                .ok_or(Refusal::Unauthorized);
            assert_eq!(
                connection.topic_for(name, &StreamArgs::default()).await,
                expected,
                "`{scope}` on `{name}`"
            );
        }
    }

    /// A connection on a state whose `RESTRICT_UNAUTHENTICATED` lists `area`,
    /// anonymous unless `reader` is given.
    async fn restricted_connection(
        db: &crate::test_db::TestDb,
        area: &str,
        reader: Option<Arc<User>>,
    ) -> Connection {
        let mut config = Config::for_tests();
        config.restrict_unauthenticated = std::iter::once(area.to_owned()).collect();
        let mut connection = Connection::new(state_with_config(db.pool(), config), None)
            .await
            .unwrap();
        connection.reader = reader;
        connection
    }

    /// The public stream names the `timelines_local` area covers.
    const LOCAL_STREAMS: [&str; 2] = [PUBLIC_LOCAL, PUBLIC_LOCAL_MEDIA];

    /// The public stream names the `timelines_federated` area covers.
    const FEDERATED_STREAMS: [&str; 4] = [PUBLIC, PUBLIC_MEDIA, PUBLIC_REMOTE, PUBLIC_REMOTE_MEDIA];

    #[tokio::test]
    async fn restricted_federated_timelines_refuse_the_anonymous_non_local_streams() {
        let db = crate::test_db::test_db().await.expect("test database slot");
        let anonymous = restricted_connection(&db, "timelines_federated", None).await;
        let args = StreamArgs::default();

        for name in FEDERATED_STREAMS {
            assert_eq!(
                anonymous.topic_for(name, &args).await,
                Err(Refusal::Unauthorized),
                "an anonymous `{name}` is refused while timelines_federated is restricted"
            );
        }
        let tag = StreamArgs {
            tag: Some("rust".to_owned()),
            ..StreamArgs::default()
        };
        assert_eq!(
            anonymous.topic_for(HASHTAG, &tag).await,
            Err(Refusal::Unauthorized),
            "a hashtag stream is under the federated area"
        );
        for name in LOCAL_STREAMS {
            assert_eq!(
                anonymous.topic_for(name, &args).await,
                Ok(name.to_owned()),
                "an anonymous `{name}` is not the federated area's business"
            );
        }

        let state = anonymous.state.clone();
        let reader = account(&state, "federated-restricted-reader").await;
        let signed_in = restricted_connection(&db, "timelines_federated", Some(reader)).await;
        for name in FEDERATED_STREAMS.into_iter().chain(LOCAL_STREAMS) {
            assert_eq!(
                signed_in.topic_for(name, &args).await,
                Ok(name.to_owned()),
                "an authenticated `{name}` is always allowed"
            );
        }
    }

    #[tokio::test]
    async fn restricted_local_timelines_refuse_only_the_anonymous_local_streams() {
        let db = crate::test_db::test_db().await.expect("test database slot");
        let anonymous = restricted_connection(&db, "timelines_local", None).await;
        let args = StreamArgs::default();

        for name in LOCAL_STREAMS {
            assert_eq!(
                anonymous.topic_for(name, &args).await,
                Err(Refusal::Unauthorized),
                "an anonymous `{name}` is refused while timelines_local is restricted"
            );
        }
        for name in FEDERATED_STREAMS {
            assert_eq!(
                anonymous.topic_for(name, &args).await,
                Ok(name.to_owned()),
                "an anonymous `{name}` is not the local area's business"
            );
        }

        let state = anonymous.state.clone();
        let reader = account(&state, "local-restricted-reader").await;
        let signed_in = restricted_connection(&db, "timelines_local", Some(reader)).await;
        for name in LOCAL_STREAMS.into_iter().chain(FEDERATED_STREAMS) {
            assert_eq!(
                signed_in.topic_for(name, &args).await,
                Ok(name.to_owned()),
                "an authenticated `{name}` is always allowed"
            );
        }
    }

    #[tokio::test]
    async fn a_restricted_instance_refuses_the_anonymous_handshake_stream() {
        let db = crate::test_db::test_db().await.expect("test database slot");
        let mut connection = restricted_connection(&db, "timelines_federated", None).await;

        let err = connection
            .open_initial(&query(&[(STREAM_FIELD, PUBLIC)]))
            .await
            .expect_err("the restricted public stream does not open anonymously");

        assert_eq!(err.into_response().status(), StatusCode::UNAUTHORIZED);
        assert!(connection.subscriptions.is_empty(), "nothing was opened");
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

        connection.scopes = Scopes::parse(READ_LISTS);
        let topic = connection
            .topic_for(LIST, &args)
            .await
            .expect("read:lists is the list scope");
        assert_eq!(
            topic,
            format!("list:{}", mine.id),
            "an owned list is a topic"
        );

        connection.scopes = Scopes::parse(READ);
        assert!(
            connection
                .topic_for(LIST, &args)
                .await
                .is_ok_and(|name| name == topic),
            "and so is read, which covers it"
        );

        for scope in [READ_STATUSES, READ_NOTIFICATIONS] {
            connection.scopes = Scopes::parse(scope);
            assert_eq!(
                connection.topic_for(LIST, &args).await,
                Err(Refusal::Unauthorized),
                "{scope} does not reach list timelines"
            );
        }

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
}
