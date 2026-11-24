//! The shared inbound-activity fixtures the misc children's tests build
//! on: the test states, the normalizing fixture-activity builders, the
//! account and status fixture writers and the outcome and row readers.
//! Test-only.

use serde_json::{Map, Value, json};
use sqlx::PgPool;
use uuid::Uuid;

pub(super) use crate::federation::handlers::test_support::{
    accepted_follow, app_state, assert_handled, fresh_user, ignored, local_user,
};
use crate::{
    federation::dispatch::ActivityView,
    models::{
        report,
        status::{self, NewStatus, Status},
        user::{NewUser, User, insert},
    },
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

/// Every stored report, oldest filing first.
pub(super) async fn stored_reports(pool: &PgPool) -> Vec<report::Report> {
    sqlx::query_as::<_, report::Report>("SELECT * FROM reports ORDER BY id")
        .fetch_all(pool)
        .await
        .expect("the reports read")
}
