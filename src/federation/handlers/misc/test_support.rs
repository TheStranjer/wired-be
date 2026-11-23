//! The shared inbound-activity fixtures the misc children's tests build
//! on: the test states, the normalizing fixture-activity builders, the
//! account and status fixture writers and the outcome and row readers.
//! Test-only.

use serde_json::{Map, Value, json};
use sqlx::PgPool;
use uuid::Uuid;

use crate::{
    config::Config,
    federation::dispatch::{ActivityView, FederationError, Outcome},
    models::{
        follow, report,
        status::{self, NewStatus, Status},
        user::{self, NewUser, User, insert},
    },
    state::AppState,
};

/// The mirror every fixture here is sent by: alice on `peer.example`.
pub(super) const ALICE: &str = "https://peer.example/users/alice";
/// MALLORY, the other `peer.example` mirror — the wrong-actor fixture.
pub(super) const MALLORY: &str = "https://peer.example/users/mallory";
/// The move target: carol on `new.example`.
pub(super) const CAROL: &str = "https://new.example/users/carol";
/// The instance origin the local fixtures are built on.
pub(super) const BASE: &str = "https://test.example";
/// The featured collection alice's `Add`/`Remove` fixtures target.
pub(super) const FEATURED: &str = "https://peer.example/users/alice/collections/featured";

/// Test state over `pool` with the fixed test config.
pub(super) fn app_state(pool: &PgPool) -> AppState {
    AppState::for_tests(pool.clone(), Config::for_tests())
}

/// Normalizes a fixture activity: `kind` named
/// `https://peer.example/activities/{n}`, sent by `actor`, carrying the
/// fields of `fields` (`object` among them, `target` and `content` when
/// the test gives them). A fixture that stops parsing says so.
pub(super) fn view(kind: &str, n: &str, actor: &str, fields: Value) -> ActivityView {
    let Value::Object(entries) = fields else {
        panic!("the fixture fields are an object");
    };
    let mut activity = json!({
        "id": format!("https://peer.example/activities/{n}"),
        "type": kind,
        "actor": actor,
    });
    let map = activity
        .as_object_mut()
        .expect("the fixture skeleton is an object");
    for (key, value) in entries {
        let _set = map.insert(key, value);
    }
    ActivityView::parse(&activity).expect("the fixture activity parses")
}

/// The `Block`/`Flag`/`Move`/`Undo` fixture with the object alone.
pub(super) fn object_view(kind: &str, n: &str, actor: &str, object: Value) -> ActivityView {
    let mut fields = Map::new();
    let _object = fields.insert("object".to_owned(), object);
    view(kind, n, actor, Value::Object(fields))
}

/// A local account named `nickname` at `{BASE}/users/{nickname}`.
pub(super) async fn local_user(pool: &PgPool, nickname: &str) -> User {
    insert(
        pool,
        &NewUser::local(nickname, &user::local_ap_id(BASE, nickname)),
    )
    .await
    .expect("the local fixture inserts")
}

/// A remote mirror at `ap_id` named `nickname`, with no other profile.
pub(super) async fn remote_at(pool: &PgPool, nickname: &str, ap_id: &str) -> User {
    let host = ap_id
        .split('/')
        .nth(2)
        .expect("the fixture id is a url")
        .to_owned();
    insert(
        pool,
        &NewUser {
            nickname: format!("{nickname}@{host}"),
            ap_id: ap_id.to_owned(),
            domain: Some(host),
            ..NewUser::default()
        },
    )
    .await
    .expect("the remote fixture inserts")
}

/// The signer mirror: [`ALICE`], with no featured url.
pub(super) async fn remote_signer(pool: &PgPool) -> User {
    remote_at(pool, "alice", ALICE).await
}

/// The signer mirror with alice's featured url stored.
pub(super) async fn featured_signer(pool: &PgPool) -> User {
    insert(
        pool,
        &NewUser {
            nickname: "alice@peer.example".to_owned(),
            ap_id: ALICE.to_owned(),
            domain: Some("peer.example".to_owned()),
            featured_url: Some(FEATURED.to_owned()),
            ..NewUser::default()
        },
    )
    .await
    .expect("the featured fixture inserts")
}

/// User `id` as it stands now.
pub(super) async fn fresh_user(pool: &PgPool, id: Uuid) -> User {
    user::get_by_id(pool, id)
        .await
        .expect("the user reads")
        .expect("the user exists")
}

/// A post by the remote `author` at a peer-named uri.
pub(super) async fn remote_post(pool: &PgPool, author: &User, tag: &str) -> Status {
    let id = Uuid::now_v7();
    status::insert(
        pool,
        BASE,
        &NewStatus {
            user_id: author.id,
            uri: Some(format!("https://peer.example/objects/{tag}-{id}")),
            content: format!("{tag} says hello"),
            raw_text: format!("{tag} says hello"),
            ..NewStatus::default()
        },
    )
    .await
    .expect("the status fixture inserts")
}

/// A public post by the local `author`.
pub(super) async fn local_post(pool: &PgPool, author: &User, text: &str) -> Status {
    status::insert(pool, BASE, &NewStatus::local(author, text))
        .await
        .expect("the local status fixture inserts")
}

/// `follower` holds an accepted follow of `target` — granted through the
/// model's own write, so the counters moved with it.
pub(super) async fn accepted_follow(pool: &PgPool, follower: &User, target: &User) {
    let requested = follow::follow(pool, BASE, follower, target)
        .await
        .expect("the follow fixture is recorded");
    let _granted = follow::accept(pool, requested.id)
        .await
        .expect("the fixture grants the request");
}

/// The reason an ignored outcome carries, panicking when the handler did
/// not ignore the activity at all.
pub(super) fn ignored(outcome: Result<Outcome, FederationError>) -> &'static str {
    match outcome {
        Ok(Outcome::Ignored(reason)) => reason,
        other => panic!("expected the activity to be ignored, got {other:?}"),
    }
}

/// Asserts a handler folded its activity.
pub(super) fn assert_handled(outcome: &Result<Outcome, FederationError>) {
    assert_eq!(
        outcome,
        &Ok(Outcome::Handled),
        "the handler folded the activity"
    );
}

/// Every stored report, oldest filing first.
pub(super) async fn stored_reports(pool: &PgPool) -> Vec<report::Report> {
    sqlx::query_as::<_, report::Report>("SELECT * FROM reports ORDER BY id")
        .fetch_all(pool)
        .await
        .expect("the reports read")
}
