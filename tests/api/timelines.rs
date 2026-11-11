//! Status timelines over HTTP: `GET /api/v1/timelines/tag/{tag}` (WBE-102)
//! and `GET /api/v1/timelines/public` (WBE-101).
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

/// Inserts a mirrored (`local = false`) public status by `author` whose
/// object uri and text are keyed by `key`.
async fn untagged_remote(pool: &PgPool, author: &User, key: &str) -> Status {
    status::insert(
        pool,
        BASE,
        &NewStatus {
            uri: Some(format!("https://origin.test.example/objects/{key}")),
            user_id: author.id,
            raw_text: key.to_owned(),
            ..NewStatus::default()
        },
    )
    .await
    .expect("the remote fixture inserts")
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
    let theirs = untagged_remote(&pool, &mallory, "public-remote").await;
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
    let theirs = untagged_remote(&pool, &mallory, "split-remote").await;
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
    let theirs = untagged_remote(&pool, &mallory, "one-domain").await;
    let others = untagged_remote(&pool, &pete, "other-domain").await;
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
    let theirs = untagged_remote(&pool, &mallory, "default-open").await;
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
    let theirs = untagged_remote(&pool, &mallory, "federated-gated").await;
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
