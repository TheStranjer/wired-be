//! The `Listen` handler's tests: the fold, the refusals, the by-reference
//! fetch and the `Undo`.

use axum::{
    Router,
    extract::State,
    http::{StatusCode, header},
    response::{IntoResponse as _, Response},
    routing::get,
};
use chrono::{Duration, Utc};
use serde_json::{Value, json};
use sqlx::PgPool;

use super::*;
use crate::{
    federation::handlers::test_support::{app_state, assert_handled, ignored, local_user},
    models::{
        activitypub_actor::ActivityPubActorWrite,
        scrobble::Scrobble,
        status::Visibility,
        user::{self, Flag, NewUser},
    },
    test_fixtures::write_actor,
    test_server::{bind, origin, spawn},
};

/// The actor every embedded fixture is delivered by.
const BOB: &str = "https://remote.example/users/bob";
/// The `ActivityStreams` public address.
const PUBLIC: &str = "https://www.w3.org/ns/activitystreams#Public";
/// A listen time safely in the past, so no clock clamp moves it.
const PAST: &str = "2024-05-01T10:00:00.000Z";

/// A remote account at `ap_id`.
async fn remote_user(pool: &PgPool, nickname: &str, ap_id: &str) -> User {
    let stored = user::insert(
        pool,
        &NewUser {
            nickname: nickname.to_owned(),
            ap_id: ap_id.to_owned(),
            local: Flag::new(false),
            domain: Some("remote.example".to_owned()),
            ..NewUser::default()
        },
    )
    .await
    .expect("the remote fixture inserts");
    let _row = write_actor(
        pool,
        &ActivityPubActorWrite {
            followers_url: Some(format!("{ap_id}/followers")),
            ..ActivityPubActorWrite::bare(stored.id)
        },
    )
    .await;
    stored
}

/// The fixture `Audio` of `actor`, named `n`.
fn audio_of(actor: &str, n: &str) -> Value {
    json!({
        "id": format!("https://remote.example/objects/{n}"),
        "type": "Audio",
        "actor": actor,
        "title": "Pleroma Song",
        "artist": "Lain Iwakura",
        "album": "Navi",
        "length": 200_000,
        "externalLink": "https://example.com/track/2",
        "published": PAST,
    })
}

/// A public `Listen` by `actor` named `n` around `object`.
fn listen_of(actor: &str, n: &str, object: Value) -> Value {
    let mut activity = json!({
        "id": format!("https://remote.example/activities/{n}"),
        "type": "Listen",
        "actor": actor,
        "to": [PUBLIC],
        "cc": [format!("{actor}/followers")],
        "object": Value::Null,
    });
    activity["object"] = object;
    activity
}

/// Runs the handler over `activity` as `signer`.
async fn run(pool: &PgPool, activity: &Value, signer: &User) -> Result<Outcome, FederationError> {
    let view = ActivityView::parse(activity).expect("the fixture is a dispatchable activity");
    handle_listen(app_state(pool), view, signer.clone()).await
}

/// Every scrobble row of `owner`, oldest first.
async fn rows_of(pool: &PgPool, owner: &User) -> Vec<Scrobble> {
    sqlx::query_as::<_, Scrobble>("SELECT * FROM scrobbles WHERE user_id = $1 ORDER BY id")
        .bind(owner.id)
        .fetch_all(pool)
        .await
        .expect("the scrobbles read")
}

/// A mock peer that answers every path with `document`, or 404 for `None`.
fn audio_peer(document: Option<Value>) -> Router {
    Router::new()
        .route("/{*path}", get(serve_document))
        .with_state(document)
}

/// [`audio_peer`]'s handler.
async fn serve_document(State(document): State<Option<Value>>) -> Response {
    document.map_or_else(
        || StatusCode::NOT_FOUND.into_response(),
        |body| {
            (
                [(header::CONTENT_TYPE, "application/activity+json")],
                body.to_string(),
            )
                .into_response()
        },
    )
}

#[tokio::test]
async fn an_embedded_listen_stores_one_scrobble() {
    let db = crate::test_db::test_db().await.expect("test database slot");
    let pool = db.pool().clone();
    let bob = remote_user(&pool, "bob@remote.example", BOB).await;
    let activity = listen_of(BOB, "one", audio_of(BOB, "one"));

    assert_handled(&run(&pool, &activity, &bob).await);

    let rows = rows_of(&pool, &bob).await;
    let [row] = rows.as_slice() else {
        panic!("exactly one scrobble is stored, got {rows:?}");
    };
    assert_eq!(row.title, "Pleroma Song", "the title");
    assert_eq!(row.artist.as_deref(), Some("Lain Iwakura"), "the artist");
    assert_eq!(row.album.as_deref(), Some("Navi"), "the album");
    assert_eq!(row.length, Some(200_000), "the length");
    assert_eq!(
        row.external_link.as_deref(),
        Some("https://example.com/track/2"),
        "the link"
    );
    assert_eq!(row.visibility, Visibility::Public, "`to: Public`");
    assert_eq!(
        row.ap_id.as_deref(),
        Some("https://remote.example/activities/one"),
        "keyed on the Listen's id"
    );
    assert_eq!(
        row.inserted_at.to_rfc3339(),
        "2024-05-01T10:00:00+00:00",
        "the listen happened when it says"
    );
}

#[tokio::test]
async fn the_same_listen_twice_leaves_one_row() {
    let db = crate::test_db::test_db().await.expect("test database slot");
    let pool = db.pool().clone();
    let bob = remote_user(&pool, "bob@remote.example", BOB).await;
    let activity = listen_of(BOB, "twice", audio_of(BOB, "twice"));

    assert_handled(&run(&pool, &activity, &bob).await);
    let again = run(&pool, &activity, &bob).await;

    assert_eq!(
        ignored(again),
        "a Listen already stored",
        "the redelivery is a no-op"
    );
    assert_eq!(rows_of(&pool, &bob).await.len(), 1, "one row stands");
}

#[tokio::test]
async fn the_audience_decides_the_visibility() {
    let db = crate::test_db::test_db().await.expect("test database slot");
    let pool = db.pool().clone();
    let bob = remote_user(&pool, "bob@remote.example", BOB).await;
    let followers = format!("{BOB}/followers");
    let cases = [
        (
            "unlisted",
            json!([followers]),
            json!([PUBLIC]),
            Visibility::Unlisted,
        ),
        (
            "private",
            json!([followers]),
            json!([]),
            Visibility::Private,
        ),
        (
            "direct",
            json!(["https://test.example/users/alice"]),
            json!([]),
            Visibility::Direct,
        ),
    ];
    for (name, to, cc, expected) in cases {
        let mut activity = listen_of(BOB, name, audio_of(BOB, name));
        activity["to"] = to;
        activity["cc"] = cc;
        assert_handled(&run(&pool, &activity, &bob).await);
        let rows = rows_of(&pool, &bob).await;
        let stored = rows
            .iter()
            .find(|row| row.ap_id.as_deref() == activity["id"].as_str())
            .expect("the listen is stored");
        assert_eq!(stored.visibility, expected, "the {name} audience");
    }
}

#[tokio::test]
async fn a_malformed_audio_is_ignored() {
    let db = crate::test_db::test_db().await.expect("test database slot");
    let pool = db.pool().clone();
    let bob = remote_user(&pool, "bob@remote.example", BOB).await;
    let actor_mismatch = audio_of("https://remote.example/users/mallory", "mismatch");
    let mut attributed = audio_of(BOB, "attributed");
    attributed["attributedTo"] = json!("https://remote.example/users/mallory");
    let mut not_audio = audio_of(BOB, "video");
    not_audio["type"] = json!("Video");
    let mut no_title = audio_of(BOB, "untitled");
    let _gone = no_title.as_object_mut().expect("an object").remove("title");
    let mut blank_title = audio_of(BOB, "blank");
    blank_title["title"] = json!("   ");
    let mut negative = audio_of(BOB, "negative");
    negative["length"] = json!(-1);
    let mut fractional = audio_of(BOB, "fractional");
    fractional["length"] = json!(1.5);
    let mut huge = audio_of(BOB, "huge");
    huge["length"] = json!(10_000_000_000_i64);
    let mut ftp = audio_of(BOB, "ftp");
    ftp["externalLink"] = json!("ftp://example.com/song");
    let cases = [
        ("actor mismatch", actor_mismatch),
        ("attributedTo mismatch", attributed),
        ("non-audio", not_audio),
        ("missing title", no_title),
        ("blank title", blank_title),
        ("negative length", negative),
        ("fractional length", fractional),
        ("oversized length", huge),
        ("non-http link", ftp),
    ];
    for (label, audio) in cases {
        let activity = listen_of(BOB, "bad", audio);
        let outcome = run(&pool, &activity, &bob).await;
        assert!(
            matches!(outcome, Ok(Outcome::Ignored(_))),
            "{label} is ignored, got {outcome:?}"
        );
    }
    assert!(rows_of(&pool, &bob).await.is_empty(), "nothing was stored");
}

#[tokio::test]
async fn optional_fields_may_be_absent_and_long_text_is_cut() {
    let db = crate::test_db::test_db().await.expect("test database slot");
    let pool = db.pool().clone();
    let bob = remote_user(&pool, "bob@remote.example", BOB).await;
    let audio = json!({
        "id": "https://remote.example/objects/long",
        "type": "Audio",
        "attributedTo": BOB,
        "title": "t".repeat(MAX_TITLE_CHARS + 50),
        "artist": "a".repeat(MAX_TEXT_CHARS + 1),
        "album": "",
        "length": null,
    });

    assert_handled(&run(&pool, &listen_of(BOB, "long", audio), &bob).await);

    let rows = rows_of(&pool, &bob).await;
    let [row] = rows.as_slice() else {
        panic!("one scrobble is stored, got {rows:?}");
    };
    assert_eq!(
        row.title.chars().count(),
        MAX_TITLE_CHARS,
        "the title is cut"
    );
    assert_eq!(
        row.artist.as_deref().map(|artist| artist.chars().count()),
        Some(MAX_TEXT_CHARS),
        "the artist is cut"
    );
    assert_eq!(row.album, None, "a blank album is none");
    assert_eq!(row.length, None, "a null length is none");
    assert_eq!(row.external_link, None, "an absent link is none");
    assert!(
        row.inserted_at <= Utc::now(),
        "stamped now when unpublished"
    );
}

#[tokio::test]
async fn a_future_published_time_is_clamped() {
    let db = crate::test_db::test_db().await.expect("test database slot");
    let pool = db.pool().clone();
    let bob = remote_user(&pool, "bob@remote.example", BOB).await;
    let mut audio = audio_of(BOB, "future");
    audio["published"] = json!((Utc::now() + Duration::days(30)).to_rfc3339());

    assert_handled(&run(&pool, &listen_of(BOB, "future", audio), &bob).await);

    let rows = rows_of(&pool, &bob).await;
    assert!(
        rows.iter().all(|row| row.inserted_at <= Utc::now()),
        "a remote clock moves nothing into the future"
    );
}

#[tokio::test]
async fn a_local_signer_is_ignored() {
    let db = crate::test_db::test_db().await.expect("test database slot");
    let pool = db.pool().clone();
    let alice = local_user(&pool, "alice").await;
    let activity = listen_of(&alice.ap_id, "local", audio_of(&alice.ap_id, "local"));

    let reason = ignored(run(&pool, &activity, &alice).await);

    assert_eq!(reason, "a Listen signed by a local account", "the reason");
    assert!(rows_of(&pool, &alice).await.is_empty(), "nothing stored");
}

#[tokio::test]
async fn a_bare_id_object_is_fetched() {
    let db = crate::test_db::test_db().await.expect("test database slot");
    let pool = db.pool().clone();
    let listener = bind().await;
    let base = origin(&listener);
    let actor = format!("{base}/users/bob");
    let bob = remote_user(&pool, "bob@127.0.0.1", &actor).await;
    let mut audio = audio_of(&actor, "fetched");
    audio["id"] = json!(format!("{base}/objects/fetched"));
    let server = spawn(listener, audio_peer(Some(audio)));
    let activity = listen_of(&actor, "fetched", json!(format!("{base}/objects/fetched")));

    assert_handled(&run(&pool, &activity, &bob).await);

    let rows = rows_of(&pool, &bob).await;
    let [row] = rows.as_slice() else {
        panic!("one scrobble is stored, got {rows:?}");
    };
    assert_eq!(row.title, "Pleroma Song", "the fetched document's title");
    server.abort();
}

/// Delivers a `Listen` whose bare-id `Audio` is served by a loopback peer.
///
/// The signer is `nickname` on the peer's origin; the document names
/// `creator` as its creator (`None` for no creator at all). Answers the
/// signer and the handler's outcome.
async fn fetched_outcome(
    pool: &PgPool,
    nickname: &str,
    creator: Option<&str>,
) -> (User, Result<Outcome, FederationError>) {
    let listener = bind().await;
    let base = origin(&listener);
    let actor = format!("{base}/users/{nickname}");
    let signer = remote_user(pool, &format!("{nickname}@127.0.0.1"), &actor).await;
    let mut audio = audio_of(
        &format!("{base}/users/{}", creator.unwrap_or(nickname)),
        "fetch",
    );
    audio["id"] = json!(format!("{base}/objects/fetch"));
    if creator.is_none() {
        let _gone = audio.as_object_mut().expect("an object").remove("actor");
    }
    let server = spawn(listener, audio_peer(Some(audio)));
    let activity = listen_of(&actor, "fetch", json!(format!("{base}/objects/fetch")));
    let outcome = run(pool, &activity, &signer).await;
    server.abort();
    (signer, outcome)
}

#[tokio::test]
async fn a_fetched_audio_of_somebody_else_is_ignored() {
    let db = crate::test_db::test_db().await.expect("test database slot");
    let pool = db.pool().clone();

    let (bob, outcome) = fetched_outcome(&pool, "bob", Some("mallory")).await;

    assert!(matches!(outcome, Ok(Outcome::Ignored(_))), "someone else's");
    assert!(rows_of(&pool, &bob).await.is_empty(), "bob has nothing");
}

#[tokio::test]
async fn a_fetched_audio_of_nobody_is_ignored() {
    let db = crate::test_db::test_db().await.expect("test database slot");
    let pool = db.pool().clone();

    let (dave, outcome) = fetched_outcome(&pool, "dave", None).await;

    assert!(matches!(outcome, Ok(Outcome::Ignored(_))), "nobody's");
    assert!(rows_of(&pool, &dave).await.is_empty(), "dave has nothing");
}

#[tokio::test]
async fn a_bare_id_that_cannot_be_fetched_is_ignored() {
    let db = crate::test_db::test_db().await.expect("test database slot");
    let pool = db.pool().clone();
    let listener = bind().await;
    let base = origin(&listener);
    let actor = format!("{base}/users/bob");
    let bob = remote_user(&pool, "bob@127.0.0.1", &actor).await;
    let server = spawn(listener, audio_peer(None));
    let activity = listen_of(&actor, "missing", json!(format!("{base}/objects/missing")));

    let reason = ignored(run(&pool, &activity, &bob).await);

    assert_eq!(
        reason, "referenced Audio could not be fetched",
        "the reason"
    );
    assert!(rows_of(&pool, &bob).await.is_empty(), "nothing stored");
    server.abort();
}

#[tokio::test]
async fn an_undo_removes_the_signers_own_scrobble_only() {
    let db = crate::test_db::test_db().await.expect("test database slot");
    let pool = db.pool().clone();
    let bob = remote_user(&pool, "bob@remote.example", BOB).await;
    let mallory_id = "https://remote.example/users/mallory";
    let mallory = remote_user(&pool, "mallory@remote.example", mallory_id).await;
    let activity = listen_of(BOB, "undone", audio_of(BOB, "undone"));
    assert_handled(&run(&pool, &activity, &bob).await);
    let undo = |actor: &str| {
        let raw = json!({
            "id": format!("https://remote.example/activities/undo-by-{actor}"),
            "type": "Undo",
            "actor": actor,
            "object": activity["id"],
        });
        ActivityView::parse(&raw).expect("the undo parses")
    };

    let stranger = handle_undo_listen(app_state(&pool), undo(mallory_id), mallory).await;
    assert_eq!(
        ignored(stranger),
        "no stored scrobble of that Listen",
        "another account cannot withdraw it"
    );
    assert_eq!(rows_of(&pool, &bob).await.len(), 1, "the row stands");

    let own = handle_undo_listen(app_state(&pool), undo(BOB), bob.clone()).await;
    assert_handled(&own);
    assert!(
        rows_of(&pool, &bob).await.is_empty(),
        "the owner's undo removes it"
    );
}

#[test]
fn register_binds_the_listen_and_its_undo() {
    let mut handlers = Handlers::new();
    register(&mut handlers);
    assert!(handlers.handles(TYPE_LISTEN), "the Listen route");
    assert!(handlers.handles_undo(TYPE_LISTEN), "the Undo route");
}
