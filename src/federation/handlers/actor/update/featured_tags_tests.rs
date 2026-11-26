//! Tests of the featured-tags refresh an actor `Update` runs.

use axum::{Router, extract::State, response::IntoResponse as _, routing::get};
use reqwest::header::CONTENT_TYPE;
use serde_json::{Value, json};
use sqlx::PgPool;
use uuid::Uuid;

use super::super::{
    test_support::{assert_handled, fresh_user, ready_for_update, view},
    update_actor,
};
use crate::{
    config::Config,
    federation::remote_actor::{parse_actor, upsert_remote_user},
    http_client::HttpClient,
    models::{
        featured_tag::{self, Entry},
        hashtag,
        user::User,
    },
    state::AppState,
    test_server::{bind, origin, spawn},
};

/// What the peer's collection route lists.
#[derive(Clone)]
struct Peer {
    /// The `http://{addr}` origin.
    base: String,
    /// The names `/users/alice/collections/tags` lists.
    tags: Vec<&'static str>,
}

/// The peer's collection, Mastodon's shape.
async fn collection(State(peer): State<Peer>) -> axum::response::Response {
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
    let document = json!({
        "id": format!("{}/users/alice/collections/tags", peer.base),
        "type": "Collection",
        "totalItems": items.len(),
        "items": items,
    });
    (
        [(CONTENT_TYPE, "application/activity+json")],
        document.to_string(),
    )
        .into_response()
}

/// The state under test: a loopback-capable client over `pool`.
fn state(pool: &PgPool) -> AppState {
    AppState {
        http_client: HttpClient::new_for_tests("Wired (https://test.example)"),
        ..AppState::for_tests(pool.clone(), Config::for_tests())
    }
}

/// alice's actor document on the peer at `base`, advertising `featured_tags`
/// when given.
fn actor_document(base: &str, featured_tags: Option<Value>) -> Value {
    let id = format!("{base}/users/alice");
    let mut document = json!({
        "id": id,
        "type": "Person",
        "preferredUsername": "alice",
        "name": "Alice",
        "inbox": format!("{id}/inbox"),
        "publicKey": {
            "id": format!("{id}#main-key"),
            "owner": id,
            "publicKeyPem": "-----BEGIN PUBLIC KEY-----\nPEM\n-----END PUBLIC KEY-----\n",
        },
    });
    if let Some(advertised) = featured_tags {
        document["featuredTags"] = advertised;
    }
    document
}

/// The names of the tags `user_id` features.
async fn featured_names(pool: &PgPool, user_id: Uuid) -> Vec<String> {
    let entries: Vec<Entry> = featured_tag::list(pool, user_id).await.expect("reads");
    entries.into_iter().map(|entry| entry.name).collect()
}

/// A mirror of alice at `base` that already features `names`, aged so an
/// `Update` may fold.
async fn mirror_featuring(pool: &PgPool, base: &str, names: &[&str]) -> User {
    let stored = upsert_remote_user(
        pool,
        parse_actor(&actor_document(base, None)).expect("the fixture parses"),
    )
    .await
    .expect("the mirror stores");
    for name in names {
        let tag = hashtag::find_or_create(pool, name)
            .await
            .expect("upserts")
            .expect("usable");
        let featured = featured_tag::feature(pool, stored.id, &tag)
            .await
            .expect("features");
        assert!(featured.entry().is_some(), "the tag fits");
    }
    ready_for_update(pool, stored.id).await
}

/// Folds an `Update` of alice carrying `document` into `mirror`.
async fn fold(state: &AppState, mirror: User, document: Value) {
    let id = mirror.ap_id.clone();
    let folded = update_actor(
        state.clone(),
        view("Update", "featured-1", &id, document),
        mirror,
    )
    .await;
    assert_handled(&folded);
}

/// Serves `tags` as alice's collection; returns the origin and the server.
async fn serve(tags: Vec<&'static str>) -> (String, tokio::task::JoinHandle<()>) {
    let listener = bind().await;
    let base = origin(&listener);
    let peer = Peer {
        base: base.clone(),
        tags,
    };
    let server = spawn(
        listener,
        Router::new()
            .route("/users/alice/collections/tags", get(collection))
            .with_state(peer),
    );
    (base, server)
}

/// An `Update` whose document advertises a collection replaces the stored set.
#[tokio::test]
async fn an_update_replaces_the_featured_tags_with_the_advertised_collection() {
    let db = crate::test_db::test_db().await.expect("test database slot");
    let pool = db.pool().clone();
    let state = state(&pool);
    let (base, server) = serve(vec!["fedlab", "Rust"]).await;
    let alice = mirror_featuring(&pool, &base, &["old"]).await;
    let alice_id = alice.id;

    let advertised = json!(format!("{base}/users/alice/collections/tags"));
    fold(&state, alice, actor_document(&base, Some(advertised))).await;
    server.abort();

    assert_eq!(
        featured_names(&pool, alice_id).await,
        ["fedlab", "Rust"],
        "the collection's tags replaced the old one"
    );
    assert_eq!(fresh_user(&pool, alice_id).await.name, "Alice");
}

/// A collection that cannot be fetched keeps the stored set.
#[tokio::test]
async fn an_update_with_an_unreadable_collection_keeps_the_featured_tags() {
    let db = crate::test_db::test_db().await.expect("test database slot");
    let pool = db.pool().clone();
    let state = state(&pool);
    let (base, server) = serve(vec!["ignored"]).await;
    let alice = mirror_featuring(&pool, &base, &["kept"]).await;
    let alice_id = alice.id;

    let advertised = json!(format!("{base}/users/alice/collections/gone"));
    fold(&state, alice, actor_document(&base, Some(advertised))).await;
    server.abort();

    assert_eq!(
        featured_names(&pool, alice_id).await,
        ["kept"],
        "a 404 collection changed nothing"
    );
}

/// A document without `featuredTags` clears the stored set.
#[tokio::test]
async fn an_update_without_featured_tags_clears_them() {
    let db = crate::test_db::test_db().await.expect("test database slot");
    let pool = db.pool().clone();
    let state = state(&pool);
    let base = "http://127.0.0.1:9";
    let alice = mirror_featuring(&pool, base, &["gone"]).await;
    let alice_id = alice.id;
    assert_eq!(featured_names(&pool, alice_id).await, ["gone"]);

    fold(&state, alice, actor_document(base, None)).await;

    assert!(
        featured_names(&pool, alice_id).await.is_empty(),
        "an actor that stops advertising the collection loses its tags"
    );
}

/// An inline collection is read without any fetch.
#[tokio::test]
async fn an_update_with_an_inline_collection_stores_its_tags() {
    let db = crate::test_db::test_db().await.expect("test database slot");
    let pool = db.pool().clone();
    let state = state(&pool);
    let base = "http://127.0.0.1:9";
    let alice = mirror_featuring(&pool, base, &["old"]).await;
    let alice_id = alice.id;

    let inline = json!({"type": "Collection", "items": [{"type": "Hashtag", "name": "#inline"}]});
    fold(&state, alice, actor_document(base, Some(inline))).await;

    assert_eq!(featured_names(&pool, alice_id).await, ["inline"]);
}
