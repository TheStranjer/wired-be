//! The public timeline over HTTP (`GET /api/v1/timelines/public`): the
//! `public` page — with `local` rows only for an authenticated local
//! viewer — the `local`/`remote`/`only_media` narrows and Pleroma's
//! `instance` extension, `reply_visibility` narrowing the page's replies
//! for their viewer, the viewer's own block/mute exclusions with
//! `with_muted` lifting them, the `Link`-keyed id cursor, and the optional
//! auth with its `read:statuses` scope.

use axum::http::StatusCode;
use wired_be::models::{
    block,
    mute::{self, MuteOptions},
    status::{self, NewStatus, Visibility},
};

use super::support::{
    BASE, PUBLIC, attach_media, get, id_of, ids, minus, newest_first, remote_tagged,
};
use crate::common;

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
