//! Every notification type rendered through the listing, and the show
//! route (`GET /api/v1/notifications/{id}`): the owned live row answers,
//! everything the caller may not see is refused.

use axum::http::StatusCode;
use serde_json::json;
use uuid::Uuid;
use wired_be::{models::status::Visibility, util::time::mastodon_time};

use super::support::{
    NOT_FOUND, READ, V1, WRITE, assert_empty_object, fixture, list, seed_listing,
};
use crate::common;

/// The ten documented types, and whether each fixture row names the status.
const TYPE_SPREAD: [(&str, bool); 10] = [
    ("mention", true),
    ("follow", false),
    ("follow_request", false),
    ("reblog", true),
    ("favourite", true),
    ("move", false),
    ("update", false),
    ("poll", true),
    ("status", true),
    ("pleroma:emoji_reaction", true),
];

/// Every notification type renders its entity in the listing: the stored
/// type, the expanded actor, the fresh `pleroma` flags, the Mastodon time
/// — and the reaction its emoji beside its status.
#[tokio::test]
async fn every_type_renders_its_entity() {
    let db = common::test_db().await;
    let pool = db.pool().clone();
    let (alice, token) = fixture(&pool, "alice", READ).await;
    let bob = common::create_local_user(&pool, "bob").await;
    let post = common::create_status(&pool, &bob, "hello", Visibility::Public).await;
    let mut seeded = Vec::new();
    for (notification_type, names_status) in TYPE_SPREAD {
        let note = if notification_type == "pleroma:emoji_reaction" {
            common::create_reaction(&pool, &alice, &bob, &post, "🎉").await
        } else {
            let status = names_status.then_some(&post);
            common::create_notification(&pool, &alice, &bob, notification_type, status).await
        };
        seeded.push(note);
    }
    let app = common::test_app(pool);

    let body = list(&app, &token, "").await.json_value();
    let entities = body.as_array().expect("an array");
    assert_eq!(entities.len(), seeded.len(), "every type is in the answer");
    for ((notification_type, names_status), note) in TYPE_SPREAD.iter().zip(&seeded) {
        let entity = entities
            .iter()
            .find(|candidate| candidate["id"] == note.id.to_string())
            .unwrap_or_else(|| panic!("{} is in the page", note.id));
        assert_eq!(entity["type"], *notification_type, "the stored type");
        assert_eq!(
            entity["account"]["id"],
            bob.id.to_string(),
            "the actor expanded for {notification_type}"
        );
        assert_eq!(
            entity["pleroma"]["is_seen"],
            json!(false),
            "a fresh row reads unseen in {notification_type}"
        );
        assert_eq!(
            entity["created_at"],
            mastodon_time(&note.inserted_at),
            "the Mastodon time of {notification_type}"
        );
        let rendered_status = entity.get("status").is_some();
        assert_eq!(
            rendered_status, *names_status,
            "the status key rides on the status-naming types only, in {notification_type}"
        );
    }
    let reaction = entities
        .iter()
        .find(|entity| entity["type"] == "pleroma:emoji_reaction")
        .expect("the reaction");
    assert_eq!(reaction["emoji"], "🎉", "the reaction carries its emoji");
    assert_eq!(
        reaction["group_key"],
        format!("pleroma:emoji_reaction-{}-🎉", post.id),
        "and the stored key that groups it"
    );
    let favourite = entities
        .iter()
        .find(|entity| entity["type"] == "favourite")
        .expect("the favourite");
    assert_eq!(
        favourite["group_key"],
        format!("favourite-{}", post.id),
        "the favourite's stored key"
    );
    let follow = entities
        .iter()
        .find(|entity| entity["type"] == "follow")
        .expect("the follow");
    assert_eq!(
        follow["group_key"],
        format!("ungrouped-{}", follow["id"].as_str().expect("an id")),
        "an ungroupable row derives its key"
    );
}

/// `GET /api/v1/notifications/{id}` answers the caller's live row as the
/// listing renders it, and Pleroma's `is_seen` flag follows the row's
/// `seen` flag.
#[tokio::test]
async fn show_answers_the_owned_live_row() {
    let db = common::test_db().await;
    let pool = db.pool().clone();
    let seed = seed_listing(&pool).await;
    let app = common::test_app(pool.clone());

    let response = common::TestRequest::get(&format!("{V1}/{}", seed.favourite.id))
        .bearer(&seed.token)
        .send(&app)
        .await;
    common::assert_answer(&response, StatusCode::OK, "the read of the owned row");
    let body = response.json_value();
    assert_eq!(body["id"], seed.favourite.id.to_string(), "the one entity");
    assert_eq!(body["type"], "favourite");
    assert_eq!(
        body["group_key"],
        format!("favourite-{}", seed.post_id),
        "the stored key"
    );
    assert_eq!(body["account"]["id"], seed.bob_id.to_string(), "the actor");
    assert_eq!(
        body["status"]["id"],
        seed.post_id.to_string(),
        "the named status rendered"
    );
    assert_eq!(body["pleroma"]["is_seen"], false, "unseen");

    let _seen = sqlx::query("UPDATE notifications SET seen = true WHERE id = $1")
        .bind(seed.favourite.id)
        .execute(&pool)
        .await
        .expect("the seen flag lands");
    let read = common::TestRequest::get(&format!("{V1}/{}", seed.favourite.id))
        .bearer(&seed.token)
        .send(&app)
        .await;
    common::assert_answer(&read, StatusCode::OK, "the read of the seen row");
    assert_eq!(
        read.json_value()["pleroma"]["is_seen"],
        true,
        "the Pleroma flag follows the row"
    );
}

/// The read 404s, one indistinguishable answer, on a dismissed, foreign,
/// unknown, deactivated-actor's or malformed id.
#[tokio::test]
async fn show_refuses_everything_the_caller_may_not_see() {
    let db = common::test_db().await;
    let pool = db.pool().clone();
    let (alice, token) = fixture(&pool, "alice", READ).await;
    let bob = common::create_local_user(&pool, "bob").await;
    let carol = common::create_local_user(&pool, "carol").await;
    let dave = common::create_local_user(&pool, "dave").await;
    let mallory = common::create_local_user(&pool, "mallory").await;
    let doomed = common::create_notification(&pool, &alice, &bob, "follow", None).await;
    let ghosted = common::create_notification(&pool, &alice, &carol, "follow", None).await;
    let foreign = common::create_notification(&pool, &dave, &mallory, "follow", None).await;
    let app = common::test_app(pool.clone());

    let writer = common::create_app_and_token(&pool, &alice, WRITE).await;
    let dismissed = common::TestRequest::post(&format!("{V1}/{}/dismiss", doomed.id))
        .bearer(&writer)
        .send(&app)
        .await;
    assert_empty_object(&dismissed, "the fixture dismissal");
    let _deactivated = sqlx::query("UPDATE users SET is_active = false WHERE id = $1")
        .bind(carol.id)
        .execute(&pool)
        .await
        .expect("the deactivation lands");

    for id_text in [
        doomed.id.to_string(),
        ghosted.id.to_string(),
        foreign.id.to_string(),
        Uuid::now_v7().to_string(),
        "not-an-id".to_owned(),
    ] {
        let response = common::TestRequest::get(&format!("{V1}/{id_text}"))
            .bearer(&token)
            .send(&app)
            .await;
        common::assert_answer(
            &response,
            StatusCode::NOT_FOUND,
            &format!("the read of `{id_text}`"),
        );
        common::assert_error(&response, NOT_FOUND, &format!("the read of `{id_text}`"));
    }
}
