//! A remote mention: resolution, mirroring and the signed delivery, against a
//! mock peer.

use super::*;

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
        json!(format!("{BASE}/activities/{}", common::id_of(status))),
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
    common::create_signer(pool, nickname).await.user
}
