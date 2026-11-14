//! End-to-end discovery and federation scenarios (WBE-223, ticket
//! scenarios 6, 7 and 8): the search trio over the full visibility matrix,
//! the v1 phrase filters applied server-side to the timeline listings
//! Pleroma's `Pleroma.Filter` restricts, the Atom/RSS feeds and the
//! rich-media link card fed by the job queue, and a remote mention
//! resolved through `WebFinger` against a loopback mock peer whose inbox
//! the signed `Create` is then delivered to.
//!
//! Everything is driven over HTTP through [`common`]'s router (the world
//! is built with real creates, follows and filter writes), the mock
//! upstream follows the loopback mock pattern of `follow_by_uri.rs`, and
//! background work is drained with [`common::run_all_jobs`]. The
//! per-endpoint refusals stay owned by the area files (`search.rs`,
//! `filters.rs`, `timelines.rs`, `feeds.rs`, `rich_media.rs`); this module
//! owns the cross-surface journeys, like `e2e_interactions.rs` owns the
//! interaction ones.

#![cfg(test)]

use std::{
    collections::HashMap,
    sync::{
        Arc, Mutex,
        atomic::{AtomicUsize, Ordering},
    },
};

use axum::{
    Router,
    body::Bytes,
    extract::{Path, Query, State},
    http::{HeaderMap, StatusCode, header::CONTENT_TYPE},
    response::{IntoResponse as _, Response},
    routing::{get, post},
};
use serde_json::{Value, json};
use sqlx::PgPool;
use tokio::{net::TcpListener, task::JoinHandle};
use wired_be::{
    models::user::{self, User},
    state::AppState,
};

use crate::common;

/// The status create/read route.
const STATUSES: &str = "/api/v1/statuses";
/// The v1 combined search.
const SEARCH_V1: &str = "/api/v1/search";
/// The v2 combined search.
const SEARCH_V2: &str = "/api/v2/search";
/// The account search.
const ACCOUNT_SEARCH: &str = "/api/v1/accounts/search";
/// The v1 filters CRUD route.
const FILTERS: &str = "/api/v1/filters";
/// The public timeline read.
const PUBLIC: &str = "/api/v1/timelines/public";
/// The home timeline read.
const HOME: &str = "/api/v1/timelines/home";
/// The caller's notification listing.
const NOTIFICATIONS: &str = "/api/v1/notifications";
/// The scopes every scenario actor carries: reads, writes and follows.
const SCOPES: &str = "read write follow";
/// The instance origin of [`common::test_config`].
const BASE: &str = "https://test.example";
/// The `ActivityStreams` public collection, as the `Create` addresses it.
const AS_PUBLIC: &str = "https://www.w3.org/ns/activitystreams#Public";

/// A local user named `nickname` plus a bearer token granting [`SCOPES`].
async fn user(pool: &PgPool, nickname: &str) -> (User, String) {
    let created = common::create_local_user(pool, nickname).await;
    let token = common::create_app_and_token(pool, &created, SCOPES).await;
    (created, token)
}

/// The router and the [`AppState`] behind it over `pool`: requests ride
/// the router, [`common::run_all_jobs`] drains on the state.
fn app_and_state(pool: PgPool) -> (Router, AppState) {
    let state = common::test_app_state(pool, common::test_config());
    (wired_be::app(state.clone()), state)
}

/// `POST /api/v1/statuses` as `token` with the JSON `body`, asserted 200
/// and answered as the status entity it wrote.
async fn created(app: &Router, token: &str, body: Value) -> Value {
    let response = common::TestRequest::post(STATUSES)
        .bearer(token)
        .json(&body)
        .send(app)
        .await;
    common::assert_answer(&response, StatusCode::OK, "the fixture post creates");
    response.json_value()
}

/// A status of `text` and `visibility` by `token`, answered as its entity.
async fn published(app: &Router, token: &str, text: &str, visibility: &str) -> Value {
    created(
        app,
        token,
        json!({"status": text, "visibility": visibility}),
    )
    .await
}

/// `GET`s `path` with `token`, answered raw.
async fn read(app: &Router, token: &str, path: &str) -> common::TestResponse {
    common::TestRequest::get(path).bearer(token).send(app).await
}

/// The ids of a rendered status array, in the order it came back.
fn ids_of(listing: &Value) -> Vec<String> {
    listing
        .as_array()
        .expect("a status listing is an array")
        .iter()
        .map(|status| {
            status["id"]
                .as_str()
                .expect("every status has a string id")
                .to_owned()
        })
        .collect()
}

/// The answered listing's ids, newest first.
fn page_ids(response: &common::TestResponse) -> Vec<String> {
    common::assert_answer(response, StatusCode::OK, "the timeline reads");
    ids_of(&response.json_value())
}

/// The `statuses` arm of an answered search document.
fn searched(response: &common::TestResponse) -> Vec<String> {
    common::assert_answer(response, StatusCode::OK, "the search answers");
    ids_of(&response.json_value()["statuses"])
}

/// The `id` of a created status entity.
fn id_of(entity: &Value) -> String {
    entity["id"]
        .as_str()
        .expect("the entity has a string id")
        .to_owned()
}

/// A local follow of `target` by the holder of `token`.
async fn follow(app: &Router, token: &str, target: &User) {
    let response = common::TestRequest::post(&format!("/api/v1/accounts/{}/follow", target.id))
        .bearer(token)
        .send(app)
        .await;
    common::assert_answer(&response, StatusCode::OK, "the fixture follow is granted");
}

/// `POST /api/v1/filters` as `token`, asserted 200 and answered as the
/// filter entity it wrote.
async fn filter_created(app: &Router, token: &str, body: Value) -> Value {
    let response = common::TestRequest::post(FILTERS)
        .bearer(token)
        .json(&body)
        .send(app)
        .await;
    common::assert_answer(&response, StatusCode::OK, "the fixture filter creates");
    response.json_value()
}

/// The phrases of an answered `GET /api/v1/filters`, in listing order.
fn listed_phrases(response: &common::TestResponse) -> Vec<String> {
    common::assert_answer(response, StatusCode::OK, "the filter listing reads");
    response
        .json_value()
        .as_array()
        .expect("the listing is an array")
        .iter()
        .map(|filter| {
            filter["phrase"]
                .as_str()
                .expect("every filter has a phrase")
                .to_owned()
        })
        .collect()
}

/// The types of an answered notification listing, in listing order.
fn notification_types(response: &common::TestResponse) -> Vec<String> {
    common::assert_answer(response, StatusCode::OK, "the notification listing reads");
    response
        .json_value()
        .as_array()
        .expect("the listing is an array")
        .iter()
        .map(|row| {
            row["type"]
                .as_str()
                .expect("every notification has a type")
                .to_owned()
        })
        .collect()
}

/// Scenario 6, the status-search visibility matrix: Alice posts one status
/// per visibility — the public, private and direct ones sharing the word
/// `zeta`, the unlisted one carrying its own word `umbra`, so the
/// stranger's shared-word answer is *only* the public row while the
/// unlisted status still searches as the publicly readable post the
/// visibility model says it is. Mallory is a stranger, Bob a follower of
/// Alice, Carol the mentioned party of the direct: each `v2` answer is
/// exactly what its viewer may see, and `v1` answers the same page.
#[tokio::test]
async fn search_answers_each_viewer_at_their_own_visibility() {
    let db = common::test_db().await;
    let pool = db.pool().clone();
    let (alice, alice_token) = user(&pool, "alice").await;
    let (_bob, bob_token) = user(&pool, "bob").await;
    let (_, carol_token) = user(&pool, "carol").await;
    let (_, mallory_token) = user(&pool, "mallory").await;
    let app = common::test_app(pool);

    let public = published(&app, &alice_token, "zeta probe public #findme", "public").await;
    let unlisted = published(&app, &alice_token, "umbra light note", "unlisted").await;
    let private = published(&app, &alice_token, "zeta probe private", "private").await;
    let direct = created(
        &app,
        &alice_token,
        json!({"status": "zeta probe direct @carol", "visibility": "direct"}),
    )
    .await;
    let pub_id = id_of(&public);
    let pri_id = id_of(&private);
    let dir_id = id_of(&direct);
    let zeta = format!("{SEARCH_V2}?q=zeta&type=statuses");

    let stranger = searched(&read(&app, &mallory_token, &zeta).await);
    assert_eq!(
        stranger,
        vec![pub_id.clone()],
        "the stranger's v2 status search finds only the public match"
    );
    let stranger_v1 = searched(
        &read(
            &app,
            &mallory_token,
            &format!("{SEARCH_V1}?q=zeta&type=statuses"),
        )
        .await,
    );
    assert_eq!(
        stranger_v1,
        vec![pub_id.clone()],
        "v1 answers the same page"
    );
    let umbra = searched(
        &read(
            &app,
            &mallory_token,
            &format!("{SEARCH_V2}?q=umbra&type=statuses"),
        )
        .await,
    );
    assert_eq!(
        umbra,
        vec![id_of(&unlisted)],
        "the unlisted row is searchable by anyone, its own word finding it"
    );

    follow(&app, &bob_token, &alice).await;
    let follower = searched(&read(&app, &bob_token, &zeta).await);
    assert_eq!(
        follower,
        vec![pri_id.clone(), pub_id.clone()],
        "the follower's answer adds the private post, newest first"
    );

    let mentioned = searched(&read(&app, &carol_token, &zeta).await);
    assert_eq!(
        mentioned,
        vec![dir_id.clone(), pub_id.clone()],
        "the mentioned party sees the direct, not the private"
    );
    let author = searched(&read(&app, &alice_token, &zeta).await);
    assert_eq!(
        author,
        vec![dir_id, pri_id, pub_id],
        "the author sees all three zeta posts, newest first"
    );
}

/// Scenario 6, the account and hashtag arms: the stranger's account search
/// finds Alice on the flat route and inside the combined document; the
/// `#findme` tag a public post created answers both versions' hashtag arm
/// — v1's plain name, v2's `Tag` entity — and an untyped `#` query fills
/// nothing else. The combined untyped document answers all three arms at
/// once, and the follower's home page keeps the same visibility answer the
/// search gives, so a regression in the shared clause shows on both.
#[tokio::test]
async fn search_fills_the_account_and_hashtag_arms() {
    let db = common::test_db().await;
    let pool = db.pool().clone();
    let (alice, alice_token) = user(&pool, "alice").await;
    let (_bob, bob_token) = user(&pool, "bob").await;
    let (_, mallory_token) = user(&pool, "mallory").await;
    let app = common::test_app(pool);
    let secret = published(&app, &alice_token, "zeta on home only", "private").await;
    let tagged = published(&app, &alice_token, "zeta probe public #findme", "public").await;

    let accounts = read(&app, &mallory_token, &format!("{ACCOUNT_SEARCH}?q=alice")).await;
    common::assert_answer(&accounts, StatusCode::OK, "the account search answers");
    assert_eq!(
        accounts.json_value()[0]["id"],
        json!(alice.id.to_string()),
        "the flat array leads with the matching account"
    );

    let combined = read(&app, &mallory_token, &format!("{SEARCH_V2}?q=alice")).await;
    common::assert_answer(&combined, StatusCode::OK, "the untyped search answers");
    let document = combined.json_value();
    assert_eq!(
        document["accounts"][0]["acct"],
        json!("alice"),
        "the combined document fills the accounts arm"
    );
    assert!(
        document["statuses"].as_array().is_some_and(Vec::is_empty),
        "no status carries the word alice"
    );

    let v1 = read(&app, &mallory_token, &format!("{SEARCH_V1}?q=%23findme")).await;
    common::assert_answer(&v1, StatusCode::OK, "the v1 hashtag search answers");
    let v1_body = v1.json_value();
    assert_eq!(
        v1_body["hashtags"],
        json!(["findme"]),
        "v1 spells plain names"
    );
    assert!(
        v1_body["statuses"].as_array().is_some_and(Vec::is_empty),
        "a `#` query is a hashtag-only search: no status arm"
    );
    let v2 = read(&app, &mallory_token, &format!("{SEARCH_V2}?q=%23findme")).await;
    assert_eq!(
        v2.json_value()["hashtags"][0],
        json!({ "name": "findme", "url": format!("{BASE}/tags/findme"), "history": [] }),
        "v2 answers the Tag entity with the empty history"
    );

    follow(&app, &bob_token, &alice).await;
    let home = read(&app, &bob_token, HOME).await;
    assert_eq!(
        page_ids(&home),
        vec![id_of(&tagged), id_of(&secret)],
        "the follower's home page carries both alice's posts, newest first"
    );
    let searched_home = searched(
        &read(
            &app,
            &bob_token,
            &format!("{SEARCH_V2}?q=zeta&type=statuses"),
        )
        .await,
    );
    assert!(
        searched_home.contains(&id_of(&secret)),
        "and the follower's search agrees it is visible"
    );
}

/// Scenario 2, the `home` filter: an irreversible home filter drops the
/// whole-word matches from the viewer's home page — the public page, the
/// same author's followers aside, stands untouched. Pleroma's
/// `restrict_filtered` exempts the actor's own posts and Pleroma's phrase
/// test is whole-word by default, and both arms show here: `bob`'s own
/// `spam diary` stays on his home page, and `spammer` never matches a
/// `spam` filter.
#[tokio::test]
async fn the_home_filter_hides_matches_from_home_alone() {
    let db = common::test_db().await;
    let pool = db.pool().clone();
    let (alice, alice_token) = user(&pool, "alice").await;
    let (_bob, bob_token) = user(&pool, "bob").await;
    let app = common::test_app(pool);
    follow(&app, &bob_token, &alice).await;

    let digest = published(&app, &alice_token, "the weekly spam digest", "public").await;
    let today = published(&app, &alice_token, "all about spam today", "public").await;
    let glued = published(&app, &alice_token, "spammer confessions", "public").await;
    let clean = published(&app, &alice_token, "clean thread", "public").await;
    let own = published(&app, &bob_token, "my own spam diary", "public").await;
    let every = || {
        vec![
            id_of(&own),
            id_of(&clean),
            id_of(&glued),
            id_of(&today),
            id_of(&digest),
        ]
    };
    assert_eq!(
        page_ids(&read(&app, &bob_token, HOME).await),
        every(),
        "home holds the whole fixture before any filter"
    );

    let _home_filter = filter_created(
        &app,
        &bob_token,
        json!({"phrase": "spam", "context": ["home"], "irreversible": true}),
    )
    .await;
    assert_eq!(
        page_ids(&read(&app, &bob_token, HOME).await),
        vec![id_of(&own), id_of(&clean), id_of(&glued)],
        "the home filter drops the whole-word matches; the glued word and \
         the exempt own post stand"
    );
    assert_eq!(
        page_ids(&read(&app, &bob_token, PUBLIC).await),
        every(),
        "a home-context filter hides nothing from the public page"
    );
    assert_eq!(
        page_ids(&read(&app, &alice_token, HOME).await),
        vec![id_of(&clean), id_of(&glued), id_of(&today), id_of(&digest)],
        "filters are per-user: alice's home page never heard of bob's"
    );
}

/// Scenario 2, the `public` filters: an irreversible public filter drops
/// its match from the public page only, a warn (non-irreversible) filter
/// of the same context hides its match from the page too — with no
/// per-status `filtered` marker on a v1 listing, drop and hide are the
/// same act — and deleting a filter or running past its expiry brings the
/// rows back.
#[tokio::test]
async fn the_public_filters_drop_and_their_removal_restores() {
    let db = common::test_db().await;
    let pool = db.pool().clone();
    let (_alice, alice_token) = user(&pool, "alice").await;
    let (_, bob_token) = user(&pool, "bob").await;
    let app = common::test_app(pool.clone());

    let digest = published(&app, &alice_token, "the weekly spam digest", "public").await;
    let thread = published(&app, &alice_token, "clean thread", "public").await;
    let kept = published(&app, &alice_token, "an unrelated note", "public").await;

    let warned = filter_created(
        &app,
        &bob_token,
        json!({"phrase": "thread", "context": ["public"]}),
    )
    .await;
    let irreversible = filter_created(
        &app,
        &bob_token,
        json!({"phrase": "digest", "context": ["public"], "irreversible": true}),
    )
    .await;
    let hidden = page_ids(&read(&app, &bob_token, PUBLIC).await);
    assert_eq!(
        hidden,
        vec![id_of(&kept)],
        "both matching rows leave the public page, warn and irreversible alike"
    );
    assert_eq!(
        listed_phrases(&read(&app, &bob_token, FILTERS).await),
        vec!["digest", "thread"],
        "the filter listing answers both, newest first"
    );

    let deleted =
        common::TestRequest::delete(&format!("{FILTERS}/{}", warned["id"].as_str().expect("id")))
            .bearer(&bob_token)
            .send(&app)
            .await;
    common::assert_answer(&deleted, StatusCode::OK, "the warn filter deletes");
    let after_removal = page_ids(&read(&app, &bob_token, PUBLIC).await);
    assert_eq!(
        after_removal,
        vec![id_of(&kept), id_of(&thread)],
        "the thread post came back with its filter gone"
    );

    let expired = sqlx::query(
        "UPDATE filters SET expires_at = now() - interval '1 hour' WHERE id = $1::uuid",
    )
    .bind(irreversible["id"].as_str().expect("id"))
    .execute(&pool)
    .await
    .expect("the expiry backdate runs");
    assert_eq!(expired.rows_affected(), 1, "and it hit the digest filter");
    let after_expiry = page_ids(&read(&app, &bob_token, PUBLIC).await);
    assert_eq!(
        after_expiry,
        vec![id_of(&kept), id_of(&thread), id_of(&digest)],
        "the expired filter hides nothing any more"
    );
    assert_eq!(
        listed_phrases(&read(&app, &bob_token, FILTERS).await),
        Vec::<String>::new(),
        "and an expired filter leaves the active listing"
    );
}

/// Scenario 2, the `notifications` filter: Pleroma's `Notification`
/// `exclude_filtered` twin. The recipient's own posts and the statusless
/// rows stand exempt; a home-context filter of the same phrase hides
/// nothing here; the notifications filter hides the matching mention from
/// the flat listing, and deleting it brings the row back.
#[tokio::test]
async fn the_notifications_filter_hides_matching_mentions() {
    let db = common::test_db().await;
    let pool = db.pool().clone();
    let (alice, alice_token) = user(&pool, "alice").await;
    let (bob, bob_token) = user(&pool, "bob").await;
    let app = common::test_app(pool);

    let alert = published(&app, &alice_token, "a spam alert lands here @bob", "public").await;
    follow(&app, &alice_token, &bob).await;
    follow(&app, &bob_token, &alice).await;
    assert_eq!(
        notification_types(&read(&app, &bob_token, NOTIFICATIONS).await),
        vec!["follow", "mention"],
        "bob's page holds alice's follow and her mention, newest first \
         (bob's own follow answers on alice's page, not here)"
    );

    let _home_filter = filter_created(
        &app,
        &bob_token,
        json!({"phrase": "spam", "context": ["home"], "irreversible": true}),
    )
    .await;
    assert_eq!(
        notification_types(&read(&app, &bob_token, NOTIFICATIONS).await),
        vec!["follow", "mention"],
        "a home filter hides no notification, and the mention's post is \
         hidden from home"
    );
    assert!(
        !page_ids(&read(&app, &bob_token, HOME).await).contains(&id_of(&alert)),
        "the same filter does hide it from the home page"
    );

    let spam_filter = filter_created(
        &app,
        &bob_token,
        json!({"phrase": "spam", "context": ["notifications"], "irreversible": true}),
    )
    .await;
    assert_eq!(
        notification_types(&read(&app, &bob_token, NOTIFICATIONS).await),
        vec!["follow"],
        "the notifications filter hides the mention and spares the \
         statusless follow row"
    );

    let deleted = common::TestRequest::delete(&format!(
        "{FILTERS}/{}",
        spam_filter["id"].as_str().expect("id")
    ))
    .bearer(&bob_token)
    .send(&app)
    .await;
    common::assert_answer(&deleted, StatusCode::OK, "the notifications filter deletes");
    assert_eq!(
        notification_types(&read(&app, &bob_token, NOTIFICATIONS).await),
        vec!["follow", "mention"],
        "and the mention notification stands again"
    );
}

/// Scenario 7, the feeds: the public roots an account posts through the
/// API surface in its Atom and RSS feeds, newest first, while a reply, an
/// unlisted, a private and a direct post stay out of them; the tag feed
/// carries the tagged public post and not the tagged unlisted one.
#[tokio::test]
async fn api_posts_surface_in_the_atom_and_rss_feeds() {
    let db = common::test_db().await;
    let pool = db.pool().clone();
    let (_alice, alice_token) = user(&pool, "alice").await;
    let (_carol, _carol_token) = user(&pool, "carol").await;
    let app = common::test_app(pool);

    let root = published(&app, &alice_token, "feed root one #feedsweeper", "public").await;
    let child = created(
        &app,
        &alice_token,
        json!({
            "status": "feed reply child",
            "visibility": "public",
            "in_reply_to_id": id_of(&root),
        }),
    )
    .await;
    let unlisted = published(&app, &alice_token, "feed unlisted #feedsweeper", "unlisted").await;
    let private = published(&app, &alice_token, "feed private words", "private").await;
    let direct = created(
        &app,
        &alice_token,
        json!({
            "status": "feed direct words @carol",
            "visibility": "direct",
        }),
    )
    .await;
    let newest = published(&app, &alice_token, "feed root two", "public").await;

    for path in ["/users/alice/feed.atom", "/users/alice/feed.rss"] {
        let response = common::TestRequest::get(path).send(&app).await;
        common::assert_answer(&response, StatusCode::OK, "the account feed reads");
        let body = response.text();
        assert!(
            body.contains(uri_of(&newest)) && body.contains(uri_of(&root)),
            "{path} carries the public roots"
        );
        assert!(
            body.find(uri_of(&newest)) < body.find(uri_of(&root)),
            "{path} answers newest first"
        );
        for absent in [&child, &unlisted, &private, &direct] {
            assert!(
                !body.contains(uri_of(absent)),
                "{path} hides the reply, unlisted, private or direct post"
            );
        }
    }

    let tag_feed = common::TestRequest::get("/tags/feedsweeper.atom")
        .send(&app)
        .await;
    common::assert_answer(&tag_feed, StatusCode::OK, "the tag feed reads");
    assert_eq!(
        tag_feed.header("content-type"),
        Some("application/atom+xml; charset=utf-8"),
        "the suffix fixes the format"
    );
    let body = tag_feed.text();
    assert!(
        body.contains(uri_of(&root)),
        "the tagged public post rides the tag feed"
    );
    assert!(
        !body.contains(uri_of(&unlisted)),
        "the tagged unlisted post does not"
    );
}

/// The `uri` a status entity carries.
fn uri_of(entity: &Value) -> &str {
    entity["uri"].as_str().expect("the entity has a uri")
}

/// Scenario 7, the link card: a mock page answers `/page` with Open Graph
/// tags; a status linking to it renders `card: null` until the job queue
/// is drained, and then carries the cached card on the show route and the
/// standalone card endpoint alike.
#[tokio::test]
async fn the_rich_media_job_turns_a_linked_page_into_a_card() {
    let db = common::test_db().await;
    let pool = db.pool().clone();
    let (_, alice_token) = user(&pool, "alice").await;
    let peer = MockPeer::start().await;
    let (app, state) = app_and_state(pool);

    let page = format!("{}/page", peer.base);
    let status = published(&app, &alice_token, &format!("look at {page}"), "public").await;
    let id = id_of(&status);

    let before = read(&app, &alice_token, &format!("{STATUSES}/{id}")).await;
    common::assert_answer(&before, StatusCode::OK, "the fresh post reads");
    assert_eq!(
        before.json_value()["card"],
        Value::Null,
        "no card exists before the jobs run"
    );

    let _ran = common::run_all_jobs(&state).await;

    let shown = read(&app, &alice_token, &format!("{STATUSES}/{id}")).await;
    common::assert_answer(&shown, StatusCode::OK, "the carded post reads");
    let card = shown.json_value()["card"].clone();
    assert_eq!(card["url"], json!(page), "the card names the page");
    assert_eq!(card["title"], json!("Mock og Title"), "the og:title");
    assert_eq!(
        card["image"],
        json!(format!("{}/og.png", peer.base)),
        "the og:image as the absolute URL the page wrote"
    );
    assert_eq!(
        card["type"],
        json!("link"),
        "Pleroma's card type is fixed to link"
    );
    assert_eq!(
        card["pleroma"]["opengraph"]["og:type"],
        json!("article"),
        "the raw og:type rides under pleroma.opengraph"
    );
    assert_eq!(
        card["description"],
        json!("Mock og description"),
        "the og:description"
    );
    let standalone = read(&app, &alice_token, &format!("{STATUSES}/{id}/card")).await;
    common::assert_answer(&standalone, StatusCode::OK, "the card endpoint answers");
    assert_eq!(standalone.json_value(), card, "with the very same JSON");
    assert!(
        peer.page_hits() >= 1,
        "the mock page was fetched by the job"
    );
}

/// Scenario 8, the remote mention: a local user with a signing key posts
/// `@remote@127.0.0.1:PORT`; the mention resolves through the mock's
/// `WebFinger` and actor document, the mirror lands in `users` under the
/// handle it vouches for (port included, decision 7) and the account
/// search finds it; draining the queues delivers the signed `Create` to
/// the mock inbox, and a second mention of the same handle resolves from
/// the stored mirror without a fresh `WebFinger` and rides out too.
#[tokio::test]
async fn a_remote_mention_resolves_mirrors_and_delivers_a_signed_create() {
    let db = common::test_db().await;
    let pool = db.pool().clone();
    let alice = keyed_user(&pool, "alice").await;
    let alice_token = common::create_app_and_token(&pool, &alice, SCOPES).await;
    let peer = MockPeer::start().await;
    let (app, state) = app_and_state(pool);

    let handle = format!("remote@{}", peer.base.trim_start_matches("http://"));
    let status = published(&app, &alice_token, &format!("hello @{handle}"), "public").await;
    assert_eq!(
        status["mentions"][0]["acct"],
        json!(handle),
        "the entity answers the resolved remote mention"
    );
    assert_eq!(
        (peer.webfinger_hits(), peer.actor_hits()),
        (1, 1),
        "one WebFinger resolve and one actor dereference"
    );
    let mirror = user::get_by_nickname(&db.pool().clone(), &handle)
        .await
        .expect("the mirror read runs")
        .expect("the resolved actor lands in users");
    assert!(!mirror.local.get(), "the mirror is remote, not local");
    assert_eq!(
        mirror.ap_id,
        format!("{}/users/remote", peer.base),
        "under the actor id the mock served"
    );

    let found = read(&app, &alice_token, &format!("{ACCOUNT_SEARCH}?q={handle}")).await;
    common::assert_answer(&found, StatusCode::OK, "the account search answers");
    assert_eq!(
        found.json_value()[0]["acct"],
        json!(handle),
        "the mirror is findable by its full handle"
    );

    let _ran = common::run_all_jobs(&state).await;
    let posts = peer.received();
    assert_eq!(posts.len(), 1, "the one status, the one delivery");
    assert_create_delivered(&posts[0], &alice, &status);

    let second = published(&app, &alice_token, &format!("again @{handle}"), "public").await;
    assert_eq!(
        peer.webfinger_hits(),
        1,
        "the stored mirror needs no refetch"
    );
    let _ran_again = common::run_all_jobs(&state).await;
    let delivered = peer.received();
    assert_eq!(delivered.len(), 2, "both statuses reached the remote inbox");
    assert_create_delivered(&delivered[1], &alice, &second);
}

/// What one recorded inbox delivery must look like: the `Create` of
/// `status`, signed as `alice`'s key.
fn assert_create_delivered(delivery: &ReceivedPost, alice: &User, status: &Value) {
    assert_eq!(delivery.body["type"], json!("Create"), "the activity type");
    assert_eq!(
        delivery.body["id"],
        json!(format!("{BASE}/activities/{}", id_of(status))),
        "decision 6's local activity id"
    );
    assert_eq!(
        delivery.body["actor"],
        json!(alice.ap_id),
        "addressed as its author"
    );
    assert_eq!(
        delivery.body["object"]["id"], status["uri"],
        "the object federates under the status uri"
    );
    assert_eq!(
        delivery.body["to"],
        json!([AS_PUBLIC]),
        "a public post addresses the public collection"
    );
    assert!(
        delivery.body["object"]["content"]
            .as_str()
            .unwrap_or_default()
            .contains("h-card"),
        "the linkified mention rides in the content"
    );
    let signature = delivery.header("signature").unwrap_or_default();
    assert!(
        signature.contains(&format!("keyId=\"{}#main-key\"", alice.ap_id)),
        "the signature names alice's key: {signature}"
    );
    assert!(
        signature.contains("(request-target) host date digest"),
        "the signature covers the four federation headers: {signature}"
    );
    assert!(
        delivery
            .header("digest")
            .unwrap_or_default()
            .starts_with("SHA-256="),
        "the body carries a digest"
    );
    assert_eq!(
        delivery.header("content-type"),
        Some("application/activity+json"),
        "the delivery is labelled as an activity"
    );
}

/// A local user named `nickname` with a generated RSA key pair — the
/// signing identity a federating poster needs.
async fn keyed_user(pool: &PgPool, nickname: &str) -> User {
    let keys = wired_be::util::keys::generate_keypair_async()
        .await
        .expect("the fixture key pair generates");
    common::create_local_user_with(pool, nickname, |mut new| {
        new.public_key = Some(keys.public_pem);
        new.private_key = Some(keys.private_pem);
        new
    })
    .await
}

/// One recorded inbox `POST`: the request headers and the decoded body.
#[derive(Clone, Debug)]
struct ReceivedPost {
    /// The headers as lower-cased name/value pairs.
    headers: Vec<(String, String)>,
    /// The activity JSON the peer was asked to fold.
    body: Value,
}

impl ReceivedPost {
    /// The value of request header `name`, `None` when it was not sent.
    fn header(&self, name: &str) -> Option<&str> {
        self.headers
            .iter()
            .find(|(key, _)| key == name)
            .map(|(_key, value)| value.as_str())
    }
}

/// The shared half of the mock peer: its origin and the counters and
/// captures the tests assert on.
#[derive(Clone, Debug)]
struct Peer {
    /// The `http://{addr}` origin this peer answers on.
    base: String,
    /// `WebFinger` requests it answered.
    webfinger_hits: Arc<AtomicUsize>,
    /// Actor dereferences it answered.
    actor_hits: Arc<AtomicUsize>,
    /// Card-page fetches it answered.
    page_hits: Arc<AtomicUsize>,
    /// The inbox `POST`s it recorded.
    inbox_posts: Arc<Mutex<Vec<ReceivedPost>>>,
}

/// The loopback mock federation peer (scenarios 7 and 8 upstream): an HTML
/// card page, a self-identifying `WebFinger`, actor documents in the
/// `follow_by_uri.rs` shape, and an inbox that records every delivery.
struct MockPeer {
    /// The `http://{addr}` origin it answers on.
    base: String,
    /// The shared counters and captures of its handlers.
    peer: Peer,
    /// The task serving it, aborted when the test is done.
    server: JoinHandle<()>,
}

impl MockPeer {
    /// Binds a port and serves the whole mock surface.
    async fn start() -> Self {
        let listener = TcpListener::bind("127.0.0.1:0")
            .await
            .expect("the mock peer binds a port");
        let base = format!("http://{}", listener.local_addr().expect("a bound address"));
        let peer = Peer {
            base: base.clone(),
            webfinger_hits: Arc::new(AtomicUsize::new(0)),
            actor_hits: Arc::new(AtomicUsize::new(0)),
            page_hits: Arc::new(AtomicUsize::new(0)),
            inbox_posts: Arc::new(Mutex::new(Vec::new())),
        };
        let app = Router::new()
            .route("/.well-known/webfinger", get(webfinger))
            .route("/users/{nick}", get(actor))
            .route("/users/{nick}/inbox", post(inbox))
            .route("/page", get(page))
            .with_state(peer.clone());
        let server = tokio::spawn(async move {
            if let Err(err) = axum::serve(listener, app).await {
                tracing::debug!("mock peer stopped: {err}");
            }
        });
        Self { base, peer, server }
    }

    /// The `WebFinger` requests this peer answered.
    fn webfinger_hits(&self) -> usize {
        self.peer.webfinger_hits.load(Ordering::SeqCst)
    }

    /// The actor dereferences this peer answered.
    fn actor_hits(&self) -> usize {
        self.peer.actor_hits.load(Ordering::SeqCst)
    }

    /// The card-page fetches this peer answered.
    fn page_hits(&self) -> usize {
        self.peer.page_hits.load(Ordering::SeqCst)
    }

    /// The inbox deliveries this peer recorded, oldest first.
    fn received(&self) -> Vec<ReceivedPost> {
        self.peer
            .inbox_posts
            .lock()
            .expect("the capture lock")
            .clone()
    }
}

impl Drop for MockPeer {
    fn drop(&mut self) {
        self.server.abort();
    }
}

/// Answers any `acct:` `WebFinger` with a JRD whose subject echoes the
/// asked resource and whose self link points at this peer's own actor.
async fn webfinger(
    State(peer): State<Peer>,
    Query(params): Query<HashMap<String, String>>,
) -> Response {
    let _hits = peer.webfinger_hits.fetch_add(1, Ordering::SeqCst);
    let Some(resource) = params
        .get("resource")
        .and_then(|value| value.strip_prefix("acct:"))
    else {
        return StatusCode::NOT_FOUND.into_response();
    };
    let nickname = resource.split('@').next().unwrap_or(resource);
    let jrd = json!({
        "subject": format!("acct:{resource}"),
        "links": [{
            "rel": "self",
            "type": "application/activity+json",
            "href": format!("{}/users/{nickname}", peer.base),
        }],
    });
    ([(CONTENT_TYPE, "application/jrd+json")], jrd.to_string()).into_response()
}

/// Answers the self-identifying actor document, the `follow_by_uri.rs`
/// shape, with an inbox under its own id.
async fn actor(State(peer): State<Peer>, Path(nick): Path<String>) -> Response {
    let _hits = peer.actor_hits.fetch_add(1, Ordering::SeqCst);
    let id = format!("{}/users/{nick}", peer.base);
    let document = json!({
        "@context": "https://www.w3.org/ns/activitystreams",
        "type": "Person",
        "id": id,
        "preferredUsername": nick,
        "name": "Remote",
        "inbox": format!("{id}/inbox"),
        "publicKey": {
            "id": format!("{id}#main-key"),
            "owner": id,
            "publicKeyPem": "-----BEGIN PUBLIC KEY-----\nPEM\n-----END PUBLIC KEY-----\n",
        },
    });
    (
        [(CONTENT_TYPE, "application/activity+json")],
        document.to_string(),
    )
        .into_response()
}

/// Records the delivery and accepts it: the happy remote inbox.
async fn inbox(State(peer): State<Peer>, headers: HeaderMap, body: Bytes) -> Response {
    let captured = ReceivedPost {
        headers: headers
            .iter()
            .map(|(name, value)| {
                (
                    name.as_str().to_owned(),
                    value.to_str().unwrap_or_default().to_owned(),
                )
            })
            .collect(),
        body: serde_json::from_slice(&body).unwrap_or(Value::Null),
    };
    peer.inbox_posts
        .lock()
        .expect("the capture lock")
        .push(captured);
    StatusCode::ACCEPTED.into_response()
}

/// Serves the Open Graph page the card is made from.
async fn page(State(peer): State<Peer>) -> Response {
    let _hits = peer.page_hits.fetch_add(1, Ordering::SeqCst);
    let html = format!(
        "<html><head><title>Plain Title</title>\
         <meta property=\"og:title\" content=\"Mock og Title\">\
         <meta property=\"og:description\" content=\"Mock og description\">\
         <meta property=\"og:image\" content=\"{}/og.png\">\
         <meta property=\"og:type\" content=\"article\">\
         </head><body><p>hello from the mock</p></body></html>",
        peer.base
    );
    ([(CONTENT_TYPE, "text/html; charset=utf-8")], html).into_response()
}
