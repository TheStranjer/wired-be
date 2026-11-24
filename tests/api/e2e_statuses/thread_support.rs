//! Fixtures the thread and account-page children share: the posted thread
//! and exhibit worlds, the context and page readers and the per-viewer page
//! sweep.

use std::path::Path;

use axum::{Router, http::StatusCode};
use serde_json::{Value, json};
use sqlx::PgPool;

use super::support::{ACCOUNTS, STATUSES, World, get, posted, uploaded, world_with_uploads};
use crate::common;

/// The conversation listing the matrix reads a participation id back from.
pub(super) const CONVERSATIONS: &str = "/api/v1/conversations";

/// The thread kinds, posted by the author through the create route: the
/// `public` root, the `private` reply to it, the `direct` reply-of-reply
/// that @-addresses `mentioned`, the `direct` post that continues the
/// thread through `in_reply_to_conversation_id`, and that arm's id — read
/// from the API's own conversation listing, never from the database.
#[derive(Debug)]
pub(super) struct Thread {
    /// The `public` root of the chain.
    pub(super) root: String,
    /// The `private` answer to the root.
    pub(super) reply: String,
    /// The `direct` answer to `reply`, addressing `mentioned`.
    pub(super) deep: String,
    /// The `direct` conversation continuation — threaded by its
    /// conversation id, so it answers no status.
    pub(super) dm: String,
    /// The author's own participation id the `dm` names.
    pub(super) side: String,
}

/// The account sweep's exhibit: a [`Thread`] plus the kinds the filters key
/// on — the `public` quote of the thread's root, the attached warning post,
/// the `#E2ETag` post, a plain post pinned to the profile, and the author's
/// boost of `mentioned`'s own original.
#[derive(Debug)]
pub(super) struct Exhibit {
    /// The thread legs the `exclude_replies` and visibility filters ride.
    pub(super) thread: Thread,
    /// The `public` quote of [`Thread::root`] — an ordinary root row on
    /// every page that answers the root.
    pub(super) quote: String,
    /// The one status carrying a media attachment (with a warning).
    pub(super) media: String,
    /// The one status carrying `#E2ETag`.
    pub(super) tagpost: String,
    /// The plain post pinned through the pin write.
    pub(super) extra: String,
    /// The author's reblog row, id-wise.
    pub(super) reblog: String,
}

/// `POST /api/v1/statuses/{id}/pin` as `token`, asserted 200 and answered
/// as the status entity with its new `pinned` flag.
pub(super) async fn pinned(app: &Router, token: &str, id: &str) -> Value {
    let response = common::TestRequest::post(&format!("{STATUSES}/{id}/pin"))
        .bearer(token)
        .send(app)
        .await;
    common::assert_answer(&response, StatusCode::OK, "the pin write lands");
    response.json_value()
}

/// `POST /api/v1/statuses/{id}/reblog` as `token`, asserted 200 and
/// answered as the boost entity — the reblog row's own id.
pub(super) async fn reblogged(app: &Router, token: &str, id: &str) -> Value {
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
pub(super) async fn own_participation(app: &Router, token: &str, last_id: &str) -> Value {
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
pub(super) async fn post_thread(app: &Router, world: &World) -> Thread {
    let root = common::id_of(
        &posted(
            app,
            &world.author.token,
            json!({"status": "the thread root"}),
        )
        .await,
    );
    let reply = common::id_of(
        &posted(
            app,
            &world.author.token,
            json!({"status": "a private answer", "visibility": "private", "in_reply_to_id": &root}),
        )
        .await,
    );
    let deep = common::id_of(
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
    let dm = common::id_of(
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

/// Post the whole [`Exhibit`]: the [`Thread`] legs first, then the public
/// quote of the root, the media post carrying a content warning, the
/// hashtag post, the plain post pinned through the pin write, and the
/// author's boost of `mentioned`'s public original — every one through
/// the client API.
pub(super) async fn post_exhibit(app: &Router, world: &World) -> Exhibit {
    let thread = post_thread(app, world).await;
    let quote = common::id_of(
        &posted(
            app,
            &world.author.token,
            json!({"status": "the root, quoted", "quote_id": &thread.root}),
        )
        .await,
    );
    let attachment = uploaded(app, &world.author.token).await;
    let media = common::id_of(
        &posted(
            app,
            &world.author.token,
            json!({
                "status": "a warned tune",
                "media_ids": [common::id_of(&attachment)],
                "spoiler_text": "audio inside",
                "sensitive": true,
            }),
        )
        .await,
    );
    let tagpost = common::id_of(
        &posted(
            app,
            &world.author.token,
            json!({"status": "tagged #E2ETag on the sweep"}),
        )
        .await,
    );
    let extra = common::id_of(
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
    let original = common::id_of(
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
        quote,
        media,
        tagpost,
        extra,
        reblog: common::id_of(&boost),
    }
}

/// The exhibit stage: the uploads-backed world of [`world_with_uploads`]
/// with [`post_exhibit`]'s full exhibit posted onto it.
pub(super) async fn exhibit(pool: &PgPool, dir: &Path) -> (Router, World, Exhibit) {
    let (app, world) = world_with_uploads(pool, dir).await;
    let shown = post_exhibit(&app, &world).await;
    (app, world, shown)
}

/// `GET /api/v1/statuses/{id}/context` as `token` (`None`: the anonymous
/// caller).
pub(super) async fn context(app: &Router, token: Option<&str>, id: &str) -> common::TestResponse {
    get(app, token, &format!("{STATUSES}/{id}/context")).await
}

/// A context answer's shape and both arms' ids, in the order they were
/// answered: an object of exactly `ancestors` and `descendants`.
pub(super) fn context_arms(body: &Value, who: &str) -> (Vec<String>, Vec<String>) {
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
pub(super) fn arm_ids(body: &Value, arm: &str) -> Vec<String> {
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
pub(super) fn parent_visible(body: &Value, id: &str) -> bool {
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
pub(super) fn page_ids(response: &common::TestResponse) -> Vec<String> {
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
pub(super) fn newest_first(ids: &[&str]) -> Vec<String> {
    let mut sorted: Vec<String> = ids.iter().map(|id| (*id).to_owned()).collect();
    sorted.sort_by(|a, b| b.cmp(a));
    sorted
}

/// The sweep's six viewers, in one fixed order: the author, the accepted
/// follower, the unrelated stranger, the blocked account, the account the
/// author muted, and the anonymous caller.
pub(super) fn sweep_viewers(world: &World) -> [(&'static str, Option<&str>); 6] {
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
pub(super) async fn sweep_page(
    app: &Router,
    world: &World,
    query: &str,
    answers: [Vec<String>; 6],
) {
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
