//! Inbound `Follow`s refused before anything is stored: a follower on a
//! domain the target blocks, and a target that has moved.

use super::*;

/// A local target the answer can be published as: keys and an inbox.
async fn answering_local(pool: &PgPool, nickname: &str) -> User {
    federating_user(pool, nickname).await
}

/// The `peer.example` mirror `nickname`, with a personal inbox to answer.
async fn answerable_mirror(pool: &PgPool, nickname: &str) -> User {
    let ap_id = format!("https://peer.example/users/{nickname}");
    let user = insert(
        pool,
        &NewUser {
            nickname: format!("{nickname}@peer.example"),
            ap_id: ap_id.clone(),
            domain: Some("peer.example".to_owned()),
            ..NewUser::default()
        },
    )
    .await
    .expect("the answerable remote fixture inserts");
    let _row = write_actor(
        pool,
        &ActivityPubActorWrite {
            inbox: Some(format!("{ap_id}/inbox")),
            public_key: Some("PUB".to_owned()),
            ..ActivityPubActorWrite::bare(user.id)
        },
    )
    .await;
    user
}

/// The args of every queued delivery job, oldest first.
async fn delivery_args(pool: &PgPool) -> Vec<Value> {
    sqlx::query_scalar::<_, Value>("SELECT args FROM jobs WHERE kind = $1 ORDER BY inserted_at, id")
        .bind(crate::federation::delivery::KIND_DELIVER)
        .fetch_all(pool)
        .await
        .expect("the delivery jobs read")
}

/// A Follow from a domain the target blocks is refused: a `Reject` of
/// that very Follow is queued and no `follows` row is stored.
#[tokio::test]
async fn a_follow_from_a_domain_the_target_blocks_is_rejected_and_not_stored() {
    let db = crate::test_db::test_db().await.expect("test database slot");
    let pool = db.pool().clone();
    let state = app_state(&pool);
    let alice = answerable_mirror(&pool, "dom-alice").await;
    let alice_id = alice.id;
    let bob = answering_local(&pool, "dom-bob").await;
    domain_block::block_domain(&pool, bob.id, "peer.example")
        .await
        .expect("the domain block stores");
    let follow_view = view(
        "Follow",
        "dom-1",
        "https://peer.example/users/dom-alice",
        &json!(bob.ap_id.clone()),
    );
    let follow_id = follow_view.id.clone();

    let outcome = handle_follow(state, follow_view, alice).await;

    assert!(matches!(outcome, Ok(Outcome::Handled)), "{outcome:?}");
    assert!(
        follow::get(&pool, alice_id, bob.id)
            .await
            .expect("the pair reads")
            .is_none(),
        "nothing was stored"
    );
    let jobs = delivery_args(&pool).await;
    assert_eq!(jobs.len(), 1, "one answer is queued");
    assert_eq!(jobs[0]["body"]["type"], "Reject");
    assert_eq!(jobs[0]["body"]["object"]["id"], follow_id.as_str());
}

/// A block of a different host leaves the Follow to succeed: stored,
/// granted and answered with an `Accept`.
#[tokio::test]
async fn a_follow_from_an_unblocked_domain_still_succeeds() {
    let db = crate::test_db::test_db().await.expect("test database slot");
    let pool = db.pool().clone();
    let state = app_state(&pool);
    let alice = answerable_mirror(&pool, "ok-alice").await;
    let alice_id = alice.id;
    let bob = answering_local(&pool, "ok-bob").await;
    domain_block::block_domain(&pool, bob.id, "other.example")
        .await
        .expect("the domain block stores");

    let outcome = handle_follow(
        state,
        view(
            "Follow",
            "ok-1",
            "https://peer.example/users/ok-alice",
            &json!(bob.ap_id.clone()),
        ),
        alice,
    )
    .await;

    assert!(matches!(outcome, Ok(Outcome::Handled)), "{outcome:?}");
    let row = follow::get(&pool, alice_id, bob.id)
        .await
        .expect("the pair reads")
        .expect("the follow is stored");
    assert_eq!(
        row.state,
        FollowState::Accepted,
        "an unlocked target grants"
    );
    let jobs = delivery_args(&pool).await;
    assert_eq!(jobs.len(), 1, "one answer is queued");
    assert_eq!(jobs[0]["body"]["type"], "Accept");
}

/// A domain block another local user holds is not this target's.
#[tokio::test]
async fn another_users_domain_block_is_not_applied_to_this_target() {
    let db = crate::test_db::test_db().await.expect("test database slot");
    let pool = db.pool().clone();
    let state = app_state(&pool);
    let alice = answerable_mirror(&pool, "other-alice").await;
    let alice_id = alice.id;
    let bob = answering_local(&pool, "other-bob").await;
    let carol = answering_local(&pool, "other-carol").await;
    domain_block::block_domain(&pool, carol.id, "peer.example")
        .await
        .expect("carol's domain block stores");

    let outcome = handle_follow(
        state,
        view(
            "Follow",
            "other-1",
            "https://peer.example/users/other-alice",
            &json!(bob.ap_id.clone()),
        ),
        alice,
    )
    .await;

    assert!(matches!(outcome, Ok(Outcome::Handled)), "{outcome:?}");
    assert!(
        follow::get(&pool, alice_id, bob.id)
            .await
            .expect("the pair reads")
            .is_some(),
        "bob has no block, so the follow is stored"
    );
}

/// A Follow of an account that has moved is refused with a `Reject` and
/// nothing stored.
#[tokio::test]
async fn a_follow_of_a_moved_local_account_is_rejected() {
    let db = crate::test_db::test_db().await.expect("test database slot");
    let pool = db.pool().clone();
    let state = app_state(&pool);
    let alice = answerable_mirror(&pool, "moved-alice").await;
    let alice_id = alice.id;
    let bob = answering_local(&pool, "moved-bob").await;
    user::set_moved_to(&pool, bob.id, Some("https://elsewhere.example/users/bob"))
        .await
        .expect("the move is recorded");

    let outcome = handle_follow(
        state,
        view(
            "Follow",
            "moved-1",
            "https://peer.example/users/moved-alice",
            &json!(bob.ap_id.clone()),
        ),
        alice,
    )
    .await;

    assert!(matches!(outcome, Ok(Outcome::Handled)), "{outcome:?}");
    assert!(
        follow::get(&pool, alice_id, bob.id)
            .await
            .expect("the pair reads")
            .is_none(),
        "nothing was stored"
    );
    let jobs = delivery_args(&pool).await;
    assert_eq!(jobs.len(), 1, "one answer is queued");
    assert_eq!(jobs[0]["body"]["type"], "Reject");
}
