//! Tests of the featured-tags collection: the document a local account
//! serves, the item reader, and the sync a remote actor refresh runs.

use axum::{Router, extract::State, response::IntoResponse as _, routing::get};
use reqwest::header::CONTENT_TYPE;

use super::*;
use crate::{
    config::TEST_BASE_URL,
    federation::fetch_actor::get_or_fetch_by_ap_id,
    http_client::HttpClient,
    models::{
        featured_tag::{self, Entry},
        hashtag,
    },
    test_fixtures::{local_user, remote_user},
    test_server::{bind, origin, spawn},
};

/// The names of the tags `user_id` features.
async fn featured_names(pool: &PgPool, user_id: uuid::Uuid) -> Vec<String> {
    let entries: Vec<Entry> = featured_tag::list(pool, user_id).await.expect("reads");
    entries.into_iter().map(|entry| entry.name).collect()
}

#[tokio::test]
async fn a_local_account_serves_its_tags_as_hashtag_items() {
    let db = crate::test_db::test_db().await.expect("test database slot");
    let pool = db.pool().clone();
    let alice = local_user(&pool, "alice").await;
    for name in ["rust", "fedlab"] {
        let tag = hashtag::find_or_create(&pool, name)
            .await
            .expect("upserts")
            .expect("usable");
        let featured = featured_tag::feature(&pool, alice.id, &tag)
            .await
            .expect("features");
        assert!(featured.entry().is_some(), "the tag fits");
    }

    let document = featured_tags_document(&pool, &Config::for_tests(), &alice)
        .await
        .expect("the document builds");

    assert_eq!(document["id"], format!("{}/collections/tags", alice.ap_id));
    assert_eq!(document["type"], "Collection");
    assert_eq!(document["totalItems"], 2);
    assert_eq!(
        document["items"],
        json!([
            {"type": "Hashtag", "href": format!("{TEST_BASE_URL}/tags/fedlab"), "name": "#fedlab"},
            {"type": "Hashtag", "href": format!("{TEST_BASE_URL}/tags/rust"), "name": "#rust"},
        ]),
        "every tag, by name, linked to this instance's tag page"
    );
}

#[tokio::test]
async fn an_account_with_no_tags_serves_an_empty_collection() {
    let db = crate::test_db::test_db().await.expect("test database slot");
    let pool = db.pool().clone();
    let alice = local_user(&pool, "alice").await;
    let document = featured_tags_document(&pool, &Config::for_tests(), &alice)
        .await
        .expect("the document builds");
    assert_eq!(document["totalItems"], 0);
    assert_eq!(document["items"], json!([]));
}

#[test]
fn names_come_from_the_item_name_or_the_href() {
    let collection = json!({
        "type": "Collection",
        "items": [
            {"type": "Hashtag", "href": "https://m.example/tags/one", "name": "#One"},
            {"type": "Hashtag", "href": "https://m.example/tags/two/"},
            "https://m.example/tags/skipped",
            {"type": "Hashtag"},
        ],
    });
    assert_eq!(hashtag_names(&collection), ["#One", "two"]);
    let ordered = json!({"orderedItems": [{"name": "#a"}]});
    assert_eq!(hashtag_names(&ordered), ["#a"], "orderedItems reads too");
    assert_eq!(
        hashtag_names(&json!({"type": "Collection"})),
        Vec::<String>::new(),
        "no items, no names"
    );
}

#[test]
fn a_hostile_collection_is_read_only_so_far() {
    let items: Vec<Value> = (0..200)
        .map(|n| json!({"name": format!("#t{n}x")}))
        .collect();
    let collection = json!({"items": items});
    assert_eq!(hashtag_names(&collection).len(), MAX_ITEMS);
}

/// What the peer's routes know: its origin and the tags it advertises.
#[derive(Clone)]
struct Peer {
    /// The `http://{addr}` origin.
    base: String,
    /// The names `/users/alice/collections/tags` lists.
    tags: Vec<&'static str>,
    /// Where the actor's `featuredTags` points, relative to the origin
    /// unless it is absolute; `None` leaves the property out.
    advertised: Option<String>,
}

/// A body labelled as `ActivityPub` JSON.
fn activity(document: &Value) -> axum::response::Response {
    (
        [(CONTENT_TYPE, "application/activity+json")],
        document.to_string(),
    )
        .into_response()
}

/// The peer's actor document.
async fn actor(State(peer): State<Peer>) -> axum::response::Response {
    let id = format!("{}/users/alice", peer.base);
    let mut document = json!({
        "@context": "https://www.w3.org/ns/activitystreams",
        "type": "Person",
        "id": id,
        "preferredUsername": "alice",
        "inbox": format!("{id}/inbox"),
        "publicKey": {
            "id": format!("{id}#main-key"),
            "owner": id,
            "publicKeyPem": "-----BEGIN PUBLIC KEY-----\nPEM\n-----END PUBLIC KEY-----\n",
        },
    });
    if let Some(advertised) = &peer.advertised {
        let url = if advertised.starts_with("http") {
            advertised.clone()
        } else {
            format!("{}{advertised}", peer.base)
        };
        document["featuredTags"] = json!(url);
    }
    activity(&document)
}

/// The peer's collection, Mastodon's shape.
async fn collection(State(peer): State<Peer>) -> axum::response::Response {
    let id = format!("{}/users/alice/collections/tags", peer.base);
    let items: Vec<Value> = peer
        .tags
        .iter()
        .map(|name| {
            json!({
                "type": "Hashtag",
                "href": format!("{}/tags/{name}", peer.base),
                "name": format!("#{name}"),
            })
        })
        .collect();
    activity(&json!({"id": id, "type": "Collection", "totalItems": items.len(), "items": items}))
}

/// The state under test: loopback-capable client over `pool`.
fn state(pool: PgPool) -> AppState {
    AppState {
        http_client: HttpClient::new_for_tests("Wired (https://test.example)"),
        ..AppState::for_tests(pool, Config::for_tests())
    }
}

/// Serves `peer` and fetches its actor, returning the mirror row and the
/// state, with the server still running until the caller aborts it.
async fn refresh(
    state: &AppState,
    advertised: Option<&str>,
    tags: Vec<&'static str>,
) -> (User, tokio::task::JoinHandle<()>) {
    let listener = bind().await;
    let base = origin(&listener);
    let peer = Peer {
        base: base.clone(),
        tags,
        advertised: advertised.map(str::to_owned),
    };
    let server = spawn(
        listener,
        Router::new()
            .route("/users/alice", get(actor))
            .route("/users/alice/collections/tags", get(collection))
            .with_state(peer),
    );
    let user = get_or_fetch_by_ap_id(state, &format!("{base}/users/alice"))
        .await
        .expect("the actor mirrors");
    (user, server)
}

#[tokio::test]
async fn a_refresh_stores_the_featured_tags_the_actor_advertises() {
    let db = crate::test_db::test_db().await.expect("test database slot");
    let state = state(db.pool().clone());

    let (user, server) = refresh(
        &state,
        Some("/users/alice/collections/tags"),
        vec!["fedlab", "Rust"],
    )
    .await;
    server.abort();

    assert_eq!(
        featured_names(&state.db, user.id).await,
        ["fedlab", "rust"],
        "the collection's tags, normalized"
    );
}

#[tokio::test]
async fn an_actor_without_featured_tags_has_none() {
    let db = crate::test_db::test_db().await.expect("test database slot");
    let state = state(db.pool().clone());
    let (user, server) = refresh(&state, None, vec!["ignored"]).await;
    server.abort();
    assert_eq!(
        featured_names(&state.db, user.id).await,
        Vec::<String>::new(),
        "no featuredTags, no tags"
    );

    let tag = hashtag::find_or_create(&state.db, "old")
        .await
        .expect("upserts")
        .expect("usable");
    let kept = featured_tag::feature(&state.db, user.id, &tag)
        .await
        .expect("features");
    assert!(kept.entry().is_some());
    let advert = json!({"id": user.ap_id});
    sync_remote(&state, &user, &advert, None).await;
    assert!(
        featured_names(&state.db, user.id).await.is_empty(),
        "an actor that stops advertising the collection loses its tags"
    );
}

#[tokio::test]
async fn an_unreadable_or_foreign_collection_keeps_the_stored_tags() {
    let db = crate::test_db::test_db().await.expect("test database slot");
    let state = state(db.pool().clone());
    let (remote, server) =
        refresh(&state, Some("/users/alice/collections/tags"), vec!["kept"]).await;

    let missing = json!({"featuredTags": format!("{}/collections/gone", remote.ap_id)});
    sync_remote(&state, &remote, &missing, None).await;
    let foreign = json!({"featuredTags": "https://elsewhere.example/tags"});
    sync_remote(&state, &remote, &foreign, None).await;
    server.abort();

    assert_eq!(
        featured_names(&state.db, remote.id).await,
        ["kept"],
        "a 404 and a collection on another origin changed nothing"
    );
}

#[tokio::test]
async fn an_inline_collection_is_read_without_a_fetch() {
    let db = crate::test_db::test_db().await.expect("test database slot");
    let state = state(db.pool().clone());
    let remote = remote_user(&state.db, "bob").await;
    let advert = json!({"featuredTags": {
        "type": "Collection",
        "items": [{"type": "Hashtag", "name": "#inline"}],
    }});
    sync_remote(&state, &remote, &advert, None).await;
    assert_eq!(featured_names(&state.db, remote.id).await, ["inline"]);
}
