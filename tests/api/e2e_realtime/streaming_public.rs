//! Streaming scenarios over the public-facing streams: the status
//! lifecycle, hashtag subscriptions, the `public:local` audience rules and
//! the home and list sockets.

use axum::http::StatusCode;
use serde_json::json;
use uuid::Uuid;

use super::server::{SCOPES, Server, Verb, action_ok, call, expect, write_ok};
use super::sockets::{
    account, assert_absent, follow, frame_payload, id_of, list_stream, next_json, open,
    post_status, read_until, read_until_json, send, settled, streams_of, user_stream,
};

// ────────────────────  Scenario 1: streaming sockets  ────────────────────

/// The whole lifecycle of one public post, produced over HTTP and streamed
/// to an anonymous `public` socket: Mastodon's `update` with the rendered
/// entity as the payload string, `status.update` for the `PUT` edit, and
/// `delete` whose payload is the withdrawn id.
#[tokio::test]
async fn a_public_status_streams_create_edit_and_delete() {
    let srv = Server::start().await;
    let (author, author_token) = account(&srv, "rt226-lc-author", SCOPES).await;
    let mut client = open(&srv.ws_url("stream=public")).await;

    let entity = post_status(&srv, &author_token, "the lifecycle post", "public").await;
    let id = id_of(&entity);
    let created = next_json(&mut client).await;
    assert_eq!(created["event"], "update", "the create arrives as update");
    assert_eq!(
        streams_of(&created),
        ["public"],
        "it rides the public stream: {created}"
    );
    let payload = frame_payload(&created);
    assert_eq!(
        payload["id"], entity["id"],
        "the entity is the created post"
    );
    assert_eq!(
        payload["account"]["id"],
        json!(author.id.to_string()),
        "rendered with its author"
    );
    assert!(
        payload["content"]
            .as_str()
            .is_some_and(|content| content.contains("the lifecycle post")),
        "and its content: {payload}"
    );

    let _edited = write_ok(
        &srv,
        Verb::Put,
        &format!("/api/v1/statuses/{id}"),
        &author_token,
        &json!({ "status": "the edited post" }),
    )
    .await;
    let edit = next_json(&mut client).await;
    assert_eq!(
        edit["event"], "status.update",
        "the edit redraws every socket that saw the post"
    );
    let edited_payload = frame_payload(&edit);
    assert_eq!(
        edited_payload["id"],
        json!(id),
        "same entity, restated, and the socket still holds its post"
    );
    assert!(
        edited_payload["content"]
            .as_str()
            .is_some_and(|content| content.contains("the edited post")),
        "rendered from the edited row: {edited_payload}"
    );

    let deleted = call(
        &srv,
        Verb::Delete,
        &format!("/api/v1/statuses/{id}"),
        Some(&author_token),
        None,
    )
    .await;
    expect(&deleted, StatusCode::OK, "the status delete");
    let frame = next_json(&mut client).await;
    assert_eq!(frame["event"], "delete", "the withdrawal streams too");
    assert_eq!(
        frame["payload"].as_str(),
        Some(id.as_str()),
        "and its payload is the deleted id"
    );
}

/// The hashtag stream moves with `subscribe` / `unsubscribe` frames: a tag
/// the handshake opened delivers the tagged post, a frame adds a second
/// tag, and once a frame takes the first tag back the same connection goes
/// silent about that tag's events — the edit of the first post reaches no
/// socket that left its tag — while the newly held tag keeps flowing.
#[tokio::test]
async fn hashtag_frames_subscribe_and_unsubscribe_a_live_socket() {
    let srv = Server::start().await;
    let (_author, author_token) = account(&srv, "rt226-tag-author", SCOPES).await;
    let mut client = open(&srv.ws_url("stream=hashtag&tag=alpha")).await;

    let first = id_of(&post_status(&srv, &author_token, "#Alpha one", "public").await);
    let arrived = next_json(&mut client).await;
    assert_eq!(arrived["event"], "update", "the tagged post arrives");
    assert_eq!(
        streams_of(&arrived),
        ["hashtag", "alpha"],
        "on the tag its query string opened: {arrived}"
    );

    send(
        &mut client,
        r#"{"type":"subscribe","stream":"hashtag","tag":"beta"}"#,
    )
    .await;
    settled(&mut client).await;
    let second = post_status(&srv, &author_token, "#Beta two", "public").await;
    let second_id = id_of(&second);
    let beta = next_json(&mut client).await;
    assert!(
        beta["payload"]
            .as_str()
            .is_some_and(|payload| payload.contains(&second_id)),
        "the newly subscribed tag flows: {beta}"
    );

    send(
        &mut client,
        r#"{"type":"unsubscribe","stream":"hashtag","tag":"alpha"}"#,
    )
    .await;
    settled(&mut client).await;
    let _edit = write_ok(
        &srv,
        Verb::Put,
        &format!("/api/v1/statuses/{first}"),
        &author_token,
        &json!({ "status": "#Alpha edited again" }),
    )
    .await;
    let control = id_of(&post_status(&srv, &author_token, "#Beta control", "public").await);
    let frames = read_until(&mut client, &control).await;
    assert_absent(&frames, &[r#""event":"status.update""#, &first]);
    assert!(
        frames
            .iter()
            .any(|frame| { frame.contains(&control) && frame.contains(r#""event":"update""#) }),
        "the still-held tag keeps streaming: {frames:?}"
    );
}

/// `public:local` carries what the public timeline's local half holds and
/// who may hold a socket on it: a `local`-visibility post renders onto an
/// authenticated socket and never onto an anonymous one, and the same
/// event stays dark on a socket whose reader blocked or muted the author —
/// suppression sets read when the socket opened, judged against a post
/// written after, bounded by a control the public-local timeline also
/// carries.
#[tokio::test]
async fn public_local_carries_local_posts_to_authenticated_unfiltered_viewers() {
    let srv = Server::start().await;
    let (author, author_token) = account(&srv, "rt226-pl-author", SCOPES).await;
    let (_mate, mate_token) = account(&srv, "rt226-pl-mate", SCOPES).await;
    let (_watcher, watcher_token) = account(&srv, "rt226-pl-watcher", SCOPES).await;
    let (_blocker, blocker_token) = account(&srv, "rt226-pl-blocker", SCOPES).await;
    let (_muter, muter_token) = account(&srv, "rt226-pl-muter", SCOPES).await;
    let _blocked = action_ok(
        &srv,
        &format!("/api/v1/accounts/{}/block", author.id),
        &blocker_token,
    )
    .await;
    let _muted = action_ok(
        &srv,
        &format!("/api/v1/accounts/{}/mute", author.id),
        &muter_token,
    )
    .await;

    let mut watcher_socket =
        open(&srv.ws_url(&format!("stream=public:local&access_token={watcher_token}"))).await;
    let mut anon_socket = open(&srv.ws_url("stream=public:local")).await;
    let mut blocker_socket =
        open(&srv.ws_url(&format!("stream=public:local&access_token={blocker_token}"))).await;
    let mut muter_socket =
        open(&srv.ws_url(&format!("stream=public:local&access_token={muter_token}"))).await;

    let local_post = post_status(&srv, &author_token, "kept at home", "local").await;
    let local_id = id_of(&local_post);
    let seen = next_json(&mut watcher_socket).await;
    assert_eq!(seen["event"], "update", "the local viewer sees the post");
    assert_eq!(
        streams_of(&seen),
        ["public:local"],
        "on the local timeline: {seen}"
    );
    assert!(
        seen["payload"]
            .as_str()
            .is_some_and(|payload| payload.contains(&local_id)),
        "rendered with the status itself"
    );

    let control = id_of(&post_status(&srv, &mate_token, "the local control", "public").await);
    let anonymous = read_until(&mut anon_socket, &control).await;
    assert_absent(&anonymous, &[&local_id, "kept at home"]);
    let blocked = read_until(&mut blocker_socket, &control).await;
    assert_absent(&blocked, &[&local_id, "kept at home"]);
    let muted = read_until(&mut muter_socket, &control).await;
    assert_absent(&muted, &[&local_id, "kept at home"]);
    for (frames, who) in [
        (&anonymous, "anonymous"),
        (&blocked, "blocker"),
        (&muted, "muter"),
    ] {
        assert!(
            frames.iter().any(|frame| frame.contains(&control)),
            "the {who} socket is open and streaming its control: {frames:?}"
        );
    }
}

/// A followed author's post reaches the follower's `user` home socket and
/// every `list` socket that holds the author as a member — both computed
/// by the real target rules over the facts the HTTP writes just made.
#[tokio::test]
async fn home_and_list_sockets_stream_the_followed_and_listed_post() {
    let srv = Server::start().await;
    let (author, author_token) = account(&srv, "rt226-hl-author", SCOPES).await;
    let (follower, follower_token) = account(&srv, "rt226-hl-follower", SCOPES).await;
    let (_owner, owner_token) = account(&srv, "rt226-hl-owner", SCOPES).await;
    follow(&srv, &follower_token, author.id).await;
    // The list membership requires the owner to follow the author first —
    // the Mastodon rule the list-accounts write enforces.
    follow(&srv, &owner_token, author.id).await;

    let list = write_ok(
        &srv,
        Verb::Post,
        "/api/v1/lists",
        &owner_token,
        &json!({ "title": "Realtime" }),
    )
    .await;
    let list_id = Uuid::parse_str(&id_of(&list)).expect("the list id is a uuid");
    let _members = write_ok(
        &srv,
        Verb::Post,
        &format!("/api/v1/lists/{list_id}/accounts"),
        &owner_token,
        &json!({ "account_ids": [author.id.to_string()] }),
    )
    .await;

    let held_home = user_stream(follower.id);
    let held_list = list_stream(list_id);
    let mut home = open(&srv.ws_url(&format!("stream=user&access_token={follower_token}"))).await;
    let mut list_socket = open(&srv.ws_url(&format!(
        "stream=list&list={list_id}&access_token={owner_token}"
    )))
    .await;
    settled(&mut home).await;
    settled(&mut list_socket).await;

    let id = id_of(&post_status(&srv, &author_token, "for home and list", "public").await);
    let home_frame = read_until_json(&mut home, &id).await;
    assert_eq!(home_frame["event"], "update", "the home socket sees it");
    assert_eq!(
        streams_of(&home_frame),
        ["user"],
        "riding the follower's own user stream, by its public name: {home_frame}"
    );
    assert!(
        !home_frame.to_string().contains(held_home.as_str()),
        "and never the internal uuid-named one: {home_frame}"
    );
    let list_frame = read_until_json(&mut list_socket, &id).await;
    assert_eq!(list_frame["event"], "update", "the list socket sees it too");
    assert_eq!(
        streams_of(&list_frame),
        ["list", list_id.to_string().as_str()],
        "on its list stream, as `[\"list\", id]`: {list_frame}"
    );
    assert!(
        !list_frame.to_string().contains(held_list.as_str()),
        "not under the internal `list:<id>` name: {list_frame}"
    );
}
