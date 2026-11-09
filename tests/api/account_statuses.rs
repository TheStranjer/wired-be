//! Integration tests for `GET /api/v1/accounts/{id}/statuses`: the account's
//! own posts and its reblogs as a page of `Status` entities, judged row by
//! row for whoever reads it — a stranger (and the anonymous request) keeps
//! to `public` and `unlisted`, a follower adds `private`, a mentioned party
//! adds `direct`, the account behind the token sees all of its own, and an
//! account that blocked the viewer answers the empty list. The filters are
//! Pleroma's `statuses_operation` (`only_media`, `exclude_replies`,
//! `exclude_reblogs`, `tagged`, `exclude_visibilities[]`, `with_muted`),
//! `pinned` is the profile's featured list in pin order with no `Link`
//! header, the ordinary page walks the id cursor under the `Link` header
//! every timeline writes, and a mirrored account reads like a local one.
//! `read:statuses` on a presented user token, the 401 for a broken one and
//! the 403 for a token without the scope ride on `MaybeAuthUser`; a
//! `RESTRICT_UNAUTHENTICATED` profile half closes an anonymous request with
//! the 403 `{"error":"This API requires an authenticated user"}`. A
//! malformed, unknown or deactivated account id is the shared 404.

#![cfg(test)]

use axum::{Router, http::StatusCode};
use serde_json::Value;
use sqlx::PgPool;
use wired_be::{
    config::{self, Config},
    models::{
        block, follow,
        media_attachment::{self, NewMediaAttachment},
        mute::{self, MuteOptions},
        pinned_status,
        status::{self, NewStatus, Status, Visibility},
        user::{Flag, User},
    },
};

use crate::common;

/// The instance the fixtures live on (`Config::for_tests`).
const BASE: &str = "https://test.example";
/// The route every test requests, `{id}` and the query appended per case.
const ROUTE: &str = "/api/v1/accounts";
/// The body every account miss carries (decision 8).
const NOT_FOUND: &str = r#"{"error":"Record not found"}"#;
/// The 401 `MaybeAuthUser` renders for a token that does not resolve.
const INVALID_TOKEN: &str = r#"{"error":"The access token is invalid"}"#;
/// The 403 the scope check renders for a user token without `read:statuses`.
const NO_SCOPE: &str = r#"{"error":"Insufficient permissions: read:statuses."}"#;
/// The 403 a `RESTRICT_UNAUTHENTICATED` profile area answers an anonymous
/// request with.
const USER_REQUIRED: &str = r#"{"error":"This API requires an authenticated user"}"#;
/// The 400 `PageParams` answers for a cursor that is not id text.
const INVALID_ID: &str = r#"{"error":"Invalid id"}"#;
/// A well-formed id no fixture row answers to, the cursor the `pinned` case
/// sends to prove the pagination keys are ignored rather than refused.
const CURSOR: &str = "01934567-89ab-7cde-f012-3456789abcde";

/// `GET /api/v1/accounts/{id}/statuses{query}`, with `token` as bearer when
/// the test presents one.
async fn statuses(
    app: &Router,
    token: Option<&str>,
    id: &str,
    query: &str,
) -> common::TestResponse {
    let path = format!("{ROUTE}/{id}/statuses{query}");
    let request = common::TestRequest::get(&path);
    match token {
        Some(token) => request.bearer(token).send(app).await,
        None => request.send(app).await,
    }
}

/// The ids of the statuses of a rendered page, in answer order.
fn ids(body: &Value) -> Vec<String> {
    body.as_array()
        .expect("a status page is a JSON array")
        .iter()
        .map(|status| {
            status["id"]
                .as_str()
                .expect("every Status entity carries its id")
                .to_owned()
        })
        .collect()
}

/// The ids of `posts`, newest first — the order the listing answers in,
/// since `UUIDv7` text sorts by creation (decision 4).
fn newest_first(posts: &[&Status]) -> Vec<String> {
    let mut ordered: Vec<_> = posts.iter().map(|post| post.id).collect();
    ordered.sort_by(|a, b| b.cmp(a));
    ordered.into_iter().map(|id| id.to_string()).collect()
}

/// The one id of `post`, as its JSON carries it.
fn id_of(post: &Status) -> String {
    post.id.to_string()
}

/// Inserts `author`'s status of `visibility` @-mentioning everyone in
/// `mentioned` — the `direct` row a mentioned party reaches and a follower
/// does not.
async fn addressed(
    pool: &PgPool,
    author: &User,
    mentioned: &[&User],
    visibility: Visibility,
    text: &str,
) -> Status {
    status::insert(
        pool,
        BASE,
        &NewStatus {
            visibility,
            mentioned_user_ids: mentioned.iter().map(|user| user.id).collect(),
            ..NewStatus::local(author, text)
        },
    )
    .await
    .expect("the addressed fixture inserts")
}

/// Inserts `author`'s public reply to `parent`.
async fn reply(pool: &PgPool, author: &User, parent: &Status, text: &str) -> Status {
    status::insert(
        pool,
        BASE,
        &NewStatus {
            in_reply_to_id: Some(parent.id),
            in_reply_to_account_id: Some(parent.user_id),
            ..NewStatus::local(author, text)
        },
    )
    .await
    .expect("the reply fixture inserts")
}

/// `reblogger`'s public reblog of `original`.
async fn reblog(pool: &PgPool, reblogger: &User, original: &Status) -> Status {
    status::create_reblog(pool, BASE, reblogger, original, Visibility::Public)
        .await
        .expect("the reblog fixture writes")
        .0
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

/// `user` pins `post`, and asserts the row landed: the pin time the profile
/// order reads is the insert's `now()`.
async fn pin(pool: &PgPool, user: &User, post: &Status) {
    let pinned = pinned_status::pin(pool, user.id, post.id)
        .await
        .expect("the pin fixture writes");
    assert!(
        pinned.is_some(),
        "{user} pins one of their own posts",
        user = user.id
    );
}

/// `follower` follows `target` through [`wired_be::models::follow`], with
/// the accepted state the visibility clause reads.
async fn accepted_follow(pool: &PgPool, follower: &User, target: &User) {
    let opened = follow::follow(pool, BASE, follower, target)
        .await
        .expect("the fixture follow is written");
    assert_eq!(
        opened.state,
        follow::FollowState::Accepted,
        "the follow of `{}` is granted",
        target.nickname
    );
}

/// The application with `RESTRICT_UNAUTHENTICATED` closed over `areas` (the
/// variable's own spelling, so the gate is read the way an operator sets
/// it).
///
/// # Panics
///
/// Never for the fixed area lists the tests pass.
fn restricted_app(pool: PgPool, areas: &str) -> Router {
    let config = Config::from_lookup(|key| match key {
        config::DATABASE_URL_VAR => Some(config::TEST_DATABASE_URL.to_owned()),
        config::BASE_URL_VAR => Some(config::TEST_BASE_URL.to_owned()),
        config::RATE_LIMITS_ENABLED_VAR => Some("false".to_owned()),
        config::RESTRICT_UNAUTHENTICATED_VAR => Some(areas.to_owned()),
        _ => None,
    })
    .expect("the fixed restriction config is valid");
    common::test_app_with_config(pool, config)
}

/// One account's four visibilities and the three readers who judge them:
/// what a stranger, a follower, a mentioned party and the author themselves
/// each reach on its status listing.
struct History {
    /// The account whose profile is being read.
    author: User,
    /// A stranger who follows nobody.
    stranger: User,
    /// A stranger with an accepted follow of [`Self::author`].
    follower: User,
    /// The party addressed by [`Self::direct`].
    addressee: User,
    /// The author's `public` post.
    public: Status,
    /// The author's `unlisted` post.
    unlisted: Status,
    /// The author's `private` post, for followers only.
    private: Status,
    /// The author's `direct` post, for [`Self::addressee`] only.
    direct: Status,
}

/// Builds a [`History`]: four accounts and the author's four visibilities.
async fn history(pool: &PgPool) -> History {
    let author = common::create_local_user(pool, "author").await;
    let stranger = common::create_local_user(pool, "stranger").await;
    let follower = common::create_local_user(pool, "follower").await;
    let addressee = common::create_local_user(pool, "addressee").await;
    accepted_follow(pool, &follower, &author).await;
    History {
        public: common::create_status(pool, &author, "public post", Visibility::Public).await,
        unlisted: common::create_status(pool, &author, "unlisted post", Visibility::Unlisted).await,
        private: common::create_status(pool, &author, "private post", Visibility::Private).await,
        direct: addressed(pool, &author, &[&addressee], Visibility::Direct, "a dm").await,
        author,
        stranger,
        follower,
        addressee,
    }
}

/// The tokens of a history's three readers, all granting `read:statuses`.
async fn tokens(pool: &PgPool, history: &History) -> (String, String, String) {
    (
        common::create_app_and_token(pool, &history.stranger, "read:statuses").await,
        common::create_app_and_token(pool, &history.follower, "read:statuses").await,
        common::create_app_and_token(pool, &history.addressee, "read:statuses").await,
    )
}

/// `public` and `unlisted` for the anonymous request and for a stranger —
/// never `private` or `direct`, which is what the ticket's "the viewer may
/// see" clause means.
#[tokio::test]
async fn a_stranger_reads_public_and_unlisted_only() {
    let db = common::test_db().await;
    let pool = db.pool().clone();
    let history = history(&pool).await;
    let (stranger, _follower, _addressee) = tokens(&pool, &history).await;
    let app = common::test_app(pool);
    let id = id_of_author(&history);

    let anonymous = statuses(&app, None, &id, "").await;
    assert_eq!(anonymous.status, StatusCode::OK, "auth is optional");
    assert_eq!(
        ids(&anonymous.json_value()),
        newest_first(&[&history.public, &history.unlisted]),
        "the anonymous page is public and unlisted"
    );
    let for_stranger = statuses(&app, Some(&stranger), &id, "").await;
    assert_eq!(
        ids(&for_stranger.json_value()),
        newest_first(&[&history.public, &history.unlisted]),
        "a follower-less user token reaches the same two rows"
    );
}

/// A follower adds the `private` rows, an addressed party adds the `direct`
/// one, and the account itself sees everything it ever wrote.
#[tokio::test]
async fn a_follower_a_party_and_the_author_see_more() {
    let db = common::test_db().await;
    let pool = db.pool().clone();
    let history = history(&pool).await;
    let (_stranger, follower, addressee) = tokens(&pool, &history).await;
    let owner = common::create_app_and_token(&pool, &history.author, "read:statuses").await;
    let app = common::test_app(pool);
    let id = id_of_author(&history);
    let all = [
        &history.direct,
        &history.private,
        &history.unlisted,
        &history.public,
    ];

    let for_follower = statuses(&app, Some(&follower), &id, "").await;
    assert_eq!(
        ids(&for_follower.json_value()),
        newest_first(&all[1..]),
        "a follower reaches `private` but not a DM they were not addressed in"
    );
    let for_party = statuses(&app, Some(&addressee), &id, "").await;
    assert_eq!(
        ids(&for_party.json_value()),
        newest_first(&[&history.direct, &history.public, &history.unlisted]),
        "an addressed party reaches `direct` but not `private`"
    );
    let for_owner = statuses(&app, Some(&owner), &id, "").await;
    assert_eq!(
        ids(&for_owner.json_value()),
        newest_first(&all),
        "the author's own token shows everything of theirs"
    );
}

/// The author's id as the route's path segment takes it.
fn id_of_author(history: &History) -> String {
    history.author.id.to_string()
}

/// A viewer the account blocked reads the empty list: the visibility
/// clause's rule, so the page is 200 with `[]`, not a 403 or a 404.
#[tokio::test]
async fn an_account_that_blocked_the_viewer_answers_the_empty_list() {
    let db = common::test_db().await;
    let pool = db.pool().clone();
    let alice = common::create_local_user(&pool, "alice").await;
    let bob = common::create_local_user(&pool, "bob").await;
    let post = common::create_status(&pool, &bob, "bob public", Visibility::Public).await;
    let blocked = block::block(&pool, BASE, &bob, &alice)
        .await
        .expect("bob blocks alice");
    assert_eq!(blocked.blocked_id, alice.id, "the block names alice");
    let token = common::create_app_and_token(&pool, &alice, "read:statuses").await;
    let app = common::test_app(pool);

    let page = statuses(&app, Some(&token), &bob.id.to_string(), "").await;
    assert_eq!(
        page.status,
        StatusCode::OK,
        "blocked readers are not refused"
    );
    assert_eq!(
        ids(&page.json_value()),
        Vec::<String>::new(),
        "and they see none of bob's public posts"
    );
    let unblocked = statuses(&app, None, &bob.id.to_string(), "").await;
    assert_eq!(
        ids(&unblocked.json_value()),
        newest_first(&[&post]),
        "the anonymous request still reads bob's public post"
    );
}

/// The account's own reblogs ride the page until `exclude_reblogs` takes
/// them; another account's boost of the author's post was never on it — it
/// is that account's row.
#[tokio::test]
async fn reblogs_are_in_until_exclude_reblogs() {
    let db = common::test_db().await;
    let pool = db.pool().clone();
    let alice = common::create_local_user(&pool, "alice").await;
    let carol = common::create_local_user(&pool, "carol").await;
    let own = common::create_status(&pool, &alice, "alice own", Visibility::Public).await;
    let boosted = common::create_status(&pool, &carol, "carol original", Visibility::Public).await;
    let alice_reblog = reblog(&pool, &alice, &boosted).await;
    let carol_reblog = reblog(&pool, &carol, &own).await;
    let app = common::test_app(pool);
    let id = alice.id.to_string();

    let whole = statuses(&app, None, &id, "").await;
    assert_eq!(
        ids(&whole.json_value()),
        newest_first(&[&alice_reblog, &own]),
        "alice's page holds her post and her boost, not carol's boost of it"
    );
    assert!(
        !ids(&whole.json_value()).contains(&id_of(&carol_reblog)),
        "another account's reblog is that account's row"
    );
    let no_reblogs = statuses(&app, None, &id, "?exclude_reblogs=true").await;
    assert_eq!(
        ids(&no_reblogs.json_value()),
        newest_first(&[&own]),
        "`exclude_reblogs` leaves the authored rows"
    );
}

/// `exclude_replies` drops the rows that answer another status; `only_media`
/// keeps the rows with an attachment.
#[tokio::test]
async fn exclude_replies_and_only_media_narrow_the_page() {
    let db = common::test_db().await;
    let pool = db.pool().clone();
    let alice = common::create_local_user(&pool, "alice").await;
    let root = common::create_status(&pool, &alice, "root post", Visibility::Public).await;
    let answer = reply(&pool, &alice, &root, "an answer").await;
    let pictured = common::create_status(&pool, &alice, "with a picture", Visibility::Public).await;
    attach_media(&pool, &alice, &pictured).await;
    let app = common::test_app(pool);
    let id = alice.id.to_string();

    let whole = statuses(&app, None, &id, "").await;
    assert_eq!(
        ids(&whole.json_value()),
        newest_first(&[&root, &answer, &pictured]),
        "before any filter the page is all three"
    );
    let no_replies = statuses(&app, None, &id, "?exclude_replies=true").await;
    assert_eq!(
        ids(&no_replies.json_value()),
        newest_first(&[&root, &pictured]),
        "`exclude_replies` keeps the roots"
    );
    let only_media = statuses(&app, None, &id, "?only_media=1").await;
    assert_eq!(
        ids(&only_media.json_value()),
        newest_first(&[&pictured]),
        "`only_media` keeps the attached row"
    );
}

/// `exclude_visibilities[]` drops named visibilities from what the viewer
/// reaches — the author's own `private` and `direct` rows included.
#[tokio::test]
async fn exclude_visibilities_drops_named_rows() {
    let db = common::test_db().await;
    let pool = db.pool().clone();
    let alice = common::create_local_user(&pool, "alice").await;
    let public = common::create_status(&pool, &alice, "public", Visibility::Public).await;
    let unlisted = common::create_status(&pool, &alice, "unlisted", Visibility::Unlisted).await;
    let private = common::create_status(&pool, &alice, "private", Visibility::Private).await;
    let token = common::create_app_and_token(&pool, &alice, "read:statuses").await;
    let app = common::test_app(pool);
    let id = alice.id.to_string();

    let whole = statuses(&app, Some(&token), &id, "").await;
    assert_eq!(
        ids(&whole.json_value()),
        newest_first(&[&public, &unlisted, &private]),
        "the author's own page starts out full"
    );
    let no_public = statuses(
        &app,
        Some(&token),
        &id,
        "?exclude_visibilities[]=public&exclude_visibilities[]=unlisted",
    )
    .await;
    assert_eq!(
        ids(&no_public.json_value()),
        newest_first(&[&private]),
        "the two named visibilities are gone"
    );
    let junk = statuses(&app, Some(&token), &id, "?exclude_visibilities[]=Private").await;
    assert_eq!(
        ids(&junk.json_value()),
        newest_first(&[&public, &unlisted, &private]),
        "a capitalization names no stored visibility and excludes nothing"
    );
}

/// `tagged` keeps the statuses carrying that hashtag name — normalized
/// against the stored spelling — and nothing else; a name no row carries,
/// and one that is nothing at all, answer the empty page.
#[tokio::test]
async fn tagged_keeps_the_rows_carrying_one_tag() {
    let db = common::test_db().await;
    let pool = db.pool().clone();
    let alice = common::create_local_user(&pool, "alice").await;
    let rust = common::create_status(&pool, &alice, "today in #Rust", Visibility::Public).await;
    let both = common::create_status(&pool, &alice, "#rust and #elixir", Visibility::Public).await;
    let elixir = common::create_status(&pool, &alice, "just #elixir", Visibility::Public).await;
    let plain = common::create_status(&pool, &alice, "untagged", Visibility::Public).await;
    let app = common::test_app(pool);
    let id = alice.id.to_string();

    let for_rust = statuses(&app, None, &id, "?tagged=%23RUST").await;
    assert_eq!(
        ids(&for_rust.json_value()),
        newest_first(&[&rust, &both]),
        "the name is trimmed of `#` and folded to the stored spelling"
    );
    let for_elixir = statuses(&app, None, &id, "?tagged=elixir").await;
    assert_eq!(
        ids(&for_elixir.json_value()),
        newest_first(&[&both, &elixir]),
        "the other tag selects its own rows"
    );
    let unused = statuses(&app, None, &id, "?tagged=neverused").await;
    assert_eq!(
        ids(&unused.json_value()),
        Vec::<String>::new(),
        "a tag no row carries answers the empty page"
    );
    let blank = statuses(&app, None, &id, "?tagged=").await;
    assert_eq!(
        ids(&blank.json_value()),
        Vec::<String>::new(),
        "and so does an empty `tagged`, which is no tag either"
    );
    let whole = statuses(&app, None, &id, "").await;
    assert_eq!(
        ids(&whole.json_value()),
        newest_first(&[&rust, &both, &elixir, &plain]),
        "without the parameter every row rides"
    );
}

/// `with_muted` lifts the viewer's own mute of the account; the author's
/// side of a block is not the viewer's to lift.
#[tokio::test]
async fn with_muted_lifts_the_viewers_own_exclusions() {
    let db = common::test_db().await;
    let pool = db.pool().clone();
    let alice = common::create_local_user(&pool, "alice").await;
    let bob = common::create_local_user(&pool, "bob").await;
    let post = common::create_status(&pool, &bob, "bob public", Visibility::Public).await;
    let muted = mute::mute(&pool, &alice, &bob, MuteOptions::default())
        .await
        .expect("alice mutes bob");
    assert_eq!(muted.muted_id, bob.id, "the mute names bob");
    let token = common::create_app_and_token(&pool, &alice, "read:statuses").await;
    let app = common::test_app(pool);
    let id = bob.id.to_string();

    let hidden = statuses(&app, Some(&token), &id, "").await;
    assert_eq!(
        ids(&hidden.json_value()),
        Vec::<String>::new(),
        "a muted account's page is empty for the muter"
    );
    let shown = statuses(&app, Some(&token), &id, "?with_muted=true").await;
    assert_eq!(
        ids(&shown.json_value()),
        newest_first(&[&post]),
        "`with_muted=true` brings bob's posts back"
    );
}

/// `pinned` answers the account's pins newest pin first — not newest post
/// first, the order the same two rows come back in when pinned by hand in
/// the other order — ignores the pagination keys and sends no `Link`
/// header.
#[tokio::test]
async fn pinned_answers_in_pin_order_with_no_link_header() {
    let db = common::test_db().await;
    let pool = db.pool().clone();
    let alice = common::create_local_user(&pool, "alice").await;
    let older = common::create_status(&pool, &alice, "the older post", Visibility::Public).await;
    let newer = common::create_status(&pool, &alice, "the newer post", Visibility::Public).await;
    let _unpinned = common::create_status(&pool, &alice, "never pinned", Visibility::Public).await;
    pin(&pool, &alice, &older).await;
    pin(&pool, &alice, &newer).await;
    let app = common::test_app(pool);
    let id = alice.id.to_string();

    let page = statuses(
        &app,
        None,
        &id,
        &format!("?pinned=true&limit=1&max_id={CURSOR}"),
    )
    .await;
    assert_eq!(
        page.status,
        StatusCode::OK,
        "the pagination keys are ignored, not refused"
    );
    assert_eq!(
        ids(&page.json_value()),
        vec![id_of(&newer), id_of(&older)],
        "newest pin first, which is the opposite of newest post first"
    );
    assert_eq!(
        page.header("link"),
        None,
        "the featured list has no pages to link"
    );
}

/// `pinned` still judges every pin by the viewer, and composes with the
/// other filters: a follower's `private` pin shows for the author only, and
/// `tagged` narrows the pins to the rows carrying the tag.
#[tokio::test]
async fn pinned_respects_the_viewer_and_the_other_filters() {
    let db = common::test_db().await;
    let pool = db.pool().clone();
    let alice = common::create_local_user(&pool, "alice").await;
    let tagged = common::create_status(&pool, &alice, "public #shown", Visibility::Public).await;
    let other = common::create_status(&pool, &alice, "public #hidden", Visibility::Public).await;
    let secret = common::create_status(&pool, &alice, "private #shown", Visibility::Private).await;
    pin(&pool, &alice, &tagged).await;
    pin(&pool, &alice, &other).await;
    pin(&pool, &alice, &secret).await;
    let app = common::test_app(pool);
    let id = alice.id.to_string();

    let anonymous = statuses(&app, None, &id, "?pinned=true").await;
    assert_eq!(
        ids(&anonymous.json_value()),
        newest_first(&[&tagged, &other]),
        "an anonymous reader reaches the public pins only"
    );
    let filtered = statuses(&app, None, &id, "?pinned=true&tagged=shown").await;
    assert_eq!(
        ids(&filtered.json_value()),
        vec![id_of(&tagged)],
        "`tagged` narrows the pins to the public row carrying the tag"
    );
}

/// The page walks the id cursor under the `Link` header: `limit` cuts it,
/// the header names the next and prev targets over the page's oldest and
/// newest ids and repeats the request's filters, `max_id` continues from
/// there, and an empty page carries no `Link` header.
#[tokio::test]
async fn pages_walk_the_id_cursor_under_link_headers() {
    let db = common::test_db().await;
    let pool = db.pool().clone();
    let alice = common::create_local_user(&pool, "alice").await;
    let oldest = common::create_status(&pool, &alice, "#walk first", Visibility::Public).await;
    let middle = common::create_status(&pool, &alice, "#walk second", Visibility::Public).await;
    let newest = common::create_status(&pool, &alice, "#walk third", Visibility::Public).await;
    let app = common::test_app(pool);
    let id = alice.id.to_string();
    let path = format!("{ROUTE}/{id}/statuses");

    let first = statuses(&app, None, &id, "?limit=2&tagged=walk").await;
    assert_eq!(
        ids(&first.json_value()),
        newest_first(&[&middle, &newest]),
        "the first page is the two newest rows"
    );
    let expected = format!(
        "<{BASE}{path}?limit=2&tagged=walk&max_id={}>; rel=\"next\", \
         <{BASE}{path}?limit=2&tagged=walk&min_id={}>; rel=\"prev\"",
        id_of(&middle),
        id_of(&newest),
    );
    assert_eq!(
        first.header("link"),
        Some(expected.as_str()),
        "the cursors are the page's ends and the filter rides on both links"
    );

    let second = statuses(
        &app,
        None,
        &id,
        &format!("?limit=2&max_id={}", id_of(&middle)),
    )
    .await;
    assert_eq!(
        ids(&second.json_value()),
        newest_first(&[&oldest]),
        "the next target answers the row older than the cursor"
    );
    let empty = statuses(&app, None, &id, &format!("?since_id={}", id_of(&newest))).await;
    assert_eq!(empty.status, StatusCode::OK, "nothing newer is still a 200");
    assert_eq!(
        ids(&empty.json_value()),
        Vec::<String>::new(),
        "and the empty page"
    );
    assert_eq!(empty.header("link"), None, "with no Link header at all");
}

/// A remote account reads like a local one: its mirrored public rows are on
/// its page for the anonymous request.
#[tokio::test]
async fn a_remote_account_reads_like_a_local_one() {
    let db = common::test_db().await;
    let pool = db.pool().clone();
    let bob = common::create_remote_user(&pool, "bob", "remote.example").await;
    let public = common::create_status(&pool, &bob, "remote public", Visibility::Public).await;
    let private = common::create_status(&pool, &bob, "remote private", Visibility::Private).await;
    let app = common::test_app(pool);
    let id = bob.id.to_string();

    let anonymous = statuses(&app, None, &id, "").await;
    assert_eq!(
        ids(&anonymous.json_value()),
        newest_first(&[&public]),
        "the mirror's public post is on its page"
    );
    assert!(
        !ids(&anonymous.json_value()).contains(&id_of(&private)),
        "and its private post is not"
    );
    assert_eq!(
        ids(&anonymous.json_value()).len(),
        1,
        "the entity list is the one live row"
    );
}

/// A `RESTRICT_UNAUTHENTICATED` profile half answers an anonymous request
/// with the 403 `USER_REQUIRED`, the other half unaffected, and a presented
/// user token never refused for it.
#[tokio::test]
async fn restricted_profiles_refuse_anonymous_reads() {
    let db = common::test_db().await;
    let pool = db.pool().clone();
    let alice = common::create_local_user(&pool, "alice").await;
    let bob = common::create_remote_user(&pool, "bobby", "other.example").await;
    let public = common::create_status(&pool, &alice, "local public", Visibility::Public).await;
    let token = common::create_app_and_token(&pool, &alice, "read:statuses").await;
    let app = restricted_app(pool, "profiles_local");
    let local = alice.id.to_string();
    let remote = bob.id.to_string();

    let refused = statuses(&app, None, &local, "").await;
    assert_eq!(
        refused.status,
        StatusCode::FORBIDDEN,
        "a closed profile half"
    );
    assert_eq!(refused.text(), USER_REQUIRED, "refuses the anonymous read");
    let open = statuses(&app, Some(&token), &local, "").await;
    assert_eq!(
        open.status,
        StatusCode::OK,
        "a user token is never refused for the restriction"
    );
    assert_eq!(
        ids(&open.json_value()),
        newest_first(&[&public]),
        "and it reads its own page"
    );
    let other_half = statuses(&app, None, &remote, "").await;
    assert_eq!(
        other_half.status,
        StatusCode::OK,
        "`profiles_local` says nothing about a mirror"
    );
}

/// The remote half closes on its own name, and the open instance default
/// closes neither.
#[tokio::test]
async fn the_remote_half_and_the_default_are_independent() {
    let db = common::test_db().await;
    let pool = db.pool().clone();
    let alice = common::create_local_user(&pool, "alice").await;
    let bob = common::create_remote_user(&pool, "bobby", "other.example").await;
    let app = restricted_app(pool.clone(), "profiles_remote");
    let open = common::test_app(pool);

    let refused = statuses(&app, None, &bob.id.to_string(), "").await;
    assert_eq!(
        refused.status,
        StatusCode::FORBIDDEN,
        "`profiles_remote` closes the mirror"
    );
    let local = statuses(&app, None, &alice.id.to_string(), "").await;
    assert_eq!(
        local.status,
        StatusCode::OK,
        "and says nothing about a local account"
    );
    assert_eq!(
        statuses(&open, None, &bob.id.to_string(), "").await.status,
        StatusCode::OK,
        "an unrestricted instance answers both halves"
    );
}

/// Every account miss is the same 404 — a malformed id, an unknown one and a
/// deactivated account — and a malformed cursor is the 400 `PageParams`
/// gives everywhere.
#[tokio::test]
async fn account_misses_are_404_and_a_bad_cursor_is_400() {
    let db = common::test_db().await;
    let pool = db.pool().clone();
    let ghost = common::create_local_user_with(&pool, "ghost", |mut new| {
        new.is_active = Flag::new(false);
        new
    })
    .await;
    let app = common::test_app(pool);

    for id in ["not-an-id", "01934567-89ab-7cde-f012-3456789abcde"] {
        let response = statuses(&app, None, id, "").await;
        assert_eq!(response.status, StatusCode::NOT_FOUND, "`{id}` is a 404");
        assert_eq!(response.text(), NOT_FOUND, "the shared body for `{id}`");
    }
    let deactivated = statuses(&app, None, &ghost.id.to_string(), "").await;
    assert_eq!(
        deactivated.status,
        StatusCode::NOT_FOUND,
        "a deactivated account is not served"
    );
    assert_eq!(deactivated.text(), NOT_FOUND, "the shared body again");

    let cursor = statuses(&app, None, &ghost.id.to_string(), "?max_id=nope").await;
    assert_eq!(
        cursor.status,
        StatusCode::BAD_REQUEST,
        "a bad cursor is 400"
    );
    assert_eq!(cursor.text(), INVALID_ID, "with PageParams' wording");
}

/// The auth gates: a user token without `read:statuses` is the scope 403, an
/// invalid bearer the 401, and an app-only token — which names no user —
/// reads the anonymous page.
#[tokio::test]
async fn the_auth_gates_answer_scope_invalidity_and_app_tokens() {
    let db = common::test_db().await;
    let pool = db.pool().clone();
    let alice = common::create_local_user(&pool, "alice").await;
    let public = common::create_status(&pool, &alice, "public post", Visibility::Public).await;
    let no_scope = common::create_app_and_token(&pool, &alice, "read:accounts").await;
    let app_only = common::create_app_only_token(&pool, "read:statuses").await;
    let app = common::test_app(pool);
    let id = alice.id.to_string();

    let refused = statuses(&app, Some(&no_scope), &id, "").await;
    assert_eq!(refused.status, StatusCode::FORBIDDEN, "no scope is a 403");
    assert_eq!(refused.text(), NO_SCOPE, "with the scope wording");
    let broken = statuses(&app, Some("nonsense"), &id, "").await;
    assert_eq!(
        broken.status,
        StatusCode::UNAUTHORIZED,
        "a bad token is 401"
    );
    assert_eq!(broken.text(), INVALID_TOKEN, "with the invalid-token body");
    let by_app = statuses(&app, Some(&app_only), &id, "").await;
    assert_eq!(
        by_app.status,
        StatusCode::OK,
        "an app-only token proceeds unauthenticated"
    );
    assert_eq!(
        ids(&by_app.json_value()),
        newest_first(&[&public]),
        "so it reads the anonymous page"
    );
}
