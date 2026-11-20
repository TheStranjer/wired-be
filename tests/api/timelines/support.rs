//! Shared consts and fixture helpers every submodule of the `timelines`
//! area draws on: the routes' paths and the instance base the `Link`
//! headers spell their targets with, the thin wrappers around the HTTP
//! harness and the rendered pages, and the model-level fixtures the tests
//! seed through `wired_be::models` directly. Everything here is
//! `pub(super)`, and a submodule imports exactly the members it calls.

use axum::Router;
use serde_json::Value;
use sqlx::PgPool;
use uuid::Uuid;
use wired_be::models::{
    follow,
    media_attachment::{self, NewMediaAttachment},
    status::{self, NewStatus, Status, Visibility},
    user::User,
};

use crate::common;

/// The instance the tests run as: what the `Link` header spells its targets
/// with.
pub(super) const BASE: &str = "https://test.example";
/// The home route's path, query strings appended per case.
pub(super) const HOME: &str = "/api/v1/timelines/home";
/// The tag route's path prefix; each test appends `/{tag}[?query]`.
pub(super) const TAG: &str = "/api/v1/timelines/tag";
/// The direct route's path, query strings appended per case.
pub(super) const DIRECT: &str = "/api/v1/timelines/direct";
/// The list route's path prefix; each test appends `/{id}[?query]`.
pub(super) const LIST: &str = "/api/v1/timelines/list";
/// The public route: `/api/v1/timelines/public[?query]`.
pub(super) const PUBLIC: &str = "/api/v1/timelines/public";
/// The exact 403 body a `RESTRICT_UNAUTHENTICATED` gate answers with.
pub(super) const RESTRICTED: &str = "Authentication is required to access this resource.";

/// `GET`s `path`, with `token` when the test presents one.
pub(super) async fn get(app: &Router, token: Option<&str>, path: &str) -> common::TestResponse {
    let request = common::TestRequest::get(path);
    match token {
        Some(token) => request.bearer(token).send(app).await,
        None => request.send(app).await,
    }
}

/// The ids of the statuses of a rendered page, in answer order.
pub(super) fn ids(body: &Value) -> Vec<String> {
    body.as_array()
        .expect("a timeline page is a JSON array")
        .iter()
        .map(|status| {
            status["id"]
                .as_str()
                .expect("every Status entity carries its id")
                .to_owned()
        })
        .collect()
}

/// The ids of `posts`, newest first — the order every timeline answers in,
/// since `UUIDv7` text sorts by creation (decision 4).
pub(super) fn newest_first(posts: &[&Status]) -> Vec<String> {
    let mut ordered: Vec<Uuid> = posts.iter().map(|post| post.id).collect();
    ordered.sort_by(|a, b| b.cmp(a));
    ordered.into_iter().map(|id| id.to_string()).collect()
}

/// The one id of `post`, as its JSON carries it.
pub(super) fn id_of(post: &Status) -> String {
    post.id.to_string()
}

/// The ids of `posts`, newest first, minus the ids of `without`: an
/// expected page written as a difference from the seeded set.
pub(super) fn minus(posts: &[&Status], without: &[&Status]) -> Vec<String> {
    let mut kept = newest_first(posts);
    for skipped in without {
        let skipped = id_of(skipped);
        kept.retain(|id| *id != skipped);
    }
    kept
}

/// Inserts `text` as `author`'s public status; `#tags` written in the text
/// are the tags it carries.
pub(super) async fn tagged(pool: &PgPool, author: &User, text: &str) -> Status {
    common::create_status(pool, author, text, Visibility::Public).await
}

/// Inserts a mirrored (`local = false`) public status carrying `hashtags`
/// as the activity's tag list named them.
pub(super) async fn remote_tagged(
    pool: &PgPool,
    author: &User,
    key: &str,
    hashtags: &[&str],
) -> Status {
    status::insert(
        pool,
        BASE,
        &NewStatus {
            uri: Some(format!("https://origin.test.example/objects/{key}")),
            user_id: author.id,
            raw_text: key.to_owned(),
            hashtags: hashtags.iter().map(|name| (*name).to_owned()).collect(),
            ..NewStatus::default()
        },
    )
    .await
    .expect("the remote fixture inserts")
}

/// Attaches one image to `post`.
pub(super) async fn attach_media(pool: &PgPool, author: &User, post: &Status) {
    let attached = media_attachment::insert(
        pool,
        &NewMediaAttachment {
            user_id: Some(author.id),
            status_id: Some(post.id),
            url: format!("{BASE}/media/{}.png", post.id),
            content_type: "image/png".to_owned(),
            ..Default::default()
        },
    )
    .await
    .expect("the media fixture attaches");
    assert_eq!(attached.status_id, Some(post.id), "one image lands");
}

/// Inserts `author`'s public reply to `parent`, @-mentioning everyone in
/// `mentioned` (empty for none), straight through
/// [`wired_be::models::status`] — the read the replies policy judges.
pub(super) async fn reply(
    pool: &PgPool,
    author: &User,
    parent: &Status,
    mentioned: &[&User],
    text: &str,
) -> Status {
    status::insert(
        pool,
        BASE,
        &NewStatus {
            in_reply_to_id: Some(parent.id),
            in_reply_to_account_id: Some(parent.user_id),
            mentioned_user_ids: mentioned.iter().map(|member| member.id).collect(),
            ..NewStatus::local(author, text)
        },
    )
    .await
    .expect("the reply fixture inserts")
}

/// `reblogger`'s public reblog of `original`, straight through
/// [`wired_be::models::status`] — the row the `show_reblogs` toggle and the
/// block/mute exclusions judge.
pub(super) async fn reblog(pool: &PgPool, reblogger: &User, original: &Status) -> Status {
    status::create_reblog(pool, BASE, reblogger, original, Visibility::Public)
        .await
        .expect("the reblog fixture writes")
        .0
}

/// `follower` follows `target` through [`wired_be::models::follow`],
/// asserting the accepted state the home timeline's source predicate
/// reads — an unlocked fixture account always grants.
pub(super) async fn accepted_follow(pool: &PgPool, follower: &User, target: &User) {
    let opened = follow::follow(pool, BASE, follower, target)
        .await
        .expect("the fixture follow is written");
    assert_eq!(
        opened.state,
        follow::FollowState::Accepted,
        "the follow of `{}` is granted, so the home source reads it",
        target.nickname
    );
}
