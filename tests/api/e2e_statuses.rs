//! End-to-end posting matrix, part 1 (WBE-223): the fixture world and the
//! show-route sweep.
//!
//! One author posts every single-status kind over HTTP — `public`,
//! `unlisted`, `private`, a `direct` addressed by an @-mention of
//! `mentioned`, a media post (a multipart `POST /api/v1/media` whose id
//! the create then claims through `media_ids`), a content warning
//! (`spoiler_text` + `sensitive`), a poll (the form's nested
//! `poll[options][]` / `poll[expires_in]` keys), a hashtag post
//! (`#E2ETag`) and a `local` post @-mentioning the follower — and every
//! viewer role reads each one back through `GET /api/v1/statuses/{id}`.
//! The answers are pinned against the visibility model in
//! `wired_be::models::status_visibility`: the viewer a rule admits reads
//! the exact entity — its `id`, its stored `visibility`, its rendered
//! `content`, the author's `acct`, and the kind's own shape (the warning,
//! the poll, the attachment, the tag, the mention) — and every viewer the
//! rules shut out reads the one identical shared
//! `{"error":"Record not found"}` 404: `private` is invisible to the
//! stranger, the still-pending follower, the blocked account and the
//! anonymous caller, `direct` to every non-participant, `local` to the
//! anonymous caller, and everything the author posted to the account the
//! author blocked (their own aside), so a probe never learns that a
//! hidden status exists.
//!
//! Reply chains, conversation directs and quote statuses are the matrix's
//! later slices and are not posted here; a tombstone and a reblog — the
//! model's rules 1 and 7 — are `e2e_interactions.rs`'s delete and boost
//! scenarios. The per-endpoint parameter refusals stay pinned by the area
//! files (`statuses.rs`, `media_upload.rs`, `polls.rs`, …); this module
//! owns the kind × viewer matrix on the show route.

#![cfg(test)]

use std::path::Path;

use axum::{Router, body::Bytes, http::StatusCode};
use serde_json::{Value, json};
use sqlx::PgPool;
use tempfile::TempDir;
use wired_be::{
    config::TEST_BASE_URL,
    models::user::User,
    util::entities::{hashtag_html, mention_html},
};

use crate::common::{self, MultipartPart};

/// The status write and read route the whole matrix rides.
const STATUSES: &str = "/api/v1/statuses";
/// The account route the world's follow, block and mute writes ride.
const ACCOUNTS: &str = "/api/v1/accounts";
/// The self-lock write that turns a later follow into a request.
const UPDATE_CREDENTIALS: &str = "/api/v1/accounts/update_credentials";
/// The upload route the media kind posts through.
const MEDIA: &str = "/api/v1/media";
/// The scopes every role carries: `read write` covers every sub-scope of
/// the posting world (decision 9's scope tree).
const SCOPES: &str = "read write";
/// The shared 404 body of the show-route gate.
const NOT_FOUND: &str = "Record not found";
/// Boundary the media upload's multipart body is framed with.
const BOUNDARY: &str = "wired-e2e-statuses-boundary";
/// An `MP3`: an `ID3v2` tag header with a zero-length tag — the upload
/// endpoint names audio from its bytes and decodes nothing.
const MP3: &[u8] = b"ID3\x03\x00\x00\x00\x00\x00\x00";
/// An id that names no stored row, for the indistinguishability probe.
const UNKNOWN_ID: &str = "01934567-89ab-7cde-f012-3456789abcde";

/// The audience of every open kind — `public`, `unlisted`, media, the
/// warning, the poll and the tag: every viewer but the account the author
/// blocked, the anonymous caller included (rule 3).
const OPEN: [&str; 7] = [
    "author",
    "follower",
    "pending",
    "stranger",
    "mentioned",
    "muter",
    "anonymous",
];
/// The audience of a `private` post: the author and every account with an
/// **accepted** follow — a pending request is not one (rule 5).
const FOLLOWERS: [&str; 2] = ["author", "follower"];
/// The audience of a `direct` post: the author and the accounts its
/// @-mention addressed (rule 6).
const ADDRESSED: [&str; 2] = ["author", "mentioned"];
/// The audience of a `local` post: every authenticated viewer this
/// instance owns and nobody else — the anonymous caller loses it, an
/// accepted follower grants nothing extra (rule 4).
const LOCAL: [&str; 6] = [
    "author",
    "follower",
    "pending",
    "stranger",
    "mentioned",
    "muter",
];
/// Every viewer the matrix asks, for a post nobody is shut out of.
const EVERYONE: [&str; 8] = [
    "author",
    "follower",
    "pending",
    "stranger",
    "mentioned",
    "blocker",
    "muter",
    "anonymous",
];

/// One world account: its stored row and the bearer token its requests
/// ride.
#[derive(Debug)]
struct Actor {
    /// The stored local account, for its id and actor URL.
    user: User,
    /// A token granting [`SCOPES`].
    token: String,
}

/// The fixture world: the author of every kind and the six other viewers,
/// each in the relation the visibility rules key on.
#[derive(Debug)]
struct World {
    /// The local account that posts every kind.
    author: Actor,
    /// A follower of the author, granted on the spot (rule 5's audience).
    follower: Actor,
    /// A follower whose follow of the now-locked author only waits.
    pending: Actor,
    /// An unrelated local account.
    stranger: Actor,
    /// The account the `direct` kind @-addresses.
    mentioned: Actor,
    /// The account the author blocked (rule 2's blind viewer).
    blocker: Actor,
    /// The account the author muted — a mute, never a visibility rule.
    muter: Actor,
}

impl World {
    /// Every viewer of the matrix, in one fixed order: the seven fixture
    /// accounts and the anonymous caller.
    fn viewers(&self) -> Vec<(&'static str, Option<&str>)> {
        vec![
            ("author", Some(self.author.token.as_str())),
            ("follower", Some(self.follower.token.as_str())),
            ("pending", Some(self.pending.token.as_str())),
            ("stranger", Some(self.stranger.token.as_str())),
            ("mentioned", Some(self.mentioned.token.as_str())),
            ("blocker", Some(self.blocker.token.as_str())),
            ("muter", Some(self.muter.token.as_str())),
            ("anonymous", None),
        ]
    }
}

/// The exact entity body the admitted viewers of one posted kind read:
/// the legs the show route pins for every reader, whatever the kind adds
/// on top.
#[derive(Debug)]
struct Shape<'a> {
    /// The status's own `id`, as the create answered it.
    id: String,
    /// The `visibility` key the entity answers.
    visibility: &'a str,
    /// The `content` the entity renders.
    content: Value,
    /// The author's `acct`.
    acct: &'a str,
}

/// A local account named `nickname` beside a bearer token granting
/// [`SCOPES`].
async fn actor(pool: &PgPool, nickname: &str) -> Actor {
    let user = common::create_local_user(pool, nickname).await;
    let token = common::create_app_and_token(pool, &user, SCOPES).await;
    Actor { user, token }
}

/// `POST /api/v1/accounts/{target}/{action}` (`follow`, `block`, `mute`)
/// as `token`, asserted 200 and answered as the relationship entity.
async fn relate(app: &Router, token: &str, target: &str, action: &str) -> Value {
    let response = common::TestRequest::post(&format!("{ACCOUNTS}/{target}/{action}"))
        .bearer(token)
        .send(app)
        .await;
    common::assert_answer(
        &response,
        StatusCode::OK,
        &format!("the {action} write stores"),
    );
    response.json_value()
}

/// `PATCH /api/v1/accounts/update_credentials` locking the caller's own
/// account, so the next follow of it files a request instead of granting.
async fn lock(app: &Router, token: &str) {
    let response = common::TestRequest::patch(UPDATE_CREDENTIALS)
        .bearer(token)
        .json(&json!({"locked": true}))
        .send(app)
        .await;
    common::assert_answer(&response, StatusCode::OK, "the self-lock lands");
    assert_eq!(
        response.json_value()["locked"],
        json!(true),
        "the account answers locked"
    );
}

/// Build the ticket's world over `app`: seven local accounts with tokens,
/// then the relationships the rules key on, every one written through
/// the client API — `follower`'s follow of the still-open author, granted
/// on the spot; the author's own lock, so `pending`'s later follow is
/// only a request; the author's block of `blocker`; and the author's mute
/// of `muter`, which is no visibility rule at all.
async fn build_world(pool: &PgPool, app: &Router) -> World {
    let author = actor(pool, "author").await;
    let follower = actor(pool, "follower").await;
    let pending = actor(pool, "pending").await;
    let stranger = actor(pool, "stranger").await;
    let mentioned = actor(pool, "mentioned").await;
    let blocker = actor(pool, "blocker").await;
    let muter = actor(pool, "muter").await;

    let author_id = author.user.id.to_string();
    let granted = relate(app, &follower.token, &author_id, "follow").await;
    assert_eq!(
        granted["following"],
        json!(true),
        "an open author grants the follow on the spot"
    );
    lock(app, &author.token).await;
    let waiting = relate(app, &pending.token, &author_id, "follow").await;
    assert_eq!(
        waiting["requested"],
        json!(true),
        "the lock turns a follow of the author into a request"
    );
    assert_eq!(
        waiting["following"],
        json!(false),
        "and the waiting request grants nothing"
    );
    let block_written = relate(app, &author.token, &blocker.user.id.to_string(), "block").await;
    assert_eq!(
        block_written["blocking"],
        json!(true),
        "the author blocks the blocker account"
    );
    let mute_written = relate(app, &author.token, &muter.user.id.to_string(), "mute").await;
    assert_eq!(
        mute_written["muting"],
        json!(true),
        "the author mutes the muter account"
    );

    World {
        author,
        follower,
        pending,
        stranger,
        mentioned,
        blocker,
        muter,
    }
}

/// The world of [`build_world`] over the stock test router.
async fn world(pool: &PgPool) -> (Router, World) {
    let app = common::test_app(pool.clone());
    let world = build_world(pool, &app).await;
    (app, world)
}

/// [`build_world`]'s world over a router that stores uploads in `dir`,
/// for the media kind: the upload lands in a temp dir, never in the
/// repository.
async fn world_with_uploads(pool: &PgPool, dir: &Path) -> (Router, World) {
    let app = common::test_app_with_uploads_dir(pool.clone(), dir);
    let world = build_world(pool, &app).await;
    (app, world)
}

/// A throwaway uploads directory, removed when the returned guard drops.
fn uploads_dir() -> TempDir {
    TempDir::new().expect("temp dir for the media upload")
}

/// The `id` of a rendered entity, as its JSON spells it.
fn entity_id(entity: &Value) -> String {
    entity["id"]
        .as_str()
        .expect("the entity carries its id")
        .to_owned()
}

/// `GET` `path` as `token` (`None`: the anonymous caller).
async fn get(app: &Router, token: Option<&str>, path: &str) -> common::TestResponse {
    let request = common::TestRequest::get(path);
    match token {
        Some(found) => request.bearer(found).send(app).await,
        None => request.send(app).await,
    }
}

/// `POST /api/v1/statuses` as `token` with the JSON `body`, asserted 200
/// and answered as the entity the create wrote. The matrix's one posting
/// helper, on top of which the media kind only ever adds an upload.
async fn posted(app: &Router, token: &str, body: Value) -> Value {
    let response = common::TestRequest::post(STATUSES)
        .bearer(token)
        .json(&body)
        .send(app)
        .await;
    common::assert_answer(&response, StatusCode::OK, "the kind posts");
    response.json_value()
}

/// The same create from a urlencoded body: the poll kind's nested
/// `poll[options][]` / `poll[expires_in]` spelling rides this one.
async fn posted_form(app: &Router, token: &str, fields: &[(&str, &str)]) -> Value {
    let response = common::TestRequest::post(STATUSES)
        .bearer(token)
        .form(fields)
        .send(app)
        .await;
    common::assert_answer(&response, StatusCode::OK, "the form's kind posts");
    response.json_value()
}

/// `POST /api/v1/media` as `token` with one MP3 file part, asserted 200
/// and answered as the uploaded attachment's entity.
async fn uploaded(app: &Router, token: &str) -> Value {
    let file = MultipartPart::file(
        "file",
        "e2e-tone.mp3",
        "audio/mpeg",
        Bytes::from_static(MP3),
    );
    let response = common::TestRequest::post(MEDIA)
        .bearer(token)
        .multipart(BOUNDARY, &[file])
        .send(app)
        .await;
    common::assert_answer(&response, StatusCode::OK, "the media upload lands");
    response.json_value()
}

/// The show-route sweep of one posted status: every viewer named in
/// `allowed` reads back the exact [`Shape`] — `id`, `visibility`,
/// rendered `content` and the author's `acct` — and every other viewer
/// answers, asserted together, the one shared 404 body, so a hidden
/// status is never distinguishable from a missing one. The admitted
/// entities come back, in viewer order, for the kind's own probes.
async fn sweep(app: &Router, world: &World, shape: &Shape<'_>, allowed: &[&str]) -> Vec<Value> {
    let mut admitted = Vec::new();
    let mut refused = Vec::new();
    for (who, token) in world.viewers() {
        let response = get(app, token, &format!("{STATUSES}/{}", shape.id)).await;
        if allowed.contains(&who) {
            common::assert_answer(&response, StatusCode::OK, &format!("{who} reads the kind"));
            let body = response.json_value();
            assert_eq!(
                body["id"],
                json!(shape.id),
                "{who}: the entity answers its id"
            );
            assert_eq!(
                body["visibility"],
                json!(shape.visibility),
                "{who}: the stored visibility answers"
            );
            assert_eq!(body["content"], shape.content, "{who}: the render answers");
            assert_eq!(
                body["account"]["acct"],
                json!(shape.acct),
                "{who}: the author speaks"
            );
            admitted.push(body);
        } else {
            refused.push(response);
        }
    }
    common::assert_all_refused(
        &refused,
        StatusCode::NOT_FOUND,
        NOT_FOUND,
        &format!(
            "every viewer the [{}] audience excludes",
            allowed.join(", ")
        ),
    );
    admitted
}

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
            id: entity_id(&created),
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
        id: entity_id(&created),
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
        id: entity_id(&created),
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
        id: entity_id(&created),
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
    let media_id = entity_id(&attachment);
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
        id: entity_id(&created),
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
        id: entity_id(&created),
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
        id: entity_id(&created),
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
    let tag = hashtag_html(TEST_BASE_URL, "e2etag");
    let shape = Shape {
        id: entity_id(&created),
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

/// Rule 2 in the matrix's own words: the account the author blocked reads
/// the shared 404 of the author's public post *and* of a `direct` that
/// @-addresses the blockee — the mention would have admitted them, the
/// block outranks it — while the same account's own post answers, in
/// full, to themselves and to every other viewer.
#[tokio::test]
async fn the_blocked_viewer_reads_nothing_of_the_author_and_their_own_everything() {
    let db = common::test_db().await;
    let pool = db.pool().clone();
    let (app, world) = world(&pool).await;

    let open = posted(
        &app,
        &world.author.token,
        json!({"status": "public past a block"}),
    )
    .await;
    let dm = posted(
        &app,
        &world.author.token,
        json!({"status": "@blocker, an unread note", "visibility": "direct"}),
    )
    .await;
    let own = posted(
        &app,
        &world.blocker.token,
        json!({"status": "the blocked still post"}),
    )
    .await;

    let mut blind = Vec::new();
    for id in [entity_id(&open), entity_id(&dm)] {
        blind.push(
            get(
                &app,
                Some(&world.blocker.token),
                &format!("{STATUSES}/{id}"),
            )
            .await,
        );
    }
    common::assert_all_refused(
        &blind,
        StatusCode::NOT_FOUND,
        NOT_FOUND,
        "the blocked reader of the author's public post and of a DM addressed to them",
    );

    let own_shape = Shape {
        id: entity_id(&own),
        visibility: "public",
        content: json!("<p>the blocked still post</p>"),
        acct: "blocker",
    };
    let admitted = sweep(&app, &world, &own_shape, &EVERYONE).await;
    assert_eq!(
        admitted.len(),
        8,
        "every viewer reaches the blockee's own post"
    );
}
