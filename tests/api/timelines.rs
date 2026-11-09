//! Status timelines over HTTP (`GET /api/v1/timelines/tag/{tag}`,
//! WBE-102, `GET /api/v1/timelines/direct`, WBE-103, and
//! `GET /api/v1/timelines/list/{list_id}`, WBE-105).
//!
//! The tag timeline answers the `public` statuses carrying the path tag —
//! URL-decoded, lowercased, `#`-stripped — newest first under a `Link`
//! header; `any[]` widens the match (with the path tag riding in the union),
//! `all[]` intersects, `none[]` subtracts, and `local`/`remote`/`only_media`/
//! `with_muted`/`exclude_visibilities[]` narrow the page. Auth is optional:
//! anonymous and app-only read the page, a user token must grant
//! `read:statuses`, a broken token is the 401. A tag that normalizes to
//! nothing is the 404, an unknown one the empty page, a malformed cursor
//! the 400.
//!
//! The direct timeline answers the viewer's own `direct` conversations —
//! sent and received, newest first, nothing of anyone else's — with auth
//! required: no token is the 401, an app-only or scope-less token the 403.
//! `with_muted` lifts the viewer's own block/mute exclusions and the `Link`
//! header works as on the tag route.
//!
//! The list timeline answers the statuses of one of the caller's lists —
//! members' posts without any follow read, replies narrowed by the list's
//! `replies_policy` (the SQL itself is pinned in
//! `src/services/timeline.rs`) — under `AuthUser` + `read:lists`: no token
//! is the 401, an app-only or scope-less token the 403, and a malformed,
//! unknown or foreign list id one and the same 404 "Record not found".
//! `only_media` and `with_muted` narrow the page and the `Link` header
//! works as on the other routes.

#![cfg(test)]

use axum::{Router, http::StatusCode};
use serde_json::Value;
use sqlx::PgPool;
use uuid::Uuid;
use wired_be::models::{
    block,
    list::{self, NewList},
    media_attachment::{self, NewMediaAttachment},
    mute::{self, MuteOptions},
    status::{self, NewStatus, Status, Visibility},
    user::User,
};

use crate::common;

/// The instance the tests run as: what the `Link` header spells its targets
/// with.
const BASE: &str = "https://test.example";
/// The tag route's path prefix; each test appends `/{tag}[?query]`.
const TAG: &str = "/api/v1/timelines/tag";
/// The direct route's path, query strings appended per case.
const DIRECT: &str = "/api/v1/timelines/direct";
/// The list route's path prefix; each test appends `/{id}[?query]`.
const LIST: &str = "/api/v1/timelines/list";

/// `GET`s `path`, with `token` when the test presents one.
async fn get(app: &Router, token: Option<&str>, path: &str) -> common::TestResponse {
    let request = common::TestRequest::get(path);
    match token {
        Some(token) => request.bearer(token).send(app).await,
        None => request.send(app).await,
    }
}

/// The ids of the statuses of a rendered page, in answer order.
fn ids(body: &Value) -> Vec<String> {
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
fn newest_first(posts: &[&Status]) -> Vec<String> {
    let mut ordered: Vec<Uuid> = posts.iter().map(|post| post.id).collect();
    ordered.sort_by(|a, b| b.cmp(a));
    ordered.into_iter().map(|id| id.to_string()).collect()
}

/// The one id of `post`, as its JSON carries it.
fn id_of(post: &Status) -> String {
    post.id.to_string()
}

/// Inserts `text` as `author`'s public status; `#tags` written in the text
/// are the tags it carries.
async fn tagged(pool: &PgPool, author: &User, text: &str) -> Status {
    common::create_status(pool, author, text, Visibility::Public).await
}

/// Inserts a mirrored (`local = false`) public status carrying `hashtags`
/// as the activity's tag list named them.
async fn remote_tagged(pool: &PgPool, author: &User, key: &str, hashtags: &[&str]) -> Status {
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
async fn attach_media(pool: &PgPool, author: &User, post: &Status) {
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

/// The tag timeline lists exactly the `public` statuses carrying the path
/// tag — replies and other authors included — and nothing else: no other
/// tag, no untagged post, and none of the `#foo` posts that are not
/// `public`.
#[tokio::test]
async fn tag_timeline_lists_the_public_statuses_carrying_the_tag() {
    let db = common::test_db().await;
    let pool = db.pool().clone();
    let alice = common::create_local_user(&pool, "alice").await;
    let bob = common::create_local_user(&pool, "bob").await;
    let foo_old = tagged(&pool, &alice, "#foo first word").await;
    let foo_new = tagged(&pool, &bob, "fresh #foo today").await;
    let _bar = tagged(&pool, &alice, "#bar other tag").await;
    let _plain = tagged(&pool, &alice, "untagged talk").await;
    let _unlisted = common::create_status(&pool, &alice, "#foo quiet", Visibility::Unlisted).await;
    let _instance_local =
        common::create_status(&pool, &alice, "#foo instance", Visibility::Local).await;
    let _friends = common::create_status(&pool, &alice, "#foo circle", Visibility::Private).await;
    let app = common::test_app(pool);

    let page = get(&app, None, &format!("{TAG}/foo")).await;
    assert_eq!(page.status, StatusCode::OK, "a tag page is a 200");
    assert_eq!(
        page.header("content-type"),
        Some("application/json"),
        "the page is JSON"
    );
    assert_eq!(
        ids(&page.json_value()),
        newest_first(&[&foo_old, &foo_new]),
        "both authors' public #foo posts, newest first, nothing else"
    );
}

/// The path segment is URL-decoded, lowercased and `#`-stripped before it
/// meets the stored (lowercase, `#`-less) names: the same page answers
/// `foo`, `FOO`, `%23foo` and `##Foo`.
#[tokio::test]
async fn the_path_tag_is_decoded_lowercased_and_hash_stripped() {
    let db = common::test_db().await;
    let pool = db.pool().clone();
    let alice = common::create_local_user(&pool, "alice").await;
    let post = tagged(&pool, &alice, "a #MixedCase tag post").await;
    let app = common::test_app(pool);

    for spelling in ["MIXEDCASE", "mixedcase", "%23mixedcase", "%23%23MixedCase"] {
        let page = get(&app, None, &format!("{TAG}/{spelling}")).await;
        assert_eq!(page.status, StatusCode::OK, "`{spelling}` is a 200");
        assert_eq!(
            ids(&page.json_value()),
            newest_first(&[&post]),
            "every spelling lands on the one stored tag"
        );
    }
}

/// `any[]` joins the path tag in the union (the path tag is never dropped);
/// `all[]` demands every named tag on top of the match; `none[]` subtracts —
/// even the path tag itself, which is Pleroma's `tags_any ∩ ¬tag_reject`.
#[tokio::test]
async fn any_all_and_none_narrow_the_tag_match() {
    let db = common::test_db().await;
    let pool = db.pool().clone();
    let alice = common::create_local_user(&pool, "alice").await;
    let foo = tagged(&pool, &alice, "#foo alone").await;
    let bar = tagged(&pool, &alice, "#bar other").await;
    let both = tagged(&pool, &alice, "#foo #bar together").await;
    let app = common::test_app(pool);

    let plain = get(&app, None, &format!("{TAG}/foo")).await;
    assert_eq!(
        ids(&plain.json_value()),
        newest_first(&[&foo, &both]),
        "the plain page is the path tag's own"
    );
    let widened = get(&app, None, &format!("{TAG}/foo?any[]=bar")).await;
    assert_eq!(
        ids(&widened.json_value()),
        newest_first(&[&foo, &bar, &both]),
        "`any[]=bar` unions with the path tag"
    );
    let narrowed = get(&app, None, &format!("{TAG}/foo?all[]=bar")).await;
    assert_eq!(
        ids(&narrowed.json_value()),
        newest_first(&[&both]),
        "`all[]=bar` intersects: only the row carrying both"
    );
    let subtracted = get(&app, None, &format!("{TAG}/foo?none[]=bar")).await;
    assert_eq!(
        ids(&subtracted.json_value()),
        newest_first(&[&foo]),
        "`none[]=bar` drops the row carrying bar"
    );
    let against_the_path = get(&app, None, &format!("{TAG}/foo?any[]=bar&none[]=foo")).await;
    assert_eq!(
        ids(&against_the_path.json_value()),
        newest_first(&[&bar]),
        "`none[]` subtracts even the path tag's rows: what is left is the `any[]` half"
    );
}

/// `local` and `remote` split the tagged page by who authored it; `local=1`
/// counts as `true` and any other text keeps the whole page.
#[tokio::test]
async fn local_and_remote_split_the_tagged_page_by_origin() {
    let db = common::test_db().await;
    let pool = db.pool().clone();
    let alice = common::create_local_user(&pool, "alice").await;
    let mallory = common::create_remote_user(&pool, "mallory", "remote.example").await;
    let ours = tagged(&pool, &alice, "#split from home").await;
    let theirs = remote_tagged(&pool, &mallory, "split-remote", &["split"]).await;
    let app = common::test_app(pool);

    let whole = get(&app, None, &format!("{TAG}/split")).await;
    assert_eq!(
        ids(&whole.json_value()),
        newest_first(&[&ours, &theirs]),
        "the unfiltered page carries both origins"
    );
    let only_local = get(&app, None, &format!("{TAG}/split?local=true")).await;
    assert_eq!(
        ids(&only_local.json_value()),
        newest_first(&[&ours]),
        "`local=true` keeps this instance's row"
    );
    let one = get(&app, None, &format!("{TAG}/split?local=1")).await;
    assert_eq!(
        ids(&one.json_value()),
        newest_first(&[&ours]),
        "`local=1` is the same truthy text"
    );
    let only_remote = get(&app, None, &format!("{TAG}/split?remote=true")).await;
    assert_eq!(
        ids(&only_remote.json_value()),
        newest_first(&[&theirs]),
        "`remote=true` keeps the mirrored row"
    );
}

/// `only_media` keeps the tagged statuses that carry an attachment and
/// drops the ones that do not.
#[tokio::test]
async fn only_media_keeps_the_tagged_statuses_with_attachments() {
    let db = common::test_db().await;
    let pool = db.pool().clone();
    let alice = common::create_local_user(&pool, "alice").await;
    let pictured = tagged(&pool, &alice, "#pic with a shot").await;
    let wordy = tagged(&pool, &alice, "#pic only words").await;
    attach_media(&pool, &alice, &pictured).await;
    let app = common::test_app(pool);

    let whole = get(&app, None, &format!("{TAG}/pic")).await;
    assert_eq!(
        ids(&whole.json_value()),
        newest_first(&[&pictured, &wordy]),
        "without the filter both ride"
    );
    let media = get(&app, None, &format!("{TAG}/pic?only_media=true")).await;
    assert_eq!(
        ids(&media.json_value()),
        newest_first(&[&pictured]),
        "`only_media=true` keeps the attached one"
    );
}

/// A page walks the id cursor: `limit` cuts it, the `Link` header names the
/// next and prev targets over the page's oldest and newest ids (repeating
/// `limit`), `max_id` continues from there, and a `min_id` page still comes
/// back newest-first.
#[tokio::test]
async fn tag_pages_walk_the_id_cursor_under_link_headers() {
    let db = common::test_db().await;
    let pool = db.pool().clone();
    let alice = common::create_local_user(&pool, "alice").await;
    let oldest = tagged(&pool, &alice, "#walk first").await;
    let middle = tagged(&pool, &alice, "#walk second").await;
    let newest = tagged(&pool, &alice, "#walk third").await;
    let app = common::test_app(pool);

    let first = get(&app, None, &format!("{TAG}/walk?limit=2")).await;
    assert_eq!(
        ids(&first.json_value()),
        newest_first(&[&middle, &newest]),
        "the first page is the two newest rows"
    );
    let expected_link = format!(
        "<{BASE}{TAG}/walk?limit=2&max_id={}>; rel=\"next\", \
         <{BASE}{TAG}/walk?limit=2&min_id={}>; rel=\"prev\"",
        id_of(&middle),
        id_of(&newest),
    );
    assert_eq!(
        first.header("link"),
        Some(expected_link.as_str()),
        "next walks to the page's oldest id, prev back to its newest, filters repeated"
    );

    let second = get(
        &app,
        None,
        &format!("{TAG}/walk?limit=2&max_id={}", id_of(&middle)),
    )
    .await;
    assert_eq!(
        ids(&second.json_value()),
        newest_first(&[&oldest]),
        "the next target answers the row older than the cursor"
    );

    let forward = get(&app, None, &format!("{TAG}/walk?min_id={}", id_of(&oldest))).await;
    assert_eq!(
        ids(&forward.json_value()),
        newest_first(&[&middle, &newest]),
        "a min_id page is read ascending and answered newest-first"
    );
}

/// A tag no status carries answers the empty page: 200 with `[]`, no `Link`
/// header — the not-found case of this endpoint is the unusable name, never
/// an unused one.
#[tokio::test]
async fn an_unknown_tag_answers_the_empty_page() {
    let db = common::test_db().await;
    let pool = db.pool().clone();
    let alice = common::create_local_user(&pool, "alice").await;
    let _other = tagged(&pool, &alice, "#somewhereelse").await;
    let app = common::test_app(pool);

    let page = get(&app, None, &format!("{TAG}/neverused")).await;
    assert_eq!(page.status, StatusCode::OK, "an unused tag is a 200");
    assert_eq!(
        page.json::<Vec<Value>>(),
        Vec::<Value>::new(),
        "and the empty page"
    );
    assert_eq!(
        page.header("link"),
        None,
        "an empty page sends no Link header"
    );
}

/// A tag with non-ASCII letters is decoded from its percent form and
/// lowercased like any other: `CAF%C3%89` and `caf%C3%A9` both reach the
/// stored `café`.
#[tokio::test]
async fn unicode_tags_match_their_stored_form() {
    let db = common::test_db().await;
    let pool = db.pool().clone();
    let alice = common::create_local_user(&pool, "alice").await;
    let post = tagged(&pool, &alice, "un #Café con leche").await;
    let app = common::test_app(pool);

    for spelling in ["caf%C3%A9", "CAF%C3%89", "%23caf%C3%A9"] {
        let page = get(&app, None, &format!("{TAG}/{spelling}")).await;
        assert_eq!(page.status, StatusCode::OK, "`{spelling}` is a 200");
        assert_eq!(
            ids(&page.json_value()),
            newest_first(&[&post]),
            "the folded unicode name finds the stored tag"
        );
    }
}

/// Auth is optional and scoped: anonymous, app-only and a `read:statuses`
/// user token all read the page; a user token without the scope is the 403
/// and a presented-but-broken token the 401.
#[tokio::test]
async fn auth_is_optional_and_a_presented_user_token_needs_read_statuses() {
    let db = common::test_db().await;
    let pool = db.pool().clone();
    let alice = common::create_local_user(&pool, "alice").await;
    let post = tagged(&pool, &alice, "#gated view").await;
    let reader = common::create_app_and_token(&pool, &alice, "read:statuses").await;
    let noscope = common::create_app_and_token(&pool, &alice, "read:accounts").await;
    let app_only = common::create_app_only_token(&pool, "read:statuses").await;
    let app = common::test_app(pool);
    let expected = newest_first(&[&post]);

    let anonymous = get(&app, None, &format!("{TAG}/gated")).await;
    assert_eq!(anonymous.status, StatusCode::OK, "anonymous reads the page");
    assert_eq!(ids(&anonymous.json_value()), expected, "the whole page");

    let scoped = get(&app, Some(&reader), &format!("{TAG}/gated")).await;
    assert_eq!(scoped.status, StatusCode::OK, "a granted user token reads");
    assert_eq!(ids(&scoped.json_value()), expected, "the whole page");

    let unscoped = get(&app, Some(&noscope), &format!("{TAG}/gated")).await;
    common::assert_answer(
        &unscoped,
        StatusCode::FORBIDDEN,
        "a token without read:statuses",
    );
    common::assert_error(
        &unscoped,
        "Insufficient permissions: read:statuses.",
        "a token without read:statuses",
    );

    let client = get(&app, Some(&app_only), &format!("{TAG}/gated")).await;
    assert_eq!(
        client.status,
        StatusCode::OK,
        "an app-only token proceeds unauthenticated"
    );
    assert_eq!(ids(&client.json_value()), expected, "the whole page");

    let broken = get(&app, Some("not-a-real-token"), &format!("{TAG}/gated")).await;
    common::assert_answer(&broken, StatusCode::UNAUTHORIZED, "a broken token");
    common::assert_error(&broken, "The access token is invalid", "a broken token");
}

/// The viewer's mutes hide a tagged author by default, and `with_muted=true`
/// brings them back — Pleroma's toggle, applied on top of the tag match.
#[tokio::test]
async fn with_muted_shows_the_statuses_of_muted_accounts() {
    let db = common::test_db().await;
    let pool = db.pool().clone();
    let alice = common::create_local_user(&pool, "alice").await;
    let bob = common::create_local_user(&pool, "bob").await;
    let hidden = tagged(&pool, &bob, "#hush from bob").await;
    let muted = mute::mute(&pool, &alice, &bob, MuteOptions::default())
        .await
        .expect("alice mutes bob");
    assert_eq!(muted.muted_id, bob.id, "the mute is live");
    let token = common::create_app_and_token(&pool, &alice, "read:statuses").await;
    let app = common::test_app(pool);

    let filtered = get(&app, Some(&token), &format!("{TAG}/hush")).await;
    assert_eq!(
        ids(&filtered.json_value()),
        Vec::<String>::new(),
        "bob's post is muted away"
    );
    let shown = get(&app, Some(&token), &format!("{TAG}/hush?with_muted=true")).await;
    assert_eq!(
        ids(&shown.json_value()),
        newest_first(&[&hidden]),
        "with_muted=true lifts the viewer's own exclusions"
    );
    let anonymous = get(&app, None, &format!("{TAG}/hush")).await;
    assert_eq!(
        ids(&anonymous.json_value()),
        newest_first(&[&hidden]),
        "and alice's mute hides nothing from anyone else"
    );
}

/// `exclude_visibilities[]` drops the named visibilities from the page; the
/// tag match itself only ever answers `public`, so excluding `public` empties
/// it and excluding anything else narrows nothing.
#[tokio::test]
async fn exclude_visibilities_drops_named_rows() {
    let db = common::test_db().await;
    let pool = db.pool().clone();
    let alice = common::create_local_user(&pool, "alice").await;
    let post = tagged(&pool, &alice, "#excluded look").await;
    let app = common::test_app(pool);

    let kept = get(
        &app,
        None,
        &format!("{TAG}/excluded?exclude_visibilities[]=direct"),
    )
    .await;
    assert_eq!(
        ids(&kept.json_value()),
        newest_first(&[&post]),
        "excluding a visibility the page never carries narrows nothing"
    );
    let emptied = get(
        &app,
        None,
        &format!("{TAG}/excluded?exclude_visibilities[]=public"),
    )
    .await;
    assert_eq!(
        ids(&emptied.json_value()),
        Vec::<String>::new(),
        "excluding `public` drops the only visibility the page answers"
    );
}

/// A path segment that normalizes to nothing — a lone `#`, whitespace — is
/// the JSON 404, not an empty page.
#[tokio::test]
async fn a_tag_that_normalizes_to_nothing_is_the_json_404() {
    let db = common::test_db().await;
    let app = common::test_app(db.pool().clone());

    for spelling in ["%23", "%23%23", "%20%23%20"] {
        let response = get(&app, None, &format!("{TAG}/{spelling}")).await;
        assert_eq!(
            response.status,
            StatusCode::NOT_FOUND,
            "`{spelling}` is the 404"
        );
        assert_eq!(
            response.header("content-type"),
            Some("application/json"),
            "the 404 answers JSON"
        );
        common::assert_error(&response, "Record not found", "an empty tag name");
    }
}

/// A malformed pagination cursor is the 400 "Invalid id" every keyset
/// listing here answers with.
#[tokio::test]
async fn a_malformed_cursor_is_a_bad_request() {
    let db = common::test_db().await;
    let app = common::test_app(db.pool().clone());

    let response = get(&app, None, &format!("{TAG}/anything?max_id=nope")).await;
    assert_eq!(
        response.status,
        StatusCode::BAD_REQUEST,
        "a bogus cursor is the 400"
    );
    common::assert_error(&response, "Invalid id", "a bogus cursor");
}

/// Inserts `text` as `author`'s `direct` status @-mentioning everyone in
/// `mentioned` (empty for none), straight through
/// [`wired_be::models::status`] — the read the visibility clause judges
/// by the stored mention rows.
async fn direct(pool: &PgPool, author: &User, mentioned: &[&User], text: &str) -> Status {
    status::insert(
        pool,
        BASE,
        &NewStatus {
            visibility: Visibility::Direct,
            mentioned_user_ids: mentioned.iter().map(|member| member.id).collect(),
            ..NewStatus::local(author, text)
        },
    )
    .await
    .expect("the direct fixture inserts")
}

/// The direct timeline answers the viewer's own conversations — what they
/// sent and what @-mentioned them, newest first — and nothing else: no
/// DM between other accounts, no `public` or `private` post of the
/// viewer's own or anyone else's.
#[tokio::test]
async fn direct_answers_the_conversations_the_viewer_is_in() {
    let db = common::test_db().await;
    let pool = db.pool().clone();
    let alice = common::create_local_user(&pool, "alice").await;
    let bob = common::create_local_user(&pool, "bob").await;
    let carol = common::create_local_user(&pool, "carol").await;
    let a_to_b = direct(&pool, &alice, &[&bob], "alice to bob").await;
    let b_to_a = direct(&pool, &bob, &[&alice], "bob to alice").await;
    let a_to_c = direct(&pool, &alice, &[&carol], "alice to carol").await;
    let c_to_b = direct(&pool, &carol, &[&bob], "carol to bob").await;
    let _alice_public = common::create_status(&pool, &alice, "not a dm", Visibility::Public).await;
    let _alice_private =
        common::create_status(&pool, &alice, "circle only", Visibility::Private).await;
    let _bob_unlisted =
        common::create_status(&pool, &bob, "quiet post", Visibility::Unlisted).await;
    let alice_token = common::create_app_and_token(&pool, &alice, "read:statuses").await;
    let bob_token = common::create_app_and_token(&pool, &bob, "read:statuses").await;
    let app = common::test_app(pool);

    for (token, expected) in [
        (&alice_token, newest_first(&[&a_to_b, &b_to_a, &a_to_c])),
        (&bob_token, newest_first(&[&a_to_b, &b_to_a, &c_to_b])),
    ] {
        let page = get(&app, Some(token), DIRECT).await;
        assert_eq!(page.status, StatusCode::OK, "a direct page is a 200");
        assert_eq!(
            page.header("content-type"),
            Some("application/json"),
            "the page is JSON"
        );
        assert_eq!(
            ids(&page.json_value()),
            expected,
            "each viewer's page is their own conversations, newest first"
        );
    }
    let alice_page = get(&app, Some(&alice_token), DIRECT).await;
    assert!(
        !ids(&alice_page.json_value()).contains(&id_of(&c_to_b)),
        "a DM between other accounts never leaks into someone else's direct timeline"
    );
}

/// The direct page walks the id cursor like every other timeline: `limit`
/// cuts it, the `Link` header names the next and prev targets over the
/// page's oldest and newest ids, `max_id` continues from there, a `min_id`
/// page still comes back newest-first, and past the oldest row the page is
/// empty and carries no `Link` header.
#[tokio::test]
async fn direct_pages_walk_the_id_cursor_under_link_headers() {
    let db = common::test_db().await;
    let pool = db.pool().clone();
    let alice = common::create_local_user(&pool, "alice").await;
    let bob = common::create_local_user(&pool, "bob").await;
    let oldest = direct(&pool, &alice, &[&bob], "dm one").await;
    let middle = direct(&pool, &alice, &[&bob], "dm two").await;
    let newest = direct(&pool, &bob, &[&alice], "dm three").await;
    let token = common::create_app_and_token(&pool, &alice, "read:statuses").await;
    let app = common::test_app(pool);

    let first = get(&app, Some(&token), &format!("{DIRECT}?limit=2")).await;
    assert_eq!(
        ids(&first.json_value()),
        newest_first(&[&middle, &newest]),
        "the first page is the two newest rows"
    );
    let expected_link = format!(
        "<{BASE}{DIRECT}?limit=2&max_id={}>; rel=\"next\", \
         <{BASE}{DIRECT}?limit=2&min_id={}>; rel=\"prev\"",
        id_of(&middle),
        id_of(&newest),
    );
    assert_eq!(
        first.header("link"),
        Some(expected_link.as_str()),
        "next walks to the page's oldest id, prev back to its newest, `limit` repeated"
    );

    let second = get(
        &app,
        Some(&token),
        &format!("{DIRECT}?limit=2&max_id={}", id_of(&middle)),
    )
    .await;
    assert_eq!(
        ids(&second.json_value()),
        newest_first(&[&oldest]),
        "the next target answers the row older than the cursor"
    );

    let forward = get(
        &app,
        Some(&token),
        &format!("{DIRECT}?min_id={}", id_of(&oldest)),
    )
    .await;
    assert_eq!(
        ids(&forward.json_value()),
        newest_first(&[&middle, &newest]),
        "a min_id page is read ascending and answered newest-first"
    );

    let empty = get(
        &app,
        Some(&token),
        &format!("{DIRECT}?max_id={}", id_of(&oldest)),
    )
    .await;
    assert_eq!(
        empty.json::<Vec<Value>>(),
        Vec::<Value>::new(),
        "past the oldest row the page is empty"
    );
    assert_eq!(
        empty.header("link"),
        None,
        "an empty page sends no Link header"
    );
}

/// A DM from an account the viewer blocked stays off their direct
/// timeline, and `with_muted=true` brings it back — the viewer's own
/// exclusion lifted, the conversation still theirs.
#[tokio::test]
async fn direct_hides_blocked_senders_unless_with_muted() {
    let db = common::test_db().await;
    let pool = db.pool().clone();
    let alice = common::create_local_user(&pool, "alice").await;
    let bob = common::create_local_user(&pool, "bob").await;
    let own = direct(&pool, &alice, &[&bob], "alice to bob").await;
    let blocked = block::block(&pool, BASE, &alice, &bob)
        .await
        .expect("alice blocks bob");
    assert_eq!(blocked.blocked_id, bob.id, "the block is live");
    let from_bob = direct(&pool, &bob, &[&alice], "bob to alice").await;
    let token = common::create_app_and_token(&pool, &alice, "read:statuses").await;
    let app = common::test_app(pool);

    let filtered = get(&app, Some(&token), DIRECT).await;
    assert_eq!(
        ids(&filtered.json_value()),
        newest_first(&[&own]),
        "bob's DM is blocked away from alice's direct timeline"
    );
    let shown = get(&app, Some(&token), &format!("{DIRECT}?with_muted=true")).await;
    assert_eq!(
        ids(&shown.json_value()),
        newest_first(&[&from_bob, &own]),
        "`with_muted=true` lifts alice's own exclusions"
    );
}

/// The direct route is gated on a user token: no token is the 401
/// "Invalid credentials" and a broken one the 401 "The access token is
/// invalid"; an app-only token and a user token without `read:statuses`
/// are the 403s; a granted token reads its page.
#[tokio::test]
async fn direct_requires_a_user_token_granting_read_statuses() {
    let db = common::test_db().await;
    let pool = db.pool().clone();
    let alice = common::create_local_user(&pool, "alice").await;
    let dm = direct(&pool, &alice, &[], "to nobody").await;
    let reader = common::create_app_and_token(&pool, &alice, "read:statuses").await;
    let noscope = common::create_app_and_token(&pool, &alice, "read:accounts").await;
    let app_only = common::create_app_only_token(&pool, "read:statuses").await;
    let app = common::test_app(pool);

    let anonymous = get(&app, None, DIRECT).await;
    common::assert_answer(&anonymous, StatusCode::UNAUTHORIZED, "no token");
    common::assert_error(&anonymous, "Invalid credentials", "no token");

    let broken = get(&app, Some("not-a-real-token"), DIRECT).await;
    common::assert_answer(&broken, StatusCode::UNAUTHORIZED, "a broken token");
    common::assert_error(&broken, "The access token is invalid", "a broken token");

    let client = get(&app, Some(&app_only), DIRECT).await;
    common::assert_answer(&client, StatusCode::FORBIDDEN, "an app-only token");
    common::assert_error(
        &client,
        "This API requires an authenticated user",
        "an app-only token",
    );

    let unscoped = get(&app, Some(&noscope), DIRECT).await;
    common::assert_answer(
        &unscoped,
        StatusCode::FORBIDDEN,
        "a token without read:statuses",
    );
    common::assert_error(
        &unscoped,
        "Insufficient permissions: read:statuses.",
        "a token without read:statuses",
    );

    let granted = get(&app, Some(&reader), DIRECT).await;
    assert_eq!(
        granted.status,
        StatusCode::OK,
        "a granted user token reads its page"
    );
    assert_eq!(
        ids(&granted.json_value()),
        newest_first(&[&dm]),
        "and sees the unaddressed DM she authored"
    );
}

/// Inserts `author`'s public reply to `parent`, @-mentioning everyone in
/// `mentioned` (empty for none), straight through
/// [`wired_be::models::status`] — the read the replies policy judges.
async fn reply(
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

/// Creates `owner`'s list titled `title` with `policy` as its replies
/// policy, straight through [`wired_be::models::list`].
async fn owned_list(pool: &PgPool, owner: &User, policy: &str) -> list::List {
    list::create(
        pool,
        owner.id,
        &NewList {
            title: "circle".to_owned(),
            replies_policy: policy.to_owned(),
            exclusive: false,
        },
    )
    .await
    .expect("the list fixture creates")
}

/// Adds `member` to `list_id` through the model, below the API's own
/// follow rule: membership is what the timeline reads.
async fn add_member(pool: &PgPool, list_id: Uuid, member: &User) {
    let added = list::add_members(pool, list_id, &[member.id])
        .await
        .expect("the membership fixture adds");
    assert_eq!(added, 1, "exactly one membership went in");
}

/// The list timeline answers the members' posts and nothing else:
/// membership alone puts a row on the page (no follow is read, in either
/// direction), and the list owner's own post stays off it because the
/// owner is not a member.
#[tokio::test]
async fn list_timeline_answers_member_posts_without_follows() {
    let db = common::test_db().await;
    let pool = db.pool().clone();
    let alice = common::create_local_user(&pool, "alice").await;
    let bob = common::create_local_user(&pool, "bob").await;
    let carol = common::create_local_user(&pool, "carol").await;
    let circle = owned_list(&pool, &alice, "list").await;
    add_member(&pool, circle.id, &bob).await;
    let member_post = common::create_status(&pool, &bob, "member talk", Visibility::Public).await;
    let member_reply = reply(&pool, &bob, &member_post, &[], "member answers themselves").await;
    let _followed_stranger =
        common::create_status(&pool, &carol, "non-member talk", Visibility::Public).await;
    let _own = common::create_status(&pool, &alice, "owner's own post", Visibility::Public).await;
    let token = common::create_app_and_token(&pool, &alice, "read:lists").await;
    let app = common::test_app(pool);

    let page = get(&app, Some(&token), &format!("{LIST}/{}", circle.id)).await;
    assert_eq!(page.status, StatusCode::OK, "a list page is a 200");
    assert_eq!(
        page.header("content-type"),
        Some("application/json"),
        "the page is JSON"
    );
    assert_eq!(
        ids(&page.json_value()),
        newest_first(&[&member_post, &member_reply]),
        "the member's root and the member's reply ride the page; the non-member's and the \
         owner's own posts do not"
    );
}

/// The endpoint takes the replies policy from the stored list row: two
/// lists, each with its own single member posting a root and a reply
/// addressed to that same member, and a `none` list answers the root
/// while a `list` list answers root and reply.
#[tokio::test]
async fn list_timeline_replies_follow_the_stored_policy() {
    let db = common::test_db().await;
    let pool = db.pool().clone();
    let alice = common::create_local_user(&pool, "alice").await;
    let bob = common::create_local_user(&pool, "bob").await;
    let dave = common::create_local_user(&pool, "dave").await;
    let talking = owned_list(&pool, &alice, "list").await;
    add_member(&pool, talking.id, &bob).await;
    let talking_root = common::create_status(&pool, &bob, "talking root", Visibility::Public).await;
    let talking_reply = reply(&pool, &bob, &talking_root, &[], "a member's answer").await;
    let silent = owned_list(&pool, &alice, "none").await;
    add_member(&pool, silent.id, &dave).await;
    let silent_root = common::create_status(&pool, &dave, "silent root", Visibility::Public).await;
    let _silent_reply = reply(&pool, &dave, &silent_root, &[], "an answer `none` drops").await;
    let token = common::create_app_and_token(&pool, &alice, "read:lists").await;
    let app = common::test_app(pool);

    let policy_list = get(&app, Some(&token), &format!("{LIST}/{}", talking.id)).await;
    assert_eq!(
        ids(&policy_list.json_value()),
        newest_first(&[&talking_root, &talking_reply]),
        "`list` keeps the reply addressed to the member"
    );
    let policy_none = get(&app, Some(&token), &format!("{LIST}/{}", silent.id)).await;
    assert_eq!(
        ids(&policy_none.json_value()),
        newest_first(&[&silent_root]),
        "`none` drops every reply, addressed however it likes"
    );
}

/// The list page walks the id cursor like every other timeline: `limit`
/// cuts it, the `Link` header names the next and prev targets over the
/// page's oldest and newest ids, `max_id` continues from there, and past
/// the oldest row the page is empty and carries no `Link` header.
#[tokio::test]
async fn list_pages_walk_the_id_cursor_under_link_headers() {
    let db = common::test_db().await;
    let pool = db.pool().clone();
    let alice = common::create_local_user(&pool, "alice").await;
    let bob = common::create_local_user(&pool, "bob").await;
    let circle = owned_list(&pool, &alice, "list").await;
    add_member(&pool, circle.id, &bob).await;
    let oldest = common::create_status(&pool, &bob, "list post one", Visibility::Public).await;
    let middle = common::create_status(&pool, &bob, "list post two", Visibility::Public).await;
    let newest = common::create_status(&pool, &bob, "list post three", Visibility::Public).await;
    let token = common::create_app_and_token(&pool, &alice, "read:lists").await;
    let app = common::test_app(pool);
    let route = format!("{LIST}/{}", circle.id);

    let first = get(&app, Some(&token), &format!("{route}?limit=2")).await;
    assert_eq!(
        ids(&first.json_value()),
        newest_first(&[&middle, &newest]),
        "the first page is the two newest rows"
    );
    let expected_link = format!(
        "<{BASE}{route}?limit=2&max_id={}>; rel=\"next\", \
         <{BASE}{route}?limit=2&min_id={}>; rel=\"prev\"",
        id_of(&middle),
        id_of(&newest),
    );
    assert_eq!(
        first.header("link"),
        Some(expected_link.as_str()),
        "next walks to the page's oldest id, prev back to its newest, `limit` repeated"
    );

    let second = get(
        &app,
        Some(&token),
        &format!("{route}?limit=2&max_id={}", id_of(&middle)),
    )
    .await;
    assert_eq!(
        ids(&second.json_value()),
        newest_first(&[&oldest]),
        "the next target answers the row older than the cursor"
    );

    let empty = get(
        &app,
        Some(&token),
        &format!("{route}?max_id={}", id_of(&oldest)),
    )
    .await;
    assert_eq!(
        empty.json::<Vec<Value>>(),
        Vec::<Value>::new(),
        "past the oldest row the page is empty"
    );
    assert_eq!(
        empty.header("link"),
        None,
        "an empty page sends no Link header"
    );
}

/// `only_media` keeps the member's attached row, the viewer's mute clears
/// the member off the list page, and `with_muted=true` brings the rows
/// back — Pleroma's toggles, applied over the membership read.
#[tokio::test]
async fn list_timeline_narrows_with_only_media_and_lifts_mutes_with_with_muted() {
    let db = common::test_db().await;
    let pool = db.pool().clone();
    let alice = common::create_local_user(&pool, "alice").await;
    let bob = common::create_local_user(&pool, "bob").await;
    let circle = owned_list(&pool, &alice, "list").await;
    add_member(&pool, circle.id, &bob).await;
    let plain = common::create_status(&pool, &bob, "plain member", Visibility::Public).await;
    let pictured = common::create_status(&pool, &bob, "pictured member", Visibility::Public).await;
    let attached = media_attachment::insert(
        &pool,
        &NewMediaAttachment {
            user_id: Some(bob.id),
            status_id: Some(pictured.id),
            url: format!("{BASE}/media/list.png"),
            content_type: "image/png".to_owned(),
            ..Default::default()
        },
    )
    .await
    .expect("the media fixture attaches");
    assert_eq!(attached.status_id, Some(pictured.id), "one image lands");
    let muted = mute::mute(&pool, &alice, &bob, MuteOptions::default())
        .await
        .expect("alice mutes bob");
    assert_eq!(muted.muted_id, bob.id, "the mute is live");
    let token = common::create_app_and_token(&pool, &alice, "read:lists").await;
    let app = common::test_app(pool);
    let route = format!("{LIST}/{}", circle.id);

    let hidden = get(&app, Some(&token), &route).await;
    assert_eq!(
        ids(&hidden.json_value()),
        Vec::<String>::new(),
        "the muted member leaves the list page"
    );
    let shown = get(&app, Some(&token), &format!("{route}?with_muted=true")).await;
    assert_eq!(
        ids(&shown.json_value()),
        newest_first(&[&plain, &pictured]),
        "`with_muted=true` lifts alice's own exclusion"
    );
    let media = get(
        &app,
        Some(&token),
        &format!("{route}?with_muted=true&only_media=true"),
    )
    .await;
    assert_eq!(
        ids(&media.json_value()),
        newest_first(&[&pictured]),
        "`only_media=true` keeps the attached row"
    );
}

/// A list that is another account's, an id that names no list and a
/// malformed id all answer the same 404 "Record not found": the timeline
/// never tells a foreign list from a missing one, nor either from a
/// misspelled id.
#[tokio::test]
async fn a_foreign_unknown_or_malformed_list_is_the_json_404() {
    let db = common::test_db().await;
    let pool = db.pool().clone();
    let alice = common::create_local_user(&pool, "alice").await;
    let bob = common::create_local_user(&pool, "bob").await;
    let his = owned_list(&pool, &bob, "list").await;
    add_member(&pool, his.id, &alice).await;
    let _his_post =
        common::create_status(&pool, &bob, "his member's post", Visibility::Public).await;
    let token = common::create_app_and_token(&pool, &alice, "read:lists").await;
    let app = common::test_app(pool);

    for spelling in [
        his.id.to_string(),
        Uuid::now_v7().to_string(),
        "not-a-real-id".to_owned(),
    ] {
        let response = get(&app, Some(&token), &format!("{LIST}/{spelling}")).await;
        assert_eq!(
            response.status,
            StatusCode::NOT_FOUND,
            "`{spelling}` is the 404"
        );
        assert_eq!(
            response.header("content-type"),
            Some("application/json"),
            "the 404 answers JSON"
        );
        common::assert_error(&response, "Record not found", "an unreadable list id");
    }
}

/// The list route is gated on a user token granting `read:lists`: no
/// token is the 401 "Invalid credentials" and a broken one the 401
/// "The access token is invalid"; an app-only token and a user token that
/// only grants `read:statuses` are the 403s; a granted token reads its
/// page — `read:statuses` alone never does.
#[tokio::test]
async fn the_list_route_requires_a_user_token_granting_read_lists() {
    let db = common::test_db().await;
    let pool = db.pool().clone();
    let alice = common::create_local_user(&pool, "alice").await;
    let bob = common::create_local_user(&pool, "bob").await;
    let circle = owned_list(&pool, &alice, "list").await;
    add_member(&pool, circle.id, &bob).await;
    let member_post = common::create_status(&pool, &bob, "listed talk", Visibility::Public).await;
    let reader = common::create_app_and_token(&pool, &alice, "read:lists").await;
    let statuses_only = common::create_app_and_token(&pool, &alice, "read:statuses").await;
    let app_only = common::create_app_only_token(&pool, "read:lists").await;
    let app = common::test_app(pool);
    let route = format!("{LIST}/{}", circle.id);

    let anonymous = get(&app, None, &route).await;
    common::assert_answer(&anonymous, StatusCode::UNAUTHORIZED, "no token");
    common::assert_error(&anonymous, "Invalid credentials", "no token");

    let broken = get(&app, Some("not-a-real-token"), &route).await;
    common::assert_answer(&broken, StatusCode::UNAUTHORIZED, "a broken token");
    common::assert_error(&broken, "The access token is invalid", "a broken token");

    let client = get(&app, Some(&app_only), &route).await;
    common::assert_answer(&client, StatusCode::FORBIDDEN, "an app-only token");
    common::assert_error(
        &client,
        "This API requires an authenticated user",
        "an app-only token",
    );

    let unscoped = get(&app, Some(&statuses_only), &route).await;
    common::assert_answer(
        &unscoped,
        StatusCode::FORBIDDEN,
        "a token without read:lists",
    );
    common::assert_error(
        &unscoped,
        "Insufficient permissions: read:lists.",
        "a token without read:lists",
    );

    let granted = get(&app, Some(&reader), &route).await;
    assert_eq!(
        granted.status,
        StatusCode::OK,
        "a granted user token reads its page"
    );
    assert_eq!(
        ids(&granted.json_value()),
        newest_first(&[&member_post]),
        "and sees the member's post"
    );
}

/// The blocked member stays off the list page like every other read's
/// excluded author — and `read:lists` on the viewer's own list answers
/// the empty page for a list with no members.
#[tokio::test]
async fn a_blocked_member_leaves_the_list_page_and_an_empty_list_answers_empty() {
    let db = common::test_db().await;
    let pool = db.pool().clone();
    let alice = common::create_local_user(&pool, "alice").await;
    let bob = common::create_local_user(&pool, "bob").await;
    let carol = common::create_local_user(&pool, "carol").await;
    let circle = owned_list(&pool, &alice, "list").await;
    add_member(&pool, circle.id, &bob).await;
    add_member(&pool, circle.id, &carol).await;
    let carol_post = common::create_status(&pool, &carol, "kept member", Visibility::Public).await;
    let blocked = block::block(&pool, BASE, &alice, &bob)
        .await
        .expect("alice blocks bob");
    assert_eq!(blocked.blocked_id, bob.id, "the block is live");
    let _bob_post = common::create_status(&pool, &bob, "blocked member", Visibility::Public).await;
    let empty_list = owned_list(&pool, &alice, "list").await;
    let token = common::create_app_and_token(&pool, &alice, "read:lists").await;
    let app = common::test_app(pool);

    let page = get(&app, Some(&token), &format!("{LIST}/{}", circle.id)).await;
    assert_eq!(
        ids(&page.json_value()),
        newest_first(&[&carol_post]),
        "the blocked member is out, the unblocked one stays"
    );
    let empty = get(&app, Some(&token), &format!("{LIST}/{}", empty_list.id)).await;
    assert_eq!(
        empty.status,
        StatusCode::OK,
        "a memberless own list is a 200"
    );
    assert_eq!(
        empty.json::<Vec<Value>>(),
        Vec::<Value>::new(),
        "and the empty page"
    );
    assert_eq!(
        empty.header("link"),
        None,
        "an empty page sends no Link header"
    );
}
