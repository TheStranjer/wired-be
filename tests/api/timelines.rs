//! Status timelines over HTTP (`GET /api/v1/timelines/home`, WBE-100,
//! `GET /api/v1/timelines/tag/{tag}`, WBE-102, `GET
//! /api/v1/timelines/direct`, WBE-103, `GET
//! /api/v1/timelines/list/{list_id}`, WBE-105, and `GET
//! /api/v1/timelines/public`, WBE-101).
//!
//! The home timeline answers the viewer's own posts and reblogs plus those
//! of their accepted follows — a follow's reblogs only while its
//! `show_reblogs` toggle stands, the followee's `private` rows within
//! reach of the visibility clause, a stranger's nowhere — under `AuthUser`
//! + `read:statuses`. `local`, `remote`, `only_media`, `with_muted` and
//! `exclude_visibilities[]` narrow the page like on the tag route,
//! `reply_visibility` (`following` or `self`) narrows the replies and any
//! other spelling is the 422, `exclude_replies` is ignored, and the `Link`
//! header repeats the request's filters on both cursors.
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
//!
//! The public timeline answers the `public` statuses — and `local` ones for
//! an authenticated local viewer — under the same optional-auth rule,
//! narrowed by `local`/`remote`/`only_media`/`with_muted`/
//! `exclude_visibilities[]` and by Pleroma's `reply_visibility` and
//! `instance` extensions. `RESTRICT_UNAUTHENTICATED` can close its two
//! anonymous reads: `timelines_local` gates `local=true` and
//! `timelines_federated` every other one, each refused with the 403
//! `{"error":"Authentication is required to access this resource."}`;
//! an authenticated request and the default configuration read normally.

#![cfg(test)]

use axum::{Router, http::StatusCode};
use serde_json::Value;
use sqlx::PgPool;
use uuid::Uuid;
use wired_be::models::{
    block,
    follow::{self, FollowToggles},
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
/// The home route's path, query strings appended per case.
const HOME: &str = "/api/v1/timelines/home";
/// The tag route's path prefix; each test appends `/{tag}[?query]`.
const TAG: &str = "/api/v1/timelines/tag";
/// The direct route's path, query strings appended per case.
const DIRECT: &str = "/api/v1/timelines/direct";
/// The list route's path prefix; each test appends `/{id}[?query]`.
const LIST: &str = "/api/v1/timelines/list";
/// The public route: `/api/v1/timelines/public[?query]`.
const PUBLIC: &str = "/api/v1/timelines/public";
/// The exact 403 body a `RESTRICT_UNAUTHENTICATED` gate answers with.
const RESTRICTED: &str = "Authentication is required to access this resource.";

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

/// The ids of `posts`, newest first, minus the ids of `without`: an
/// expected page written as a difference from the seeded set.
fn minus(posts: &[&Status], without: &[&Status]) -> Vec<String> {
    let mut kept = newest_first(posts);
    for skipped in without {
        let skipped = id_of(skipped);
        kept.retain(|id| *id != skipped);
    }
    kept
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

/// The public timeline is the `public` page: unlisted, private and direct
/// rows stay out for everyone — even their author — while `local` rows
/// appear exactly for an authenticated viewer this instance owns.
#[tokio::test]
async fn the_public_timeline_lists_public_statuses_only() {
    let db = common::test_db().await;
    let pool = db.pool().clone();
    let alice = common::create_local_user(&pool, "alice").await;
    let mallory = common::create_remote_user(&pool, "mallory", "remote.example").await;
    let ours = common::create_status(&pool, &alice, "public from home", Visibility::Public).await;
    let theirs = remote_tagged(&pool, &mallory, "public-remote", &[]).await;
    let _unlisted = common::create_status(&pool, &alice, "quiet", Visibility::Unlisted).await;
    let instance_local =
        common::create_status(&pool, &alice, "instance only", Visibility::Local).await;
    let _friends = common::create_status(&pool, &alice, "circle", Visibility::Private).await;
    let _dm = common::create_status(&pool, &alice, "sealed", Visibility::Direct).await;
    let reader = common::create_app_and_token(&pool, &alice, "read:statuses").await;
    let app = common::test_app(pool);

    let anonymous = get(&app, None, PUBLIC).await;
    assert_eq!(anonymous.status, StatusCode::OK, "a public page is a 200");
    assert_eq!(
        anonymous.header("content-type"),
        Some("application/json"),
        "the page is JSON"
    );
    assert_eq!(
        ids(&anonymous.json_value()),
        newest_first(&[&ours, &theirs]),
        "public rows only: no unlisted, local, private or direct, from either origin"
    );

    let for_alice = get(&app, Some(&reader), PUBLIC).await;
    assert_eq!(for_alice.status, StatusCode::OK, "a local user reads too");
    assert_eq!(
        ids(&for_alice.json_value()),
        newest_first(&[&ours, &theirs, &instance_local]),
        "an authenticated local viewer gains the `local` row and nothing else"
    );
}

/// `reply_visibility` narrows the public page's replies for its viewer:
/// `self` keeps only replies answering them, `following` keeps replies
/// answering them or an account they follow, and roots are nobody's
/// business to drop.
#[tokio::test]
async fn reply_visibility_narrows_the_public_page_for_its_viewer() {
    let db = common::test_db().await;
    let pool = db.pool().clone();
    let alice = common::create_local_user(&pool, "alice").await;
    let bob = common::create_local_user(&pool, "bob").await;
    let carol = common::create_local_user(&pool, "carol").await;
    let alice_root = common::create_status(&pool, &alice, "alice root", Visibility::Public).await;
    let carol_root = common::create_status(&pool, &carol, "carol root", Visibility::Public).await;
    let to_alice = status::insert(
        &pool,
        BASE,
        &NewStatus {
            in_reply_to_id: Some(alice_root.id),
            in_reply_to_account_id: Some(alice.id),
            ..NewStatus::local(&bob, "bob to alice")
        },
    )
    .await
    .expect("the addressed reply fixture inserts");
    let to_carol = status::insert(
        &pool,
        BASE,
        &NewStatus {
            in_reply_to_id: Some(carol_root.id),
            in_reply_to_account_id: Some(carol.id),
            ..NewStatus::local(&bob, "bob to carol")
        },
    )
    .await
    .expect("the stranger reply fixture inserts");
    let reader = common::create_app_and_token(&pool, &alice, "read:statuses").await;
    let app = common::test_app(pool);
    let all = [&alice_root, &carol_root, &to_alice, &to_carol];

    let whole = get(&app, Some(&reader), PUBLIC).await;
    assert_eq!(
        ids(&whole.json_value()),
        newest_first(&all),
        "without the parameter every reply rides"
    );
    let selfed = get(
        &app,
        Some(&reader),
        &format!("{PUBLIC}?reply_visibility=self"),
    )
    .await;
    assert_eq!(
        ids(&selfed.json_value()),
        minus(&all, &[&to_carol]),
        "`self` drops the reply addressed to a stranger"
    );
    let anonymous = get(&app, None, &format!("{PUBLIC}?reply_visibility=self")).await;
    assert_eq!(
        ids(&anonymous.json_value()),
        newest_first(&all),
        "with no viewer there is nobody to reply to, and the filter asks nothing"
    );
}

/// `local` and `remote` split the public page by who authored it; `local=1`
/// counts as `true` just like on the tag route.
#[tokio::test]
async fn local_and_remote_split_the_public_page_by_origin() {
    let db = common::test_db().await;
    let pool = db.pool().clone();
    let alice = common::create_local_user(&pool, "alice").await;
    let mallory = common::create_remote_user(&pool, "mallory", "remote.example").await;
    let ours = common::create_status(&pool, &alice, "from home", Visibility::Public).await;
    let theirs = remote_tagged(&pool, &mallory, "split-remote", &[]).await;
    let app = common::test_app(pool);

    let whole = get(&app, None, PUBLIC).await;
    assert_eq!(
        ids(&whole.json_value()),
        newest_first(&[&ours, &theirs]),
        "the unfiltered page carries both origins"
    );
    let only_local = get(&app, None, &format!("{PUBLIC}?local=true")).await;
    assert_eq!(
        ids(&only_local.json_value()),
        newest_first(&[&ours]),
        "`local=true` keeps this instance's row"
    );
    let one = get(&app, None, &format!("{PUBLIC}?local=1")).await;
    assert_eq!(
        ids(&one.json_value()),
        newest_first(&[&ours]),
        "`local=1` is the same truthy text"
    );
    let only_remote = get(&app, None, &format!("{PUBLIC}?remote=true")).await;
    assert_eq!(
        ids(&only_remote.json_value()),
        newest_first(&[&theirs]),
        "`remote=true` keeps the mirrored row"
    );
}

/// `only_media` keeps the public statuses that carry an attachment and drops
/// the ones that do not.
#[tokio::test]
async fn only_media_keeps_the_public_statuses_with_attachments() {
    let db = common::test_db().await;
    let pool = db.pool().clone();
    let alice = common::create_local_user(&pool, "alice").await;
    let pictured = common::create_status(&pool, &alice, "with a shot", Visibility::Public).await;
    let wordy = common::create_status(&pool, &alice, "only words", Visibility::Public).await;
    attach_media(&pool, &alice, &pictured).await;
    let app = common::test_app(pool);

    let whole = get(&app, None, PUBLIC).await;
    assert_eq!(
        ids(&whole.json_value()),
        newest_first(&[&pictured, &wordy]),
        "without the filter both ride"
    );
    let media = get(&app, None, &format!("{PUBLIC}?only_media=true")).await;
    assert_eq!(
        ids(&media.json_value()),
        newest_first(&[&pictured]),
        "`only_media=true` keeps the attached one"
    );
}

/// Pleroma's `instance` extension keeps only the statuses whose author is a
/// remote account of the named domain — a local author has no domain and
/// never matches — and a domain nobody lives under answers the empty page.
#[tokio::test]
async fn instance_keeps_the_statuses_of_one_authors_domain() {
    let db = common::test_db().await;
    let pool = db.pool().clone();
    let alice = common::create_local_user(&pool, "alice").await;
    let mallory = common::create_remote_user(&pool, "mallory", "remote.example").await;
    let pete = common::create_remote_user(&pool, "pete", "other.example").await;
    let _ours = common::create_status(&pool, &alice, "from home", Visibility::Public).await;
    let theirs = remote_tagged(&pool, &mallory, "one-domain", &[]).await;
    let others = remote_tagged(&pool, &pete, "other-domain", &[]).await;
    let app = common::test_app(pool);

    let one = get(&app, None, &format!("{PUBLIC}?instance=remote.example")).await;
    assert_eq!(
        ids(&one.json_value()),
        newest_first(&[&theirs]),
        "the named domain keeps its own rows and drops the other mirrors and the local post"
    );
    let other = get(&app, None, &format!("{PUBLIC}?instance=other.example")).await;
    assert_eq!(
        ids(&other.json_value()),
        newest_first(&[&others]),
        "the other name keeps its rows instead"
    );
    let nobody = get(&app, None, &format!("{PUBLIC}?instance=nowhere.example")).await;
    assert_eq!(
        ids(&nobody.json_value()),
        Vec::<String>::new(),
        "a domain nobody lives under is the empty page, not an error"
    );
}

/// A public page walks the id cursor under the same `Link` header as the tag
/// route: `limit` cuts it, `next`/`prev` name the page's oldest/newest ids,
/// the request's own parameters (`local` here) repeat on both links while
/// the cursors do not, `max_id` continues and a `min_id` page comes back
/// newest-first.
#[tokio::test]
async fn public_pages_walk_the_id_cursor_under_link_headers() {
    let db = common::test_db().await;
    let pool = db.pool().clone();
    let alice = common::create_local_user(&pool, "alice").await;
    let oldest = common::create_status(&pool, &alice, "walk first", Visibility::Public).await;
    let middle = common::create_status(&pool, &alice, "walk second", Visibility::Public).await;
    let newest = common::create_status(&pool, &alice, "walk third", Visibility::Public).await;
    let app = common::test_app(pool);

    let first = get(&app, None, &format!("{PUBLIC}?local=true&limit=2")).await;
    assert_eq!(
        ids(&first.json_value()),
        newest_first(&[&middle, &newest]),
        "the first page is the two newest rows"
    );
    let expected_link = format!(
        "<{BASE}{PUBLIC}?local=true&limit=2&max_id={}>; rel=\"next\", \
         <{BASE}{PUBLIC}?local=true&limit=2&min_id={}>; rel=\"prev\"",
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
        &format!("{PUBLIC}?limit=2&max_id={}", id_of(&middle)),
    )
    .await;
    assert_eq!(
        ids(&second.json_value()),
        newest_first(&[&oldest]),
        "the next target answers the row older than the cursor"
    );

    let forward = get(&app, None, &format!("{PUBLIC}?min_id={}", id_of(&oldest))).await;
    assert_eq!(
        ids(&forward.json_value()),
        newest_first(&[&middle, &newest]),
        "a min_id page is read ascending and answered newest-first"
    );
}

/// The viewer's own exclusions apply to the public page as everywhere else:
/// blocked and muted authors leave it (until `with_muted=true` waves them
/// back), and nobody else reads through the exclusions.
#[tokio::test]
async fn the_public_page_hides_blocked_and_muted_authors_from_its_viewer() {
    let db = common::test_db().await;
    let pool = db.pool().clone();
    let alice = common::create_local_user(&pool, "alice").await;
    let bob = common::create_local_user(&pool, "bob").await;
    let carol = common::create_local_user(&pool, "carol").await;
    let from_bob = common::create_status(&pool, &bob, "muted bob", Visibility::Public).await;
    let from_carol =
        common::create_status(&pool, &carol, "blocked carol", Visibility::Public).await;
    let from_alice = common::create_status(&pool, &alice, "plain alice", Visibility::Public).await;
    let carol_block = block::block(&pool, BASE, &alice, &carol)
        .await
        .expect("alice blocks carol");
    assert_eq!(
        carol_block.blocked_id, carol.id,
        "the fixture block is on carol"
    );
    let bob_mute = mute::mute(&pool, &alice, &bob, MuteOptions::default())
        .await
        .expect("alice mutes bob");
    assert_eq!(bob_mute.muted_id, bob.id, "the fixture mute is on bob");
    let reader = common::create_app_and_token(&pool, &alice, "read:statuses").await;
    let app = common::test_app(pool);

    let filtered = get(&app, Some(&reader), PUBLIC).await;
    assert_eq!(
        ids(&filtered.json_value()),
        newest_first(&[&from_alice]),
        "bob's and carol's rows are alice's own business to hide"
    );
    let shown = get(&app, Some(&reader), &format!("{PUBLIC}?with_muted=true")).await;
    assert_eq!(
        ids(&shown.json_value()),
        newest_first(&[&from_bob, &from_carol, &from_alice]),
        "`with_muted=true` lifts the viewer's own exclusions"
    );
    let anonymous = get(&app, None, PUBLIC).await;
    assert_eq!(
        ids(&anonymous.json_value()),
        newest_first(&[&from_bob, &from_carol, &from_alice]),
        "alice's block and mute hide nothing from anyone else"
    );
}

/// Auth on the public route is optional and scoped exactly like the tag
/// route: anonymous, app-only and a `read:statuses` user token all read the
/// page; a user token without the scope is the 403 and a
/// presented-but-broken token the 401.
#[tokio::test]
async fn public_auth_is_optional_and_a_presented_user_token_needs_read_statuses() {
    let db = common::test_db().await;
    let pool = db.pool().clone();
    let alice = common::create_local_user(&pool, "alice").await;
    let post = common::create_status(&pool, &alice, "open page", Visibility::Public).await;
    let reader = common::create_app_and_token(&pool, &alice, "read:statuses").await;
    let noscope = common::create_app_and_token(&pool, &alice, "read:accounts").await;
    let app_only = common::create_app_only_token(&pool, "read:statuses").await;
    let app = common::test_app(pool);
    let expected = newest_first(&[&post]);

    let anonymous = get(&app, None, PUBLIC).await;
    assert_eq!(anonymous.status, StatusCode::OK, "anonymous reads the page");
    assert_eq!(ids(&anonymous.json_value()), expected, "the whole page");

    let scoped = get(&app, Some(&reader), PUBLIC).await;
    assert_eq!(scoped.status, StatusCode::OK, "a granted user token reads");
    assert_eq!(ids(&scoped.json_value()), expected, "the whole page");

    let unscoped = get(&app, Some(&noscope), PUBLIC).await;
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

    let client = get(&app, Some(&app_only), PUBLIC).await;
    assert_eq!(
        client.status,
        StatusCode::OK,
        "an app-only token proceeds unauthenticated"
    );
    assert_eq!(ids(&client.json_value()), expected, "the whole page");

    let broken = get(&app, Some("not-a-real-token"), PUBLIC).await;
    common::assert_answer(&broken, StatusCode::UNAUTHORIZED, "a broken token");
    common::assert_error(&broken, "The access token is invalid", "a broken token");
}

/// The default configuration restricts nothing: every anonymous read —
/// federated or `local=true` — answers the normal page.
#[tokio::test]
async fn unauthenticated_public_reads_are_allowed_by_default() {
    let db = common::test_db().await;
    let pool = db.pool().clone();
    let alice = common::create_local_user(&pool, "alice").await;
    let mallory = common::create_remote_user(&pool, "mallory", "remote.example").await;
    let ours = common::create_status(&pool, &alice, "from home", Visibility::Public).await;
    let theirs = remote_tagged(&pool, &mallory, "default-open", &[]).await;
    let app = common::test_app(pool);

    for (query, want) in [
        ("", newest_first(&[&ours, &theirs])),
        ("?local=true", newest_first(&[&ours])),
        ("?remote=true", newest_first(&[&theirs])),
    ] {
        let page = get(&app, None, &format!("{PUBLIC}{query}")).await;
        assert_eq!(
            page.status,
            StatusCode::OK,
            "the default instance refuses nothing"
        );
        assert_eq!(
            ids(&page.json_value()),
            want,
            "`GET {PUBLIC}{query}` reads normally"
        );
    }
}

/// With `timelines_local` restricted, the anonymous `local=true` read is the
/// 403 with the ticket's exact wording; every other read — the plain and
/// `local=false` anonymous pages and any authenticated one — stays normal.
#[tokio::test]
async fn restricted_timelines_local_refuses_anonymous_local_reads() {
    let db = common::test_db().await;
    let pool = db.pool().clone();
    let alice = common::create_local_user(&pool, "alice").await;
    let post = common::create_status(&pool, &alice, "from home", Visibility::Public).await;
    let reader = common::create_app_and_token(&pool, &alice, "read:statuses").await;
    let app_only = common::create_app_only_token(&pool, "read:statuses").await;
    let app = common::test_app_with_restricted_areas(pool, &["timelines_local"]);
    let expected = newest_first(&[&post]);

    let anonymous_local = get(&app, None, &format!("{PUBLIC}?local=true")).await;
    common::assert_answer(
        &anonymous_local,
        StatusCode::FORBIDDEN,
        "an anonymous `local=true` read of a `timelines_local`-restricted instance",
    );
    common::assert_error(
        &anonymous_local,
        RESTRICTED,
        "an anonymous `local=true` read of a `timelines_local`-restricted instance",
    );

    let client_local = get(&app, Some(&app_only), &format!("{PUBLIC}?local=true")).await;
    common::assert_answer(
        &client_local,
        StatusCode::FORBIDDEN,
        "an app-only token is no user to this gate",
    );
    common::assert_error(
        &client_local,
        RESTRICTED,
        "an app-only token reads anonymously",
    );

    let anonymous = get(&app, None, &format!("{PUBLIC}?local=false")).await;
    assert_eq!(
        anonymous.status,
        StatusCode::OK,
        "`local=false` is the federated page, which this area does not gate"
    );
    assert_eq!(ids(&anonymous.json_value()), expected, "and reads normally");

    let federated = get(&app, None, PUBLIC).await;
    assert_eq!(
        federated.status,
        StatusCode::OK,
        "the plain anonymous page stays open"
    );

    let authenticated = get(&app, Some(&reader), &format!("{PUBLIC}?local=true")).await;
    assert_eq!(
        authenticated.status,
        StatusCode::OK,
        "an authenticated `local=true` read was never the gate's business"
    );
    assert_eq!(
        ids(&authenticated.json_value()),
        expected,
        "and reads normally"
    );
}

/// With `timelines_federated` restricted, the anonymous non-`local` reads —
/// the plain page and `remote=true` included — are the same 403, while
/// `local=true` belongs to the other area and stays open.
#[tokio::test]
async fn restricted_timelines_federated_refuses_anonymous_federated_reads() {
    let db = common::test_db().await;
    let pool = db.pool().clone();
    let alice = common::create_local_user(&pool, "alice").await;
    let mallory = common::create_remote_user(&pool, "mallory", "remote.example").await;
    let ours = common::create_status(&pool, &alice, "from home", Visibility::Public).await;
    let theirs = remote_tagged(&pool, &mallory, "federated-gated", &[]).await;
    let reader = common::create_app_and_token(&pool, &alice, "read:statuses").await;
    let app = common::test_app_with_restricted_areas(pool, &["timelines_federated"]);

    for query in ["", "?remote=true", "?local=false"] {
        let refused = get(&app, None, &format!("{PUBLIC}{query}")).await;
        common::assert_answer(
            &refused,
            StatusCode::FORBIDDEN,
            &format!("an anonymous read `{query}` of a `timelines_federated`-restricted instance"),
        );
        common::assert_error(&refused, RESTRICTED, &format!("the refusal for `{query}`"));
    }

    let local_page = get(&app, None, &format!("{PUBLIC}?local=true")).await;
    assert_eq!(
        local_page.status,
        StatusCode::OK,
        "`local=true` belongs to `timelines_local`, which is not restricted here"
    );
    assert_eq!(
        ids(&local_page.json_value()),
        newest_first(&[&ours]),
        "and reads its own page normally"
    );

    let authenticated = get(&app, Some(&reader), PUBLIC).await;
    assert_eq!(
        authenticated.status,
        StatusCode::OK,
        "an authenticated federated read passes whatever is restricted"
    );
    assert_eq!(
        ids(&authenticated.json_value()),
        newest_first(&[&ours, &theirs]),
        "and sees the whole page"
    );
}

/// With both areas listed, every anonymous read is the same 403 — an
/// authenticated read stays normal — and a presented-but-broken token is
/// still the extractor's 401: the restriction never papers over a bad
/// credential.
#[tokio::test]
async fn both_areas_restricted_close_the_timeline_to_anonymous_reads_only() {
    let db = common::test_db().await;
    let pool = db.pool().clone();
    let alice = common::create_local_user(&pool, "alice").await;
    let post = common::create_status(&pool, &alice, "from home", Visibility::Public).await;
    let reader = common::create_app_and_token(&pool, &alice, "read:statuses").await;
    let app =
        common::test_app_with_restricted_areas(pool, &["timelines_local", "timelines_federated"]);

    for query in ["", "?local=true", "?local=false&remote=true"] {
        let refused = get(&app, None, &format!("{PUBLIC}{query}")).await;
        common::assert_answer(
            &refused,
            StatusCode::FORBIDDEN,
            &format!("the anonymous read `{query}` of a fully restricted instance"),
        );
        common::assert_error(&refused, RESTRICTED, &format!("the refusal for `{query}`"));
    }

    let authenticated = get(&app, Some(&reader), &format!("{PUBLIC}?local=true")).await;
    assert_eq!(
        authenticated.status,
        StatusCode::OK,
        "a user token reads whatever the instance restricts to strangers"
    );
    assert_eq!(ids(&authenticated.json_value()), newest_first(&[&post]));

    let broken = get(&app, Some("not-a-real-token"), PUBLIC).await;
    common::assert_answer(&broken, StatusCode::UNAUTHORIZED, "a broken token");
    common::assert_error(&broken, "The access token is invalid", "a broken token");
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

/// `reblogger`'s public reblog of `original`, straight through
/// [`wired_be::models::status`] — the row the `show_reblogs` toggle and the
/// block/mute exclusions judge.
async fn reblog(pool: &PgPool, reblogger: &User, original: &Status) -> Status {
    status::create_reblog(pool, BASE, reblogger, original, Visibility::Public)
        .await
        .expect("the reblog fixture writes")
        .0
}

/// `follower` follows `target` through [`wired_be::models::follow`],
/// asserting the accepted state the home timeline's source predicate
/// reads — an unlocked fixture account always grants.
async fn accepted_follow(pool: &PgPool, follower: &User, target: &User) {
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

/// The home timeline answers the viewer's own posts — reblogs included —
/// and the accepted follows' posts at every visibility a follower reaches
/// (public, unlisted and `private` alike), and nothing of a stranger's; a
/// user with neither follows nor posts reads the empty page.
#[tokio::test]
async fn home_answers_the_viewers_own_posts_and_their_follows() {
    let db = common::test_db().await;
    let pool = db.pool().clone();
    let alice = common::create_local_user(&pool, "alice").await;
    let bob = common::create_local_user(&pool, "bob").await;
    let carol = common::create_local_user(&pool, "carol").await;
    let dave = common::create_local_user(&pool, "dave").await;
    accepted_follow(&pool, &alice, &bob).await;
    let own = common::create_status(&pool, &alice, "my own post", Visibility::Public).await;
    let own_unlisted =
        common::create_status(&pool, &alice, "my quiet post", Visibility::Unlisted).await;
    let stranger_root =
        common::create_status(&pool, &carol, "stranger original", Visibility::Public).await;
    let own_reblog = reblog(&pool, &alice, &stranger_root).await;
    let followed_public =
        common::create_status(&pool, &bob, "followed public", Visibility::Public).await;
    let followed_unlisted =
        common::create_status(&pool, &bob, "followed unlisted", Visibility::Unlisted).await;
    let followed_private =
        common::create_status(&pool, &bob, "followed private", Visibility::Private).await;
    let _stranger_plain =
        common::create_status(&pool, &carol, "stranger talk", Visibility::Public).await;
    let alice_token = common::create_app_and_token(&pool, &alice, "read:statuses").await;
    let dave_token = common::create_app_and_token(&pool, &dave, "read:statuses").await;
    let app = common::test_app(pool);

    let page = get(&app, Some(&alice_token), HOME).await;
    assert_eq!(page.status, StatusCode::OK, "a home page is a 200");
    assert_eq!(
        page.header("content-type"),
        Some("application/json"),
        "the page is JSON"
    );
    assert_eq!(
        ids(&page.json_value()),
        newest_first(&[
            &own,
            &own_unlisted,
            &own_reblog,
            &followed_public,
            &followed_unlisted,
            &followed_private
        ]),
        "own rows (the boost included) and the follow's rows at every visibility a follower \
         reaches; the stranger's own rows do not ride"
    );

    let empty = get(&app, Some(&dave_token), HOME).await;
    assert_eq!(
        empty.json::<Vec<Value>>(),
        Vec::<Value>::new(),
        "a user with no follows and no posts reads the empty page"
    );
    assert_eq!(
        empty.header("link"),
        None,
        "an empty page sends no Link header"
    );
}

/// A reblog by a followed user rides the home timeline until that follow's
/// `show_reblogs` toggle goes off — the follow's plain posts and the
/// viewer's own boost stay.
#[tokio::test]
async fn home_follow_reblogs_honour_the_show_reblogs_toggle() {
    let db = common::test_db().await;
    let pool = db.pool().clone();
    let alice = common::create_local_user(&pool, "alice").await;
    let bob = common::create_local_user(&pool, "bob").await;
    let carol = common::create_local_user(&pool, "carol").await;
    let dave = common::create_local_user(&pool, "dave").await;
    accepted_follow(&pool, &alice, &bob).await;
    accepted_follow(&pool, &alice, &carol).await;
    let original =
        common::create_status(&pool, &dave, "reblogged original", Visibility::Public).await;
    let bob_reblog = reblog(&pool, &bob, &original).await;
    let carol_reblog = reblog(&pool, &carol, &original).await;
    let carol_plain = common::create_status(&pool, &carol, "carol plain", Visibility::Public).await;
    let own_reblog = reblog(&pool, &alice, &original).await;
    let token = common::create_app_and_token(&pool, &alice, "read:statuses").await;
    let app = common::test_app(pool.clone());

    let before = get(&app, Some(&token), HOME).await;
    assert_eq!(
        ids(&before.json_value()),
        newest_first(&[&bob_reblog, &carol_reblog, &carol_plain, &own_reblog]),
        "fresh follows show reblogs: both boosts ride, the unfollowed original does not"
    );

    let hidden = follow::set_toggles(
        &pool,
        alice.id,
        carol.id,
        FollowToggles {
            show_reblogs: Some(false),
            notify: None,
        },
    )
    .await
    .expect("the toggle writes")
    .expect("alice's follow of carol is open");
    assert!(!hidden.show_reblogs, "the follow now hides reblogs");

    let after = get(&app, Some(&token), HOME).await;
    assert_eq!(
        ids(&after.json_value()),
        newest_first(&[&bob_reblog, &carol_plain, &own_reblog]),
        "carol's boost is gone, her plain posts, bob's boost and alice's own boost stay"
    );
}

/// The home page walks the id cursor like every other timeline: `limit`
/// cuts it, the `Link` header names the next and prev targets over the
/// page's oldest and newest ids and repeats the request's other parameters,
/// `max_id` continues from there, `since_id` and `min_id` pages come back
/// newest-first, and past the oldest row the page is empty and carries no
/// `Link` header.
#[tokio::test]
async fn home_pages_walk_the_id_cursor_under_link_headers() {
    let db = common::test_db().await;
    let pool = db.pool().clone();
    let alice = common::create_local_user(&pool, "alice").await;
    let bob = common::create_local_user(&pool, "bob").await;
    accepted_follow(&pool, &alice, &bob).await;
    let oldest = common::create_status(&pool, &bob, "home post one", Visibility::Public).await;
    let middle = common::create_status(&pool, &bob, "home post two", Visibility::Public).await;
    let newest = common::create_status(&pool, &bob, "home post three", Visibility::Public).await;
    let token = common::create_app_and_token(&pool, &alice, "read:statuses").await;
    let app = common::test_app(pool);

    let first = get(&app, Some(&token), &format!("{HOME}?local=true&limit=2")).await;
    assert_eq!(
        ids(&first.json_value()),
        newest_first(&[&middle, &newest]),
        "the first page is the two newest rows"
    );
    let expected_link = format!(
        "<{BASE}{HOME}?local=true&limit=2&max_id={}>; rel=\"next\", \
         <{BASE}{HOME}?local=true&limit=2&min_id={}>; rel=\"prev\"",
        id_of(&middle),
        id_of(&newest),
    );
    assert_eq!(
        first.header("link"),
        Some(expected_link.as_str()),
        "next walks to the page's oldest id, prev back to its newest, `local` and `limit` repeated"
    );

    let second = get(
        &app,
        Some(&token),
        &format!("{HOME}?local=true&limit=2&max_id={}", id_of(&middle)),
    )
    .await;
    assert_eq!(
        ids(&second.json_value()),
        newest_first(&[&oldest]),
        "the next target answers the row older than the cursor"
    );

    let polled = get(
        &app,
        Some(&token),
        &format!("{HOME}?since_id={}", id_of(&middle)),
    )
    .await;
    assert_eq!(
        ids(&polled.json_value()),
        newest_first(&[&newest]),
        "a since_id poll answers the newer rows newest-first"
    );

    let forward = get(
        &app,
        Some(&token),
        &format!("{HOME}?min_id={}", id_of(&oldest)),
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
        &format!("{HOME}?max_id={}", id_of(&oldest)),
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

/// `local` and `remote` split the home page by the author's origin: a
/// followed local account's post and a followed remote account's mirrored
/// post each answer only their own half.
#[tokio::test]
async fn home_local_and_remote_split_the_page_by_author_origin() {
    let db = common::test_db().await;
    let pool = db.pool().clone();
    let alice = common::create_local_user(&pool, "alice").await;
    let bob = common::create_local_user(&pool, "bob").await;
    let mallory = common::create_remote_user(&pool, "mallory", "remote.example").await;
    accepted_follow(&pool, &alice, &bob).await;
    let mallory_request = follow::follow(&pool, BASE, &alice, &mallory)
        .await
        .expect("alice follows mallory");
    let mallory_followed = follow::accept(&pool, mallory_request.id)
        .await
        .expect("the remote instance accepts the request");
    assert_eq!(
        mallory_followed.state,
        follow::FollowState::Accepted,
        "the remote follow reads accepted, the state a mirrored `Accept` leaves it in"
    );
    let ours = common::create_status(&pool, &bob, "local follow", Visibility::Public).await;
    let theirs = remote_tagged(&pool, &mallory, "home-remote", &[]).await;
    let token = common::create_app_and_token(&pool, &alice, "read:statuses").await;
    let app = common::test_app(pool);

    let whole = get(&app, Some(&token), HOME).await;
    assert_eq!(
        ids(&whole.json_value()),
        newest_first(&[&ours, &theirs]),
        "the unfiltered page carries both follows"
    );
    let only_local = get(&app, Some(&token), &format!("{HOME}?local=true")).await;
    assert_eq!(
        ids(&only_local.json_value()),
        newest_first(&[&ours]),
        "`local=true` keeps this instance's follow"
    );
    let one = get(&app, Some(&token), &format!("{HOME}?local=1")).await;
    assert_eq!(
        ids(&one.json_value()),
        newest_first(&[&ours]),
        "`local=1` is the same truthy text"
    );
    let only_remote = get(&app, Some(&token), &format!("{HOME}?remote=true")).await;
    assert_eq!(
        ids(&only_remote.json_value()),
        newest_first(&[&theirs]),
        "`remote=true` keeps the remote follow's mirrored row"
    );
}

/// `only_media` keeps the followed posts that carry an attachment and drops
/// the ones that do not.
#[tokio::test]
async fn home_only_media_keeps_the_posts_with_attachments() {
    let db = common::test_db().await;
    let pool = db.pool().clone();
    let alice = common::create_local_user(&pool, "alice").await;
    let bob = common::create_local_user(&pool, "bob").await;
    accepted_follow(&pool, &alice, &bob).await;
    let pictured = common::create_status(&pool, &bob, "with a shot", Visibility::Public).await;
    let wordy = common::create_status(&pool, &bob, "only words", Visibility::Public).await;
    attach_media(&pool, &bob, &pictured).await;
    let token = common::create_app_and_token(&pool, &alice, "read:statuses").await;
    let app = common::test_app(pool);

    let whole = get(&app, Some(&token), HOME).await;
    assert_eq!(
        ids(&whole.json_value()),
        newest_first(&[&pictured, &wordy]),
        "without the filter both ride"
    );
    let media = get(&app, Some(&token), &format!("{HOME}?only_media=true")).await;
    assert_eq!(
        ids(&media.json_value()),
        newest_first(&[&pictured]),
        "`only_media=true` keeps the attached one"
    );
}

/// The viewer's own exclusions clear a home page: a blocked account's
/// status disappears from a followed booster's reblog, muting the followed
/// author empties the page, and `with_muted=true` lifts both — Pleroma's
/// toggle over the home read.
#[tokio::test]
async fn home_hides_muted_and_blocked_accounts_unless_with_muted() {
    let db = common::test_db().await;
    let pool = db.pool().clone();
    let alice = common::create_local_user(&pool, "alice").await;
    let bob = common::create_local_user(&pool, "bob").await;
    let carol = common::create_local_user(&pool, "carol").await;
    accepted_follow(&pool, &alice, &bob).await;
    let blocked = block::block(&pool, BASE, &alice, &carol)
        .await
        .expect("alice blocks carol");
    assert_eq!(blocked.blocked_id, carol.id, "the block is live");
    let carol_root =
        common::create_status(&pool, &carol, "blocked original", Visibility::Public).await;
    let bob_plain = common::create_status(&pool, &bob, "kept follow", Visibility::Public).await;
    let bob_reblog = reblog(&pool, &bob, &carol_root).await;
    let token = common::create_app_and_token(&pool, &alice, "read:statuses").await;
    let app = common::test_app(pool.clone());

    let after_block = get(&app, Some(&token), HOME).await;
    assert_eq!(
        ids(&after_block.json_value()),
        newest_first(&[&bob_plain]),
        "the boost of the blocked account's original is excluded, the plain follow stays"
    );

    let muted = mute::mute(&pool, &alice, &bob, MuteOptions::default())
        .await
        .expect("alice mutes bob");
    assert_eq!(muted.muted_id, bob.id, "the mute is live");
    let after_mute = get(&app, Some(&token), HOME).await;
    assert_eq!(
        ids(&after_mute.json_value()),
        Vec::<String>::new(),
        "the muted follow leaves the page entirely"
    );

    let shown = get(&app, Some(&token), &format!("{HOME}?with_muted=true")).await;
    assert_eq!(
        ids(&shown.json_value()),
        newest_first(&[&bob_plain, &bob_reblog]),
        "`with_muted=true` lifts the viewer's own block and mute exclusions"
    );
}

/// `exclude_visibilities[]` drops the named visibilities from the home
/// page; a name outside the five stored spellings narrows nothing.
#[tokio::test]
async fn home_exclude_visibilities_drops_the_named_rows() {
    let db = common::test_db().await;
    let pool = db.pool().clone();
    let alice = common::create_local_user(&pool, "alice").await;
    let bob = common::create_local_user(&pool, "bob").await;
    accepted_follow(&pool, &alice, &bob).await;
    let own_unlisted =
        common::create_status(&pool, &alice, "my quiet post", Visibility::Unlisted).await;
    let bob_public =
        common::create_status(&pool, &bob, "followed public", Visibility::Public).await;
    let bob_private =
        common::create_status(&pool, &bob, "followed private", Visibility::Private).await;
    let token = common::create_app_and_token(&pool, &alice, "read:statuses").await;
    let app = common::test_app(pool);
    let expected = newest_first(&[&own_unlisted, &bob_public, &bob_private]);

    let kept = get(
        &app,
        Some(&token),
        &format!("{HOME}?exclude_visibilities[]=Public"),
    )
    .await;
    assert_eq!(
        ids(&kept.json_value()),
        expected,
        "a name that spells no stored visibility excludes nothing"
    );
    let narrowed = get(
        &app,
        Some(&token),
        &format!("{HOME}?exclude_visibilities[]=private&exclude_visibilities[]=unlisted"),
    )
    .await;
    assert_eq!(
        ids(&narrowed.json_value()),
        newest_first(&[&bob_public]),
        "excluding `private` and `unlisted` leaves only the public row"
    );
}

/// Replies ride the home timeline by default — answering a non-followed
/// account included, and `exclude_replies` is not a home parameter and
/// changes nothing — while `reply_visibility=following` keeps only replies
/// addressed to the viewer or a follow and `self` only those addressed to
/// the viewer, by target or by @-mention.
#[tokio::test]
async fn home_reply_visibility_narrows_the_replies_and_exclude_replies_is_ignored() {
    let db = common::test_db().await;
    let pool = db.pool().clone();
    let alice = common::create_local_user(&pool, "alice").await;
    let bob = common::create_local_user(&pool, "bob").await;
    let carol = common::create_local_user(&pool, "carol").await;
    let dave = common::create_local_user(&pool, "dave").await;
    accepted_follow(&pool, &alice, &bob).await;
    accepted_follow(&pool, &alice, &dave).await;
    let alice_root =
        common::create_status(&pool, &alice, "viewer's own root", Visibility::Public).await;
    let dave_root = common::create_status(&pool, &dave, "followed root", Visibility::Public).await;
    let carol_root =
        common::create_status(&pool, &carol, "stranger root", Visibility::Public).await;
    let to_carol = reply(&pool, &bob, &carol_root, &[], "answers a stranger").await;
    let to_alice = reply(&pool, &bob, &alice_root, &[], "answers the viewer").await;
    let to_dave = reply(&pool, &bob, &dave_root, &[], "answers a follow").await;
    let mentions_alice = reply(
        &pool,
        &bob,
        &carol_root,
        &[&alice],
        "stranger's thread, names alice",
    )
    .await;
    let token = common::create_app_and_token(&pool, &alice, "read:statuses").await;
    let app = common::test_app(pool);

    let whole = get(&app, Some(&token), HOME).await;
    assert_eq!(
        ids(&whole.json_value()),
        newest_first(&[
            &alice_root,
            &dave_root,
            &to_carol,
            &to_alice,
            &to_dave,
            &mentions_alice
        ]),
        "by default every reply of a followed author rides, a stranger's own root does not"
    );
    let ignored = get(&app, Some(&token), &format!("{HOME}?exclude_replies=true")).await;
    assert_eq!(
        ids(&ignored.json_value()),
        newest_first(&[
            &alice_root,
            &dave_root,
            &to_carol,
            &to_alice,
            &to_dave,
            &mentions_alice
        ]),
        "`exclude_replies` is not a home parameter and drops nothing"
    );
    let following = get(
        &app,
        Some(&token),
        &format!("{HOME}?reply_visibility=following"),
    )
    .await;
    assert_eq!(
        ids(&following.json_value()),
        newest_first(&[
            &alice_root,
            &dave_root,
            &to_alice,
            &to_dave,
            &mentions_alice
        ]),
        "`following` keeps replies addressed to the viewer or a follow"
    );
    let only_self = get(&app, Some(&token), &format!("{HOME}?reply_visibility=self")).await;
    assert_eq!(
        ids(&only_self.json_value()),
        newest_first(&[&alice_root, &dave_root, &to_alice, &mentions_alice]),
        "`self` keeps only the replies addressed to the viewer, by target or mention"
    );
}

/// A `reply_visibility` outside `following` and `self` — the enum
/// Pleroma's `timeline_operation` lists — is the 422 with Mastodon's
/// changeset wording; the empty spelling is outside it too.
#[tokio::test]
async fn an_unknown_home_reply_visibility_is_the_422() {
    let db = common::test_db().await;
    let pool = db.pool().clone();
    let alice = common::create_local_user(&pool, "alice").await;
    let token = common::create_app_and_token(&pool, &alice, "read:statuses").await;
    let app = common::test_app(pool);

    for spelling in ["bogus", "Following", ""] {
        let response = get(
            &app,
            Some(&token),
            &format!("{HOME}?reply_visibility={spelling}"),
        )
        .await;
        assert_eq!(
            response.status,
            StatusCode::UNPROCESSABLE_ENTITY,
            "`{spelling}` is the 422"
        );
        assert_eq!(
            response.header("content-type"),
            Some("application/json"),
            "the 422 answers JSON"
        );
        common::assert_error(
            &response,
            "Validation failed: Reply visibility is not included in the list",
            "an unusable `reply_visibility`",
        );
    }
}

/// The home route is gated on a user token granting `read:statuses`: no
/// token is the 401 "Invalid credentials" and a broken one the 401 "The
/// access token is invalid"; an app-only token and a user token without the
/// scope are the 403s; a granted token reads its page. A malformed cursor
/// is the 400 "Invalid id" every keyset listing here answers with.
#[tokio::test]
async fn home_requires_a_user_token_granting_read_statuses() {
    let db = common::test_db().await;
    let pool = db.pool().clone();
    let alice = common::create_local_user(&pool, "alice").await;
    let own = common::create_status(&pool, &alice, "my own post", Visibility::Public).await;
    let reader = common::create_app_and_token(&pool, &alice, "read:statuses").await;
    let noscope = common::create_app_and_token(&pool, &alice, "read:accounts").await;
    let app_only = common::create_app_only_token(&pool, "read:statuses").await;
    let app = common::test_app(pool);

    let anonymous = get(&app, None, HOME).await;
    common::assert_answer(&anonymous, StatusCode::UNAUTHORIZED, "no token");
    common::assert_error(&anonymous, "Invalid credentials", "no token");

    let broken = get(&app, Some("not-a-real-token"), HOME).await;
    common::assert_answer(&broken, StatusCode::UNAUTHORIZED, "a broken token");
    common::assert_error(&broken, "The access token is invalid", "a broken token");

    let client = get(&app, Some(&app_only), HOME).await;
    common::assert_answer(&client, StatusCode::FORBIDDEN, "an app-only token");
    common::assert_error(
        &client,
        "This API requires an authenticated user",
        "an app-only token",
    );

    let unscoped = get(&app, Some(&noscope), HOME).await;
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

    let granted = get(&app, Some(&reader), HOME).await;
    assert_eq!(
        granted.status,
        StatusCode::OK,
        "a granted user token reads its page"
    );
    assert_eq!(
        ids(&granted.json_value()),
        newest_first(&[&own]),
        "and sees their own post"
    );

    let bad_cursor = get(&app, Some(&reader), &format!("{HOME}?max_id=nope")).await;
    assert_eq!(
        bad_cursor.status,
        StatusCode::BAD_REQUEST,
        "a bogus cursor is the 400"
    );
    common::assert_error(&bad_cursor, "Invalid id", "a bogus cursor");
}
