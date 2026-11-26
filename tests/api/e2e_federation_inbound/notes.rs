//! Inbound notes end to end: public, unlisted, followers-only and direct
//! deliveries, and the rich and question notes.

use axum::http::StatusCode;
use serde_json::{Value, json};

use super::support::{
    AS_PUBLIC, BOB, STATUSES, account_statuses, activity, carries_uri, conversation_for,
    create_note, deliver, entry_for, follow_bob, get, home_timeline, local, note_object,
    public_timeline, tag_timeline, told_of,
};
use crate::{common, remote};

/// Scenario: a public remote note. It lands on the anonymous public timeline
/// and on the author's own profile listing, each as the mock's `bob` mirror
/// under its loopback host, with the zero counters a mirrored post starts at.
#[tokio::test]
async fn a_public_remote_note_lands_on_the_public_timeline_and_its_profile() {
    let db = common::test_db().await;
    let pool = db.pool().clone();
    let (app, state) = remote::app_and_state(pool.clone());
    let peer = remote::MockPeer::start().await;
    let actor = peer.actor(BOB);

    let uri = peer.note("public-1");
    let create = activity(
        &peer,
        "Create",
        "public-1",
        &actor,
        note_object(
            &peer,
            &actor,
            "public-1",
            "<p>hello from bob</p>",
            json!({}),
        ),
    );
    deliver(&app, &state, &peer, BOB, &create).await;

    let public = public_timeline(&app).await;
    let entry = entry_for(&public, &uri);
    assert_eq!(
        entry["account"]["acct"],
        json!(peer.handle(BOB)),
        "the status reads under its author's loopback handle (decision 7)"
    );
    assert_eq!(
        entry["visibility"],
        json!("public"),
        "the `to: Public` addressing"
    );
    assert_eq!(
        entry["content"],
        json!("<p>hello from bob</p>"),
        "the delivered body"
    );
    assert_eq!(
        entry["reblogs_count"],
        json!(0),
        "a mirror starts uncounted"
    );
    assert_eq!(entry["favourites_count"], json!(0), "on both counters");

    let bob = entry["account"]["id"].as_str().expect("the author id");
    let profile = account_statuses(&app, None, bob).await;
    assert!(
        carries_uri(&profile, &uri),
        "and the author's profile carries it"
    );
}

/// Scenario: an unlisted remote note. The public address in `cc` only keeps it
/// out of the public timeline while a reader who has the link — an anonymous
/// status read, the author's profile, and a follower's home timeline — still
/// reaches it.
#[tokio::test]
async fn an_unlisted_remote_note_stays_off_the_public_timeline() {
    let db = common::test_db().await;
    let pool = db.pool().clone();
    let (app, state) = remote::app_and_state(pool.clone());
    let peer = remote::MockPeer::start().await;
    let (_alice, alice_token) = local(&pool, "ei-unlisted-alice").await;
    follow_bob(&app, &state, &peer, &alice_token, "unlisted-accept").await;
    let actor = peer.actor(BOB);

    let uri = peer.note("unlisted-1");
    let create = activity(
        &peer,
        "Create",
        "unlisted-1",
        &actor,
        note_object(
            &peer,
            &actor,
            "unlisted-1",
            "<p>quietly public</p>",
            json!({"to": [format!("{actor}/followers")], "cc": [AS_PUBLIC]}),
        ),
    );
    deliver(&app, &state, &peer, BOB, &create).await;

    let public = public_timeline(&app).await;
    assert!(
        !carries_uri(&public, &uri),
        "an unlisted post is absent from the public timeline"
    );

    // Reachable to a follower's home, on the author's profile, and to anyone
    // who holds the id directly — that is what separates unlisted from private.
    let home = home_timeline(&app, &alice_token).await;
    assert!(carries_uri(&home, &uri), "a follower's home carries it");
    let entry = entry_for(&home, &uri);
    let id = common::id_of(entry);
    let bob = entry["account"]["id"].as_str().expect("the author id");
    let profile = account_statuses(&app, None, bob).await;
    assert!(
        carries_uri(&profile, &uri),
        "the author's profile carries it"
    );
    let anon = get(&app, None, &format!("{STATUSES}/{id}")).await;
    common::assert_answer(&anon, StatusCode::OK, "an unlisted post is readable by id");
}

/// Scenario: a followers-only remote note. Addressed to the author's followers
/// collection and to nobody public, it reaches only an approved follower: the
/// anonymous read is the 404 and a follower's home carries it.
#[tokio::test]
async fn a_followers_only_remote_note_reaches_only_an_approved_follower() {
    let db = common::test_db().await;
    let pool = db.pool().clone();
    let (app, state) = remote::app_and_state(pool.clone());
    let peer = remote::MockPeer::start().await;
    let (_alice, alice_token) = local(&pool, "ei-private-alice").await;
    follow_bob(&app, &state, &peer, &alice_token, "private-accept").await;
    let actor = peer.actor(BOB);

    let uri = peer.note("private-1");
    let create = activity(
        &peer,
        "Create",
        "private-1",
        &actor,
        note_object(
            &peer,
            &actor,
            "private-1",
            "<p>for followers</p>",
            json!({"to": [format!("{actor}/followers")]}),
        ),
    );
    deliver(&app, &state, &peer, BOB, &create).await;

    let home = home_timeline(&app, &alice_token).await;
    assert!(
        carries_uri(&home, &uri),
        "the approved follower's home carries it"
    );
    let follower_id = entry_for(&home, &uri)["id"].as_str().expect("a local id");
    let as_follower = get(
        &app,
        Some(&alice_token),
        &format!("{STATUSES}/{follower_id}"),
    )
    .await;
    common::assert_answer(&as_follower, StatusCode::OK, "the follower's status read");
    let anonymous = get(&app, None, &format!("{STATUSES}/{follower_id}")).await;
    assert_eq!(
        anonymous.status,
        StatusCode::NOT_FOUND,
        "a stranger reading the private post by id gets the 404"
    );
}

/// Scenario: a direct remote note. Addressed to the mentioned local user and
/// to nobody public, it appears only in that recipient's conversations, is the
/// 404 to an anonymous read, and never reaches the public timeline.
#[tokio::test]
async fn a_direct_remote_note_reaches_its_recipient_and_no_timeline() {
    let db = common::test_db().await;
    let pool = db.pool().clone();
    let (app, state) = remote::app_and_state(pool.clone());
    let peer = remote::MockPeer::start().await;
    let (alice, alice_token) = local(&pool, "ei-direct-alice").await;
    let actor = peer.actor(BOB);

    let uri = peer.note("direct-1");
    let object = note_object(
        &peer,
        &actor,
        "direct-1",
        &format!(
            "<p>psst <span class=\"h\"><a href=\"{}\">@alice</a></span></p>",
            alice.ap_id
        ),
        json!({
            "to": [alice.ap_id],
            "tag": [{"type": "Mention", "href": alice.ap_id, "name": "@alice"}],
        }),
    );
    deliver(
        &app,
        &state,
        &peer,
        BOB,
        &activity(&peer, "Create", "direct-1", &actor, object),
    )
    .await;

    let public = public_timeline(&app).await;
    assert!(
        !carries_uri(&public, &uri),
        "a direct post leaks to no public timeline"
    );
    let conversation = conversation_for(&app, &alice_token, &uri)
        .await
        .expect("the recipient's conversations carry the DM");
    assert_eq!(
        conversation["last_status"]["account"]["acct"],
        json!(peer.handle(BOB)),
        "read back as the mock's actor mirror"
    );
    let dm_id = common::id_of(&conversation["last_status"]);
    let as_recipient = get(&app, Some(&alice_token), &format!("{STATUSES}/{dm_id}")).await;
    common::assert_answer(&as_recipient, StatusCode::OK, "the recipient reads the DM");
    let anonymous = get(&app, None, &format!("{STATUSES}/{dm_id}")).await;
    assert_eq!(
        anonymous.status,
        StatusCode::NOT_FOUND,
        "the direct post is the 404 to an anonymous read"
    );
}

/// Scenario: one delivered note's rich content — a `Document` attachment with
/// its remote media shape, a `Hashtag` tag listing the status under
/// `timelines/tag/{name}`, a `Mention` of a local user raising a `mention`
/// telling and visibility, and a `sensitive` content warning rendered as
/// `spoiler_text` over the marked body.
#[tokio::test]
async fn a_rich_remote_note_lands_its_media_hashtag_mention_and_warning() {
    let db = common::test_db().await;
    let pool = db.pool().clone();
    let (app, state) = remote::app_and_state(pool.clone());
    let peer = remote::MockPeer::start().await;
    let (alice, alice_token) = local(&pool, "ei-rich-alice").await;
    let actor = peer.actor(BOB);

    let uri = peer.note("rich-1");
    let create = create_note(
        &peer,
        &actor,
        "rich-1",
        "<p>look, a photo</p>",
        json!({
            "summary": "a content warning",
            "sensitive": true,
            "tag": [
                {"type": "Hashtag", "name": "#Rust", "href": "https://remote.example/tags/rust"},
                {"type": "Mention", "href": alice.ap_id, "name": "@alice"},
            ],
            "attachment": [{
                "type": "Document",
                "mediaType": "image/png",
                "url": "https://remote.example/media/rich.png",
                "name": "a chart",
                "width": 800,
                "height": 600,
            }],
        }),
    );
    deliver(&app, &state, &peer, BOB, &create).await;

    let entry = entry_for(&public_timeline(&app).await, &uri).clone();
    assert_eq!(
        entry["spoiler_text"],
        json!("a content warning"),
        "the CW renders"
    );
    assert_eq!(entry["sensitive"], json!(true), "folded behind the flag");
    let media = entry["media_attachments"]
        .as_array()
        .expect("media is an array");
    assert_remote_media(&media[0]);
    let tags: Vec<&str> = entry["tags"]
        .as_array()
        .expect("tags is an array")
        .iter()
        .map(|tag| tag["name"].as_str().expect("a named tag"))
        .collect();
    assert_eq!(tags, ["rust"], "the hashtag tag renders, lowercased");

    assert!(
        carries_uri(&tag_timeline(&app, "rust").await, &uri),
        "the status shows in the tag listing"
    );
    let told = told_of(&app, &alice_token, "mention").await;
    assert_eq!(told.len(), 1, "the mentioned local user is told once");
    assert_eq!(
        told[0]["status"]["uri"],
        json!(uri),
        "and the telling names the note"
    );
}

/// Asserts `attachment` renders the delivered remote `Document`'s media shape:
/// the document url served at `url`, `preview_url` and `remote_url` alike, with
/// its alt text.
fn assert_remote_media(attachment: &Value) {
    let expected = json!("https://remote.example/media/rich.png");
    assert_eq!(attachment["url"], expected, "the media url");
    assert_eq!(attachment["remote_url"], expected, "and the remote origin");
    assert_eq!(attachment["preview_url"], expected, "with its preview");
    assert_eq!(
        attachment["description"],
        json!("a chart"),
        "and its alt text"
    );
}

/// Scenario: a delivered `Question` poll. Its options and future `endTime`
/// land, the viewer reads them through `GET /api/v1/polls/{id}`, and voting
/// back on the remote poll marks the option the same way a local one would.
#[tokio::test]
async fn a_remote_question_note_lands_a_poll_the_viewer_can_vote_on() {
    let db = common::test_db().await;
    let pool = db.pool().clone();
    let (app, state) = remote::app_and_state(pool.clone());
    let peer = remote::MockPeer::start().await;
    let (_alice, alice_token) = local(&pool, "ei-poll-alice").await;
    let actor = peer.actor(BOB);

    let uri = peer.note("poll-1");
    let object = note_object(
        &peer,
        &actor,
        "poll-1",
        "<p>red or blue?</p>",
        json!({
            "type": "Question",
            "endTime": "2999-01-01T00:00:00Z",
            "oneOf": [
                {"type": "Note", "name": "Red", "replies": {"totalItems": 3}},
                {"type": "Note", "name": "Blue", "replies": {"totalItems": 7}},
            ],
        }),
    );
    deliver(
        &app,
        &state,
        &peer,
        BOB,
        &activity(&peer, "Create", "poll-1", &actor, object),
    )
    .await;

    let public = public_timeline(&app).await;
    let entry = entry_for(&public, &uri);
    let poll_id = common::id_of(&entry["poll"]);
    let stand = get(
        &app,
        Some(&alice_token),
        &format!("/api/v1/polls/{poll_id}"),
    )
    .await
    .json_value();
    assert_eq!(
        stand["votes_count"],
        json!(10),
        "the reported option totals"
    );
    assert_eq!(
        stand["expired"],
        json!(false),
        "a future endTime leaves it open"
    );
    assert_eq!(
        stand["options"]
            .as_array()
            .expect("options is an array")
            .iter()
            .map(|option| option["votes_count"].clone())
            .collect::<Vec<_>>(),
        vec![json!(3), json!(7)],
        "option by option, in order"
    );

    let vote = common::TestRequest::post(&format!("/api/v1/polls/{poll_id}/votes"))
        .bearer(&alice_token)
        .json(&json!({"choices": [0]}))
        .send(&app)
        .await;
    common::assert_answer(&vote, StatusCode::OK, "voting back on the remote poll");
    assert_eq!(
        vote.json_value()["votes_count"],
        json!(11),
        "the ballot counts"
    );
    assert_eq!(
        vote.json_value()["voted"],
        json!(true),
        "and marks the voter as having voted"
    );
}

/// Scenario: a delivered Mastodon-shaped video `Document` with an `icon`.
/// The icon url is the attachment's `preview_url` (the video file stays its
/// `url`), and a video without an icon has a `null` preview.
#[tokio::test]
async fn a_remote_video_icon_is_its_preview_url() {
    let db = common::test_db().await;
    let pool = db.pool().clone();
    let (app, state) = remote::app_and_state(pool.clone());
    let peer = remote::MockPeer::start().await;
    let actor = peer.actor(BOB);

    let uri = peer.note("video-1");
    let create = create_note(
        &peer,
        &actor,
        "video-1",
        "<p>a clip</p>",
        json!({
            "attachment": [
                {
                    "type": "Document",
                    "mediaType": "video/mp4",
                    "url": "https://remote.example/media/clip.mp4",
                    "name": "",
                    "width": 640,
                    "height": 360,
                    "duration": "PT3S",
                    "icon": {
                        "type": "Image",
                        "mediaType": "image/jpeg",
                        "url": "https://remote.example/media/clip_thumb.jpg",
                    },
                },
                {
                    "type": "Document",
                    "mediaType": "video/mp4",
                    "url": "https://remote.example/media/bare.mp4",
                    "name": "",
                },
            ],
        }),
    );
    deliver(&app, &state, &peer, BOB, &create).await;

    let entry = entry_for(&public_timeline(&app).await, &uri).clone();
    let media = entry["media_attachments"]
        .as_array()
        .expect("media is an array");
    let with_icon = media
        .iter()
        .find(|item| item["url"] == "https://remote.example/media/clip.mp4")
        .expect("the video with an icon landed");
    assert_eq!(
        with_icon["preview_url"],
        json!("https://remote.example/media/clip_thumb.jpg"),
        "the icon is the preview"
    );
    let bare = media
        .iter()
        .find(|item| item["url"] == "https://remote.example/media/bare.mp4")
        .expect("the video without an icon landed");
    assert_eq!(
        bare["preview_url"],
        Value::Null,
        "no icon, no preview: never the video file"
    );
}
