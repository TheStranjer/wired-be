//! Authorized fetch with RFC 9421 `Signature-Input` / `Signature` headers,
//! alone and beside a draft-cavage signature.

use super::*;
use crate::federation::signature::verify::rfc9421_test_support::{Rfc9421Key, sign};

/// The absolute URL [`TARGET_PATH`] is signed as.
fn target_url() -> String {
    format!("{TEST_BASE_URL}{TARGET_PATH}")
}

/// The headers of a `GET` of [`target_url`] signed the RFC 9421 way as
/// `key_id` with `private_pem` at `created`.
fn rfc9421_get(key_id: &str, private_pem: &str, created: i64) -> SignedGet {
    let key = Rfc9421Key {
        key_id,
        private_pem,
        created,
    };
    let mut headers = HeaderMap::new();
    for (name, value) in sign("GET", &target_url(), None, &key) {
        let previous = headers.insert(
            HeaderName::from_static(name),
            value.parse().expect("test header value"),
        );
        drop(previous);
    }
    SignedGet { headers }
}

/// The instance actor's key id and private key: a signer the gate knows
/// without any fetch.
async fn instance_signer(state: &AppState) -> (User, String, String) {
    let actor = ensure_instance_actor(&state.db, &state.config)
        .await
        .unwrap();
    let pem = private_pem_of(state, &actor).await;
    let key_id = format!("{}#main-key", actor.ap_id);
    (actor, key_id, pem)
}

#[tokio::test]
async fn an_rfc9421_signed_fetch_proves_the_read() {
    let db = crate::test_db::test_db().await.expect("test database slot");
    let state = state_with(db.pool().clone(), gate_on());
    let (actor, key_id, pem) = instance_signer(&state).await;
    let get = rfc9421_get(&key_id, &pem, Utc::now().timestamp());

    check(&state, &get)
        .await
        .expect("an RFC 9421 signature proves the read");
    let signer = ask_signer(&state, &get)
        .await
        .unwrap()
        .expect("and names its signer");
    assert_eq!(signer.id, actor.id, "the row the keyid names");
}

#[tokio::test]
async fn a_tampered_stale_or_foreign_rfc9421_fetch_is_refused() {
    let db = crate::test_db::test_db().await.expect("test database slot");
    let state = state_with(db.pool().clone(), gate_on());
    let (_actor, key_id, pem) = instance_signer(&state).await;
    let get = rfc9421_get(&key_id, &pem, Utc::now().timestamp());

    assert!(
        refused(&check_target(&state, &get, "/users/bob").await),
        "another path than the one signed"
    );
    let aged = rfc9421_get(&key_id, &pem, Utc::now().timestamp() - 13 * 3600);
    assert!(refused(&check(&state, &aged).await), "13 hours old");
    let foreign = rfc9421_get(&key_id, &remote_keys().private_pem, Utc::now().timestamp());
    assert!(
        refused(&check(&state, &foreign).await),
        "the instance actor's keyid under another key"
    );
}

#[tokio::test]
async fn a_bad_rfc9421_signature_off_the_gate_is_refused_not_anonymous() {
    let db = crate::test_db::test_db().await.expect("test database slot");
    let state = state_with(db.pool().clone(), Config::for_tests());
    let (actor, key_id, pem) = instance_signer(&state).await;

    let good = rfc9421_get(&key_id, &pem, Utc::now().timestamp());
    let signer = ask_signer(&state, &good).await.unwrap();
    assert_eq!(
        signer.map(|user| user.id),
        Some(actor.id),
        "a proven RFC 9421 reader is named while the gate is off"
    );
    let bad = rfc9421_get(&key_id, &remote_keys().private_pem, Utc::now().timestamp());
    assert!(
        matches!(
            ask_signer(&state, &bad).await,
            Err(AppError::Unauthorized(_))
        ),
        "a signature that does not verify is no anonymous read"
    );
}

#[tokio::test]
async fn either_valid_signature_carries_an_invalid_one_of_the_other_scheme() {
    let db = crate::test_db::test_db().await.expect("test database slot");
    let state = state_with(db.pool().clone(), gate_on());
    let (actor, key_id, pem) = instance_signer(&state).await;
    let created = Utc::now().timestamp();

    // A valid cavage signature under `Authorization`, an invalid RFC 9421 pair.
    let mut valid_cavage = as_authorization(&sign_as(&key_id, &pem));
    let invalid = rfc9421_get(&key_id, &remote_keys().private_pem, created);
    merge_rfc9421(&mut valid_cavage, &invalid);
    let signer = ask_signer(&state, &valid_cavage)
        .await
        .unwrap()
        .expect("the valid cavage header proves the read");
    assert_eq!(signer.id, actor.id, "valid cavage, invalid RFC 9421");

    // The reverse: an invalid cavage signature, a valid RFC 9421 pair.
    let mut invalid_cavage = as_authorization(&sign_as(&key_id, &remote_keys().private_pem));
    merge_rfc9421(&mut invalid_cavage, &rfc9421_get(&key_id, &pem, created));
    let rfc_signer = ask_signer(&state, &invalid_cavage)
        .await
        .unwrap()
        .expect("the valid RFC 9421 pair proves the read");
    assert_eq!(rfc_signer.id, actor.id, "invalid cavage, valid RFC 9421");
}

/// Copies the RFC 9421 headers of `source` onto `target`, which keeps its
/// own `Host` and its `Authorization` cavage signature.
fn merge_rfc9421(target: &mut SignedGet, source: &SignedGet) {
    for name in ["signature-input", "signature"] {
        let value = source
            .headers
            .get(name)
            .expect("an RFC 9421 header")
            .clone();
        let previous = target.headers.insert(HeaderName::from_static(name), value);
        drop(previous);
    }
}
