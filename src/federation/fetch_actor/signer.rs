//! The signing identity behind actor `GET`s: [`Signer`] with its
//! instance-actor provisioning and redacting `Debug`, `fetch_signer`'s
//! `SIGN_FETCHES` check and `sign_get`'s one signed request.

use std::fmt;

use chrono::Utc;

use crate::{
    error::{AppError, AppResult},
    federation::{
        actor::MAIN_KEY_FRAGMENT,
        signature::{SignedHeaders, SigningKey, sign_request},
    },
    models::activitypub_actor,
    services::instance_actor::ensure_instance_actor,
    state::AppState,
};

use super::{FetchError, error::server_error};

/// The identity that signs an actor `GET`: the instance actor's key URI
/// and private key, passed into
/// [`fetch_actor_json`](super::fetch_actor_json) for authorized-fetch
/// peers while `SIGN_FETCHES` is on.
#[derive(Clone, Eq, PartialEq)]
pub struct Signer {
    /// The `keyId` Signature parameter: the instance actor's key id, e.g.
    /// `https://social.example/actor#main-key`.
    pub key_id: String,
    /// The signing key as PEM (see
    /// [`crate::util::keys::parse_private_key`]).
    pub private_pem: String,
}

/// The [`private_pem`](Self::private_pem) is secret key material and
/// renders as `<redacted>`, like every other holder of a private key
/// (see [`crate::util::keys::KeyPair`]).
impl fmt::Debug for Signer {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("Signer")
            .field("key_id", &self.key_id)
            .field("private_pem", &"<redacted>")
            .finish()
    }
}

impl Signer {
    /// The instance actor as this process's signing identity: key id
    /// `{ap_id}#main-key`, private key from its row — provisioning the
    /// actor on first use (see [`ensure_instance_actor`]).
    ///
    /// # Errors
    ///
    /// Whatever instance-actor provisioning reports (database or
    /// key-generation failures), plus
    /// [`AppError::Internal`] if the instance actor has no
    /// `activitypub_actors` row or its row carries no private key.
    pub async fn instance(state: &AppState) -> AppResult<Self> {
        let actor = ensure_instance_actor(&state.db, &state.config).await?;
        let private_pem = activitypub_actor::get(&state.db, actor.id)
            .await?
            .and_then(|row| row.private_key)
            .ok_or_else(|| {
                AppError::Internal("the instance actor carries no private key".to_owned())
            })?;
        Ok(Self {
            key_id: format!("{}{MAIN_KEY_FRAGMENT}", actor.ap_id),
            private_pem,
        })
    }
}

/// The identity to sign this fetch with: the instance actor while
/// `SIGN_FETCHES` is on (provisioned on first use), [`None`] when the
/// operator switched signed fetches off. Shared with the object fetcher
/// (see [`crate::federation::fetch_object`]), which signs the same way.
pub(crate) async fn fetch_signer(state: &AppState) -> Result<Option<Signer>, FetchError> {
    if !state.config.fetch.sign_fetches.get() {
        return Ok(None);
    }
    match Signer::instance(state).await {
        Ok(signer) => Ok(Some(signer)),
        Err(err) => Err(server_error(&err)),
    }
}

/// Signs the `GET` of `url`, folding a signing failure into
/// [`FetchError::Invalid`]: the request never left this process because it
/// could not be built.
pub(super) fn sign_get(url: &str, signer: &Signer) -> Result<SignedHeaders, FetchError> {
    let key = SigningKey {
        key_id: signer.key_id.as_str(),
        private_pem: signer.private_pem.as_str(),
    };
    sign_request("GET", url, None, &key, Utc::now())
        .map_err(|err| FetchError::Invalid(format!("could not sign the fetch of `{url}`: {err}")))
}

#[cfg(test)]
mod tests {
    use axum::{
        Router,
        http::{HeaderMap, header},
        routing::get,
    };
    use base64::{Engine as _, engine::general_purpose::STANDARD as BASE64};

    use super::*;
    use crate::{
        config::{Config, TEST_BASE_URL},
        federation::{
            fetch_actor::{ACTOR_ACCEPT, fetch_actor_json, get_or_fetch_by_ap_id, test_support::*},
            signature::signing_string,
        },
        models::user::{INSTANCE_ACTOR_NICKNAME, get_by_nickname},
        util::keys::{FIXTURE_PRIVATE_PKCS1_PEM, FIXTURE_PUBLIC_PKCS1_PEM, verify_sha256},
    };

    /// The key id the signed-fetch test presents.
    const TEST_KEY_ID: &str = "https://test.example/actor#main-key";

    /// The instance key id [`Signer::instance`] and the signed fetches must
    /// carry on the test instance.
    fn instance_key_id() -> String {
        format!("{TEST_BASE_URL}/internal/fetch#main-key")
    }

    /// The named parameter of a `Signature` header value, unquoted.
    fn signature_param(header: &str, name: &str) -> String {
        let prefix = format!("{name}=\"");
        let pair = header
            .split(',')
            .map(str::trim)
            .find(|pair| pair.starts_with(&prefix))
            .unwrap_or_else(|| panic!("signature header carries a {name} parameter: {header}"));
        let value = pair
            .strip_prefix(prefix.as_str())
            .unwrap_or_else(|| panic!("the {name} parameter is quoted: {header}"));
        value.trim_end_matches('"').to_owned()
    }

    /// The `Signature` header of the one request the recording peer saw,
    /// plus the captured headers beside it.
    fn captured_signature(peer: &Peer) -> (String, HeaderMap) {
        let captured = peer
            .captured
            .lock()
            .unwrap()
            .clone()
            .expect("the mock recorded the request");
        let signature = captured
            .get("signature")
            .and_then(|value| value.to_str().ok())
            .expect("a Signature header was sent")
            .to_owned();
        (signature, captured)
    }

    #[tokio::test]
    async fn a_signed_fetch_sends_a_verifiable_signature_header() {
        let listener = bind().await;
        let base = origin(&listener);
        let url = format!("{base}/users/alice");
        let peer = peer_at(&base);
        let observer = peer.clone();
        let server = spawn(
            listener,
            Router::new()
                .route("/users/alice", get(self_actor_recording))
                .with_state(peer),
        );

        let signer = Signer {
            key_id: TEST_KEY_ID.to_owned(),
            private_pem: FIXTURE_PRIVATE_PKCS1_PEM.to_owned(),
        };
        let document = fetch_actor_json(&client(), &url, Some(&signer))
            .await
            .unwrap();
        server.abort();

        assert_eq!(document["id"], url, "the document still arrives");
        let captured = observer
            .captured
            .lock()
            .unwrap()
            .clone()
            .expect("the mock recorded the request");
        assert_eq!(
            captured
                .get(header::ACCEPT)
                .and_then(|value| value.to_str().ok()),
            Some(ACTOR_ACCEPT),
            "a signed fetch keeps asking for actor JSON"
        );
        let signature = captured
            .get("signature")
            .and_then(|value| value.to_str().ok())
            .expect("a Signature header was sent");
        assert_eq!(
            signature_param(signature, "keyId"),
            TEST_KEY_ID,
            "the signature names our key id"
        );
        assert_eq!(
            signature_param(signature, "algorithm"),
            "rsa-sha256",
            "the algorithm Mastodon and Pleroma speak"
        );
        assert_eq!(
            signature_param(signature, "headers"),
            "(request-target) host date",
            "a bodyless GET signs the three lines"
        );
        let raw = BASE64
            .decode(signature_param(signature, "signature"))
            .unwrap();
        let signing_text = signing_string(&[
            ("(request-target)", "get /users/alice"),
            (
                "host",
                captured
                    .get("host")
                    .and_then(|value| value.to_str().ok())
                    .unwrap(),
            ),
            (
                "date",
                captured
                    .get("date")
                    .and_then(|value| value.to_str().ok())
                    .unwrap(),
            ),
        ]);
        assert!(
            verify_sha256(FIXTURE_PUBLIC_PKCS1_PEM, signing_text.as_bytes(), &raw),
            "the signature verifies over exactly what the peer saw"
        );
    }

    #[tokio::test]
    async fn get_or_fetch_by_ap_id_signs_with_the_instance_actor_key() {
        let db = crate::test_db::test_db().await.expect("test database slot");
        let pool = db.pool().clone();
        let listener = bind().await;
        let base = origin(&listener);
        let ap_id = format!("{base}/users/alice");
        let peer = peer_at(&base);
        let observer = peer.clone();
        let server = spawn(
            listener,
            Router::new()
                .route("/users/alice", get(self_actor_recording))
                .with_state(peer),
        );
        let state = test_state(pool);

        let _mirrored = get_or_fetch_by_ap_id(&state, &ap_id).await.unwrap();
        server.abort();

        let (signature, captured) = captured_signature(&observer);
        assert_eq!(
            signature_param(&signature, "keyId"),
            instance_key_id(),
            "the fetch signed as the instance actor"
        );
        let actor = get_by_nickname(&state.db, INSTANCE_ACTOR_NICKNAME)
            .await
            .unwrap()
            .expect("the signed fetch provisioned the instance actor");
        let raw = BASE64
            .decode(signature_param(&signature, "signature"))
            .unwrap();
        let signing_text = signing_string(&[
            ("(request-target)", "get /users/alice"),
            (
                "host",
                captured
                    .get("host")
                    .and_then(|value| value.to_str().ok())
                    .unwrap(),
            ),
            (
                "date",
                captured
                    .get("date")
                    .and_then(|value| value.to_str().ok())
                    .unwrap(),
            ),
        ]);
        assert!(
            verify_sha256(
                actor.public_key.as_deref().unwrap(),
                signing_text.as_bytes(),
                &raw
            ),
            "the peer verifies us with the key our document advertises"
        );
    }

    #[tokio::test]
    async fn fetches_go_unsigned_when_sign_fetches_is_off() {
        let db = crate::test_db::test_db().await.expect("test database slot");
        let pool = db.pool().clone();
        let listener = bind().await;
        let base = origin(&listener);
        let ap_id = format!("{base}/users/alice");
        let peer = peer_at(&base);
        let observer = peer.clone();
        let server = spawn(
            listener,
            Router::new()
                .route("/users/alice", get(self_actor_recording))
                .with_state(peer),
        );
        let mut config = Config::for_tests();
        config.fetch.sign_fetches = false.into();
        let state = test_state_with_config(pool, config);

        let _mirrored = get_or_fetch_by_ap_id(&state, &ap_id).await.unwrap();
        server.abort();

        let captured = observer
            .captured
            .lock()
            .unwrap()
            .clone()
            .expect("the mock recorded the request");
        assert!(
            captured.get("signature").is_none(),
            "no Signature header goes out while the toggle is off"
        );
        assert!(
            get_by_nickname(&state.db, INSTANCE_ACTOR_NICKNAME)
                .await
                .unwrap()
                .is_none(),
            "and an unsigned fetch never provisions the instance actor"
        );
    }

    #[tokio::test]
    async fn the_instance_signer_carries_the_stored_actor_key() {
        let db = crate::test_db::test_db().await.expect("test database slot");
        let state = test_state(db.pool().clone());

        let signer = Signer::instance(&state).await.unwrap();

        assert_eq!(
            signer.key_id,
            instance_key_id(),
            "the instance actor key id"
        );
        let actor = get_by_nickname(&state.db, INSTANCE_ACTOR_NICKNAME)
            .await
            .unwrap()
            .expect("asking for the signer provisions the actor");
        assert_eq!(
            signer.private_pem,
            actor.private_key.unwrap(),
            "the signer carries the stored private key"
        );
        let debug = format!("{signer:?}");
        assert!(
            !debug.contains("BEGIN") && debug.contains("<redacted>"),
            "Debug never prints the private key material: {debug}"
        );
    }

    #[tokio::test]
    async fn the_instance_signer_reads_its_key_from_the_actor_row() {
        let db = crate::test_db::test_db().await.expect("test database slot");
        let state = test_state(db.pool().clone());
        let instance = Signer::instance(&state).await.unwrap();
        let actor = get_by_nickname(&state.db, INSTANCE_ACTOR_NICKNAME)
            .await
            .unwrap()
            .expect("asking for the signer provisions the actor");

        let changed = sqlx::query(
            "UPDATE activitypub_actors SET private_key = 'only-in-actor-row' WHERE user_id = $1",
        )
        .bind(actor.id)
        .execute(&state.db)
        .await
        .unwrap()
        .rows_affected();
        assert_eq!(changed, 1, "the instance actor has one actor row");
        let signer = Signer::instance(&state).await.unwrap();
        assert_eq!(
            signer.private_pem, "only-in-actor-row",
            "the key comes from activitypub_actors, not users"
        );
        assert_ne!(signer.private_pem, instance.private_pem, "it did change");

        let removed = sqlx::query("DELETE FROM activitypub_actors WHERE user_id = $1")
            .bind(actor.id)
            .execute(&state.db)
            .await
            .unwrap()
            .rows_affected();
        assert_eq!(removed, 1, "the actor row goes");
        let error = Signer::instance(&state).await.unwrap_err();
        assert!(
            matches!(&error, AppError::Internal(message) if message.contains("no private key")),
            "no row is the same error as no key: {error:?}"
        );
    }
}
