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
/// The account carries [`local_keys`] and the endpoint set a registered
/// account gets — `followers_url` and friends, which the publisher's follower
/// expansion reads — plus whatever else `customize` changes on top:
/// [`keyed_local_user`], or the same account locked, a bot, hidden, and still
/// able to federate.
///
/// # Panics
///
/// If the insert fails, e.g. because `nickname` is taken in this slot.
pub async fn keyed_local_user_with<F>(pool: &PgPool, nickname: &str, customize: F) -> User
where
    F: FnOnce(NewUser) -> NewUser,
{
    let keys = local_keys().clone();
    let shared_inbox = format!("{}/inbox", common::test_config().base_url);
    common::create_local_user_with(pool, nickname, move |user| {
        let ap_id = user.ap_id.clone();
        customize(NewUser {
            inbox: Some(user::inbox(&ap_id)),
            shared_inbox: Some(shared_inbox),
            outbox: Some(user::outbox(&ap_id)),
            followers_url: Some(user::followers(&ap_id)),
            following_url: Some(user::following(&ap_id)),
            featured_url: Some(user::featured(&ap_id)),
            public_key: Some(keys.public_pem),
            private_key: Some(keys.private_pem),
            ..user
        })
    })
    .await
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
