//! Authorized fetch: the inbound half of HTTP-signed `ActivityPub` reads.
//!
//! When `AUTHORIZED_FETCH` is on, this instance accepts `GET`s of actor
//! (and, when they arrive, object and collection) documents only from
//! requesters that prove the read with a valid HTTP Signature — Pleroma's
//! `:authorized_fetch` mode. [`fetch_allowed`] is the gate every such route
//! calls before it answers: while the flag is off it is the no-op every
//! federation client expects; when on it parses the request's `Signature`
//! header, finds the signing actor's stored public key (dereferencing an
//! unknown `keyId`'s actor through [`get_or_fetch_by_ap_id`]), and verifies
//! the signature through [`crate::federation::signature`].
//!
//! The instance actor document at `/internal/fetch` is deliberately not
//! gated: a peer cannot sign with a key it has not fetched yet, so the
//! signer's own document has to stay readable without one.

use axum::http::{HeaderMap, HeaderName, Method, Uri};
use chrono::Utc;

use crate::{
    error::{AppError, AppResult},
    federation::{
        fetch_actor::get_or_fetch_by_ap_id,
        signature::verify::{ParsedSignature, SignedRequestParts, parse_signature_header, verify},
    },
    models::user::{User, get_by_ap_id},
    state::AppState,
};

/// The refusal every unsatisfied check answers with: the requester is
/// unknown or unproven, which is an authentication failure (401).
const REFUSAL: &str = "This instance requires authenticated fetches";

/// Whether this request may be answered.
///
/// While `AUTHORIZED_FETCH` is off — the default — every fetch is allowed
/// and no header is read. When on, `method`, `uri` and `headers` (the
/// values as received) must carry a `Signature` header that verifies
/// against the public key stored for — or fetched for — the actor its
/// `keyId` names.
///
/// # Errors
///
/// [`AppError::Unauthorized`] (401) when the flag is on and the request
/// carries no parseable signature, the signature does not verify, or the
/// signing actor's key cannot be established; database failures arrive as
/// the usual [`AppError::Database`](crate::error::AppError).
pub async fn fetch_allowed(
    state: &AppState,
    method: &Method,
    uri: &Uri,
    headers: &HeaderMap,
) -> AppResult<()> {
    if !state.config.authorized_fetch.get() {
        return Ok(());
    }
    let Some(signature) = received_signature(headers) else {
        return Err(AppError::Unauthorized(REFUSAL.to_owned()));
    };
    let public_pem = signing_key_of(state, actor_of_key_id(&signature.key_id)).await?;
    let target = request_target(uri);
    let request = SignedRequestParts {
        method: method.as_str(),
        path_and_query: &target,
        headers: headers.clone(),
    };
    verify(&signature, &request, &public_pem, Utc::now()).map_err(|err| {
        // The requester proved nothing this instance accepts: log why
        // internally, answer the one refusal externally.
        tracing::debug!(%err, "the fetch signature did not verify");
        AppError::Unauthorized(REFUSAL.to_owned())
    })
}

/// The parsed `Signature` header of a received request, `None` when the
/// header is absent, unreadable or malformed.
fn received_signature(headers: &HeaderMap) -> Option<ParsedSignature> {
    headers
        .get(HeaderName::from_static("signature"))
        .and_then(|value| value.to_str().ok())
        .and_then(|value| parse_signature_header(value).ok())
}

/// The actor a `keyId` names: the key URI up to its `#fragment`, e.g.
/// `https://social.example/users/alice` for
/// `https://social.example/users/alice#main-key`.
fn actor_of_key_id(key_id: &str) -> &str {
    key_id.split('#').next().unwrap_or(key_id)
}

/// The `(request-target)` value for the signed text: the received path
/// with its query, exactly as the signer spells it.
fn request_target(uri: &Uri) -> String {
    uri.query().map_or_else(
        || uri.path().to_owned(),
        |query| format!("{}?{query}", uri.path()),
    )
}

/// The public key that must verify the signature of an actor: the stored
/// one when this instance knows the actor, and after one network
/// dereference when it does not (the unknown-`keyId` rule).
async fn signing_key_of(state: &AppState, actor_id: &str) -> AppResult<String> {
    if let Some(stored) = get_by_ap_id(&state.db, actor_id).await? {
        return key_material(&stored);
    }
    let fetched = get_or_fetch_by_ap_id(state, actor_id)
        .await
        .map_err(|err| {
            // The signer is only as trustworthy as its own document: an
            // actor that cannot be dereferenced authenticates nobody.
            tracing::debug!(%err, "could not dereference the signing actor");
            AppError::Unauthorized(REFUSAL.to_owned())
        })?;
    key_material(&fetched)
}

/// The PEM a user's signatures must verify against, or the refusal when
/// the row carries no key material at all.
fn key_material(user: &User) -> AppResult<String> {
    user.public_key.clone().ok_or_else(|| {
        tracing::debug!(ap_id = %user.ap_id, "the signing actor carries no public key");
        AppError::Unauthorized(REFUSAL.to_owned())
    })
}

#[cfg(test)]
mod tests {
    use std::sync::{Arc, OnceLock};

    use axum::{
        Router,
        extract::State,
        http::header,
        response::{IntoResponse as _, Response},
        routing::get,
    };
    use chrono::Utc;
    use serde_json::json;
    use sqlx::PgPool;
    use tokio::net::TcpListener;

    use super::*;
    use crate::{
        config::{Config, TEST_BASE_URL},
        federation::signature::{SignedHeaders, SigningKey, sign_request},
        http_client::HttpClient,
        services::instance_actor::{ensure_instance_actor, instance_actor_ap_id},
        uploads::UploaderBackend,
        util::keys::{KeyPair, generate_keypair},
    };

    /// The signed path under gate tests.
    const TARGET_PATH: &str = "/users/alice";
    /// `User-Agent` for the test client.
    const TEST_UA: &str = "Wired (https://test.example)";

    /// The state under test: `pool` on top of `config`, with the client's
    /// SSRF guard off so the unknown-`keyId` test can reach its mock.
    fn state_with(pool: PgPool, config: Config) -> AppState {
        let config = Arc::new(config);
        let hub = crate::streaming::hub::Hub::new(&pool);
        AppState {
            db: pool,
            hub,
            config: Arc::clone(&config),
            http_client: HttpClient::new_for_tests(TEST_UA),
            uploader: UploaderBackend::from(config.as_ref()),
            instance_stats_cache: Arc::default(),
            mrf_config_cache: Arc::default(),
            translation_languages_cache: Arc::default(),
            rate_limiter: Arc::new(crate::rate_limit::RateLimiter::from_config(config.as_ref())),
            mailer: Arc::new(crate::mailer::MemoryTransport::new()),
        }
    }

    /// [`Config::for_tests`] with authorized fetch switched on.
    fn gate_on() -> Config {
        let mut config = Config::for_tests();
        config.authorized_fetch = true.into();
        config
    }

    /// The signed `GET` of `https://test.example/users/alice` that
    /// [`fetch_allowed`] reads.
    #[derive(Debug)]
    struct SignedGet {
        /// The received headers: host, date, signature.
        headers: HeaderMap,
    }

    /// The received-request headers for `signed`: the host and date it
    /// signed, plus the `Signature` header itself.
    fn received_headers(signed: &SignedHeaders) -> HeaderMap {
        let mut headers = HeaderMap::new();
        for (name, value) in [
            ("host", &signed.host),
            ("date", &signed.date),
            ("signature", &signed.signature),
        ] {
            let previous = headers.insert(
                HeaderName::from_bytes(name.as_bytes()).expect("test header name"),
                value.parse().expect("test header value"),
            );
            drop(previous);
        }
        headers
    }

    /// Signs `GET https://test.example/users/alice` under `key_id` with
    /// `private_pem`.
    fn sign_as(key_id: &str, private_pem: &str) -> SignedGet {
        let signed = sign_request(
            "GET",
            &format!("{TEST_BASE_URL}{TARGET_PATH}"),
            None,
            &SigningKey {
                key_id,
                private_pem,
            },
            Utc::now(),
        )
        .expect("signing the test GET");
        SignedGet {
            headers: received_headers(&signed),
        }
    }

    /// The same `GET` with no `Signature` header at all.
    fn unsigned_get() -> SignedGet {
        SignedGet {
            headers: HeaderMap::new(),
        }
    }

    /// The test instance's absolute URI of `path`, for the `GET`s the
    /// gate is asked about.
    fn uri_of(path: &str) -> Uri {
        Uri::try_from(format!("{TEST_BASE_URL}{path}")).expect("test URI is valid")
    }

    /// Runs [`fetch_allowed`] against `get`, optionally asking for a
    /// different path than the one that was signed.
    async fn check(state: &AppState, get: &SignedGet) -> AppResult<()> {
        check_target(state, get, TARGET_PATH).await
    }

    /// [`check`] with the requested URI path set to `path` — the tampered
    /// test signs one path and asks for another through here.
    async fn check_target(state: &AppState, get: &SignedGet, path: &str) -> AppResult<()> {
        let uri = uri_of(path);
        fetch_allowed(state, &Method::GET, &uri, &get.headers).await
    }

    /// Whether `outcome` is the 401 refusal.
    fn refused(outcome: &AppResult<()>) -> bool {
        matches!(outcome, Err(AppError::Unauthorized(_)))
    }

    /// Serves `app` on `listener` until the test aborts the task.
    fn spawn(listener: TcpListener, app: Router) -> tokio::task::JoinHandle<()> {
        tokio::spawn(async move {
            if let Err(err) = axum::serve(listener, app).await {
                tracing::debug!("test server stopped: {err}");
            }
        })
    }

    /// The remote signer's key pair, made once for the process (RSA-2048
    /// generation is slow in debug builds).
    fn remote_keys() -> &'static KeyPair {
        static KEYS: OnceLock<KeyPair> = OnceLock::new();
        KEYS.get_or_init(|| generate_keypair().expect("test key generation"))
    }

    #[tokio::test]
    async fn fetches_are_allowed_while_the_flag_is_off() {
        // A pool that never connects proves the gate reads nothing: not a
        // header, not the database.
        let db = sqlx::postgres::PgPoolOptions::new()
            .connect_lazy("postgres://localhost/unused")
            .unwrap();
        let state = state_with(db, Config::for_tests());

        let outcome = check(&state, &unsigned_get()).await;

        assert!(outcome.is_ok(), "an unsigned fetch passes while off");
    }

    #[tokio::test]
    async fn the_unsigned_and_the_tampered_fetch_are_refused() {
        let db = crate::test_db::test_db().await.expect("test database slot");
        let state = state_with(db.pool().clone(), gate_on());

        let missing = check(&state, &unsigned_get()).await.unwrap_err();
        assert!(
            matches!(missing, AppError::Unauthorized(_)),
            "no Signature header cannot be verified"
        );
        assert_eq!(missing.to_string(), REFUSAL, "the refusal is the message");

        let actor = ensure_instance_actor(&state.db, &state.config)
            .await
            .unwrap();
        let key_id = format!("{}#main-key", actor.ap_id);
        let signed = sign_as(&key_id, actor.private_key.as_deref().unwrap());
        let tampered = check_target(&state, &signed, "/users/mallory").await;
        assert!(
            refused(&tampered),
            "a signature over a different path does not unlock this one"
        );
    }

    #[tokio::test]
    async fn a_stored_local_actor_unlocks_the_gate() {
        let db = crate::test_db::test_db().await.expect("test database slot");
        let state = state_with(db.pool().clone(), gate_on());
        let actor = ensure_instance_actor(&state.db, &state.config)
            .await
            .unwrap();
        let get = sign_as(
            &format!("{}#main-key", actor.ap_id),
            actor.private_key.as_deref().unwrap(),
        );

        check(&state, &get).await.expect("the instance actor signs");

        assert!(
            check(&state, &unsigned_get()).await.is_err(),
            "and without its signature the gate stays shut"
        );
    }

    /// Answers with the self-identifying actor document of `base`, carrying
    /// the remote signer's public key.
    async fn remote_actor_document(State(base): State<String>) -> Response {
        (
            [(header::CONTENT_TYPE, "application/activity+json")],
            json!({
                "@context": "https://www.w3.org/ns/activitystreams",
                "type": "Person",
                "id": format!("{base}{TARGET_PATH}"),
                "preferredUsername": "signer",
                "name": "Signer",
                "inbox": format!("{base}{TARGET_PATH}/inbox"),
                "publicKey": {
                    "id": format!("{base}{TARGET_PATH}#main-key"),
                    "owner": format!("{base}{TARGET_PATH}"),
                    "publicKeyPem": remote_keys().public_pem,
                },
            })
            .to_string(),
        )
            .into_response()
    }

    #[tokio::test]
    async fn an_unknown_key_id_is_dereferenced_and_stored() {
        let db = crate::test_db::test_db().await.expect("test database slot");
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let base = format!("http://{}", listener.local_addr().unwrap());
        let server = spawn(
            listener,
            Router::new()
                .route(TARGET_PATH, get(remote_actor_document))
                .with_state(base.clone()),
        );
        let state = state_with(db.pool().clone(), gate_on());
        let key_id = format!("{base}{TARGET_PATH}#main-key");
        let get = sign_as(&key_id, &remote_keys().private_pem);

        check(&state, &get)
            .await
            .expect("the fetched actor's key verifies its signature");

        let mirror = get_by_ap_id(&state.db, &format!("{base}{TARGET_PATH}"))
            .await
            .unwrap()
            .expect("the signing actor is now a stored mirror");
        assert_eq!(mirror.nickname, "signer@127.0.0.1", "mirrored as any actor");
        server.abort();
    }

    #[tokio::test]
    async fn an_actor_id_on_this_origin_that_does_not_exist_is_refused() {
        let db = crate::test_db::test_db().await.expect("test database slot");
        let state = state_with(db.pool().clone(), gate_on());
        let key_id = format!("{TEST_BASE_URL}/users/nobody#main-key");
        let get = sign_as(&key_id, &remote_keys().private_pem);

        let outcome = check(&state, &get).await;

        assert!(
            refused(&outcome),
            "a keyId pointing at this instance's own missing actor unlocks nothing"
        );
    }

    #[test]
    fn the_key_owner_is_the_key_id_without_its_fragment() {
        assert_eq!(
            actor_of_key_id("https://social.example/users/alice#main-key"),
            "https://social.example/users/alice",
            "the fragment names the key, not the actor"
        );
        assert_eq!(
            actor_of_key_id("https://social.example/users/alice"),
            "https://social.example/users/alice",
            "a fragmentless id is its own owner"
        );
    }

    #[test]
    fn the_request_target_keeps_the_query() {
        let plain = Uri::from_static("https://test.example/users/alice");
        assert_eq!(request_target(&plain), "/users/alice", "path only");
        let queried = Uri::from_static("https://test.example/followers?page=2");
        assert_eq!(
            request_target(&queried),
            "/followers?page=2",
            "the query rides along, as the signer spells it"
        );
    }

    #[test]
    fn the_gate_asks_for_the_internal_fetch_document() {
        // `/internal/fetch` is the document a peer must read to learn the
        // key it signs with, so its route is deliberately not gated by
        // `fetch_allowed` — see `src/api/activitypub.rs`.
        assert_eq!(
            instance_actor_ap_id(TEST_BASE_URL),
            format!(
                "{TEST_BASE_URL}{}",
                crate::services::instance_actor::INSTANCE_ACTOR_PATH
            ),
            "the instance actor lives at the ungated internal fetch URI"
        );
    }
}
