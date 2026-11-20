//! Fixtures the timeline tests share: the timeline routes and refusal
//! bodies, the posted kinds and the list and request helpers.

use std::path::Path;

use axum::{Router, http::StatusCode};
use serde_json::{Value, json};
use sqlx::PgPool;

use super::support::{
    Actor, World, entity_id, get, posted, posted_form, uploaded, world_with_uploads,
};
use super::thread_support::{Thread, newest_first, page_ids, post_thread};
use crate::common;

/// The home timeline the author, follower, muter and blocked viewer each
/// read as their own following page.
pub(super) const HOME: &str = "/api/v1/timelines/home";
/// The instance-wide public page, anonymous or tokened.
pub(super) const PUBLIC: &str = "/api/v1/timelines/public";
/// The hashtag route; each test appends `/{tag}`.
pub(super) const TAG: &str = "/api/v1/timelines/tag";
/// The viewer's own direct conversations.
pub(super) const DIRECT: &str = "/api/v1/timelines/direct";
/// The list route; each test appends `/{list_id}`.
pub(super) const LIST: &str = "/api/v1/timelines/list";
/// The list CRUD and membership writes the list scenario rides.
pub(super) const LISTS: &str = "/api/v1/lists";
/// The follow-request queue and its `authorize` answer, which grants the
/// muter's request the locked author's account has queued.
pub(super) const FOLLOW_REQUESTS: &str = "/api/v1/follow_requests";
/// The exact 403 body a `RESTRICT_UNAUTHENTICATED` gate answers with.
pub(super) const RESTRICTED: &str = "Authentication is required to access this resource.";
/// The 401 body the extractor answers a viewer-less required route with.
pub(super) const NO_TOKEN: &str = "Invalid credentials";
/// The 403 body a user token without `read:statuses` is refused with.
pub(super) const NO_SCOPE: &str = "Insufficient permissions: read:statuses.";

/// Every kind the timeline matrix posts, all authored by the author: the
/// eleven stand-alone kind posts of part 1's vocabulary plus part 2's
/// [`Thread`], fifteen ids the five timeline routes are swept by. The
/// [`Kinds::all`] set is what the author's home page and their own list's
/// page must answer in full; every other viewer's page is a subset written
/// out in the test that pins it.
#[derive(Debug)]
pub(super) struct Kinds {
    /// The plain `public` post.
    pub(super) public: String,
    /// The `unlisted` post — readable, never on the public page.
    pub(super) unlisted: String,
    /// The `private` post, the author's and the follower's only.
    pub(super) private: String,
    /// The `direct` post @-addressed to `mentioned`.
    pub(super) direct: String,
    /// The `direct` post @-addressed to `follower`, so the follower's home
    /// and direct pages hold the DM that names them and not the one that
    /// does not.
    pub(super) follower_dm: String,
    /// The media post: an upload claimed through `media_ids`.
    pub(super) media: String,
    /// The content-warning post (`spoiler_text` + `sensitive`).
    pub(super) warning: String,
    /// The poll post.
    pub(super) poll: String,
    /// The `#E2ETag` hashtag post — the tag route's only row.
    pub(super) tagpost: String,
    /// The `local` post @-mentioning `follower` — every local token's, no
    /// anonymous caller's.
    pub(super) local: String,
    /// The `public` quote of the plain `public` post — the create's
    /// `quote_id` arm, riding every page that post rides.
    pub(super) quote: String,
    /// Part 2's reply chain: the public root, the private reply, the direct
    /// reply-of-reply and the direct conversation continuation.
    pub(super) thread: Thread,
}

impl Kinds {
    /// Every matrix id, newest first: the page the author — the only viewer
    /// every one of these rows is open to — reads on their home timeline
    /// and on their own list's page.
    pub(super) fn all(&self) -> Vec<String> {
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
            &self.quote,
            &self.thread.root,
            &self.thread.reply,
            &self.thread.deep,
            &self.thread.dm,
        ])
    }
}

/// One plain-text kind: `POST /api/v1/statuses` as the author with `text`
/// stored at `visibility`, answered by its `id` alone.
pub(super) async fn plain_kind(
    app: &Router,
    world: &World,
    visibility: &str,
    text: &str,
) -> String {
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
pub(super) async fn posted_id(app: &Router, token: &str, body: Value) -> String {
    entity_id(&posted(app, token, body).await)
}

/// Post every timeline kind as the author through the client API: part 2's
/// [`Thread`] first, then the six text kinds by stored visibility, the
/// media, warning, poll and hashtag posts, the `local` mention, and the
/// `public` quote of the plain `public` post through the create's
/// `quote_id` arm — the whole [`Kinds`] exhibit on one world.
pub(super) async fn post_kinds(app: &Router, world: &World) -> Kinds {
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
    let quote = posted_id(
        app,
        &world.author.token,
        json!({"status": "a timeline word, quoted", "quote_id": &public}),
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
        quote,
        thread,
    }
}

/// The timeline matrix: the uploads-backed [`world_with_uploads`] with
/// [`post_kinds`]'s full kind set posted onto it.
pub(super) async fn timelines_matrix(pool: &PgPool, dir: &Path) -> (Router, World, Kinds) {
    let (app, world) = world_with_uploads(pool, dir).await;
    let shown = post_kinds(&app, &world).await;
    (app, world, shown)
}

/// A timeline `GET` as `token` (`None`: the anonymous caller) that must
/// answer 200 with exactly `expected`, in order, as the page's ids — a
/// wrong viewer's page simply lacks the id, never half-renders it.
pub(super) async fn page_shows(
    app: &Router,
    token: Option<&str>,
    path: &str,
    expected: &[String],
    who: &str,
) {
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
pub(super) async fn authorize_request(app: &Router, author: &Actor, requester: &Actor) {
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
pub(super) async fn created_list(app: &Router, token: &str, title: &str) -> Value {
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
pub(super) async fn members_added(app: &Router, token: &str, list_id: &str, account_id: &str) {
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
