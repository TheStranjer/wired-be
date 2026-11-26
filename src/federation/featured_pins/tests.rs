//! Tests of the featured-pins sync: the collection reader, the job that runs
//! it after an actor fetch, and the non-fatal call an actor `Update` makes.

use axum::{
    Router,
    extract::{Path, State},
    http::StatusCode,
    response::{IntoResponse as _, Response},
    routing::get,
};
use chrono::{Duration, Utc};
use reqwest::header::CONTENT_TYPE;
use sqlx::PgPool;

use super::*;
use crate::{
    config::{Config, TEST_BASE_URL},
    federation::fetch_actor::get_or_fetch_by_ap_id,
    http_client::HttpClient,
    models::{
        activitypub_actor::ActivityPubActorWrite,
        status::{self, NewStatus, Status},
        user::{self, Flag, NewUser},
    },
    test_fixtures::{local_user, remote_user, write_actor},
    test_server::{bind, origin, spawn},
};

/// What the peer's routes know: its origin and the two collection documents
/// it serves (`None` answers 404).
#[derive(Clone)]
struct Peer {
    /// The `http://{addr}` origin.
    base: String,
    /// The document of `/users/alice/collections/featured`.
    featured: Option<Value>,
    /// The document of `/users/alice/collections/featured/page`.
    page: Option<Value>,
}

/// A body labelled as `ActivityPub` JSON.
fn activity(document: &Value) -> Response {
    (
        [(CONTENT_TYPE, "application/activity+json")],
        document.to_string(),
    )
        .into_response()
}

/// The collection document answering at `url`, listing `items` newest first.
fn ordered(url: &str, items: &[String]) -> Value {
    json!({
        "@context": "https://www.w3.org/ns/activitystreams",
        "id": url,
        "type": "OrderedCollection",
        "totalItems": items.len(),
        "orderedItems": items,
    })
}

/// The peer's actor document, advertising its featured collection.
async fn actor(State(peer): State<Peer>) -> Response {
    let id = format!("{}/users/alice", peer.base);
    activity(&json!({
        "@context": "https://www.w3.org/ns/activitystreams",
        "type": "Person",
        "id": id,
        "preferredUsername": "alice",
        "inbox": format!("{id}/inbox"),
        "featured": format!("{id}/collections/featured"),
        "publicKey": {
            "id": format!("{id}#main-key"),
            "owner": id,
            "publicKeyPem": "-----BEGIN PUBLIC KEY-----\nPEM\n-----END PUBLIC KEY-----\n",
        },
    }))
}

/// The peer's featured collection.
async fn featured(State(peer): State<Peer>) -> Response {
    peer.featured
        .as_ref()
        .map_or_else(|| StatusCode::NOT_FOUND.into_response(), activity)
}

/// The peer's featured collection page.
async fn page(State(peer): State<Peer>) -> Response {
    peer.page
        .as_ref()
        .map_or_else(|| StatusCode::NOT_FOUND.into_response(), activity)
}

/// One of the peer's notes: alice's, except `other`, which is bob's.
async fn object(State(peer): State<Peer>, Path(name): Path<String>) -> Response {
    let author = if name == "other" { "bob" } else { "alice" };
    activity(&json!({
        "id": format!("{}/objects/{name}", peer.base),
        "type": "Note",
        "attributedTo": format!("{}/users/{author}", peer.base),
        "to": ["https://www.w3.org/ns/activitystreams#Public"],
        "content": format!("<p>object {name}</p>"),
        "published": "2024-03-01T10:00:00.000Z",
    }))
}

/// The state under test: a loopback-capable client over `pool`.
fn state(pool: PgPool) -> AppState {
    AppState {
        http_client: HttpClient::new_for_tests("Wired (https://test.example)"),
        ..AppState::for_tests(pool, Config::for_tests())
    }
}

/// Starts a peer whose documents are built from its own origin by `build`
/// (featured collection, page); returns the origin and the server.
async fn serve<F>(build: F) -> (String, tokio::task::JoinHandle<()>)
where
    F: FnOnce(&str) -> (Option<Value>, Option<Value>),
{
    let listener = bind().await;
    let base = origin(&listener);
    let (featured_doc, page_doc) = build(&base);
    let peer = Peer {
        base: base.clone(),
        featured: featured_doc,
        page: page_doc,
    };
    let server = spawn(
        listener,
        Router::new()
            .route("/users/alice", get(actor))
            .route("/users/alice/collections/featured", get(featured))
            .route("/users/alice/collections/featured/page", get(page))
            .route("/objects/{name}", get(object))
            .with_state(peer),
    );
    (base, server)
}

/// The URL of the peer's featured collection.
fn featured_url(base: &str) -> String {
    format!("{base}/users/alice/collections/featured")
}

/// The URL of the peer's note `name`.
fn note_url(base: &str, name: &str) -> String {
    format!("{base}/objects/{name}")
}

/// The stored mirror of alice on the peer at `base`.
async fn alice(pool: &PgPool, base: &str) -> User {
    let host = base.strip_prefix("http://").unwrap_or(base).to_owned();
    let stored = user::insert(
        pool,
        &NewUser {
            nickname: format!("alice@{host}"),
            ap_id: format!("{base}/users/alice"),
            local: Flag::new(false),
            domain: Some(host),
            ..NewUser::default()
        },
    )
    .await
    .expect("the mirror fixture inserts");
    let _row = write_actor(
        pool,
        &ActivityPubActorWrite {
            followers_url: Some(format!("{base}/users/alice/followers")),
            ..ActivityPubActorWrite::bare(stored.id)
        },
    )
    .await;
    stored
}

/// A status of `author` stored at `uri`.
async fn stored_status(pool: &PgPool, author: &User, uri: &str) -> Status {
    status::insert(
        pool,
        TEST_BASE_URL,
        &NewStatus {
            user_id: author.id,
            uri: Some(uri.to_owned()),
            content: "stored".to_owned(),
            raw_text: "stored".to_owned(),
            ..NewStatus::default()
        },
    )
    .await
    .expect("the status fixture inserts")
}

/// The uris of the statuses `user_id` pins, newest pin first.
async fn pinned_uris(pool: &PgPool, user_id: Uuid) -> Vec<String> {
    pinned_status::list_for_user(pool, user_id)
        .await
        .expect("the pins read")
        .into_iter()
        .map(|pinned| pinned.uri)
        .collect()
}

/// The args of every queued sync job.
async fn queued_syncs(pool: &PgPool) -> Vec<Value> {
    sqlx::query_scalar::<_, Value>("SELECT args FROM jobs WHERE kind = $1")
        .bind(KIND_SYNC)
        .fetch_all(pool)
        .await
        .expect("the job queue reads")
}

#[tokio::test]
async fn a_collection_of_two_own_statuses_pins_both_in_order() {
    let db = crate::test_db::test_db().await.expect("test database slot");
    let state = state(db.pool().clone());
    let (base, server) = serve(|base| {
        let items = [note_url(base, "unknown"), note_url(base, "stored")];
        (Some(ordered(&featured_url(base), &items)), None)
    })
    .await;
    let alice = alice(&state.db, &base).await;
    let _stored = stored_status(&state.db, &alice, &note_url(&base, "stored")).await;

    sync(&state, &alice, &json!(featured_url(&base)), None)
        .await
        .expect("the sync runs");
    server.abort();

    assert_eq!(
        pinned_uris(&state.db, alice.id).await,
        [note_url(&base, "unknown"), note_url(&base, "stored")],
        "the stored status and the fetched one, in the collection's order"
    );
}

#[tokio::test]
async fn a_collection_that_drops_a_status_unpins_it() {
    let db = crate::test_db::test_db().await.expect("test database slot");
    let state = state(db.pool().clone());
    let (base, server) = serve(|base| {
        let items = [note_url(base, "kept")];
        (Some(ordered(&featured_url(base), &items)), None)
    })
    .await;
    let alice = alice(&state.db, &base).await;
    let kept = stored_status(&state.db, &alice, &note_url(&base, "kept")).await;
    let dropped = stored_status(&state.db, &alice, &note_url(&base, "dropped")).await;
    for pinned in [&kept, &dropped] {
        let _row = pinned_status::pin(&state.db, alice.id, pinned.id)
            .await
            .expect("the fixture pin stores");
    }

    sync(&state, &alice, &json!(featured_url(&base)), None)
        .await
        .expect("the sync runs");
    server.abort();

    assert_eq!(
        pinned_uris(&state.db, alice.id).await,
        [note_url(&base, "kept")],
        "the dropped status is no longer pinned"
    );
}

#[tokio::test]
async fn foreign_and_foreign_authored_items_are_ignored() {
    let db = crate::test_db::test_db().await.expect("test database slot");
    let state = state(db.pool().clone());
    let (base, server) = serve(|base| {
        let items = [
            "https://elsewhere.example/objects/theirs".to_owned(),
            note_url(base, "other"),
            note_url(base, "mine"),
        ];
        (Some(ordered(&featured_url(base), &items)), None)
    })
    .await;
    let alice = alice(&state.db, &base).await;

    sync(&state, &alice, &json!(featured_url(&base)), None)
        .await
        .expect("the sync runs");
    server.abort();

    assert_eq!(
        pinned_uris(&state.db, alice.id).await,
        [note_url(&base, "mine")],
        "another origin's item and another author's note are not alice's pins"
    );
}

#[tokio::test]
async fn an_unreadable_collection_keeps_the_stored_pins() {
    let db = crate::test_db::test_db().await.expect("test database slot");
    let state = state(db.pool().clone());
    let (base, server) = serve(|_base| (None, None)).await;
    let alice = alice(&state.db, &base).await;
    let pinned = stored_status(&state.db, &alice, &note_url(&base, "kept")).await;
    let _row = pinned_status::pin(&state.db, alice.id, pinned.id)
        .await
        .expect("the fixture pin stores");

    for unreadable in [
        json!(featured_url(&base)),
        json!("https://elsewhere.example/featured"),
    ] {
        sync(&state, &alice, &unreadable, None)
            .await
            .expect("an unreadable collection is not a failure");
    }
    server.abort();

    assert_eq!(
        pinned_uris(&state.db, alice.id).await,
        [note_url(&base, "kept")],
        "a 404 and a collection on another origin changed nothing"
    );
}

#[tokio::test]
async fn a_collection_that_hides_its_items_keeps_the_stored_pins() {
    let db = crate::test_db::test_db().await.expect("test database slot");
    let state = state(db.pool().clone());
    let (base, server) = serve(|base| {
        let hidden = json!({
            "id": featured_url(base),
            "type": "OrderedCollection",
            "totalItems": 3,
        });
        (Some(hidden), None)
    })
    .await;
    let alice = alice(&state.db, &base).await;
    let pinned = stored_status(&state.db, &alice, &note_url(&base, "kept")).await;
    let _row = pinned_status::pin(&state.db, alice.id, pinned.id)
        .await
        .expect("the fixture pin stores");

    sync(&state, &alice, &json!(featured_url(&base)), None)
        .await
        .expect("the sync runs");
    server.abort();

    assert_eq!(
        pinned_uris(&state.db, alice.id).await,
        [note_url(&base, "kept")],
        "a collection that lists no items but claims some says nothing"
    );
}

#[tokio::test]
async fn an_empty_collection_clears_the_pins() {
    let db = crate::test_db::test_db().await.expect("test database slot");
    let state = state(db.pool().clone());
    let (base, server) = serve(|base| (Some(ordered(&featured_url(base), &[])), None)).await;
    let alice = alice(&state.db, &base).await;
    let pinned = stored_status(&state.db, &alice, &note_url(&base, "old")).await;
    let _row = pinned_status::pin(&state.db, alice.id, pinned.id)
        .await
        .expect("the fixture pin stores");

    sync(&state, &alice, &json!(featured_url(&base)), None)
        .await
        .expect("the sync runs");
    server.abort();

    assert!(
        pinned_uris(&state.db, alice.id).await.is_empty(),
        "an actor that lists nothing pins nothing"
    );
}

#[tokio::test]
async fn a_paged_collection_is_read_through_its_first_page() {
    let db = crate::test_db::test_db().await.expect("test database slot");
    let state = state(db.pool().clone());
    let (base, server) = serve(|base| {
        let page_url = format!("{}/page", featured_url(base));
        let root = json!({
            "id": featured_url(base),
            "type": "OrderedCollection",
            "totalItems": 1,
            "first": page_url,
        });
        let page_doc = json!({
            "id": page_url,
            "type": "OrderedCollectionPage",
            "orderedItems": [note_url(base, "paged")],
        });
        (Some(root), Some(page_doc))
    })
    .await;
    let alice = alice(&state.db, &base).await;

    sync(&state, &alice, &json!(featured_url(&base)), None)
        .await
        .expect("the sync runs");
    server.abort();

    assert_eq!(
        pinned_uris(&state.db, alice.id).await,
        [note_url(&base, "paged")]
    );
}

#[tokio::test]
async fn an_inline_collection_with_embedded_items_is_read_without_a_fetch() {
    let db = crate::test_db::test_db().await.expect("test database slot");
    let state = state(db.pool().clone());
    let base = "http://127.0.0.1:9";
    let alice = alice(&state.db, base).await;
    let _stored = stored_status(&state.db, &alice, &note_url(base, "inline")).await;
    let document = json!({"featured": {
        "type": "OrderedCollection",
        "orderedItems": [{"id": note_url(base, "inline"), "type": "Note"}],
    }});

    sync_from_actor(&state, &alice, &document, None).await;

    assert_eq!(
        pinned_uris(&state.db, alice.id).await,
        [note_url(base, "inline")]
    );
    sync_from_actor(&state, &alice, &json!({"id": alice.ap_id}), None).await;
    assert_eq!(
        pinned_uris(&state.db, alice.id).await,
        [note_url(base, "inline")],
        "an actor document that names no featured collection changes nothing"
    );
}

#[test]
fn only_a_collection_that_lists_items_or_none_is_readable() {
    let listed = json!({"orderedItems": ["a"]});
    assert_eq!(collection_items(&listed).map(<[Value]>::len), Some(1));
    let items = json!({"items": "a"});
    assert_eq!(collection_items(&items).map(<[Value]>::len), Some(1));
    let empty = json!({"type": "OrderedCollection", "totalItems": 0});
    assert_eq!(collection_items(&empty).map(<[Value]>::len), Some(0));
    let silent = json!({"type": "OrderedCollection", "totalItems": 3});
    assert!(collection_items(&silent).is_none());
}

#[tokio::test]
async fn an_actor_fetch_queues_the_sync_and_the_job_pins_the_collection() {
    let db = crate::test_db::test_db().await.expect("test database slot");
    let state = state(db.pool().clone());
    let (base, server) = serve(|base| {
        let items = [note_url(base, "second"), note_url(base, "first")];
        (Some(ordered(&featured_url(base), &items)), None)
    })
    .await;

    let alice = get_or_fetch_by_ap_id(&state, &format!("{base}/users/alice"))
        .await
        .expect("the actor mirrors");
    let queued = queued_syncs(&state.db).await;
    assert_eq!(queued, [json!({"user_id": alice.id.to_string()})]);
    assert!(
        pinned_uris(&state.db, alice.id).await.is_empty(),
        "the fetch itself pins nothing"
    );

    let args = queued.first().cloned().expect("the job was queued");
    handle_sync(state.clone(), args)
        .await
        .expect("the job runs");
    server.abort();

    assert_eq!(
        pinned_uris(&state.db, alice.id).await,
        [note_url(&base, "second"), note_url(&base, "first")],
        "an account discovered later shows its pins"
    );
}

#[tokio::test]
async fn the_sync_is_queued_once_and_only_for_a_remote_actor_with_a_collection() {
    let db = crate::test_db::test_db().await.expect("test database slot");
    let state = state(db.pool().clone());
    let bob = remote_user(&state.db, "bob").await;
    let local = local_user(&state.db, "carol").await;
    let advertising = json!({"featured": "https://remote.example/users/bob/featured"});

    enqueue_sync(&state, &local, &advertising).await;
    enqueue_sync(&state, &bob, &json!({"id": bob.ap_id})).await;
    enqueue_sync(&state, &bob, &json!({"featured": null})).await;
    assert!(queued_syncs(&state.db).await.is_empty(), "nothing to sync");

    enqueue_sync(&state, &bob, &advertising).await;
    enqueue_sync(&state, &bob, &advertising).await;
    assert_eq!(
        queued_syncs(&state.db).await,
        [json!({"user_id": bob.id.to_string()})],
        "one live job per account"
    );
}

#[tokio::test]
async fn the_job_discards_unusable_args_and_skips_what_it_cannot_sync() {
    let db = crate::test_db::test_db().await.expect("test database slot");
    let state = state(db.pool().clone());

    let no_id = handle_sync(state.clone(), json!({})).await;
    assert!(matches!(no_id, Err(JobError::Discard(_))));
    let bad_id = handle_sync(state.clone(), json!({"user_id": "nope"})).await;
    assert!(matches!(bad_id, Err(JobError::Discard(_))));
    let gone = handle_sync(
        state.clone(),
        json!({"user_id": Uuid::now_v7().to_string()}),
    )
    .await;
    assert!(matches!(gone, Err(JobError::Discard(_))));

    let local = local_user(&state.db, "dave").await;
    let local_args = json!({"user_id": local.id.to_string()});
    handle_sync(state.clone(), local_args)
        .await
        .expect("a local account is skipped quietly");

    let bob = remote_user(&state.db, "bob").await;
    let bare = json!({"user_id": bob.id.to_string()});
    handle_sync(state.clone(), bare.clone())
        .await
        .expect("no actor row, no collection to read");
    let _row = write_actor(&state.db, &ActivityPubActorWrite::bare(bob.id)).await;
    handle_sync(state, bare)
        .await
        .expect("an actor row without a featured url is the same");
}

/// The job inside the grace of a live pin change keeps the stored pins; once
/// the mark is back-dated past the grace it syncs the collection again.
#[tokio::test]
async fn the_job_skips_a_recent_live_pin_change_and_syncs_after_the_grace() {
    let db = crate::test_db::test_db().await.expect("test database slot");
    let state = state(db.pool().clone());
    let (base, server) = serve(|base| {
        let items = [note_url(base, "listed")];
        (Some(ordered(&featured_url(base), &items)), None)
    })
    .await;
    let alice = alice(&state.db, &base).await;
    let _row = write_actor(
        &state.db,
        &ActivityPubActorWrite {
            featured_url: Some(featured_url(&base)),
            ..ActivityPubActorWrite::bare(alice.id)
        },
    )
    .await;
    let live = stored_status(&state.db, &alice, &note_url(&base, "live")).await;
    let _pinned = pinned_status::pin(&state.db, alice.id, live.id)
        .await
        .expect("the live pin stores");
    let args = json!({"user_id": alice.id.to_string()});

    featured_live::mark(&state.db, alice.id, Collection::Pins)
        .await
        .expect("the mark stores");
    handle_sync(state.clone(), args.clone())
        .await
        .expect("the job runs");
    assert_eq!(
        pinned_uris(&state.db, alice.id).await,
        [note_url(&base, "live")],
        "inside the grace the stale collection does not undo the live pin"
    );

    featured_live::mark_at(
        &state.db,
        alice.id,
        Collection::Pins,
        Utc::now() - featured_live::FEATURED_SYNC_GRACE - Duration::seconds(1),
    )
    .await
    .expect("the mark back-dates");
    handle_sync(state.clone(), args)
        .await
        .expect("the job runs");
    server.abort();
    assert_eq!(
        pinned_uris(&state.db, alice.id).await,
        [note_url(&base, "listed")],
        "past the grace the collection replaces the set"
    );
}

/// A recent live change to the featured tags does not hold the pins back.
#[tokio::test]
async fn a_recent_tag_change_does_not_suppress_the_pin_sync() {
    let db = crate::test_db::test_db().await.expect("test database slot");
    let state = state(db.pool().clone());
    let (base, server) = serve(|base| {
        let items = [note_url(base, "listed")];
        (Some(ordered(&featured_url(base), &items)), None)
    })
    .await;
    let alice = alice(&state.db, &base).await;
    featured_live::mark(&state.db, alice.id, Collection::Tags)
        .await
        .expect("the mark stores");

    sync(&state, &alice, &json!(featured_url(&base)), None)
        .await
        .expect("the sync runs");
    server.abort();

    assert_eq!(
        pinned_uris(&state.db, alice.id).await,
        [note_url(&base, "listed")]
    );
}
