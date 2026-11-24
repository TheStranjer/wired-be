//! The posting matrix: every kind of status posted once and read by every
//! viewer, the audience of each kind pinned exactly.

use axum::http::StatusCode;
use serde_json::json;
use wired_be::{
    config::TEST_BASE_URL,
    util::entities::{hashtag_link_html, mention_html},
};

use super::support::{
    ADDRESSED, FOLLOWERS, LOCAL, NOT_FOUND, OPEN, STATUSES, Shape, UNKNOWN_ID, get, posted,
    posted_form, sweep, uploaded, uploads_dir, world, world_with_uploads,
};
use crate::common;

/// The open pair: a `public` and an `unlisted` post answer the same
/// entity to the author, every other viewer and the anonymous caller
/// alike — rule 3 — and only the account the author blocked reads the
/// shared 404 of them.
#[tokio::test]
async fn public_and_unlisted_posts_answer_every_viewer_but_the_blocked_one() {
    let db = common::test_db().await;
    let pool = db.pool().clone();
    let (app, world) = world(&pool).await;

    for visibility in ["public", "unlisted"] {
        let text = format!("a {visibility} word");
        let created = posted(
            &app,
            &world.author.token,
            json!({"status": text, "visibility": visibility}),
        )
        .await;
        let shape = Shape {
            id: common::id_of(&created),
            visibility,
            content: json!(format!("<p>{text}</p>")),
            acct: "author",
        };
        let admitted = sweep(&app, &world, &shape, &OPEN).await;
        assert_eq!(admitted.len(), 7, "seven viewers read a {visibility} post");
    }
}

/// A `private` post answers the author and the follower whose request was
/// granted — and the shared 404 to the follower whose request still
/// waits, to the stranger, to the unaddressed `mentioned`, to the muted
/// account, to the blocked account and to the anonymous caller (rule 5).
#[tokio::test]
async fn a_private_post_reaches_the_author_and_the_accepted_follower_only() {
    let db = common::test_db().await;
    let pool = db.pool().clone();
    let (app, world) = world(&pool).await;

    let created = posted(
        &app,
        &world.author.token,
        json!({"status": "for followers only", "visibility": "private"}),
    )
    .await;
    let shape = Shape {
        id: common::id_of(&created),
        visibility: "private",
        content: json!("<p>for followers only</p>"),
        acct: "author",
    };
    let admitted = sweep(&app, &world, &shape, &FOLLOWERS).await;
    assert_eq!(
        admitted.len(),
        2,
        "author and granted follower alone read it"
    );
    assert_eq!(
        admitted[1]["account"]["acct"],
        json!("author"),
        "as its author"
    );

    // The no-existence-leak claim made explicit: the stranger cannot tell
    // this hidden post from an id that names no row at all.
    let hidden = get(
        &app,
        Some(&world.stranger.token),
        &format!("{STATUSES}/{}", shape.id),
    )
    .await;
    let absent = get(
        &app,
        Some(&world.stranger.token),
        &format!("{STATUSES}/{UNKNOWN_ID}"),
    )
    .await;
    assert_eq!(
        hidden.body, absent.body,
        "one body for hidden and for absent"
    );
    common::assert_all_refused(
        &[hidden, absent],
        StatusCode::NOT_FOUND,
        NOT_FOUND,
        "the invisible private post and a never-stored id",
    );
}

/// A `direct` post @-addressed to `mentioned` answers its author and that
/// one addressee — the addressee's read renders the same h-card — and the
/// shared 404 to everybody else, the author's accepted follower included:
/// a granted follow is not a participant (rule 6).
#[tokio::test]
async fn a_direct_post_reaches_the_author_and_the_addressed_account_only() {
    let db = common::test_db().await;
    let pool = db.pool().clone();
    let (app, world) = world(&pool).await;

    let created = posted(
        &app,
        &world.author.token,
        json!({"status": "@mentioned, a word between us", "visibility": "direct"}),
    )
    .await;
    let card = mention_html(&world.mentioned.user.ap_id, "mentioned");
    let shape = Shape {
        id: common::id_of(&created),
        visibility: "direct",
        content: json!(format!("<p>{card}, a word between us</p>")),
        acct: "author",
    };
    let admitted = sweep(&app, &world, &shape, &ADDRESSED).await;
    assert_eq!(admitted.len(), 2, "author and addressee alone read a DM");
    for entity in &admitted {
        assert_eq!(
            entity["mentions"][0]["acct"],
            json!("mentioned"),
            "the addressed account rides the entity"
        );
    }
}

/// A `local` post @-mentioning the follower answers every authenticated
/// viewer this instance owns — the pending requester and the stranger
/// included — and the shared 404 to the anonymous caller, which never
/// reaches a non-federating post, and to the blocked account, whose rule
/// outranks it (rule 4).
#[tokio::test]
async fn a_local_mention_post_reaches_local_viewers_and_loses_the_anonymous_caller() {
    let db = common::test_db().await;
    let pool = db.pool().clone();
    let (app, world) = world(&pool).await;

    let created = posted(
        &app,
        &world.author.token,
        json!({"status": "@follower, kept on the instance", "visibility": "local"}),
    )
    .await;
    let card = mention_html(&world.follower.user.ap_id, "follower");
    let shape = Shape {
        id: common::id_of(&created),
        visibility: "local",
        content: json!(format!("<p>{card}, kept on the instance</p>")),
        acct: "author",
    };
    let admitted = sweep(&app, &world, &shape, &LOCAL).await;
    assert_eq!(
        admitted.len(),
        6,
        "the instance's viewers alone read a local post"
    );
    for entity in &admitted {
        assert_eq!(
            entity["mentions"][0]["acct"],
            json!("follower"),
            "the local mention rides the entity"
        );
    }
}

/// The media kind: a multipart upload whose id the create claims through
/// `media_ids`. Every open viewer reads the post with the uploaded
/// attachment rendered in `media_attachments` — the id, the audio kind
/// the detector named from the bytes, and the serving URL the upload
/// itself answered.
#[tokio::test]
async fn a_media_post_carries_its_upload_to_the_open_audience() {
    let db = common::test_db().await;
    let pool = db.pool().clone();
    let dir = uploads_dir();
    let (app, world) = world_with_uploads(&pool, dir.path()).await;

    let attachment = uploaded(&app, &world.author.token).await;
    let media_id = common::id_of(&attachment);
    assert_eq!(
        attachment["type"],
        json!("audio"),
        "the upload answers its kind"
    );
    let created = posted(
        &app,
        &world.author.token,
        json!({"status": "a tune, hummed", "media_ids": [media_id.clone()]}),
    )
    .await;
    let shape = Shape {
        id: common::id_of(&created),
        visibility: "public",
        content: json!("<p>a tune, hummed</p>"),
        acct: "author",
    };
    let admitted = sweep(&app, &world, &shape, &OPEN).await;
    for entity in &admitted {
        let attached = &entity["media_attachments"][0];
        assert_eq!(
            attached["id"],
            json!(media_id),
            "the post carries the upload"
        );
        assert_eq!(
            attached["type"],
            json!("audio"),
            "typed from the file's bytes"
        );
        assert_eq!(
            attached["url"], attachment["url"],
            "served at the URL the upload announced"
        );
    }
}

/// The content-warning kind: `spoiler_text` and `sensitive` ride every
/// admitted reader's entity beside the untouched rendered content, and
/// the audience is the open one — a warning folds, it never hides.
#[tokio::test]
async fn a_content_warning_post_states_its_subject_to_the_open_audience() {
    let db = common::test_db().await;
    let pool = db.pool().clone();
    let (app, world) = world(&pool).await;

    let created = posted(
        &app,
        &world.author.token,
        json!({"status": "the butler did it", "spoiler_text": "twist ahead", "sensitive": true}),
    )
    .await;
    let shape = Shape {
        id: common::id_of(&created),
        visibility: "public",
        content: json!("<p>the butler did it</p>"),
        acct: "author",
    };
    let admitted = sweep(&app, &world, &shape, &OPEN).await;
    for entity in &admitted {
        assert_eq!(
            entity["spoiler_text"],
            json!("twist ahead"),
            "the warning subject answers"
        );
        assert_eq!(entity["sensitive"], json!(true), "and the fold flag");
    }
}

/// The poll kind, posted through the form's nested `poll[options][]` /
/// `poll[expires_in]` keys: every admitted reader gets the same open
/// stand — options in submission order at zero counts, nobody's ballot,
/// an hour-out deadline.
#[tokio::test]
async fn a_poll_post_carries_its_open_stand_to_the_open_audience() {
    let db = common::test_db().await;
    let pool = db.pool().clone();
    let (app, world) = world(&pool).await;

    let created = posted_form(
        &app,
        &world.author.token,
        &[
            ("status", "soup or salad?"),
            ("poll[options][]", "soup"),
            ("poll[options][]", "salad"),
            ("poll[expires_in]", "3600"),
        ],
    )
    .await;
    let shape = Shape {
        id: common::id_of(&created),
        visibility: "public",
        content: json!("<p>soup or salad?</p>"),
        acct: "author",
    };
    let admitted = sweep(&app, &world, &shape, &OPEN).await;
    for entity in &admitted {
        let stand = &entity["poll"];
        assert_eq!(
            stand["options"],
            json!([
                {"title": "soup", "votes_count": 0},
                {"title": "salad", "votes_count": 0},
            ]),
            "the options ride every read"
        );
        assert_eq!(stand["votes_count"], json!(0), "the stand opens at zero");
        assert_eq!(stand["voted"], json!(false), "and nobody's marked anything");
        assert_eq!(stand["expired"], json!(false), "an hour out is open");
        assert!(
            stand["expires_at"].is_string(),
            "with a stated deadline: {stand}"
        );
    }
}

/// The hashtag kind: every admitted reader sees the tag linked in the
/// content and answered in `tags`, folded to the one stored name.
#[tokio::test]
async fn a_hashtag_post_links_its_tag_for_the_open_audience() {
    let db = common::test_db().await;
    let pool = db.pool().clone();
    let (app, world) = world(&pool).await;

    let created = posted(
        &app,
        &world.author.token,
        json!({"status": "tagged #E2ETag for the matrix"}),
    )
    .await;
    let tag = hashtag_link_html(TEST_BASE_URL, "e2etag", "E2ETag");
    let shape = Shape {
        id: common::id_of(&created),
        visibility: "public",
        content: json!(format!("<p>tagged {tag} for the matrix</p>")),
        acct: "author",
    };
    let admitted = sweep(&app, &world, &shape, &OPEN).await;
    for entity in &admitted {
        assert_eq!(
            entity["tags"],
            json!([{"name": "e2etag", "url": format!("{TEST_BASE_URL}/tags/e2etag")}]),
            "the folded tag answers on every read"
        );
    }
}
