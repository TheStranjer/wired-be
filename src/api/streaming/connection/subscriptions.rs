//! The client messages one streaming connection answers: the `subscribe` and
//! `unsubscribe` frames that move a stream in and out of its subscription set,
//! and the `pleroma:authenticate` frame that gives an anonymous socket a user.
//!
//! The frames themselves are [`ClientMessage`]'s, the stream names are
//! resolved by [`Connection::topic_for`], and [`Connection::on_client_text`]
//! is the dispatcher that lands here.

use std::sync::Arc;

use tracing::warn;

use crate::{
    api::streaming::{
        args::ClientMessage,
        connection::Connection,
        filters::Filters,
        names::{FULL_FRAME, MAX_SUBSCRIPTIONS, UNAUTHORIZED_FRAME, UNKNOWN_STREAM_FRAME},
    },
    auth,
    models::user::User,
    oauth::scopes::Scopes,
};

/// The `subscribe` message: the stream is resolved first, and only one
/// this socket may watch joins its subscriptions.
pub(super) async fn on_subscribe(
    connection: &mut Connection,
    message: &ClientMessage,
) -> Vec<String> {
    let Some(name) = message.stream.as_deref() else {
        return vec![UNKNOWN_STREAM_FRAME.to_owned()];
    };
    match connection.topic_for(name, &message.args()).await {
        Err(refusal) => vec![refusal.frame().to_owned()],
        Ok(topic) if connection.subscriptions.contains(&topic) => Vec::new(),
        Ok(_) if connection.subscriptions.len() >= MAX_SUBSCRIPTIONS => vec![FULL_FRAME.to_owned()],
        Ok(topic) => {
            connection.subscriptions.push(topic);
            Vec::new()
        }
    }
}

/// The `unsubscribe` message: the same resolution, and the stream leaves
/// the subscription set. A stream the socket never subscribed to changes
/// nothing and answers nothing.
pub(super) async fn on_unsubscribe(
    connection: &mut Connection,
    message: &ClientMessage,
) -> Vec<String> {
    let Some(name) = message.stream.as_deref() else {
        return vec![UNKNOWN_STREAM_FRAME.to_owned()];
    };
    match connection.topic_for(name, &message.args()).await {
        Err(refusal) => vec![refusal.frame().to_owned()],
        Ok(topic) => {
            connection.subscriptions.retain(|held| *held != topic);
            Vec::new()
        }
    }
}

/// The `pleroma:authenticate` message: an anonymous socket takes the
/// token's user, scopes and suppression sets. Every other case — no token,
/// a token that names nobody, a socket that already has a user — answers
/// [`UNAUTHORIZED_FRAME`] and changes nothing.
pub(super) async fn on_authenticate(
    connection: &mut Connection,
    message: &ClientMessage,
) -> Vec<String> {
    let refused = vec![UNAUTHORIZED_FRAME.to_owned()];
    if connection.reader.is_some() {
        return refused;
    }
    let Some(raw) = message.token.as_deref().filter(|token| !token.is_empty()) else {
        return refused;
    };
    match auth::credential(&connection.state, raw).await {
        Ok(Some(found)) => match found.user {
            Some(reader) => {
                adopt(connection, reader, &found.token.scopes).await;
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
async fn adopt(connection: &mut Connection, reader: User, scopes: &str) {
    connection.scopes = Scopes::parse(scopes);
    let reader_id = reader.id;
    connection.reader = Some(Arc::new(reader));
    match Filters::load(&connection.state.db, Some(reader_id)).await {
        Ok(filters) => connection.filters = filters,
        Err(err) => warn!(%err, "streaming filters could not be loaded on authenticate"),
    }
}

#[cfg(test)]
mod tests {
    use uuid::Uuid;

    use crate::{
        api::streaming::{
            names::USER,
            test_support::{account, block_author, event, state, status_payload, token_for},
        },
        oauth::scopes::READ,
        streaming::hub::{Audience, StreamEvent},
    };

    use super::*;

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
}
