//! End-to-end posting matrix (WBE-223): the fixture world, the show-route
//! sweep of the single-status kinds, the thread-context reads with the
//! account statuses filter sweep, and the five-timeline membership sweep.
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
//! Reply chains and the conversation direct are the matrix's second slice,
//! posted through the same create route and read back per viewer through
//! `GET /api/v1/statuses/{id}/context` as `wired_be::api::statuses`'s
//! handler documents it: ancestors root-first (oldest first), descendants
//! flat in chronological thread order, an invisible link skipped but walked
//! past rather than truncating the walk, and every descendant's
//! `pleroma.parent_visible` answering whether its own direct parent came
//! back in this very answer. The same slice then sweeps
//! `GET /api/v1/accounts/{author}/statuses` with every real `StatusesParams`
//! filter — `pinned`, `only_media`, `exclude_replies`, `exclude_reblogs`,
//! `with_muted`, `tagged`, `exclude_visibilities[]` — across the six sweep
//! viewers, asserting each page's membership by id set. Quote statuses are
//! still not posted here; a tombstone and a reblog — the model's rules 1
//! and 7 — are `e2e_interactions.rs`'s delete and boost scenarios (the
//! sweep's one boost is the author's own, posted here to be filtered). The
//! per-endpoint parameter refusals stay pinned by the area files
//! (`statuses.rs`, `account_statuses.rs`, `media_upload.rs`, `polls.rs`, …);
//! this module owns the kind × viewer matrix on the show route, the thread
//! reads, the filter sweep on the account's page, and the membership sweep of
//! every posted kind across all five timelines of `src/api/timelines.rs` —
//! home, public, tag, direct and list — per viewer, by id set: the author's
//! and the follower's home audiences, the muter's silence and its
//! `with_muted` restore, the blocked viewer's emptiness, the public page
//! narrowed to its public rows (plus `local` for a token this instance
//! owns) with its `only_media` cut and its documented
//! `RESTRICT_UNAUTHENTICATED` gate, the one `#E2ETag` page every named
//! viewer shares, the direct page held to its participants under
//! `read:statuses`, and the author's own list — the author added as its own
//! member — holding the whole posting under the stored `replies_policy`
//! while another account's list id answers the same shared 404 as the show
//! route.

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

// ---------------------------------------------------------------------------
// Part 2: the thread kinds over HTTP, the context reads, and the account
// statuses filter sweep.
// ---------------------------------------------------------------------------

/// The conversation listing the matrix reads a participation id back from.
const CONVERSATIONS: &str = "/api/v1/conversations";

/// The thread kinds, posted by the author through the create route: the
/// `public` root, the `private` reply to it, the `direct` reply-of-reply
/// that @-addresses `mentioned`, the `direct` post that continues the
/// thread through `in_reply_to_conversation_id`, and that arm's id — read
/// from the API's own conversation listing, never from the database.
#[derive(Debug)]
struct Thread {
    /// The `public` root of the chain.
    root: String,
    /// The `private` answer to the root.
    reply: String,
    /// The `direct` answer to `reply`, addressing `mentioned`.
    deep: String,
    /// The `direct` conversation continuation — threaded by its
    /// conversation id, so it answers no status.
    dm: String,
    /// The author's own participation id the `dm` names.
    side: String,
}

/// The account sweep's exhibit: a [`Thread`] plus the kinds the filters key
/// on — the attached warning post, the `#E2ETag` post, a plain post pinned
/// to the profile, and the author's boost of `mentioned`'s own original.
#[derive(Debug)]
struct Exhibit {
    /// The thread legs the `exclude_replies` and visibility filters ride.
    thread: Thread,
    /// The one status carrying a media attachment (with a warning).
    media: String,
    /// The one status carrying `#E2ETag`.
    tagpost: String,
    /// The plain post pinned through the pin write.
    extra: String,
    /// The author's reblog row, id-wise.
    reblog: String,
}

/// `POST /api/v1/statuses/{id}/pin` as `token`, asserted 200 and answered
/// as the status entity with its new `pinned` flag.
async fn pinned(app: &Router, token: &str, id: &str) -> Value {
    let response = common::TestRequest::post(&format!("{STATUSES}/{id}/pin"))
        .bearer(token)
        .send(app)
        .await;
    common::assert_answer(&response, StatusCode::OK, "the pin write lands");
    response.json_value()
}

/// `POST /api/v1/statuses/{id}/reblog` as `token`, asserted 200 and
/// answered as the boost entity — the reblog row's own id.
async fn reblogged(app: &Router, token: &str, id: &str) -> Value {
    let response = common::TestRequest::post(&format!("{STATUSES}/{id}/reblog"))
        .bearer(token)
        .send(app)
        .await;
    common::assert_answer(&response, StatusCode::OK, "the boost lands");
    response.json_value()
}

/// The caller's own participation whose `last_status` is the status
/// `last_id`: [`wired_be::api::conversations`]'s listing answers it, and
/// its `id` — which the entity also renders as
/// `pleroma.direct_conversation_id` — is exactly what a create's
/// `in_reply_to_conversation_id` takes.
async fn own_participation(app: &Router, token: &str, last_id: &str) -> Value {
    let response = get(app, Some(token), CONVERSATIONS).await;
    common::assert_answer(
        &response,
        StatusCode::OK,
        "the conversation listing answers",
    );
    let body = response.json_value();
    let rows = body.as_array().expect("the listing is an array");
    let found = rows
        .iter()
        .find(|row| row["last_status"]["id"] == json!(last_id))
        .expect("the author's own side of the thread they addressed is listed");
    (*found).clone()
}

/// Post the thread kinds as the author, over HTTP only: root, private
/// reply, direct reply-of-reply addressed to `mentioned` — then read the
/// author's own participation back from `GET /api/v1/conversations` and
/// post a `direct` continuation naming it as `in_reply_to_conversation_id`.
async fn post_thread(app: &Router, world: &World) -> Thread {
    let root = entity_id(
        &posted(
            app,
            &world.author.token,
            json!({"status": "the thread root"}),
        )
        .await,
    );
    let reply = entity_id(
        &posted(
            app,
            &world.author.token,
            json!({"status": "a private answer", "visibility": "private", "in_reply_to_id": &root}),
        )
        .await,
    );
    let deep = entity_id(
        &posted(
            app,
            &world.author.token,
            json!({"status": "@mentioned, between us below", "visibility": "direct", "in_reply_to_id": &reply}),
        )
        .await,
    );
    let participation = own_participation(app, &world.author.token, &deep).await;
    let side = participation["id"]
        .as_str()
        .expect("the participation answers its id")
        .to_owned();
    assert_eq!(
        participation["last_status"]["pleroma"]["direct_conversation_id"],
        json!(&side),
        "the entity renders its own side id"
    );
    let dm = entity_id(
        &posted(
            app,
            &world.author.token,
            json!({"status": "still between us", "visibility": "direct", "in_reply_to_conversation_id": &side}),
        )
        .await,
    );
    Thread {
        root,
        reply,
        deep,
        dm,
        side,
    }
}

/// Post the whole [`Exhibit`]: the [`Thread`] legs first, then the media
/// post carrying a content warning, the hashtag post, the plain post
/// pinned through the pin write, and the author's boost of `mentioned`'s
/// public original — every one through the client API.
async fn post_exhibit(app: &Router, world: &World) -> Exhibit {
    let thread = post_thread(app, world).await;
    let attachment = uploaded(app, &world.author.token).await;
    let media = entity_id(
        &posted(
            app,
            &world.author.token,
            json!({
                "status": "a warned tune",
                "media_ids": [entity_id(&attachment)],
                "spoiler_text": "audio inside",
                "sensitive": true,
            }),
        )
        .await,
    );
    let tagpost = entity_id(
        &posted(
            app,
            &world.author.token,
            json!({"status": "tagged #E2ETag on the sweep"}),
        )
        .await,
    );
    let extra = entity_id(
        &posted(
            app,
            &world.author.token,
            json!({"status": "the featured word"}),
        )
        .await,
    );
    let pin = pinned(app, &world.author.token, &extra).await;
    assert_eq!(
        pin["pinned"],
        json!(true),
        "the pin write answers the entity flagged"
    );
    let original = entity_id(
        &posted(
            app,
            &world.mentioned.token,
            json!({"status": "the original the author boosts"}),
        )
        .await,
    );
    let boost = reblogged(app, &world.author.token, &original).await;
    Exhibit {
        thread,
        media,
        tagpost,
        extra,
        reblog: entity_id(&boost),
    }
}

/// The exhibit stage: the uploads-backed world of [`world_with_uploads`]
/// with [`post_exhibit`]'s full exhibit posted onto it.
async fn exhibit(pool: &PgPool, dir: &Path) -> (Router, World, Exhibit) {
    let (app, world) = world_with_uploads(pool, dir).await;
    let shown = post_exhibit(&app, &world).await;
    (app, world, shown)
}

/// `GET /api/v1/statuses/{id}/context` as `token` (`None`: the anonymous
/// caller).
async fn context(app: &Router, token: Option<&str>, id: &str) -> common::TestResponse {
    get(app, token, &format!("{STATUSES}/{id}/context")).await
}

/// A context answer's shape and both arms' ids, in the order they were
/// answered: an object of exactly `ancestors` and `descendants`.
fn context_arms(body: &Value, who: &str) -> (Vec<String>, Vec<String>) {
    let arms: Vec<&String> = body
        .as_object()
        .unwrap_or_else(|| panic!("the context answer for {who} is an object"))
        .keys()
        .collect();
    assert_eq!(
        arms,
        vec!["ancestors", "descendants"],
        "{who}: the answer carries the two arms"
    );
    (arm_ids(body, "ancestors"), arm_ids(body, "descendants"))
}

/// The `id` values of one arm of a context answer, in answer order.
fn arm_ids(body: &Value, arm: &str) -> Vec<String> {
    body[arm]
        .as_array()
        .unwrap_or_else(|| panic!("the `{arm}` arm is an array"))
        .iter()
        .map(|status| {
            status["id"]
                .as_str()
                .expect("an arm entry carries its id")
                .to_owned()
        })
        .collect()
}

/// The answer's `pleroma.parent_visible` for the descendant `id`.
fn parent_visible(body: &Value, id: &str) -> bool {
    body["descendants"]
        .as_array()
        .expect("the descendants arm is an array")
        .iter()
        .find(|status| status["id"] == json!(id))
        .unwrap_or_else(|| panic!("`{id}` is listed under `descendants`"))["pleroma"]["parent_visible"]
        .as_bool()
        .expect("parent_visible is a bool")
}

/// The `id` values of one answered status page, in answer order.
fn page_ids(response: &common::TestResponse) -> Vec<String> {
    let body = response.json_value();
    body.as_array()
        .expect("the page is a JSON array")
        .iter()
        .map(|status| {
            status["id"]
                .as_str()
                .expect("every entity carries its id")
                .to_owned()
        })
        .collect()
}

/// The ids in the order a statuses page answers them: newest first, because
/// `UUIDv7` text is creation order (decision 4).
fn newest_first(ids: &[&str]) -> Vec<String> {
    let mut sorted: Vec<String> = ids.iter().map(|id| (*id).to_owned()).collect();
    sorted.sort_by(|a, b| b.cmp(a));
    sorted
}

/// The sweep's six viewers, in one fixed order: the author, the accepted
/// follower, the unrelated stranger, the blocked account, the account the
/// author muted, and the anonymous caller.
fn sweep_viewers(world: &World) -> [(&'static str, Option<&str>); 6] {
    [
        ("author", Some(world.author.token.as_str())),
        ("follower", Some(world.follower.token.as_str())),
        ("stranger", Some(world.stranger.token.as_str())),
        ("blocker", Some(world.blocker.token.as_str())),
        ("muter", Some(world.muter.token.as_str())),
        ("anonymous", None),
    ]
}

/// One filter row of the account sweep: `GET /api/v1/accounts/{author}
/// /statuses{query}` as every [`sweep_viewers`] role — the six `answers`,
/// paired in order, name exactly the ids that role must read newest-first.
/// Every role answered empty must read the byte-identical body of the same
/// request run against `pending`, an account that posted nothing: no
/// filter, exclusion or silence ever leaks that a hidden row exists.
async fn sweep_page(app: &Router, world: &World, query: &str, answers: [Vec<String>; 6]) {
    let author = world.author.user.id.to_string();
    let anchor = get(
        app,
        Some(world.blocker.token.as_str()),
        &format!("{ACCOUNTS}/{}/statuses{query}", world.pending.user.id),
    )
    .await;
    for (index, (who, token)) in sweep_viewers(world).into_iter().enumerate() {
        let response = get(app, token, &format!("{ACCOUNTS}/{author}/statuses{query}")).await;
        common::assert_answer(&response, StatusCode::OK, &format!("{who} reads the page"));
        assert_eq!(
            page_ids(&response),
            answers[index],
            "{who}: the membership of the {query} page"
        );
        if answers[index].is_empty() {
            assert_eq!(
                response.body, anchor.body,
                "{who}: the empty answer to {query} is the never-posted account's"
            );
        }
    }
}

/// The author reads their own thread whole, from every leg: ancestors
/// root-first, descendants in flat chronological order, and every
/// descendant's `parent_visible` true because the chain's direct parents
/// all came back (or are the named status itself).
#[tokio::test]
async fn the_author_reads_their_whole_thread_back_in_both_arms() {
    let db = common::test_db().await;
    let pool = db.pool().clone();
    let (app, world) = world(&pool).await;
    let thread = post_thread(&app, &world).await;
    let author = Some(world.author.token.as_str());

    {
        let response = context(&app, author, &thread.root).await;
        common::assert_answer(&response, StatusCode::OK, "the author at the root");
        let body = response.json_value();
        let (ancestors, descendants) = context_arms(&body, "the author at the root");
        assert!(
            ancestors.is_empty(),
            "the root answers nothing above itself"
        );
        assert_eq!(
            descendants,
            vec![thread.reply.clone(), thread.deep.clone()],
            "the descent is flat and chronological"
        );
        assert!(
            parent_visible(&body, &thread.reply),
            "the reply's parent is the named root"
        );
        assert!(
            parent_visible(&body, &thread.deep),
            "the reply-of-reply's parent came back too"
        );
    }

    {
        let response = context(&app, author, &thread.reply).await;
        common::assert_answer(&response, StatusCode::OK, "the author at the reply");
        let body = response.json_value();
        let (ancestors, descendants) = context_arms(&body, "the author at the reply");
        assert_eq!(ancestors, vec![thread.root.clone()], "root first");
        assert_eq!(
            descendants,
            vec![thread.deep.clone()],
            "and the rest of the chain below"
        );
        assert!(
            parent_visible(&body, &thread.deep),
            "the whole chain is visible to its author"
        );
    }

    {
        let response = context(&app, author, &thread.deep).await;
        common::assert_answer(&response, StatusCode::OK, "the author at the leaf");
        let body = response.json_value();
        let (ancestors, descendants) = context_arms(&body, "the author at the leaf");
        assert_eq!(
            ancestors,
            vec![thread.root.clone(), thread.reply.clone()],
            "the chain above a reply-of-reply arrives oldest first"
        );
        assert!(descendants.is_empty(), "nothing answered the leaf");
    }
}

/// The accepted follower — invisible to neither the root nor the private
/// reply — reads both, and loses the `direct` leg: the reply-of-reply is
/// the shared 404 named directly, and drops out below the private reply
/// without breaking anything above it.
#[tokio::test]
async fn the_follower_reads_the_private_leg_and_loses_the_direct_one() {
    let db = common::test_db().await;
    let pool = db.pool().clone();
    let (app, world) = world(&pool).await;
    let thread = post_thread(&app, &world).await;

    let from_root = context(&app, Some(&world.follower.token), &thread.root).await;
    let root_body = from_root.json_value();
    let (_ancestors, descendants) = context_arms(&root_body, "the follower at the root");
    assert_eq!(
        descendants,
        vec![thread.reply.clone()],
        "the private reply is the follower's, the direct one is not"
    );
    assert!(
        parent_visible(&root_body, &thread.reply),
        "whose parent — the named root — came back"
    );

    let reply_read = context(&app, Some(&world.follower.token), &thread.reply).await;
    let reply_body = reply_read.json_value();
    let (reply_ancestors, reply_descendants) =
        context_arms(&reply_body, "the follower at the reply");
    assert_eq!(reply_ancestors, vec![thread.root.clone()], "up to the root");
    assert!(
        reply_descendants.is_empty(),
        "the direct leg below drops for them"
    );

    let hidden = context(&app, Some(&world.follower.token), &thread.deep).await;
    let absent = context(&app, Some(&world.follower.token), UNKNOWN_ID).await;
    assert_eq!(
        hidden.body, absent.body,
        "one body for the hidden leg and a never-stored id"
    );
    common::assert_all_refused(
        &[hidden, absent],
        StatusCode::NOT_FOUND,
        NOT_FOUND,
        "the follower's direct-leg probe",
    );
}

/// The addressed reader of the `direct` reply-of-reply: the `private` link
/// between them and the root is invisible — but the walk passes *through*
/// it. From the root their descendants list carries the direct leg alone
/// with `parent_visible` false, because its own direct parent was dropped;
/// from the leg the ancestors list still reaches the root past the hole.
#[tokio::test]
async fn the_addressed_reader_walks_past_the_hidden_link() {
    let db = common::test_db().await;
    let pool = db.pool().clone();
    let (app, world) = world(&pool).await;
    let thread = post_thread(&app, &world).await;

    let from_root = context(&app, Some(&world.mentioned.token), &thread.root).await;
    let root_body = from_root.json_value();
    let (_ancestors, descendants) = context_arms(&root_body, "the addressed reader at the root");
    assert_eq!(
        descendants,
        vec![thread.deep.clone()],
        "the private reply drops and the direct leg below it stays"
    );
    assert!(
        !parent_visible(&root_body, &thread.deep),
        "its direct parent was dropped from this answer"
    );

    let deep_read = context(&app, Some(&world.mentioned.token), &thread.deep).await;
    let deep_body = deep_read.json_value();
    let (deep_ancestors, deep_descendants) =
        context_arms(&deep_body, "the addressed reader at the leaf");
    assert_eq!(
        deep_ancestors,
        vec![thread.root.clone()],
        "the walk passes through the hidden link to the root"
    );
    assert!(deep_descendants.is_empty(), "nothing answered the leaf");

    let hidden = context(&app, Some(&world.mentioned.token), &thread.reply).await;
    let absent = context(&app, Some(&world.mentioned.token), UNKNOWN_ID).await;
    assert_eq!(
        hidden.body, absent.body,
        "the hidden middle is the absent id for this reader"
    );
    common::assert_all_refused(
        &[hidden, absent],
        StatusCode::NOT_FOUND,
        NOT_FOUND,
        "the addressed reader's private-leg probe",
    );
}

/// A viewer the thread's private and direct legs are closed to — the
/// stranger, the still-pending requester, the account the author muted
/// (a mute is no visibility rule, but grants nothing either), and the
/// anonymous caller alike — reads the public root's context as two empty
/// arms, and every other leg as the shared 404, byte-identical to a
/// never-stored id.
#[tokio::test]
async fn closed_readers_of_the_thread_reach_only_the_root_arms() {
    let db = common::test_db().await;
    let pool = db.pool().clone();
    let (app, world) = world(&pool).await;
    let thread = post_thread(&app, &world).await;

    for (who, token) in [
        ("the stranger", Some(world.stranger.token.as_str())),
        ("the pending requester", Some(world.pending.token.as_str())),
        ("the muter", Some(world.muter.token.as_str())),
        ("anonymous", None),
    ] {
        let from_root = context(&app, token, &thread.root).await;
        common::assert_answer(&from_root, StatusCode::OK, &format!("{who} at the root"));
        let body = from_root.json_value();
        let (ancestors, descendants) = context_arms(&body, who);
        assert!(ancestors.is_empty(), "{who}: the root stands alone");
        assert!(
            descendants.is_empty(),
            "{who}: the private and direct legs never arrive"
        );
        let mut probes = Vec::new();
        for leg in [&thread.reply, &thread.deep, &thread.dm] {
            probes.push(context(&app, token, leg).await);
        }
        probes.push(context(&app, token, UNKNOWN_ID).await);
        let absent = probes
            .pop()
            .expect("the probe list carries the never-stored id");
        for probe in &probes {
            assert_eq!(
                probe.body, absent.body,
                "{who}: one body for hidden legs and absent ids"
            );
        }
        probes.push(absent);
        common::assert_all_refused(
            &probes,
            StatusCode::NOT_FOUND,
            NOT_FOUND,
            &format!("{who} probing the closed legs"),
        );
    }
}

/// The conversation continuation: the `direct` post naming the author's
/// own participation id arrives threaded into the thread (`pleroma
/// .direct_conversation_id`, every participant's own side id) yet answers
/// *no status*, so its context is two empty arms — it is a new root, not a
/// reply — and its listing moved to it for both the author and the
/// addressee, who reads their own distinct side id on it.
#[tokio::test]
async fn a_conversation_post_continues_the_thread_without_replying() {
    let db = common::test_db().await;
    let pool = db.pool().clone();
    let (app, world) = world(&pool).await;
    let thread = post_thread(&app, &world).await;

    let shown = get(
        &app,
        Some(&world.author.token),
        &format!("{STATUSES}/{}", thread.dm),
    )
    .await;
    common::assert_answer(
        &shown,
        StatusCode::OK,
        "the author reads their conversation post",
    );
    let entity = shown.json_value();
    assert_eq!(
        entity["visibility"],
        json!("direct"),
        "the conversation arm forces the private scope"
    );
    assert_eq!(
        entity["in_reply_to_id"],
        Value::Null,
        "it answers the thread, not a status"
    );
    assert_eq!(
        entity["pleroma"]["direct_conversation_id"],
        json!(&thread.side),
        "the author's read renders the author's side id"
    );
    assert_eq!(
        entity["mentions"][0]["acct"],
        json!("mentioned"),
        "the participation's recipients ride the mentions"
    );

    // The thread's listing moved to the continuation for both sides — and
    // each side names the thread with its OWN participation id.
    let own_row = own_participation(&app, &world.author.token, &thread.dm).await;
    assert_eq!(own_row["id"], json!(&thread.side), "the side keeps its id");
    let their_row = own_participation(&app, &world.mentioned.token, &thread.dm).await;
    assert_ne!(
        their_row["id"],
        json!(&thread.side),
        "the addressee's side of the same thread is a different id"
    );

    // Participants read it as a root of its own: neither walk reaches it.
    for (who, token) in [
        ("the author", Some(world.author.token.as_str())),
        ("the addressee", Some(world.mentioned.token.as_str())),
    ] {
        let arms = context(&app, token, &thread.dm).await.json_value();
        let (ancestors, descendants) = context_arms(&arms, who);
        assert!(
            ancestors.is_empty(),
            "{who}: a conversation continuation hangs from no status"
        );
        assert!(descendants.is_empty(), "{who}: nothing answered it");
    }

    let mut closed = Vec::new();
    for token in [
        Some(world.follower.token.as_str()),
        Some(world.stranger.token.as_str()),
        None,
    ] {
        closed.push(context(&app, token, &thread.dm).await);
    }
    common::assert_all_refused(
        &closed,
        StatusCode::NOT_FOUND,
        NOT_FOUND,
        "the conversation post read by non-participants",
    );
}

/// Rule 2 holds on the context route too: the account the author blocked
/// reads every leg of the thread — the public root included — as the same
/// shared 404, byte-identical to a never-stored id.
#[tokio::test]
async fn the_blocked_viewer_reads_no_context_of_the_thread() {
    let db = common::test_db().await;
    let pool = db.pool().clone();
    let (app, world) = world(&pool).await;
    let thread = post_thread(&app, &world).await;

    let mut probes = Vec::new();
    for leg in [&thread.root, &thread.reply, &thread.deep, &thread.dm] {
        probes.push(context(&app, Some(world.blocker.token.as_str()), leg).await);
    }
    let absent = context(&app, Some(world.blocker.token.as_str()), UNKNOWN_ID).await;
    for probe in &probes {
        assert_eq!(
            probe.body, absent.body,
            "the thread's own legs answer like a never-stored id"
        );
    }
    probes.push(absent);
    common::assert_all_refused(
        &probes,
        StatusCode::NOT_FOUND,
        NOT_FOUND,
        "the blocked reader of every thread leg",
    );
}

/// The sweep's unfiltered page: the author reads all eight rows; the
/// accepted follower adds nothing over `public` but the `private` reply;
/// the stranger, the muter and the anonymous caller reach exactly the
/// five open rows; the blocked account reads the byte-identical empty
/// page of a never-posting account (rules 3-6, again, by id set).
#[tokio::test]
async fn the_unfiltered_page_answers_every_viewer_exactly_what_visibility_allows() {
    let db = common::test_db().await;
    let pool = db.pool().clone();
    let dir = uploads_dir();
    let (app, world, show) = exhibit(&pool, dir.path()).await;
    let thread = &show.thread;

    let author = newest_first(&[
        &thread.root,
        &thread.reply,
        &thread.deep,
        &thread.dm,
        &show.media,
        &show.tagpost,
        &show.extra,
        &show.reblog,
    ]);
    let follower = newest_first(&[
        &thread.root,
        &thread.reply,
        &show.media,
        &show.tagpost,
        &show.extra,
        &show.reblog,
    ]);
    let open = newest_first(&[
        &thread.root,
        &show.media,
        &show.tagpost,
        &show.extra,
        &show.reblog,
    ]);
    sweep_page(
        &app,
        &world,
        "",
        [author, follower, open.clone(), vec![], open.clone(), open],
    )
    .await;
}

/// `pinned=true` answers the account's one pin — and ignores the
/// pagination keys it was sent: a `limit` of one and a `max_id` older than
/// the pinned status would page it away on the ordinary route. No `Link`
/// header rides a pinned answer, for any viewer; the blocked reader still
/// gets the shared empty page.
#[tokio::test]
async fn pinned_answers_the_one_pin_ignoring_the_pagination_keys() {
    let db = common::test_db().await;
    let pool = db.pool().clone();
    let dir = uploads_dir();
    let (app, world, show) = exhibit(&pool, dir.path()).await;

    let query = format!("?pinned=true&limit=1&max_id={}", show.thread.root);
    let author = Some(world.author.token.as_str());
    let page = get(
        &app,
        author,
        &format!("{ACCOUNTS}/{}/statuses{query}", world.author.user.id),
    )
    .await;
    common::assert_answer(&page, StatusCode::OK, "the author's pinned page");
    assert_eq!(
        page_ids(&page),
        vec![show.extra.clone()],
        "the key that pages the ordinary route away is ignored"
    );
    assert_eq!(
        page.header("link"),
        None,
        "a pinned answer sends no Link header"
    );
    let one = vec![show.extra.clone()];
    sweep_page(
        &app,
        &world,
        &query,
        [
            one.clone(),
            one.clone(),
            one.clone(),
            vec![],
            one.clone(),
            one,
        ],
    )
    .await;
}

/// `only_media=true` keeps exactly the one status carrying an attachment —
/// the warning post — for every viewer that reaches it at all.
#[tokio::test]
async fn only_media_keeps_the_attached_post_alone() {
    let db = common::test_db().await;
    let pool = db.pool().clone();
    let dir = uploads_dir();
    let (app, world, show) = exhibit(&pool, dir.path()).await;

    let one = vec![show.media.clone()];
    sweep_page(
        &app,
        &world,
        "?only_media=true",
        [
            one.clone(),
            one.clone(),
            one.clone(),
            vec![],
            one.clone(),
            one,
        ],
    )
    .await;
}

/// `exclude_replies=true` drops the whole reply chain — the private reply
/// and the direct reply-of-reply — while the conversation continuation
/// stays: it threads by conversation id and answers no status, so it is no
/// reply. The boost, the media post, the tag post, the pin and the root
/// are all roots.
#[tokio::test]
async fn exclude_replies_drops_the_chain_but_not_the_conversation_leg() {
    let db = common::test_db().await;
    let pool = db.pool().clone();
    let dir = uploads_dir();
    let (app, world, show) = exhibit(&pool, dir.path()).await;
    let thread = &show.thread;

    let author = newest_first(&[
        &thread.root,
        &thread.dm,
        &show.media,
        &show.tagpost,
        &show.extra,
        &show.reblog,
    ]);
    let open = newest_first(&[
        &thread.root,
        &show.media,
        &show.tagpost,
        &show.extra,
        &show.reblog,
    ]);
    sweep_page(
        &app,
        &world,
        "?exclude_replies=true",
        [
            author,
            open.clone(),
            open.clone(),
            vec![],
            open.clone(),
            open,
        ],
    )
    .await;
}

/// `exclude_reblogs=true` takes the author's boost out and nothing else:
/// the page loses exactly the reblog row's id, original untouched.
#[tokio::test]
async fn exclude_reblogs_drops_the_boost() {
    let db = common::test_db().await;
    let pool = db.pool().clone();
    let dir = uploads_dir();
    let (app, world, show) = exhibit(&pool, dir.path()).await;
    let thread = &show.thread;

    let author = newest_first(&[
        &thread.root,
        &thread.reply,
        &thread.deep,
        &thread.dm,
        &show.media,
        &show.tagpost,
        &show.extra,
    ]);
    let follower = newest_first(&[
        &thread.root,
        &thread.reply,
        &show.media,
        &show.tagpost,
        &show.extra,
    ]);
    let open = newest_first(&[&thread.root, &show.media, &show.tagpost, &show.extra]);
    sweep_page(
        &app,
        &world,
        "?exclude_reblogs=true",
        [author, follower, open.clone(), vec![], open.clone(), open],
    )
    .await;
}

/// `with_muted=true` brings back exactly the rows the *viewer's own* mute
/// of the author silenced: the muter's page empties first, then fills
/// again — the author's own read of their page never needed it, and the
/// blocked reader's blindness is not the viewer's silence to lift.
#[tokio::test]
async fn with_muted_returns_the_viewers_own_exclusions() {
    let db = common::test_db().await;
    let pool = db.pool().clone();
    let dir = uploads_dir();
    let (app, world, show) = exhibit(&pool, dir.path()).await;
    let thread = &show.thread;

    let author_full = newest_first(&[
        &thread.root,
        &thread.reply,
        &thread.deep,
        &thread.dm,
        &show.media,
        &show.tagpost,
        &show.extra,
        &show.reblog,
    ]);
    let follower = newest_first(&[
        &thread.root,
        &thread.reply,
        &show.media,
        &show.tagpost,
        &show.extra,
        &show.reblog,
    ]);
    let open = newest_first(&[
        &thread.root,
        &show.media,
        &show.tagpost,
        &show.extra,
        &show.reblog,
    ]);

    // The muter mutes the author: the author's whole page goes silent.
    let account = world.author.user.id.to_string();
    let wrote = relate(&app, &world.muter.token, &account, "mute").await;
    assert_eq!(
        wrote["muting"],
        json!(true),
        "the muter mutes the author in turn"
    );
    sweep_page(
        &app,
        &world,
        "",
        [
            author_full.clone(),
            follower.clone(),
            open.clone(),
            vec![],
            vec![],
            open.clone(),
        ],
    )
    .await;

    // `with_muted=true` restates exactly what the mute silenced.
    sweep_page(
        &app,
        &world,
        "?with_muted=true",
        [
            author_full,
            follower,
            open.clone(),
            vec![],
            open.clone(),
            open,
        ],
    )
    .await;
}

/// `tagged=E2ETag` keeps the one hashtag post for every viewer that
/// reaches it; the folded name answers either spelling, and the blockee's
/// page stays the shared empty one.
#[tokio::test]
async fn tagged_keeps_the_hashtagged_post() {
    let db = common::test_db().await;
    let pool = db.pool().clone();
    let dir = uploads_dir();
    let (app, world, show) = exhibit(&pool, dir.path()).await;

    let one = vec![show.tagpost.clone()];
    sweep_page(
        &app,
        &world,
        "?tagged=E2ETag",
        [
            one.clone(),
            one.clone(),
            one.clone(),
            vec![],
            one.clone(),
            one,
        ],
    )
    .await;
}

/// `exclude_visibilities[]`: naming `private` takes the follower-visible
/// reply out of the author's and the follower's pages alike; naming
/// `direct` takes both DM legs out of the author's page and changes
/// nothing for readers who never reached them.
#[tokio::test]
async fn exclude_visibilities_drops_named_scopes() {
    let db = common::test_db().await;
    let pool = db.pool().clone();
    let dir = uploads_dir();
    let (app, world, show) = exhibit(&pool, dir.path()).await;
    let thread = &show.thread;

    let open = newest_first(&[
        &thread.root,
        &show.media,
        &show.tagpost,
        &show.extra,
        &show.reblog,
    ]);

    let no_private = newest_first(&[
        &thread.root,
        &thread.deep,
        &thread.dm,
        &show.media,
        &show.tagpost,
        &show.extra,
        &show.reblog,
    ]);
    sweep_page(
        &app,
        &world,
        "?exclude_visibilities[]=private",
        [
            no_private,
            open.clone(),
            open.clone(),
            vec![],
            open.clone(),
            open.clone(),
        ],
    )
    .await;

    let no_direct = newest_first(&[
        &thread.root,
        &thread.reply,
        &show.media,
        &show.tagpost,
        &show.extra,
        &show.reblog,
    ]);
    sweep_page(
        &app,
        &world,
        "?exclude_visibilities[]=direct",
        [
            no_direct.clone(),
            no_direct,
            open.clone(),
            vec![],
            open.clone(),
            open,
        ],
    )
    .await;
}

// ---------------------------------------------------------------------------
// Part 3: every posted kind across all five timelines of
// `src/api/timelines.rs`, per viewer, by id set.
// ---------------------------------------------------------------------------

/// The home timeline the author, follower, muter and blocked viewer each
/// read as their own following page.
const HOME: &str = "/api/v1/timelines/home";
/// The instance-wide public page, anonymous or tokened.
const PUBLIC: &str = "/api/v1/timelines/public";
/// The hashtag route; each test appends `/{tag}`.
const TAG: &str = "/api/v1/timelines/tag";
/// The viewer's own direct conversations.
const DIRECT: &str = "/api/v1/timelines/direct";
/// The list route; each test appends `/{list_id}`.
const LIST: &str = "/api/v1/timelines/list";
/// The list CRUD and membership writes the list scenario rides.
const LISTS: &str = "/api/v1/lists";
/// The follow-request queue and its `authorize` answer, which grants the
/// muter's request the locked author's account has queued.
const FOLLOW_REQUESTS: &str = "/api/v1/follow_requests";
/// The exact 403 body a `RESTRICT_UNAUTHENTICATED` gate answers with.
const RESTRICTED: &str = "Authentication is required to access this resource.";
/// The 401 body the extractor answers a viewer-less required route with.
const NO_TOKEN: &str = "Invalid credentials";
/// The 403 body a user token without `read:statuses` is refused with.
const NO_SCOPE: &str = "Insufficient permissions: read:statuses.";

/// Every kind the timeline matrix posts, all authored by the author: the
/// ten stand-alone kind posts of part 1's vocabulary plus part 2's
/// [`Thread`], fourteen ids the five timeline routes are swept by. The
/// [`Kinds::all`] set is what the author's home page and their own list's
/// page must answer in full; every other viewer's page is a subset written
/// out in the test that pins it.
#[derive(Debug)]
struct Kinds {
    /// The plain `public` post.
    public: String,
    /// The `unlisted` post — readable, never on the public page.
    unlisted: String,
    /// The `private` post, the author's and the follower's only.
    private: String,
    /// The `direct` post @-addressed to `mentioned`.
    direct: String,
    /// The `direct` post @-addressed to `follower`, so the follower's home
    /// and direct pages hold the DM that names them and not the one that
    /// does not.
    follower_dm: String,
    /// The media post: an upload claimed through `media_ids`.
    media: String,
    /// The content-warning post (`spoiler_text` + `sensitive`).
    warning: String,
    /// The poll post.
    poll: String,
    /// The `#E2ETag` hashtag post — the tag route's only row.
    tagpost: String,
    /// The `local` post @-mentioning `follower` — every local token's, no
    /// anonymous caller's.
    local: String,
    /// Part 2's reply chain: the public root, the private reply, the direct
    /// reply-of-reply and the direct conversation continuation.
    thread: Thread,
}

impl Kinds {
    /// Every matrix id, newest first: the page the author — the only viewer
    /// every one of these rows is open to — reads on their home timeline
    /// and on their own list's page.
    fn all(&self) -> Vec<String> {
        newest_first(&[
            &self.public,
            &self.unlisted,
            &self.private,
            &self.direct,
            &self.follower_dm,
            &self.media,
            &self.warning,
            &self.poll,
            &self.tagpost,
            &self.local,
            &self.thread.root,
            &self.thread.reply,
            &self.thread.deep,
            &self.thread.dm,
        ])
    }
}

/// One plain-text kind: `POST /api/v1/statuses` as the author with `text`
/// stored at `visibility`, answered by its `id` alone.
async fn plain_kind(app: &Router, world: &World, visibility: &str, text: &str) -> String {
    entity_id(
        &posted(
            app,
            &world.author.token,
            json!({"status": text, "visibility": visibility}),
        )
        .await,
    )
}

/// The [`posted`] create answered by its `id` alone.
async fn posted_id(app: &Router, token: &str, body: Value) -> String {
    entity_id(&posted(app, token, body).await)
}

/// Post every timeline kind as the author through the client API: part 2's
/// [`Thread`] first, then the six text kinds by stored visibility, the
/// media, warning, poll and hashtag posts, and the `local` mention — the
/// whole [`Kinds`] exhibit on one world.
async fn post_kinds(app: &Router, world: &World) -> Kinds {
    let thread = post_thread(app, world).await;
    let public = plain_kind(app, world, "public", "a public timeline word").await;
    let unlisted = plain_kind(app, world, "unlisted", "an unlisted timeline word").await;
    let private = plain_kind(app, world, "private", "a private timeline word").await;
    let direct = plain_kind(
        app,
        world,
        "direct",
        "@mentioned, a direct across the timelines",
    )
    .await;
    let follower_dm = plain_kind(app, world, "direct", "@follower, a direct of your own").await;
    let local = plain_kind(app, world, "local", "@follower, kept on the instance").await;
    let attachment = uploaded(app, &world.author.token).await;
    let media = posted_id(
        app,
        &world.author.token,
        json!({"status": "a timeline tune", "media_ids": [entity_id(&attachment)]}),
    )
    .await;
    let warned = json!({"status": "a warned timeline word", "spoiler_text": "twist inside", "sensitive": true});
    let warning = posted_id(app, &world.author.token, warned).await;
    let ballot = posted_form(
        app,
        &world.author.token,
        &[
            ("status", "soup or salad on the timelines?"),
            ("poll[options][]", "soup"),
            ("poll[options][]", "salad"),
            ("poll[expires_in]", "3600"),
        ],
    )
    .await;
    let poll = entity_id(&ballot);
    let tagpost = posted_id(
        app,
        &world.author.token,
        json!({"status": "tagged #E2ETag for the timelines"}),
    )
    .await;
    Kinds {
        public,
        unlisted,
        private,
        direct,
        follower_dm,
        media,
        warning,
        poll,
        tagpost,
        local,
        thread,
    }
}

/// The timeline matrix: the uploads-backed [`world_with_uploads`] with
/// [`post_kinds`]'s full kind set posted onto it.
async fn timelines_matrix(pool: &PgPool, dir: &Path) -> (Router, World, Kinds) {
    let (app, world) = world_with_uploads(pool, dir).await;
    let shown = post_kinds(&app, &world).await;
    (app, world, shown)
}

/// A timeline `GET` as `token` (`None`: the anonymous caller) that must
/// answer 200 with exactly `expected`, in order, as the page's ids — a
/// wrong viewer's page simply lacks the id, never half-renders it.
async fn page_shows(app: &Router, token: Option<&str>, path: &str, expected: &[String], who: &str) {
    let response = get(app, token, path).await;
    common::assert_answer(&response, StatusCode::OK, &format!("{who} reads {path}"));
    assert_eq!(
        page_ids(&response),
        expected,
        "{who}: the membership of {path}"
    );
}

/// The author grants `requester`'s waiting follow: their row is read out of
/// `GET /api/v1/follow_requests` and answered through
/// `POST /api/v1/follow_requests/{id}/authorize`, which lands the accepted
/// follow the timeline pages will read off — the answer is the author's own
/// view of the requester, now `followed_by`.
async fn authorize_request(app: &Router, author: &Actor, requester: &Actor) {
    let queue = get(app, Some(&author.token), FOLLOW_REQUESTS).await;
    common::assert_answer(&queue, StatusCode::OK, "the follow-request queue answers");
    let body = queue.json_value();
    let rows = body.as_array().expect("the queue is an array");
    let wanted = json!(requester.user.id.to_string());
    assert!(
        rows.iter().any(|row| row["id"] == wanted),
        "the waiting request rides the queue"
    );
    let response = common::TestRequest::post(&format!(
        "{FOLLOW_REQUESTS}/{}/authorize",
        requester.user.id
    ))
    .bearer(&author.token)
    .send(app)
    .await;
    common::assert_answer(&response, StatusCode::OK, "the authorization lands");
    assert_eq!(
        response.json_value()["followed_by"],
        json!(true),
        "the requester now follows, accepted"
    );
}

/// `POST /api/v1/lists` as `token`, titled `title`, answered 200 as the
/// stored list entity with Mastodon's default `list` replies policy.
async fn created_list(app: &Router, token: &str, title: &str) -> Value {
    let response = common::TestRequest::post(LISTS)
        .bearer(token)
        .json(&json!({"title": title}))
        .send(app)
        .await;
    common::assert_answer(&response, StatusCode::OK, "the list create answers");
    response.json_value()
}

/// `POST /api/v1/lists/{list_id}/accounts` adding `account_id`, asserted 200
/// with the empty-object answer.
async fn members_added(app: &Router, token: &str, list_id: &str, account_id: &str) {
    let response = common::TestRequest::post(&format!("{LISTS}/{list_id}/accounts"))
        .bearer(token)
        .json(&json!({"account_ids": [account_id]}))
        .send(app)
        .await;
    common::assert_answer(&response, StatusCode::OK, "the membership write lands");
    assert_eq!(
        response.json_value(),
        json!({}),
        "the add answers the empty object"
    );
}

/// Home, first row: the author's page is their own whole posting — the
/// `private` post, the `direct`s they authored and the conversation
/// continuation all ride it, because the viewer is every row's author —
/// while the follower's page adds the followee's `private` post and the DM
/// that @-names them, and loses the `direct`s that do not; the account the
/// author blocked reads no row of theirs at all.
#[tokio::test]
async fn the_home_timeline_answers_author_follower_and_the_blind_blocker() {
    let db = common::test_db().await;
    let pool = db.pool().clone();
    let dir = uploads_dir();
    let (app, world, kinds) = timelines_matrix(&pool, dir.path()).await;
    let thread = &kinds.thread;

    page_shows(
        &app,
        Some(&world.author.token),
        HOME,
        &kinds.all(),
        "the author",
    )
    .await;

    let for_follower = newest_first(&[
        &kinds.public,
        &kinds.unlisted,
        &kinds.private,
        &kinds.follower_dm,
        &kinds.media,
        &kinds.warning,
        &kinds.poll,
        &kinds.tagpost,
        &kinds.local,
        &thread.root,
        &thread.reply,
    ]);
    page_shows(
        &app,
        Some(&world.follower.token),
        HOME,
        &for_follower,
        "the follower",
    )
    .await;

    page_shows(
        &app,
        Some(&world.blocker.token),
        HOME,
        &[],
        "the blocked viewer",
    )
    .await;
}

/// Home, second row: the muter's own view. Their follow of the locked
/// author only waits until the author grants it; their mute of the author
/// then silences every one of the author's rows off their home page, and
/// `with_muted=true` restates exactly the audience their account reaches —
/// the granted follow's whole view, `private` included, and only the
/// `direct`s (which they are addressed in nowhere) stay out: those were
/// never the mute's to lift.
#[tokio::test]
async fn the_home_timeline_goes_quiet_for_the_muter_and_returns_with_with_muted() {
    let db = common::test_db().await;
    let pool = db.pool().clone();
    let dir = uploads_dir();
    let (app, world, kinds) = timelines_matrix(&pool, dir.path()).await;
    let thread = &kinds.thread;

    let author_id = world.author.user.id.to_string();
    let asked = relate(&app, &world.muter.token, &author_id, "follow").await;
    assert_eq!(
        asked["requested"],
        json!(true),
        "the lock files the muter's follow as a request"
    );
    authorize_request(&app, &world.author, &world.muter).await;
    let muting = relate(&app, &world.muter.token, &author_id, "mute").await;
    assert_eq!(muting["muting"], json!(true), "the muter mutes the author");

    page_shows(&app, Some(&world.muter.token), HOME, &[], "the muter").await;

    let restored = newest_first(&[
        &kinds.public,
        &kinds.unlisted,
        &kinds.private,
        &kinds.media,
        &kinds.warning,
        &kinds.poll,
        &kinds.tagpost,
        &kinds.local,
        &thread.root,
        &thread.reply,
    ]);
    page_shows(
        &app,
        Some(&world.muter.token),
        &format!("{HOME}?with_muted=true"),
        &restored,
        "the muter, reading through their own mute",
    )
    .await;
}

/// The public page keeps only its public rows: the anonymous caller reads
/// the open posts and the thread's public root — never the `unlisted`
/// post, the `private` one, either `direct`, or the `local` mention — every
/// token this instance owns additionally reads the `local` post, the
/// blocked viewer reads none of the author's, and `only_media=true` keeps
/// exactly the post carrying the upload.
#[tokio::test]
async fn the_public_timeline_narrows_the_kinds_to_public_rows_plus_local_for_tokens() {
    let db = common::test_db().await;
    let pool = db.pool().clone();
    let dir = uploads_dir();
    let (app, world, kinds) = timelines_matrix(&pool, dir.path()).await;

    let open = newest_first(&[
        &kinds.public,
        &kinds.media,
        &kinds.warning,
        &kinds.poll,
        &kinds.tagpost,
        &kinds.thread.root,
    ]);
    page_shows(&app, None, PUBLIC, &open, "the anonymous caller").await;

    let for_token = newest_first(&[
        &kinds.public,
        &kinds.media,
        &kinds.warning,
        &kinds.poll,
        &kinds.tagpost,
        &kinds.local,
        &kinds.thread.root,
    ]);
    page_shows(
        &app,
        Some(&world.follower.token),
        PUBLIC,
        &for_token,
        "a follower's token",
    )
    .await;
    page_shows(
        &app,
        Some(&world.stranger.token),
        PUBLIC,
        &for_token,
        "a stranger's token",
    )
    .await;
    page_shows(
        &app,
        Some(&world.blocker.token),
        PUBLIC,
        &[],
        "the blocked viewer",
    )
    .await;

    let attached = vec![kinds.media.clone()];
    page_shows(
        &app,
        None,
        &format!("{PUBLIC}?only_media=true"),
        &attached,
        "the anonymous media cut",
    )
    .await;
    page_shows(
        &app,
        Some(&world.follower.token),
        &format!("{PUBLIC}?only_media=true"),
        &attached,
        "a token's media cut",
    )
    .await;
}

/// The documented gate: while `RESTRICT_UNAUTHENTICATED` lists
/// `timelines_federated` and `timelines_local`, every anonymous public
/// read — the plain page and the `local=true` one — is the 403
/// [`RESTRICTED`], while a token still reads the same membership the open
/// instance answered it with.
#[tokio::test]
async fn the_restriction_gate_closes_only_the_anonymous_public_reads() {
    let db = common::test_db().await;
    let pool = db.pool().clone();
    let dir = uploads_dir();
    let (_app, world, kinds) = timelines_matrix(&pool, dir.path()).await;

    let gated = common::test_app_with_restricted_areas(
        pool.clone(),
        &["timelines_federated", "timelines_local"],
    );
    for query in ["", "?local=true"] {
        let refused = get(&gated, None, &format!("{PUBLIC}{query}")).await;
        common::assert_answer(
            &refused,
            StatusCode::FORBIDDEN,
            &format!("the gated anonymous `{query}` read"),
        );
        common::assert_error(&refused, RESTRICTED, &format!("the `{query}` refusal"));
    }

    let for_token = newest_first(&[
        &kinds.public,
        &kinds.media,
        &kinds.warning,
        &kinds.poll,
        &kinds.tagpost,
        &kinds.local,
        &kinds.thread.root,
    ]);
    page_shows(
        &gated,
        Some(&world.follower.token),
        PUBLIC,
        &for_token,
        "a token on the gated instance",
    )
    .await;
}

/// The tag route: the one `#E2ETag` post is the whole page for the author,
/// the follower, a stranger and the anonymous caller alike — the stored
/// lowercase fold answers the path's `E2ETag` spelling, and no other kind
/// of the matrix rides it.
#[tokio::test]
async fn the_tag_timeline_answers_the_one_tagged_post_to_every_named_viewer() {
    let db = common::test_db().await;
    let pool = db.pool().clone();
    let dir = uploads_dir();
    let (app, world, kinds) = timelines_matrix(&pool, dir.path()).await;

    let one = vec![kinds.tagpost.clone()];
    for (who, token) in [
        ("the author", Some(world.author.token.as_str())),
        ("the follower", Some(world.follower.token.as_str())),
        ("the stranger", Some(world.stranger.token.as_str())),
        ("the anonymous caller", None),
    ] {
        page_shows(&app, token, &format!("{TAG}/E2ETag"), &one, who).await;
    }
}

/// The direct page is held to its participants: the author reads all four
/// `direct`s they authored, the @-addressed account reads the three naming
/// them (the reply-of-reply and the conversation continuation included),
/// the follower named by the other DM reads exactly that one and none of
/// the `mentioned`'s, and a stranger — no participant of anything — reads
/// the empty page.
#[tokio::test]
async fn the_direct_timeline_stays_inside_its_participants() {
    let db = common::test_db().await;
    let pool = db.pool().clone();
    let dir = uploads_dir();
    let (app, world, kinds) = timelines_matrix(&pool, dir.path()).await;
    let thread = &kinds.thread;

    let authored = newest_first(&[&kinds.direct, &kinds.follower_dm, &thread.deep, &thread.dm]);
    page_shows(
        &app,
        Some(&world.author.token),
        DIRECT,
        &authored,
        "the author",
    )
    .await;

    let addressed = newest_first(&[&kinds.direct, &thread.deep, &thread.dm]);
    page_shows(
        &app,
        Some(&world.mentioned.token),
        DIRECT,
        &addressed,
        "the addressed participant",
    )
    .await;

    let theirs = vec![kinds.follower_dm.clone()];
    page_shows(
        &app,
        Some(&world.follower.token),
        DIRECT,
        &theirs,
        "the follower they addressed",
    )
    .await;

    page_shows(
        &app,
        Some(&world.stranger.token),
        DIRECT,
        &[],
        "the non-participant stranger",
    )
    .await;
}

/// The direct route demands a user token granting `read:statuses`: no
/// token is the 401 [`NO_TOKEN`], a `read:accounts` token the 403
/// [`NO_SCOPE`], and a granted one reads its page.
#[tokio::test]
async fn the_direct_timeline_refuses_the_tokenless_and_the_scopeless() {
    let db = common::test_db().await;
    let pool = db.pool().clone();
    let dir = uploads_dir();
    let (app, world, kinds) = timelines_matrix(&pool, dir.path()).await;

    let anonymous = get(&app, None, DIRECT).await;
    common::assert_answer(&anonymous, StatusCode::UNAUTHORIZED, "no token");
    common::assert_error(&anonymous, NO_TOKEN, "no token");

    let noscope = common::create_app_and_token(&pool, &world.stranger.user, "read:accounts").await;
    let unscoped = get(&app, Some(&noscope), DIRECT).await;
    common::assert_answer(
        &unscoped,
        StatusCode::FORBIDDEN,
        "a token without read:statuses",
    );
    common::assert_error(&unscoped, NO_SCOPE, "a token without read:statuses");

    let authored = newest_first(&[
        &kinds.direct,
        &kinds.follower_dm,
        &kinds.thread.deep,
        &kinds.thread.dm,
    ]);
    page_shows(
        &app,
        Some(&world.author.token),
        DIRECT,
        &authored,
        "the author's granted token",
    )
    .await;
}

/// The list page: the author creates a list and adds the author — their own
/// account, listable because the owner is always member-worth of their own
/// list — and the member-watching viewer's page then holds the member's
/// whole posting under the stored `list` replies policy: every reply of the
/// chain answers the member, so none drops.
#[tokio::test]
async fn the_list_timeline_holds_the_member_authors_posting_under_the_stored_policy() {
    let db = common::test_db().await;
    let pool = db.pool().clone();
    let dir = uploads_dir();
    let (app, world, kinds) = timelines_matrix(&pool, dir.path()).await;

    let list = created_list(&app, &world.author.token, "the matrix circle").await;
    assert_eq!(
        list["replies_policy"],
        json!("list"),
        "the stored default policy"
    );
    let list_id = entity_id(&list);
    let author_id = world.author.user.id.to_string();
    members_added(&app, &world.author.token, &list_id, &author_id).await;

    page_shows(
        &app,
        Some(&world.author.token),
        &format!("{LIST}/{list_id}"),
        &kinds.all(),
        "the member-watching viewer",
    )
    .await;
}

/// A list someone else owns is not the caller's to read: the follower's
/// probe of the author's list id answers the shared 404 [`NOT_FOUND`] —
/// byte-identical to a never-minted list id and to the show route's 404, so
/// the list's existence leaks nowhere, and only the owner reads the page.
#[tokio::test]
async fn another_accounts_list_is_the_shows_shared_404() {
    let db = common::test_db().await;
    let pool = db.pool().clone();
    let dir = uploads_dir();
    let (app, world, _kinds) = timelines_matrix(&pool, dir.path()).await;

    let list = created_list(&app, &world.author.token, "someone else's circle").await;
    let route = format!("{LIST}/{}", entity_id(&list));
    page_shows(
        &app,
        Some(&world.author.token),
        &route,
        &[],
        "the owner of an empty-members list",
    )
    .await;

    let hidden = get(&app, Some(&world.follower.token), &route).await;
    let absent = get(
        &app,
        Some(&world.follower.token),
        &format!("{LIST}/{UNKNOWN_ID}"),
    )
    .await;
    let show = get(
        &app,
        Some(&world.follower.token),
        &format!("{STATUSES}/{UNKNOWN_ID}"),
    )
    .await;
    assert_eq!(
        hidden.body, absent.body,
        "one body for the foreign list and a never-minted id"
    );
    assert_eq!(
        hidden.body, show.body,
        "and the show route's 404 is that same shape"
    );
    common::assert_all_refused(
        &[hidden, absent, show],
        StatusCode::NOT_FOUND,
        NOT_FOUND,
        "the foreign list, the absent list and the absent status",
    );
}
