//! The key pairs of the mock peer and of the local fixture accounts, and the
//! keyed local users.

use super::*;

/// The mock peer's key pair, made once for the process.
///
/// RSA-2048 generation is slow in a debug build, and one identity for every
/// mock actor is enough for a peer that only ever signs its own activities.
pub(super) fn peer_keys() -> &'static KeyPair {
    static KEYS: OnceLock<KeyPair> = OnceLock::new();
    KEYS.get_or_init(|| generate_keypair().expect("the mock peer's key pair generates"))
}

/// A key pair for *local* fixture accounts, made once for the process.
///
/// Deliberately not the [mock peer's](peer_keys): a delivery that verifies
/// against this public key was signed by this instance's own actor, not by the
/// peer. A local account needs a private key to federate at all — the delivery
/// job discards an activity its signing actor cannot sign — so a scenario that
/// expects a real `POST` out of this instance builds its local accounts with
/// [`keyed_local_user`].
///
/// # Panics
///
/// If the key pair cannot be generated, which it always can here.
pub fn local_keys() -> &'static KeyPair {
    static KEYS: OnceLock<KeyPair> = OnceLock::new();
    KEYS.get_or_init(|| generate_keypair().expect("the local fixture key pair generates"))
}

/// A local [`User`] named `nickname` as registration leaves it.
///
/// The account's `activitypub_actors` row carries [`local_keys`] and the
/// endpoint set a registered account gets — `followers_url` and friends,
/// which the publisher's follower expansion reads — and the `users` row
/// takes whatever `customize` changes: [`keyed_local_user`], or the same
/// account locked, a bot, hidden, and still able to federate.
///
/// # Panics
///
/// If either insert fails, e.g. because `nickname` is taken in this slot.
pub async fn keyed_local_user_with<F>(pool: &PgPool, nickname: &str, customize: F) -> User
where
    F: FnOnce(NewUser) -> NewUser,
{
    let user = common::create_local_user_with(pool, nickname, customize).await;
    let _row = common::write_actor(
        pool,
        &ActivityPubActorWrite::for_local_account(
            &user,
            &common::test_config().base_url,
            local_keys().clone(),
        ),
    )
    .await;
    user
}

/// This instance's own actor, its signing keys swapped for [`local_keys`].
///
/// A delivery the instance actor signs then verifies against `local_keys()`,
/// like one signed by a [`keyed_local_user`]. Call it before the delivery is
/// made, since the signer reads the stored keys at send time.
///
/// # Panics
///
/// If the instance actor cannot be provisioned or has no `activitypub_actors`
/// row.
pub async fn keyed_instance_actor(pool: &PgPool) -> User {
    let actor =
        wired_be::services::instance_actor::ensure_instance_actor(pool, &common::test_config())
            .await
            .expect("the instance actor stands");
    let keyed = sqlx::query(
        "UPDATE activitypub_actors SET public_key = $2, private_key = $3 WHERE user_id = $1",
    )
    .bind(actor.id)
    .bind(&local_keys().public_pem)
    .bind(&local_keys().private_pem)
    .execute(pool)
    .await
    .expect("the instance actor's keys are swapped")
    .rows_affected();
    assert_eq!(keyed, 1, "the instance actor has exactly one actor row");
    actor
}

/// A local [`User`] named `nickname` that can sign its own deliveries:
/// [`keyed_local_user_with`] with nothing else changed.
///
/// # Panics
///
/// If the insert fails, e.g. because `nickname` is taken in this slot.
pub async fn keyed_local_user(pool: &PgPool, nickname: &str) -> User {
    keyed_local_user_with(pool, nickname, std::convert::identity).await
}
