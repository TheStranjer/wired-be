//! The signature gates.
//!
//! The domain reject lists (step 3 of the [module docs](super)), checked
//! before anything is fetched, then after resolution the cryptographic
//! verification and its once-per-window refetch retry (step 6) and the
//! signer-containment rule (step 7).

use chrono::Utc;
use serde_json::Value;
use tracing::debug;

use super::{
    Inbound, InboxAuthError,
    signer::{is_self_delete, key_refetch_allowed},
};
use crate::{
    federation::{
        fetch_actor::{FetchError, refresh_user},
        json::value_or_id,
        policy::{host_refused, signer_host},
        signature::verify::{SignedRequestParts, verify},
    },
    models::{activitypub_actor, user::User},
    state::AppState,
};

/// Why [`verify_with_refetch`] gave up on a signer.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum RefetchOutcome {
    /// Neither the stored key nor the refetched one verified the
    /// signature, the refetch window was closed, or the refetch failed.
    Invalid,
    /// The refetch answered `410 Gone`: the actor no longer exists.
    Gone,
}

/// Verifies a signature against the signer's key, refetching it once.
///
/// `check` takes a public PEM; on failure the key is refetched and checked
/// again (step 6 of the [module docs](super)). The authorized-fetch gate
/// runs the very same retry.
///
/// The retry refetches `signer`'s actor document — the peer may have
/// rotated its key since the mirror was written — but at most once per
/// actor per 60 s window, so a flood of bad signatures cannot turn into a
/// fetch storm. The retried check uses the refreshed mirror, which the
/// successful caller then receives.
///
/// # Errors
///
/// [`RefetchOutcome::Gone`] when the refetch proved the actor gone;
/// [`RefetchOutcome::Invalid`] for every other way of not verifying.
pub async fn verify_with_refetch<F>(
    state: &AppState,
    signer: User,
    check: F,
) -> Result<User, RefetchOutcome>
where
    F: Fn(&str) -> bool + Send,
{
    if let Some(pem) = stored_public_key(state, &signer).await?
        && check(&pem)
    {
        return Ok(signer);
    }
    if !key_refetch_allowed(&signer.ap_id) {
        debug!(actor = %signer.ap_id, "signature failed; refetch window still closed");
        return Err(RefetchOutcome::Invalid);
    }
    match refresh_user(state, &signer).await {
        Ok(refreshed) => match stored_public_key(state, &refreshed).await? {
            Some(pem) if check(&pem) => Ok(refreshed),
            Some(_) | None => Err(RefetchOutcome::Invalid),
        },
        Err(FetchError::Gone) => Err(RefetchOutcome::Gone),
        Err(_err) => Err(RefetchOutcome::Invalid),
    }
}

/// The public key PEM stored for `signer`, from its `activitypub_actors`
/// row.
///
/// A signer with no row, like one with no key, has nothing to verify with
/// (`None`). A database failure leaves the signature unproven, so it is
/// [`RefetchOutcome::Invalid`] too.
async fn stored_public_key(
    state: &AppState,
    signer: &User,
) -> Result<Option<String>, RefetchOutcome> {
    match activitypub_actor::get(&state.db, signer.id).await {
        Ok(actor) => Ok(actor.and_then(|row| row.public_key)),
        Err(err) => {
            debug!(%err, actor = %signer.ap_id, "could not read the signer's public key");
            Err(RefetchOutcome::Invalid)
        }
    }
}

/// Verifies the inbox signature through [`verify_with_refetch`] and maps
/// its outcome: 401 `Invalid signature`, or the dropped 202 when the
/// refetch proved the actor gone and the activity is its own `Delete`.
pub(super) async fn verify_signature(
    state: &AppState,
    inbound: &Inbound<'_>,
    signer: User,
) -> Result<User, InboxAuthError> {
    let request = inbound.signed_request();
    let signer_id = signer.ap_id.clone();
    verify_with_refetch(state, signer, |pem| verifies(inbound, &request, pem))
        .await
        .map_err(|outcome| match outcome {
            RefetchOutcome::Gone if is_self_delete(inbound.json, &signer_id) => {
                InboxAuthError::Discarded
            }
            RefetchOutcome::Gone | RefetchOutcome::Invalid => InboxAuthError::invalid_signature(),
        })
}

/// The cryptographic check at the current moment; [`verify`] owns the
/// signed-header and `Date` skew rules.
fn verifies(inbound: &Inbound<'_>, request: &SignedRequestParts<'_>, public_pem: &str) -> bool {
    verify(inbound.parsed, request, public_pem, Utc::now()).is_ok()
}

/// Step 7 of the [module docs](super): the activity's `actor` (string or
/// object id) must name the authenticated signer's `ap_id`. An absent
/// `actor` names nobody and fails the same way; forwarding authenticated
/// by an `LD` signature is not supported yet, so a different server's
/// activity cannot pass either.
pub(super) fn require_containment(json: &Value, signer: &User) -> Result<(), InboxAuthError> {
    let claimed_actor = json.get("actor").and_then(value_or_id);
    if claimed_actor.as_deref() == Some(signer.ap_id.as_str()) {
        return Ok(());
    }
    debug!(
        signer = %signer.ap_id,
        claimed = %claimed_actor.as_deref().unwrap_or_default(),
        "inbox activity actor does not match the signature signer"
    );
    Err(InboxAuthError::actor_mismatch())
}

/// Step 3 of the [module docs](super): the signer's instance may not be on
/// a reject list — neither the instance-policy table nor the domain lists
/// of the [`mrf`](crate::federation::mrf) configuration. Runs before any
/// fetch, since the decision depends only on the `keyId` host. An
/// unparsable `keyId` names no host to check — such a signer never gets
/// past resolution anyway, so it cannot smuggle a blocked delivery in.
pub(super) async fn require_unblocked(
    state: &AppState,
    key_id: &str,
) -> Result<(), InboxAuthError> {
    let Some(host) = signer_host(key_id) else {
        return Ok(());
    };
    if host_refused(state, &host).await? {
        debug!(key_id, "inbox delivery from a rejected instance");
        return Err(InboxAuthError::blocked());
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use std::sync::{Arc, atomic::Ordering};

    use axum::http::StatusCode;
    use serde_json::json;

    use crate::{
        federation::inbox_auth::{
            ACTOR_MISMATCH, INVALID_SIGNATURE, REJECTED,
            test_support::{
                LOOPBACK_HOST, ROTATED_PRIVATE_PEM, ROTATED_PUBLIC_PEM, STALE_KEY_PEM, activity,
                bind, block_domain, deliver, inbox_app, origin, peer, peer_app, seed_remote_user,
                send, set_mrf_lists, spawn, test_state,
            },
        },
        models::{activitypub_actor, user::get_by_ap_id},
        services::runtime_config,
        util::keys::{FIXTURE_PRIVATE_PKCS1_PEM, FIXTURE_PUBLIC_PKCS1_PEM},
    };

    /// A rotated key: the stored public key rejects the signature, the
    /// one forced refetch replaces it, and the retry succeeds. The next
    /// delivery answers from the refreshed mirror with no new fetch.
    #[tokio::test]
    async fn a_rotated_key_succeeds_after_one_refetch() {
        let db = crate::test_db::test_db().await.expect("test database slot");
        let pool = db.pool().clone();
        let listener = bind().await;
        let base = origin(&listener);
        let ap_id = format!("{base}/users/alice");
        // The peer serves the NEW key; the database still holds the old.
        let peer = peer(base.clone(), ROTATED_PUBLIC_PEM.to_owned());
        let hits = Arc::clone(&peer.hits);
        let server = spawn(listener, peer_app(peer));
        seed_remote_user(&pool, &ap_id, FIXTURE_PUBLIC_PKCS1_PEM).await;
        let app = inbox_app(test_state(pool.clone()));

        let body = activity(&ap_id, "https://test.example/users/bob");
        let rotated = deliver(&format!("{ap_id}#main-key"), ROTATED_PRIVATE_PEM, &body);
        let rotated_reply = send(&app, &rotated).await;
        assert_eq!(
            rotated_reply.status,
            StatusCode::OK,
            "the refetched key accepts the signature: {}",
            rotated_reply.error
        );
        assert_eq!(hits.load(Ordering::SeqCst), 1, "exactly one refetch");

        let mirror_reply = send(&app, &rotated).await;
        assert_eq!(
            mirror_reply.status,
            StatusCode::OK,
            "the refreshed mirror answers"
        );
        assert_eq!(hits.load(Ordering::SeqCst), 1, "and no second fetch");
        let stored = get_by_ap_id(&pool, &ap_id).await.unwrap().unwrap();
        let stored_key = activitypub_actor::get(&pool, stored.id)
            .await
            .unwrap()
            .and_then(|row| row.public_key)
            .expect("the mirror's actor row holds a key");
        assert_eq!(
            stored_key.trim(),
            ROTATED_PUBLIC_PEM.trim(),
            "the mirror row now carries the rotated key"
        );
        server.abort();
    }

    /// The signer's key comes from `activitypub_actors`: the seeded row holds
    /// a stale key, the test swaps in the matching key with a fresh stamp, and
    /// no peer is running to refetch from.
    #[tokio::test]
    async fn the_signers_key_is_read_from_the_actor_row() {
        let db = crate::test_db::test_db().await.expect("test database slot");
        let pool = db.pool().clone();
        let ap_id = "https://remote.example/users/alice";
        seed_remote_user(&pool, ap_id, STALE_KEY_PEM).await;
        let updated = sqlx::query(
            "UPDATE activitypub_actors SET public_key = $2, last_fetched_at = now()
              WHERE user_id = (SELECT id FROM users WHERE ap_id = $1)",
        )
        .bind(ap_id)
        .bind(FIXTURE_PUBLIC_PKCS1_PEM)
        .execute(&pool)
        .await
        .unwrap()
        .rows_affected();
        assert_eq!(updated, 1, "the seeded signer has one actor row");
        let app = inbox_app(test_state(pool.clone()));

        let body = activity(ap_id, "https://test.example/users/bob");
        let signed = deliver(
            &format!("{ap_id}#main-key"),
            FIXTURE_PRIVATE_PKCS1_PEM,
            &body,
        );
        let reply = send(&app, &signed).await;

        assert_eq!(
            reply.status,
            StatusCode::OK,
            "the actor row's key verifies the signature: {}",
            reply.error
        );
    }

    /// A signature no key can answer: the first delivery spends the
    /// actor's one refetch of the window, the second fails without
    /// touching the network again.
    #[tokio::test]
    async fn the_key_refetch_is_rate_limited_to_once_per_window() {
        let db = crate::test_db::test_db().await.expect("test database slot");
        let pool = db.pool().clone();
        let listener = bind().await;
        let base = origin(&listener);
        let ap_id = format!("{base}/users/alice");
        // The peer keeps serving the fixture key; the request is signed
        // with the unrelated generated pair, so neither key ever fits.
        let peer = peer(base.clone(), FIXTURE_PUBLIC_PKCS1_PEM.to_owned());
        let hits = Arc::clone(&peer.hits);
        let server = spawn(listener, peer_app(peer));
        seed_remote_user(&pool, &ap_id, STALE_KEY_PEM).await;
        let app = inbox_app(test_state(pool.clone()));

        let body = activity(&ap_id, "https://test.example/users/bob");
        let wrong = deliver(&format!("{ap_id}#main-key"), ROTATED_PRIVATE_PEM, &body);
        let first_reply = send(&app, &wrong).await;
        assert_eq!(
            first_reply.status,
            StatusCode::UNAUTHORIZED,
            "a stranger's signature fails"
        );
        assert_eq!(
            first_reply.error, INVALID_SIGNATURE,
            "with the ticket wording"
        );
        assert_eq!(hits.load(Ordering::SeqCst), 1, "the retry refetched once");

        let throttled_reply = send(&app, &wrong).await;
        assert_eq!(throttled_reply.status, StatusCode::UNAUTHORIZED, "again");
        assert_eq!(throttled_reply.error, INVALID_SIGNATURE, "same answer");
        assert_eq!(
            hits.load(Ordering::SeqCst),
            1,
            "the window is closed: no second fetch"
        );
        server.abort();
    }

    /// A signature that authenticates alice cannot post as anyone else:
    /// `LD`-signature forwarding is not supported yet.
    #[tokio::test]
    async fn an_activity_by_another_actor_is_rejected() {
        let db = crate::test_db::test_db().await.expect("test database slot");
        let pool = db.pool().clone();
        let listener = bind().await;
        let base = origin(&listener);
        let ap_id = format!("{base}/users/alice");
        let peer = peer(base.clone(), FIXTURE_PUBLIC_PKCS1_PEM.to_owned());
        let server = spawn(listener, peer_app(peer));
        let app = inbox_app(test_state(pool.clone()));

        let mismatched = deliver(
            &format!("{ap_id}#main-key"),
            FIXTURE_PRIVATE_PKCS1_PEM,
            &activity(
                "https://elsewhere.example/users/mallory",
                "https://test.example/o",
            ),
        );
        let answer = send(&app, &mismatched).await;
        assert_eq!(answer.status, StatusCode::UNAUTHORIZED, "forwarding");
        assert_eq!(
            answer.error, ACTOR_MISMATCH,
            "is rejected with the containment wording"
        );

        // An object-actor spelling that does name alice passes containment.
        let embedded = json!({
            "@context": "https://www.w3.org/ns/activitystreams",
            "type": "Follow",
            "actor": { "id": ap_id },
            "object": "https://test.example/users/bob",
        })
        .to_string()
        .into_bytes();
        let contained_reply = send(
            &app,
            &deliver(
                &format!("{ap_id}#main-key"),
                FIXTURE_PRIVATE_PKCS1_PEM,
                &embedded,
            ),
        )
        .await;
        assert_eq!(
            contained_reply.status,
            StatusCode::OK,
            "the object actor spelling is contained: {}",
            contained_reply.error
        );
        server.abort();
    }

    /// A signer from a domain on the instance-policy reject list is
    /// refused — signature and containment valid, the 403 is the gate.
    #[tokio::test]
    async fn a_rejected_instance_is_refused() {
        let db = crate::test_db::test_db().await.expect("test database slot");
        let pool = db.pool().clone();
        let listener = bind().await;
        let base = origin(&listener);
        let ap_id = format!("{base}/users/alice");
        let peer = peer(base.clone(), FIXTURE_PUBLIC_PKCS1_PEM.to_owned());
        let server = spawn(listener, peer_app(peer));
        block_domain(&pool, "127.0.0.1").await;
        let app = inbox_app(test_state(pool.clone()));

        let body = activity(&ap_id, "https://test.example/users/bob");
        let answer = send(
            &app,
            &deliver(
                &format!("{ap_id}#main-key"),
                FIXTURE_PRIVATE_PKCS1_PEM,
                &body,
            ),
        )
        .await;
        assert_eq!(answer.status, StatusCode::FORBIDDEN, "the block answers");
        assert_eq!(answer.error, REJECTED, "with the ticket wording");
        server.abort();
    }

    /// A trailing-dot or Unicode/punycode spelling of a rejected host in the
    /// `keyId` is refused at the door, before any key fetch: no peer is
    /// listening, so a delivery that got past the gate would answer 401.
    #[tokio::test]
    async fn a_respelled_rejected_host_is_refused_without_a_fetch() {
        let db = crate::test_db::test_db().await.expect("test database slot");
        let pool = db.pool().clone();
        block_domain(&pool, "evil.example").await;
        set_mrf_lists(&pool, &json!({"reject": ["Bücher.Example."]})).await;
        let app = inbox_app(test_state(pool.clone()));

        for key_id in [
            "https://evil.example./users/alice#main-key",
            "https://EVIL.example./users/alice#main-key",
            "https://sub.evil.example./users/alice#main-key",
            "https://xn--bcher-kva.example/users/alice#main-key",
            "https://xn--bcher-kva.example./users/alice#main-key",
        ] {
            let actor = key_id.trim_end_matches("#main-key");
            let body = activity(actor, "https://test.example/users/bob");
            let answer = send(&app, &deliver(key_id, FIXTURE_PRIVATE_PKCS1_PEM, &body)).await;
            assert_eq!(
                answer.status,
                StatusCode::FORBIDDEN,
                "`{key_id}` is a rejected host: {}",
                answer.error
            );
            assert_eq!(answer.error, REJECTED, "with the ticket wording");
        }
    }

    /// The MRF domain lists are the same gate at the same step: a host the
    /// configured `simple` policy rejects is refused with the very 403, and
    /// a host its allowlist does not name is refused with it too — the
    /// extractor asks [`MrfConfig::rejected`](crate::federation::mrf::MrfConfig::rejected) beside the table, so an
    /// instance that federates with a chosen few turns everyone else away
    /// at the door.
    #[tokio::test]
    async fn a_domain_the_mrf_lists_reject_is_refused() {
        for lists in [
            json!({"reject": [LOOPBACK_HOST]}),
            json!({"accept": ["friend.example"]}),
        ] {
            let db = crate::test_db::test_db().await.expect("test database slot");
            let pool = db.pool().clone();
            set_mrf_lists(&pool, &lists).await;
            let listener = bind().await;
            let base = origin(&listener);
            let ap_id = format!("{base}/users/alice");
            let peer = peer(base.clone(), FIXTURE_PUBLIC_PKCS1_PEM.to_owned());
            let server = spawn(listener, peer_app(peer));
            let app = inbox_app(test_state(pool.clone()));

            let body = activity(&ap_id, "https://test.example/users/bob");
            let answer = send(
                &app,
                &deliver(
                    &format!("{ap_id}#main-key"),
                    FIXTURE_PRIVATE_PKCS1_PEM,
                    &body,
                ),
            )
            .await;
            assert_eq!(
                answer.status,
                StatusCode::FORBIDDEN,
                "the domain lists refuse the delivery: {lists}"
            );
            assert_eq!(answer.error, REJECTED, "the same wording the table gives");
            server.abort();
        }
    }

    /// The same delivery is admitted when the configured policy is not
    /// enabled: lists behind a switched-off policy govern nothing.
    #[tokio::test]
    async fn a_disabled_mrf_policy_leaves_the_door_open() {
        let db = crate::test_db::test_db().await.expect("test database slot");
        let pool = db.pool().clone();
        runtime_config::write(
            &pool,
            ":pleroma",
            ":mrf",
            &json!({"policies": ["noop"], "simple": {"reject": [LOOPBACK_HOST]}}),
        )
        .await
        .unwrap();
        let listener = bind().await;
        let base = origin(&listener);
        let ap_id = format!("{base}/users/alice");
        let peer = peer(base.clone(), FIXTURE_PUBLIC_PKCS1_PEM.to_owned());
        let server = spawn(listener, peer_app(peer));
        let app = inbox_app(test_state(pool.clone()));

        let body = activity(&ap_id, "https://test.example/users/bob");
        let answer = send(
            &app,
            &deliver(
                &format!("{ap_id}#main-key"),
                FIXTURE_PRIVATE_PKCS1_PEM,
                &body,
            ),
        )
        .await;
        assert_eq!(
            answer.status,
            StatusCode::OK,
            "the noop chain admits the delivery: {}",
            answer.error
        );
        server.abort();
    }
}
