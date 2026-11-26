//! Suspended or disabled (WBE-878): the umbrella's `suspend` and `disable`
//! read back as the two different states they are, in the admin entity, the
//! `status` filters and the federated actor, and `unsuspend` and `enable`
//! each reverse only their own.

use axum::http::StatusCode;
use serde_json::{Value, json};
use uuid::Uuid;

use super::support::{V1, listed, read, v1_page, v2_page};
use super::write_support::{
    DISABLED, Writes, account_action_path, act, job_count, logged, post_json, seed_writes, verify,
};
use crate::common::{self, TestRequest};

/// A bearer token of the admin who can read and write the accounts.
async fn full_token(seeded: &Writes) -> String {
    common::create_app_and_token(&seeded.pool, &seeded.root, "admin:read admin:write").await
}

/// The nicknames one listing page answers with, in a fixed order.
fn names(page: &Value) -> Vec<&str> {
    let mut found = listed(page);
    found.sort_unstable();
    found
}

/// The admin entity of `id` as the show route answers it.
async fn entity_of(seeded: &Writes, token: &str, id: Uuid) -> Value {
    read(&seeded.app, token, &format!("{V1}/{id}")).await
}

/// The actor document of `nickname` as a federation peer fetches it.
async fn actor_of(seeded: &Writes, nickname: &str) -> Value {
    let response = TestRequest::get(&format!("/users/{nickname}"))
        .header("accept", "application/activity+json")
        .send(&seeded.app)
        .await;
    common::assert_answer(&response, StatusCode::OK, "the actor document");
    response.json_value()
}

/// Posts the `suffix` route of account `id` and answers its entity.
async fn lift(seeded: &Writes, token: &str, id: Uuid, suffix: &str) -> Value {
    let response = post_json(
        &seeded.app,
        token,
        &account_action_path(id, suffix),
        &json!({}),
    )
    .await;
    common::assert_answer(&response, StatusCode::OK, "the reversal");
    response.json_value()
}

/// `suspend` reads `suspended: true` (and not `disabled`) in the admin and
/// the public entity, and `status=suspended` finds it where `status=disabled`
/// does not.
#[tokio::test]
async fn a_suspended_account_reads_suspended_and_the_filters_find_it() {
    let db = common::test_db().await;
    let seeded = seed_writes(&db).await;
    let token = full_token(&seeded).await;

    act(
        &seeded.app,
        &seeded.token,
        seeded.plain.id,
        &json!({"type": "suspend"}),
    )
    .await;

    let shown = entity_of(&seeded, &token, seeded.plain.id).await;
    assert_eq!(shown["suspended"], json!(true), "the admin entity");
    assert_eq!(shown["disabled"], json!(false), "is no disabled one");
    assert_eq!(
        shown["account"]["suspended"],
        json!(true),
        "and the profile entity agrees"
    );
    let suspended = v2_page(&seeded.app, &token, "status=suspended").await;
    assert_eq!(names(&suspended), vec!["plain"], "v2 `status=suspended`");
    let flagged = v1_page(&seeded.app, &token, "suspended=true").await;
    assert_eq!(names(&flagged), vec!["plain"], "v1 `suspended=true`");
    let disabled = v2_page(&seeded.app, &token, "status=disabled").await;
    assert_eq!(
        names(&disabled),
        vec!["asleep"],
        "`status=disabled` keeps to the account that was only disabled"
    );
    let unsuspended = v1_page(&seeded.app, &token, "suspended=false").await;
    assert!(
        !names(&unsuspended).contains(&"plain"),
        "`suspended=false` leaves it out"
    );
}

/// `disable` reads `disabled: true` and `suspended: false`, and the filters
/// say the same.
#[tokio::test]
async fn a_disabled_account_reads_disabled_and_not_suspended() {
    let db = common::test_db().await;
    let seeded = seed_writes(&db).await;
    let token = full_token(&seeded).await;

    act(
        &seeded.app,
        &seeded.token,
        seeded.plain.id,
        &json!({"type": "disable"}),
    )
    .await;

    let shown = entity_of(&seeded, &token, seeded.plain.id).await;
    assert_eq!(shown["disabled"], json!(true), "the admin entity");
    assert_eq!(shown["suspended"], json!(false), "is no suspended one");
    assert_eq!(
        shown["account"]["suspended"],
        json!(false),
        "and neither is the profile entity"
    );
    let disabled = v2_page(&seeded.app, &token, "status=disabled").await;
    assert_eq!(
        names(&disabled),
        vec!["asleep", "plain"],
        "`status=disabled` lists it"
    );
    let suspended = v2_page(&seeded.app, &token, "status=suspended").await;
    assert!(names(&suspended).is_empty(), "`status=suspended` does not");
}

/// `unsuspend` lifts a suspension and `enable` a disable; each leaves the
/// other state as it is.
#[tokio::test]
async fn unsuspend_and_enable_each_reverse_their_own_state() {
    let db = common::test_db().await;
    let seeded = seed_writes(&db).await;
    let token = full_token(&seeded).await;
    let id = seeded.plain.id;

    act(&seeded.app, &seeded.token, id, &json!({"type": "suspend"})).await;
    let kept = lift(&seeded, &token, id, "enable").await;
    assert_eq!(
        kept["suspended"],
        json!(true),
        "`enable` does not lift a suspension"
    );
    let freed = lift(&seeded, &token, id, "unsuspend").await;
    assert_eq!(freed["suspended"], json!(false), "`unsuspend` does");
    assert_eq!(freed["disabled"], json!(false), "and the account is active");

    act(&seeded.app, &seeded.token, id, &json!({"type": "disable"})).await;
    let held = lift(&seeded, &token, id, "unsuspend").await;
    assert_eq!(
        held["disabled"],
        json!(true),
        "`unsuspend` does not lift a disable"
    );
    let enabled = lift(&seeded, &token, id, "enable").await;
    assert_eq!(enabled["disabled"], json!(false), "`enable` does");
    assert_eq!(
        enabled["suspended"],
        json!(false),
        "and it is not suspended"
    );
    let page = v2_page(&seeded.app, &token, "status=disabled").await;
    assert_eq!(
        names(&page),
        vec!["asleep"],
        "only the fixture stays disabled"
    );
}

/// The federated actor says `suspended` exactly while `suspended_at` is set:
/// a `disable` serves the full profile, a `suspend` the minimal document, and
/// `unsuspend` the full profile again.
#[tokio::test]
async fn the_actor_document_follows_the_suspension() {
    let db = common::test_db().await;
    let seeded = seed_writes(&db).await;
    let token = full_token(&seeded).await;
    let id = seeded.plain.id;

    act(&seeded.app, &seeded.token, id, &json!({"type": "disable"})).await;
    let disabled = actor_of(&seeded, "plain").await;
    assert!(
        disabled.get("suspended").is_none(),
        "a disabled account federates as itself"
    );
    assert!(disabled.get("summary").is_some(), "with its profile");

    act(&seeded.app, &seeded.token, id, &json!({"type": "suspend"})).await;
    let suspended = actor_of(&seeded, "plain").await;
    assert_eq!(
        suspended["suspended"],
        json!(true),
        "a suspended one says so"
    );
    assert!(
        suspended.get("summary").is_none(),
        "with the profile stripped"
    );

    let _freed = lift(&seeded, &token, id, "unsuspend").await;
    let restored = actor_of(&seeded, "plain").await;
    assert!(
        restored.get("suspended").is_none(),
        "unsuspending drops the flag"
    );
    assert!(
        restored.get("summary").is_some(),
        "and restores the profile"
    );
}

/// An inactive account whose `suspended_at` is stamped reads `suspended`
/// rather than `disabled`, and `unsuspend` lifts it.
#[tokio::test]
async fn a_stamped_account_reads_suspended_until_it_is_unsuspended() {
    let db = common::test_db().await;
    let seeded = seed_writes(&db).await;
    let token = full_token(&seeded).await;

    let stamped = sqlx::query(
        "UPDATE users SET suspended_at = now(), is_active = false, local_deactivated = true
         WHERE id = $1",
    )
    .bind(seeded.plain.id)
    .execute(&seeded.pool)
    .await
    .expect("the stamp writes")
    .rows_affected();
    assert_eq!(stamped, 1, "one row");
    let shown = entity_of(&seeded, &token, seeded.plain.id).await;
    assert_eq!(shown["suspended"], json!(true), "stamped means suspended");
    assert_eq!(shown["disabled"], json!(false), "and not disabled");

    let freed = lift(&seeded, &token, seeded.plain.id, "unsuspend").await;
    assert_eq!(freed["suspended"], json!(false), "unsuspend lifts it");
    assert_eq!(freed["disabled"], json!(false), "and activates the account");
    let none = v2_page(&seeded.app, &token, "status=suspended").await;
    assert!(names(&none).is_empty(), "and nothing is suspended any more");
}

/// A suspend and an unsuspend leave a disable where it was, and `enable` on
/// a suspended account that is not disabled changes nothing at all.
#[tokio::test]
async fn suspending_and_unsuspending_leave_a_disable_alone() {
    let db = common::test_db().await;
    let seeded = seed_writes(&db).await;
    let token = full_token(&seeded).await;
    let id = seeded.plain.id;
    let owner = common::create_app_and_token(&seeded.pool, &seeded.plain, "read").await;

    act(&seeded.app, &seeded.token, id, &json!({"type": "disable"})).await;
    act(&seeded.app, &seeded.token, id, &json!({"type": "suspend"})).await;
    let both = entity_of(&seeded, &token, id).await;
    assert_eq!(both["disabled"], json!(true), "disabled");
    assert_eq!(both["suspended"], json!(true), "and suspended");
    let disabled = v2_page(&seeded.app, &token, "status=disabled").await;
    assert_eq!(names(&disabled), vec!["asleep", "plain"], "both listed");

    let freed = lift(&seeded, &token, id, "unsuspend").await;
    assert_eq!(freed["suspended"], json!(false), "the suspension is lifted");
    assert_eq!(freed["disabled"], json!(true), "the disable is not");
    let refused = verify(&seeded.app, &owner).await;
    common::assert_answer(
        &refused,
        StatusCode::FORBIDDEN,
        "the owner stays locked out",
    );
    common::assert_error(&refused, DISABLED, "as disabled");

    act(&seeded.app, &seeded.token, id, &json!({"type": "suspend"})).await;
    let jobs = job_count(&seeded.pool).await;
    let entries = logged(&seeded.pool, "activate").await.len();
    let kept = lift(&seeded, &token, id, "enable").await;
    assert_eq!(kept["disabled"], json!(false), "`enable` lifts the disable");
    assert_eq!(kept["suspended"], json!(true), "and not the suspension");
    assert_eq!(job_count(&seeded.pool).await, jobs, "federating nothing");
    assert_eq!(logged(&seeded.pool, "activate").await.len(), entries + 1);
    let still = lift(&seeded, &token, id, "enable").await;
    assert_eq!(
        still["suspended"],
        json!(true),
        "a second enable is a no-op"
    );
    assert_eq!(
        logged(&seeded.pool, "activate").await.len(),
        entries + 1,
        "and writes no entry on a suspended, enabled account"
    );
    assert_eq!(job_count(&seeded.pool).await, jobs, "or any job");
}

/// A disable queues no federation job and leaves the account's own row
/// active, and `enable` queues none either.
#[tokio::test]
async fn disabling_and_enabling_federate_nothing() {
    let db = common::test_db().await;
    let seeded = seed_writes(&db).await;
    let token = full_token(&seeded).await;
    let id = seeded.plain.id;
    let jobs = job_count(&seeded.pool).await;

    act(&seeded.app, &seeded.token, id, &json!({"type": "disable"})).await;
    let _enabled = lift(&seeded, &token, id, "enable").await;

    assert_eq!(job_count(&seeded.pool).await, jobs, "no job was queued");
}
