//! Shared consts and fixtures every submodule of the status end-to-end area
//! draws on: the routes, the audience tables, the actors and the world they
//! live in, the request verbs and the visibility sweep.

use std::path::Path;

use axum::{Router, body::Bytes, http::StatusCode};
use serde_json::{Value, json};
use sqlx::PgPool;
use tempfile::TempDir;
use wired_be::models::user::User;

use crate::common::{self, MultipartPart};

/// The status write and read route the whole matrix rides.
pub(super) const STATUSES: &str = "/api/v1/statuses";
/// The account route the world's follow, block and mute writes ride.
pub(super) const ACCOUNTS: &str = "/api/v1/accounts";
/// The self-lock write that turns a later follow into a request.
pub(super) const UPDATE_CREDENTIALS: &str = "/api/v1/accounts/update_credentials";
/// The upload route the media kind posts through.
pub(super) const MEDIA: &str = "/api/v1/media";
/// The scopes every role carries: `read write` covers every sub-scope of
/// the posting world (decision 9's scope tree).
pub(super) const SCOPES: &str = "read write";
/// The shared 404 body of the show-route gate.
pub(super) const NOT_FOUND: &str = "Record not found";
/// Boundary the media upload's multipart body is framed with.
pub(super) const BOUNDARY: &str = "wired-e2e-statuses-boundary";
/// An `MP3`: an `ID3v2` tag header with a zero-length tag — the upload
/// endpoint names audio from its bytes and decodes nothing.
pub(super) const MP3: &[u8] = b"ID3\x03\x00\x00\x00\x00\x00\x00";
/// An id that names no stored row, for the indistinguishability probe.
pub(super) const UNKNOWN_ID: &str = "01934567-89ab-7cde-f012-3456789abcde";

/// The audience of every open kind — `public`, `unlisted`, media, the
/// warning, the poll and the tag: every viewer but the account the author
/// blocked, the anonymous caller included (rule 3).
pub(super) const OPEN: [&str; 7] = [
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
pub(super) const FOLLOWERS: [&str; 2] = ["author", "follower"];
/// The audience of a `direct` post: the author and the accounts its
/// @-mention addressed (rule 6).
pub(super) const ADDRESSED: [&str; 2] = ["author", "mentioned"];
/// The audience of a `local` post: every authenticated viewer this
/// instance owns and nobody else — the anonymous caller loses it, an
/// accepted follower grants nothing extra (rule 4).
pub(super) const LOCAL: [&str; 6] = [
    "author",
    "follower",
    "pending",
    "stranger",
    "mentioned",
    "muter",
];
/// Every viewer the matrix asks, for a post nobody is shut out of.
pub(super) const EVERYONE: [&str; 8] = [
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
pub(super) struct Actor {
    /// The stored local account, for its id and actor URL.
    pub(super) user: User,
    /// A token granting [`SCOPES`].
    pub(super) token: String,
}

/// The fixture world: the author of every kind and the six other viewers,
/// each in the relation the visibility rules key on.
#[derive(Debug)]
pub(super) struct World {
    /// The local account that posts every kind.
    pub(super) author: Actor,
    /// A follower of the author, granted on the spot (rule 5's audience).
    pub(super) follower: Actor,
    /// A follower whose follow of the now-locked author only waits.
    pub(super) pending: Actor,
    /// An unrelated local account.
    pub(super) stranger: Actor,
    /// The account the `direct` kind @-addresses.
    pub(super) mentioned: Actor,
    /// The account the author blocked (rule 2's blind viewer).
    pub(super) blocker: Actor,
    /// The account the author muted — a mute, never a visibility rule.
    pub(super) muter: Actor,
}

impl World {
    /// Every viewer of the matrix, in one fixed order: the seven fixture
    /// accounts and the anonymous caller.
    pub(super) fn viewers(&self) -> Vec<(&'static str, Option<&str>)> {
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
pub(super) struct Shape<'a> {
    /// The status's own `id`, as the create answered it.
    pub(super) id: String,
    /// The `visibility` key the entity answers.
    pub(super) visibility: &'a str,
    /// The `content` the entity renders.
    pub(super) content: Value,
    /// The author's `acct`.
    pub(super) acct: &'a str,
}

/// A local account named `nickname` beside a bearer token granting
/// [`SCOPES`].
pub(super) async fn actor(pool: &PgPool, nickname: &str) -> Actor {
    let user = common::create_local_user(pool, nickname).await;
    let token = common::create_app_and_token(pool, &user, SCOPES).await;
    Actor { user, token }
}

/// `POST /api/v1/accounts/{target}/{action}` (`follow`, `block`, `mute`)
/// as `token`, asserted 200 and answered as the relationship entity.
pub(super) async fn relate(app: &Router, token: &str, target: &str, action: &str) -> Value {
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
pub(super) async fn lock(app: &Router, token: &str) {
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
pub(super) async fn build_world(pool: &PgPool, app: &Router) -> World {
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
pub(super) async fn world(pool: &PgPool) -> (Router, World) {
    let app = common::test_app(pool.clone());
    let world = build_world(pool, &app).await;
    (app, world)
}

/// [`build_world`]'s world over a router that stores uploads in `dir`,
/// for the media kind: the upload lands in a temp dir, never in the
/// repository.
pub(super) async fn world_with_uploads(pool: &PgPool, dir: &Path) -> (Router, World) {
    let app = common::test_app_with_uploads_dir(pool.clone(), dir);
    let world = build_world(pool, &app).await;
    (app, world)
}

/// A throwaway uploads directory, removed when the returned guard drops.
pub(super) fn uploads_dir() -> TempDir {
    TempDir::new().expect("temp dir for the media upload")
}

/// The `id` of a rendered entity, as its JSON spells it.
pub(super) fn entity_id(entity: &Value) -> String {
    entity["id"]
        .as_str()
        .expect("the entity carries its id")
        .to_owned()
}

/// `GET` `path` as `token` (`None`: the anonymous caller).
pub(super) async fn get(app: &Router, token: Option<&str>, path: &str) -> common::TestResponse {
    let request = common::TestRequest::get(path);
    match token {
        Some(found) => request.bearer(found).send(app).await,
        None => request.send(app).await,
    }
}

/// `POST /api/v1/statuses` as `token` with the JSON `body`, asserted 200
/// and answered as the entity the create wrote. The matrix's one posting
/// helper, on top of which the media kind only ever adds an upload.
pub(super) async fn posted(app: &Router, token: &str, body: Value) -> Value {
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
pub(super) async fn posted_form(app: &Router, token: &str, fields: &[(&str, &str)]) -> Value {
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
pub(super) async fn uploaded(app: &Router, token: &str) -> Value {
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
pub(super) async fn sweep(
    app: &Router,
    world: &World,
    shape: &Shape<'_>,
    allowed: &[&str],
) -> Vec<Value> {
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
