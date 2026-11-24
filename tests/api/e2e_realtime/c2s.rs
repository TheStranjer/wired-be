//! Client-to-server `ActivityPub` scenarios: the whoami read, an uploaded
//! attachment folded into an outbox `Create`, and the owner's inbox read.

use axum::http::StatusCode;
use serde_json::json;
use uuid::Uuid;

use super::server::{
    BASE, Part, SCOPES, Server, Verb, call, expect, get_value, png_fixture, post_multipart,
};
use super::sockets::{account, post_status};
use crate::common;

// ────────────────  Scenario 3: client-to-server ActivityPub  ────────────────

/// The C2S walk a native client takes: `GET /api/ap/whoami` answers the
/// token's own actor document (and nothing at all to the anonymous
/// request), `POST /api/ap/upload_media` stores a real image and answers
/// its `Document`, and `POST /users/{nickname}/outbox` folds a `Create` of
/// a `Note` — with that upload among its `attachment`s and the public
/// addressee in `to` — into a real status through the client API's own
/// create path, `Location` naming the `Create` the post federates as. The
/// posted note then reads back at `GET /api/v1/statuses/{id}` with its
/// content rendered and its media attached.
#[tokio::test]
async fn the_c2s_outbox_creates_from_an_uploaded_attachment() {
    let uploads = tempfile::TempDir::new().expect("a scratch uploads dir");
    let mut config = common::test_config();
    config.uploads_dir = uploads.path().to_path_buf();
    let srv = Server::with_config(config).await;
    let (alice, alice_token) = account(&srv, "rt226-c2s-alice", "read write write:media").await;

    let anonymous = call(&srv, Verb::Get, "/api/ap/whoami", None, None).await;
    expect(
        &anonymous,
        StatusCode::UNAUTHORIZED,
        "whoami without a token",
    );

    let actor = call(&srv, Verb::Get, "/api/ap/whoami", Some(&alice_token), None).await;
    expect(&actor, StatusCode::OK, "whoami");
    let document = actor.json();
    assert_eq!(
        document["id"],
        json!(alice.ap_id),
        "the token's own actor, the same bytes a peer fetches"
    );
    assert_eq!(document["type"], json!("Person"), "a Person document");

    let media_id = c2s_upload(&srv, &alice_token).await;
    let status_id = c2s_create_note(&srv, &alice_token, &media_id).await;

    let shown = get_value(
        &srv,
        Some(&alice_token),
        &format!("/api/v1/statuses/{status_id}"),
    )
    .await;
    assert_eq!(shown["id"], json!(status_id), "the status reads back");
    assert!(
        shown["content"]
            .as_str()
            .is_some_and(|content| content.contains("posted through the outbox")),
        "rendered from the note's content: {}",
        shown["content"]
    );
    assert_eq!(shown["visibility"], json!("public"), "the to-addressee");
    let attachments = shown["media_attachments"]
        .as_array()
        .expect("the entity carries its media");
    assert_eq!(attachments.len(), 1, "the uploaded attachment rode along");
    assert_eq!(
        attachments[0]["id"],
        json!(media_id),
        "the very upload the Create named"
    );
    assert_eq!(
        attachments[0]["description"],
        json!("a realtime pixel grid"),
        "with its alt text"
    );
}

/// `POST /api/ap/upload_media` with one generated `PNG` and a
/// description, asserted answered as the `Document` Pleroma spells; the
/// media id it carries is what the create names in its `attachment`.
async fn c2s_upload(srv: &Server, token: &str) -> String {
    let upload = post_multipart(
        srv,
        "/api/ap/upload_media",
        token,
        &[
            Part::file("file", "pic.png", png_fixture(3)),
            Part::text("description", "a realtime pixel grid"),
        ],
    )
    .await;
    expect(&upload, StatusCode::OK, "the C2S upload");
    let media = upload.json();
    assert_eq!(
        media["type"],
        json!("Document"),
        "Pleroma's Document answer"
    );
    let media_id = media["id"].as_str().expect("the media id").to_owned();
    assert!(
        media["url"][0]["href"]
            .as_str()
            .is_some_and(|href| href.starts_with(BASE)),
        "the file hangs off the instance: {}",
        media["url"]
    );
    media_id
}

/// `POST /users/rt226-c2s-alice/outbox` with a `Create` of a `Note`
/// carrying `media_id` as its attachment and the public addressee in
/// `to`; asserts the `201` and its `Location`, and answers the status id
/// the `Create` federates from.
async fn c2s_create_note(srv: &Server, token: &str, media_id: &str) -> String {
    let activity = json!({
        "type": "Create",
        "to": ["https://www.w3.org/ns/activitystreams#Public"],
        "cc": [],
        "object": {
            "type": "Note",
            "content": "posted through the outbox",
            "attachment": [{"id": media_id}],
        },
    });
    let posted = call(
        srv,
        Verb::Post,
        "/users/rt226-c2s-alice/outbox",
        Some(token),
        Some(&activity),
    )
    .await;
    expect(&posted, StatusCode::CREATED, "the outbox Create");
    let location = posted
        .header("location")
        .expect("a 201 carries a Location")
        .to_owned();
    let status_id = location
        .strip_prefix(&format!("{BASE}/activities/"))
        .unwrap_or(&location)
        .to_owned();
    assert!(
        Uuid::parse_str(&status_id).is_ok(),
        "Location names the federated activity id: {location}"
    );
    status_id
}

/// The owner's C2S inbox read: the collection root names its paged first
/// page, and with `?page=true` the statuses that @-mention the owner —
/// written through the ordinary client API by another account — arrive as
/// their `Create` activities, the unmentioned post absent.
#[tokio::test]
async fn the_c2s_inbox_reads_mentions_as_create_activities() {
    let srv = Server::start().await;
    let (alice, alice_token) = account(&srv, "rt226-in-alice", SCOPES).await;
    let (dave, dave_token) = account(&srv, "rt226-in-dave", SCOPES).await;

    let root = get_value(&srv, Some(&alice_token), "/users/rt226-in-alice/inbox").await;
    assert_eq!(
        root["id"],
        json!(format!("{}/inbox", alice.ap_id)),
        "the collection answers at the actor's inbox address"
    );
    assert_eq!(root["type"], json!("OrderedCollection"), "ordered");
    assert_eq!(
        root["first"],
        json!(format!("{}/inbox?page=true", alice.ap_id)),
        "and names its first page"
    );

    let mentioned = common::id_of(
        &post_status(
            &srv,
            &dave_token,
            "@rt226-in-alice this names you",
            "public",
        )
        .await,
    );
    let _unmentioned = common::id_of(&post_status(&srv, &dave_token, "to nobody", "public").await);

    let page = get_value(
        &srv,
        Some(&alice_token),
        "/users/rt226-in-alice/inbox?page=true",
    )
    .await;
    assert_eq!(
        page["type"],
        json!("OrderedCollectionPage"),
        "the paged read is a page"
    );
    let items = page["orderedItems"].as_array().expect("ordered items");
    assert_eq!(items.len(), 1, "only the mentioning post is in the inbox");
    assert_eq!(items[0]["type"], json!("Create"), "as its Create");
    assert_eq!(items[0]["actor"], json!(dave.ap_id), "authored by Dave");
    assert!(
        items[0]["object"]["content"]
            .as_str()
            .is_some_and(|content| content.contains("this names you")),
        "carrying the note: {}",
        items[0]["object"]
    );
    let object_id = items[0]["object"]["id"]
        .as_str()
        .expect("the object's id")
        .rsplit('/')
        .next()
        .expect("a final segment")
        .to_owned();
    assert_eq!(object_id, mentioned, "the very status the API posted");
}
