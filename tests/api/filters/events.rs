//! The `filters_changed` streaming events (WBE-191 slice 5): every
//! successful write publishes Mastodon's bare `filters_changed` onto the
//! hub — the owner's `user:` stream, the owner's audience — while a refused
//! write publishes nothing.

use axum::http::StatusCode;
use serde_json::json;
use std::sync::Arc;
use tokio::sync::broadcast;
use uuid::Uuid;
use wired_be::{
    models::user::User,
    streaming::hub::{Audience, StreamEvent},
};

use super::support::{create_body, create_json, destroy, fixture, seed, update};
use crate::common;

/// The `filters_changed` event the hub holds after a successful write,
/// asserted to be exactly what the emitter owes (WBE-191 slice 5): bare,
/// on the owner's own `user:` stream, with the owner as the whole audience
/// — which at the socket is what reaches nobody else.
fn assert_filters_changed(event: &StreamEvent, owner: &User) {
    assert_eq!(
        event.event, "filters_changed",
        "Mastodon's filter-changed event"
    );
    assert_eq!(
        event.streams,
        vec![format!("user:{}", owner.id)],
        "the event rides exactly the owner's user stream: {:?}",
        event.streams
    );
    assert_eq!(
        event.audience,
        Audience::Users(vec![owner.id]),
        "the owner is the whole audience"
    );
    assert!(event.payload.is_none(), "the event is bare");
    assert!(event.render.is_none(), "and there is no entity to render");
    assert!(
        !event.local_only,
        "the audience already names a local owner"
    );
}

/// One write, one event: nothing else waits on the hub.
///
/// # Panics
///
/// If the hub holds an event the test did not account for.
fn assert_one_event_only(rx: &mut broadcast::Receiver<Arc<StreamEvent>>) {
    assert!(
        matches!(rx.try_recv(), Err(broadcast::error::TryRecvError::Empty)),
        "the write published exactly one event"
    );
}

/// A successful `POST` publishes Mastodon's `filters_changed` onto the hub,
/// on the owner's stream and to the owner's audience alone.
#[tokio::test]
async fn a_created_filter_publishes_filters_changed_to_the_owner_only() {
    let db = common::test_db().await;
    let state = common::test_app_state(db.pool().clone(), common::test_config());
    let app = wired_be::app(state.clone());
    let (alice, token) = fixture(&state.db, "push-alice", "write:filters").await;
    let mut rx = state.hub.subscribe();

    let response = create_json(&app, &token, create_body()).await;
    assert_eq!(response.status, StatusCode::OK, "the create answers 200");

    let event = rx.recv().await.expect("the create published an event");
    assert_filters_changed(&event, &alice);
    assert_one_event_only(&mut rx);
}

/// A successful `PUT` publishes it the same way.
#[tokio::test]
async fn an_updated_filter_publishes_filters_changed_to_the_owner_only() {
    let db = common::test_db().await;
    let state = common::test_app_state(db.pool().clone(), common::test_config());
    let app = wired_be::app(state.clone());
    let (alice, token) = fixture(&state.db, "push-alice", "write:filters").await;
    let row = seed(&state.db, &alice, "Spoilers", None).await;
    let mut rx = state.hub.subscribe();

    let response = update(&app, &token, row.id, create_body()).await;
    assert_eq!(response.status, StatusCode::OK, "the update answers 200");

    let event = rx.recv().await.expect("the update published an event");
    assert_filters_changed(&event, &alice);
    assert_one_event_only(&mut rx);
}

/// A successful `DELETE` publishes it too.
#[tokio::test]
async fn a_deleted_filter_publishes_filters_changed_to_the_owner_only() {
    let db = common::test_db().await;
    let state = common::test_app_state(db.pool().clone(), common::test_config());
    let app = wired_be::app(state.clone());
    let (alice, token) = fixture(&state.db, "push-alice", "write:filters").await;
    let row = seed(&state.db, &alice, "Spoilers", None).await;
    let mut rx = state.hub.subscribe();

    let response = destroy(&app, &token, row.id).await;
    assert_eq!(response.status, StatusCode::OK, "the delete answers 200");

    let event = rx.recv().await.expect("the delete published an event");
    assert_filters_changed(&event, &alice);
    assert_one_event_only(&mut rx);
}

/// The refusal paths publish nothing: a write that never happened tells the
/// owner's sockets nothing. A 422 create, a 404 update and a 404 delete are
/// the three ways these routes refuse.
#[tokio::test]
async fn a_refused_filter_write_publishes_nothing() {
    let db = common::test_db().await;
    let state = common::test_app_state(db.pool().clone(), common::test_config());
    let app = wired_be::app(state.clone());
    let (alice, token) = fixture(&state.db, "push-alice", "write:filters").await;
    let row = seed(&state.db, &alice, "Spoilers", None).await;
    let mut rx = state.hub.subscribe();

    let rejected = create_json(&app, &token, json!({"phrase": "", "context": ["home"]})).await;
    assert_eq!(rejected.status, StatusCode::UNPROCESSABLE_ENTITY);
    let missing = update(&app, &token, Uuid::now_v7(), create_body()).await;
    assert_eq!(missing.status, StatusCode::NOT_FOUND);
    let gone = destroy(&app, &token, Uuid::now_v7()).await;
    assert_eq!(gone.status, StatusCode::NOT_FOUND);
    let expired = destroy(&app, &token, row.id).await;
    assert_eq!(
        expired.status,
        StatusCode::OK,
        "the real delete does answer"
    );

    let event = rx.recv().await.expect("the delete published an event");
    assert_filters_changed(&event, &alice);
    assert_one_event_only(&mut rx);
}
