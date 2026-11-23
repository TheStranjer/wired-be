//! Fixtures the actor handler tests share: states, activities and
//! mirror rows.

use serde_json::{Value, json};
use sqlx::{PgPool, postgres::PgPoolOptions};
use uuid::Uuid;

use crate::{
    config::Config,
    federation::{
        dispatch::{ActivityView, FederationError, Outcome},
        remote_actor::{parse_actor, upsert_remote_user},
    },
    jobs::deletion::KIND_DELETE_REMOTE,
    models::{
        follow::{self, Follow},
        status::{self, NewStatus, Status},
        user::{self, Flag, NewUser, User, insert},
    },
    state::AppState,
};

/// The mirror every fixture here speaks about: alice on `peer.example`,
/// the actor id the signer rows and the activities agree on.
pub(super) const ALICE: &str = "https://peer.example/users/alice";
/// The instance origin the local fixtures and the follow rows are built
/// on, as `Config::for_tests` names it.
pub(super) const BASE: &str = "https://test.example";
/// A public key the parser accepts: only its being a non-empty string is
/// what `parse_actor` checks.
pub(super) const PEM: &str =
    "-----BEGIN PUBLIC KEY-----\nMIIBIjANBgkqhkiG9w0BAQEFAAOCAQ8A\n-----END PUBLIC KEY-----\n";
/// The replacement key one update brings.
pub(super) const NEW_PEM: &str =
    "-----BEGIN PUBLIC KEY-----\nNEWKEYMATERIAL\n-----END PUBLIC KEY-----\n";

/// Test state over `pool` with the fixed test config.
pub(super) fn app_state(pool: &PgPool) -> AppState {
    AppState::for_tests(pool.clone(), Config::for_tests())
}

/// Test state over a pool that cannot answer: the database having gone
/// away, as the handlers see it.
pub(super) async fn broken_state() -> AppState {
    let pool = PgPoolOptions::new()
        .connect_lazy("postgres://localhost/unused")
        .expect("a lazy pool needs no database");
    pool.close().await;
    app_state(&pool)
}

/// alice's `Person` document as a fetch last saw it: what `parse_actor`
/// requires, plus the profile the assertions watch change.
pub(super) fn person() -> Value {
    json!({
        "id": ALICE,
        "type": "Person",
        "preferredUsername": "alice",
        "name": "Alice",
        "summary": "<p>the old note</p>",
        "inbox": "https://peer.example/users/alice/inbox",
        "outbox": "https://peer.example/users/alice/outbox",
        "followers": "https://peer.example/users/alice/followers",
        "icon": {"type": "Image", "url": "https://peer.example/avatar/old.png"},
        "attachment": [
            {"type": "PropertyValue", "name": "Site", "value": "https://old.example"}
        ],
        "publicKey": {
            "id": "https://peer.example/users/alice#main-key",
            "owner": ALICE,
            "publicKeyPem": PEM,
        },
    })
}

/// Normalizes an activity document the way the dispatcher does: the
/// `Update` or `Delete` named by `id`, sent by `actor`, carrying
/// `object`. A fixture that stopped parsing says so through `expect`.
pub(super) fn view(kind: &str, id: &str, actor: &str, object: Value) -> ActivityView {
    let mut activity = json!({
        "id": format!("https://peer.example/activities/{id}"),
        "type": kind,
        "actor": actor,
    });
    let _object = activity
        .as_object_mut()
        .expect("the fixture skeleton is an object")
        .insert("object".to_owned(), object);
    ActivityView::parse(&activity).expect("the fixture activity parses")
}

/// The stored mirror of [`person`], as a completed fetch leaves it and
/// then aged past the flood guard: the state an `Update` arrives in.
pub(super) async fn mirror(pool: &PgPool) -> User {
    let stored = upsert_remote_user(pool, parse_actor(&person()).expect("the fixture parses"))
        .await
        .expect("the mirror fixture stores");
    ready_for_update(pool, stored.id).await
}

/// Ages a mirror's fetch stamp and hands back the row as it stands
/// afterwards: the account an `Update` may fold.
pub(super) async fn ready_for_update(pool: &PgPool, id: Uuid) -> User {
    let _aged =
        sqlx::query("UPDATE users SET last_fetched_at = now() - interval '1 minute' WHERE id = $1")
            .bind(id)
            .execute(pool)
            .await
            .expect("the fixture ages the fetch");
    fresh_user(pool, id).await
}

/// User `id` as it stands now.
pub(super) async fn fresh_user(pool: &PgPool, id: Uuid) -> User {
    user::get_by_id(pool, id)
        .await
        .expect("the user reads")
        .expect("the user exists")
}

/// A local account named `nickname` at `{BASE}/users/{nickname}`, for
/// the arms that must not let an inbound activity write over one.
pub(super) async fn local_user(pool: &PgPool, nickname: &str) -> User {
    let ap_id = user::local_ap_id(BASE, nickname);
    insert(pool, &NewUser::local(nickname, &ap_id))
        .await
        .expect("the local fixture inserts")
}

/// A remote mirror named `nickname` on `peer.example`, at `ap_id`.
pub(super) async fn remote_user(pool: &PgPool, nickname: &str, ap_id: &str) -> User {
    insert(
        pool,
        &NewUser {
            nickname: format!("{nickname}@peer.example"),
            ap_id: ap_id.to_owned(),
            local: Flag::new(false),
            domain: Some("peer.example".to_owned()),
            ..NewUser::default()
        },
    )
    .await
    .expect("the remote fixture inserts")
}

/// One of the mirror's own posts, at a peer-named `uri`.
pub(super) async fn mirror_post(pool: &PgPool, author: &User, tag: &str) -> Status {
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

/// The row of the `users.delete_remote` job queued for account `id`, as
/// `(queue, kind, args)`.
pub(super) async fn queued_sweep(pool: &PgPool, id: Uuid) -> Option<(String, String, Value)> {
    sqlx::query_as::<_, (String, String, Value)>(
        "SELECT queue, kind, args FROM jobs WHERE kind = $1 AND args->>'user_id' = $2",
    )
    .bind(KIND_DELETE_REMOTE)
    .bind(id.to_string())
    .fetch_optional(pool)
    .await
    .expect("the job queue reads")
}

/// `follower` holds an accepted follow of `target` — granted through the
/// model's own write, so the counters moved with it.
pub(super) async fn accepted_follow(pool: &PgPool, follower: &User, target: &User) -> Follow {
    let requested = follow::follow(pool, BASE, follower, target)
        .await
        .expect("the follow fixture is recorded");
    follow::accept(pool, requested.id)
        .await
        .expect("the fixture grants the request")
}
