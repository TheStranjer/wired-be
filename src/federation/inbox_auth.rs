//! Inbox signature authentication: the [`SignedActivity`] extractor every
//! inbound `ActivityPub` `POST` builds on.
//!
//! The reference is Pleroma's `HTTPSignaturePlug`
//! (`lib/pleroma/web/plugs/http_signature_plug.ex`) feeding
//! `Pleroma.Signature` (`lib/pleroma/signature.ex`) and the inbox action of
//! `ActivityPubController`
//! (`lib/pleroma/web/activity_pub/activity_pub_controller.ex`): a peer's
//! delivery is only acted on once its `draft-cavage` `Signature` header
//! authenticates the actor the activity claims. Here the whole gate is one
//! body-consuming [`FromRequest`] extractor rather than a plug pipeline.
//!
//! The checks, in the order the ticket fixes them:
//!
//! 1. the body is read under the hard `INBOX_MAX_BYTES` cap (413), must be
//!    labelled a JSON `ActivityPub` media type (415) and must parse (400);
//! 2. a `Signature` header — or the same parameters under
//!    `Authorization: Signature ...` — must carry parseable parameters
//!    (401 `Missing signature`);
//! 3. a signer whose host the instance refuses — a row of the
//!    instance-policy `reject` list
//!    ([`crate::models::instance_policy_domain`]) or a host the
//!    [`mrf`] configuration's domain lists reject
//!    ([`MrfConfig::rejected`](crate::federation::mrf::MrfConfig::rejected))
//!    — is refused (403 `Rejected`). The host is the `keyId`'s, known
//!    before anything is fetched, so a rejected peer costs no outbound
//!    request;
//! 4. the `Digest` header must describe the bytes actually received
//!    (401 `Invalid digest`, [`verify_digest`] and its body rule);
//! 5. the `keyId` minus its fragment is the signer's actor URL (Mastodon's
//!    `#main-key`): the stored mirror answers, else the actor is fetched
//!    (401 `Unknown key` when neither works); a `keyId` naming a standalone
//!    key object is resolved by fetching that document and following its
//!    `owner`;
//! 6. the signature verifies against the stored `public_key` (the
//!    [`verify`] rules, `Date` skew included); on failure the actor is
//!    refetched at most once per 60 s and the check retried, then 401
//!    `Invalid signature`;
//! 7. the activity's `actor` must equal the signer's `ap_id` — forwarded
//!    activities authenticated by an `LD` signature are not supported and
//!    are rejected the same way (401 `Signature actor does not match
//!    activity actor`). A `Delete` of an actor that answers `410 Gone` and
//!    is about itself is answered `202 Accepted` and dropped.
//!
//! Every failure is an [`AppError`], rendered by [`InboxAuthError`]; the
//! request body is never logged.
//!
//! [`AppError`]: crate::error::AppError
//! [`mrf`]: crate::federation::mrf
//! [`verify`]: crate::federation::signature::verify::verify

use std::time::Duration;

use axum::{
    extract::{FromRequest, Request},
    http::HeaderMap,
};
use bytes::Bytes;
use serde_json::Value;

use crate::{
    federation::signature::verify::{
        ParsedSignature, SignedRequestParts, path_and_query, verify_digest,
    },
    models::user::User,
    state::AppState,
};

mod body;
mod error;
pub mod signer;
#[cfg(test)]
mod test_support;
pub mod verify;

pub use crate::federation::policy::signer_host;
pub use error::InboxAuthError;

use body::{declares_json_type, parse_json_body, present_signature, read_limited};
use signer::resolve_signer;
use verify::{require_containment, require_unblocked, verify_signature};

/// 413 body when the inbox body passes the `INBOX_MAX_BYTES` cap.
const PAYLOAD_TOO_LARGE: &str = "Payload too large";
/// 415 body when the `Content-Type` names no JSON `ActivityPub` type (the
/// same wording [`crate::extract`] uses).
const UNSUPPORTED_TYPE: &str = "Unsupported media type";
/// 400 body when the capped body is not JSON at all.
const INVALID_JSON: &str = "Invalid JSON";
/// 401 body when no `Signature` header (or `Authorization` equivalent) is
/// presented, or its parameters cannot be parsed.
const MISSING_SIGNATURE: &str = "Missing signature";
/// 401 body when the `Digest` header does not describe the received body.
const INVALID_DIGEST: &str = "Invalid digest";
/// 401 body when the `keyId` resolves to no actor whose key can be read.
const UNKNOWN_KEY: &str = "Unknown key";
/// 401 body when the cryptographic check fails against the stored and the
/// refetched key alike.
const INVALID_SIGNATURE: &str = "Invalid signature";
/// 401 body when the activity's `actor` is not the authenticated signer:
/// `LD`-signature forwarding is not supported yet.
const ACTOR_MISMATCH: &str = "Signature actor does not match activity actor";
/// 403 body when the signer's host is on a reject list — the instance's
/// own table or the [`mrf`](crate::federation::mrf) domain lists.
const REJECTED: &str = "Rejected";
/// The `ActivityStreams` type of the one activity accepted while unsigned:
/// an actor's own deletion (see the [module docs](self) step 7).
const DELETE_TYPE: &str = "Delete";
/// The media types an inbox delivery may be labelled with, lowercase and
/// without parameters: the `Content-Type` must name exactly one.
const JSON_MEDIA_TYPES: [&str; 3] = [
    "application/activity+json",
    "application/ld+json",
    "application/json",
];
/// How often one actor's key may be refetched after a failed verification:
/// Pleroma's `refetch_public_key` retry, rate limited to once per minute.
const KEY_REFETCH_WINDOW: Duration = Duration::from_secs(60);

/// An inbound `ActivityPub` activity whose HTTP signature authenticated
/// against its actor: the body, its parsed JSON, and the signer's `users`
/// mirror.
///
/// Extracted with [`FromRequest`], so it consumes the request body and must
/// be the handler's last extractor. Every rejection step and its status is
/// the [module docs'][module-auth]'s; the extractor answers them itself, so
/// a handler written as `async fn inbox(activity: SignedActivity)` only ever
/// sees authenticated activities.
///
/// [module-auth]: self
#[derive(Debug)]
pub struct SignedActivity {
    /// The exact bytes received, size-capped and digest-checked.
    pub body: Bytes,
    /// [`body`](Self::body) parsed as JSON, the activity document itself.
    pub json: Value,
    /// The `users` row of the actor whose key signed the request: a local
    /// row, or a mirror this request fetched or refreshed.
    pub signer: User,
}

/// The parts of the received request the signature steps read, grouped to
/// keep every helper under the five-parameter budget (decision 14).
#[derive(Debug)]
struct Inbound<'a> {
    /// The parsed `Signature` header parameters.
    pub(super) parsed: &'a ParsedSignature,
    /// The request headers as received.
    pub(super) headers: &'a HeaderMap,
    /// The method, as received (e.g. `POST`).
    pub(super) method: &'a str,
    /// The request target: path with its query string.
    pub(super) target: &'a str,
    /// The parsed activity body.
    pub(super) json: &'a Value,
}

impl Inbound<'_> {
    /// The [`SignedRequestParts`] [`verify`] rebuilds the signing text
    /// from: the headers cloned, since the parts own their map.
    ///
    /// [`verify`]: crate::federation::signature::verify::verify
    pub(super) fn signed_request(&self) -> SignedRequestParts<'_> {
        SignedRequestParts {
            method: self.method,
            path_and_query: self.target,
            headers: self.headers.clone(),
        }
    }
}

impl FromRequest<AppState> for SignedActivity {
    type Rejection = InboxAuthError;

    async fn from_request(req: Request, state: &AppState) -> Result<Self, Self::Rejection> {
        authenticate(req, state).await
    }
}

/// The whole gate: the numbered checks of the [module docs](self), in
/// order, ending in the authenticated [`SignedActivity`].
async fn authenticate(
    request: Request,
    state: &AppState,
) -> Result<SignedActivity, InboxAuthError> {
    let (parts, stream) = request.into_parts();
    let body = read_limited(stream, state.config.inbox_max_bytes).await?;
    if !declares_json_type(&parts.headers) {
        return Err(InboxAuthError::wrong_type());
    }
    let json = parse_json_body(&body)?;
    let parsed = present_signature(&parts.headers)?;
    require_unblocked(state, &parsed.key_id).await?;
    verify_digest(&parts.headers, &body).map_err(|_err| InboxAuthError::invalid_digest())?;
    let signer = resolve_signer(state, &parsed.key_id, &json).await?;
    let target = path_and_query(&parts.uri);
    let inbound = Inbound {
        parsed: &parsed,
        headers: &parts.headers,
        method: parts.method.as_str(),
        target: &target,
        json: &json,
    };
    let signer = verify_signature(state, &inbound, signer).await?;
    require_containment(&json, &signer)?;
    Ok(SignedActivity { body, json, signer })
}

#[cfg(test)]
mod tests {
    use std::sync::{Arc, atomic::Ordering};

    use axum::http::StatusCode;

    use super::*;
    use crate::{
        config::Config,
        federation::inbox_auth::test_support::{
            BODY, activity, bind, block_domain, deliver, drop_header, header_value, inbox_app,
            origin, peer, peer_app, replace_header, send, spawn, test_state,
            test_state_with_config,
        },
        util::keys::{FIXTURE_PRIVATE_PKCS1_PEM, FIXTURE_PUBLIC_PKCS1_PEM},
    };

    /// An unknown actor, fetched on the fly, authenticates its delivery —
    /// and the mirror answers the next one without a second fetch.
    #[tokio::test]
    async fn a_valid_signature_authenticates_the_signer() {
        let db = crate::test_db::test_db().await.expect("test database slot");
        let pool = db.pool().clone();
        let listener = bind().await;
        let base = origin(&listener);
        let ap_id = format!("{base}/users/alice");
        let peer = peer(base.clone(), FIXTURE_PUBLIC_PKCS1_PEM.to_owned());
        let hits = Arc::clone(&peer.hits);
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
        assert_eq!(answer.status, StatusCode::OK, "the valid delivery");
        assert_eq!(
            answer.json.get("signer").and_then(Value::as_str),
            Some(format!("alice@{}", base.strip_prefix("http://").expect("http base")).as_str()),
            "the on-the-fly mirror is the signer, its id's host and port"
        );
        assert_eq!(
            answer.json.get("bytes").and_then(Value::as_u64),
            Some(u64::try_from(body.len()).unwrap()),
            "the handler received the exact body"
        );
        assert_eq!(hits.load(Ordering::SeqCst), 1, "one fetch resolved the key");

        let mirror_reply = send(
            &app,
            &deliver(
                &format!("{ap_id}#main-key"),
                FIXTURE_PRIVATE_PKCS1_PEM,
                &body,
            ),
        )
        .await;
        assert_eq!(mirror_reply.status, StatusCode::OK, "the second delivery");
        assert_eq!(hits.load(Ordering::SeqCst), 1, "answered from the mirror");
        server.abort();
    }

    /// A `Signature` header moved into `Authorization: Signature …`
    /// authenticates exactly the same.
    #[tokio::test]
    async fn an_authorization_signature_header_authenticates() {
        let db = crate::test_db::test_db().await.expect("test database slot");
        let pool = db.pool().clone();
        let listener = bind().await;
        let base = origin(&listener);
        let ap_id = format!("{base}/users/alice");
        let peer = peer(base.clone(), FIXTURE_PUBLIC_PKCS1_PEM.to_owned());
        let server = spawn(listener, peer_app(peer));
        let app = inbox_app(test_state(pool.clone()));

        let body = activity(&ap_id, "https://test.example/users/bob");
        let mut moved = deliver(
            &format!("{ap_id}#main-key"),
            FIXTURE_PRIVATE_PKCS1_PEM,
            &body,
        );
        let signature = header_value(&moved, "signature");
        drop_header(&mut moved, "signature");
        moved
            .headers
            .push(("authorization", format!("Signature {signature}")));
        let answer = send(&app, &moved).await;
        assert_eq!(
            answer.status,
            StatusCode::OK,
            "the Authorization spelling verifies: {}",
            answer.error
        );
        server.abort();
    }

    /// A body that changed after signing fails the digest check before
    /// any key is fetched, and so does a `Digest` header that simply lies.
    #[tokio::test]
    async fn a_tampered_body_or_digest_is_rejected() {
        let db = crate::test_db::test_db().await.expect("test database slot");
        let pool = db.pool().clone();
        let listener = bind().await;
        let base = origin(&listener);
        let ap_id = format!("{base}/users/alice");
        let peer = peer(base.clone(), FIXTURE_PUBLIC_PKCS1_PEM.to_owned());
        let hits = Arc::clone(&peer.hits);
        let server = spawn(listener, peer_app(peer));
        let app = inbox_app(test_state(pool.clone()));
        let key_id = format!("{ap_id}#main-key");

        let body = activity(&ap_id, "https://test.example/users/bob");
        let mut tampered = deliver(&key_id, FIXTURE_PRIVATE_PKCS1_PEM, &body);
        tampered.body = activity(&ap_id, "https://evil.example/steal");
        let tamper_reply = send(&app, &tampered).await;
        assert_eq!(
            tamper_reply.status,
            StatusCode::UNAUTHORIZED,
            "the swapped body"
        );
        assert_eq!(tamper_reply.error, INVALID_DIGEST, "fails the digest check");

        let mut lying = deliver(&key_id, FIXTURE_PRIVATE_PKCS1_PEM, &body);
        replace_header(
            &mut lying,
            "digest",
            "SHA-256=AAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAA=".to_owned(),
        );
        let lie_reply = send(&app, &lying).await;
        assert_eq!(
            lie_reply.status,
            StatusCode::UNAUTHORIZED,
            "the lying digest"
        );
        assert_eq!(lie_reply.error, INVALID_DIGEST, "fails the same check");
        assert_eq!(
            hits.load(Ordering::SeqCst),
            0,
            "neither delivery reached the network"
        );
        server.abort();
    }

    /// A rejected domain is refused before its key is fetched: the peer
    /// sees no request, for a valid delivery and for a garbage one alike.
    #[tokio::test]
    async fn a_rejected_domain_is_refused_without_fetching_its_key() {
        let db = crate::test_db::test_db().await.expect("test database slot");
        let pool = db.pool().clone();
        let listener = bind().await;
        let base = origin(&listener);
        let ap_id = format!("{base}/users/alice");
        let peer = peer(base.clone(), FIXTURE_PUBLIC_PKCS1_PEM.to_owned());
        let hits = Arc::clone(&peer.hits);
        let server = spawn(listener, peer_app(peer));
        block_domain(&pool, "127.0.0.1").await;
        let app = inbox_app(test_state(pool.clone()));
        let key_id = format!("{ap_id}#main-key");

        let body = activity(&ap_id, "https://test.example/users/bob");
        let valid = send(&app, &deliver(&key_id, FIXTURE_PRIVATE_PKCS1_PEM, &body)).await;
        assert_eq!(valid.status, StatusCode::FORBIDDEN, "the block answers");
        assert_eq!(valid.error, REJECTED, "with the ticket wording");

        let mut lying = deliver(&key_id, FIXTURE_PRIVATE_PKCS1_PEM, &body);
        replace_header(
            &mut lying,
            "digest",
            "SHA-256=AAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAA=".to_owned(),
        );
        let garbage = send(&app, &lying).await;
        assert_eq!(garbage.status, StatusCode::FORBIDDEN, "garbage is refused");
        assert_eq!(garbage.error, REJECTED, "by the same gate");

        assert_eq!(
            hits.load(Ordering::SeqCst),
            0,
            "the rejected peer was never fetched"
        );
        server.abort();
    }

    /// Every request-header failure answers before any fetch: unsigned,
    /// mislabelled and unparsable bodies all get their ticket status.
    #[tokio::test]
    async fn unsigned_mislabelled_and_unparsable_requests_are_rejected() {
        let db = crate::test_db::test_db().await.expect("test database slot");
        let pool = db.pool().clone();
        let listener = bind().await;
        let base = origin(&listener);
        let ap_id = format!("{base}/users/alice");
        let peer = peer(base.clone(), FIXTURE_PUBLIC_PKCS1_PEM.to_owned());
        let server = spawn(listener, peer_app(peer));
        let app = inbox_app(test_state(pool.clone()));
        let key_id = format!("{ap_id}#main-key");
        let body = activity(&ap_id, "https://test.example/users/bob");

        let mut unsigned = deliver(&key_id, FIXTURE_PRIVATE_PKCS1_PEM, &body);
        drop_header(&mut unsigned, "signature");
        let unsigned_reply = send(&app, &unsigned).await;
        assert_eq!(
            unsigned_reply.status,
            StatusCode::UNAUTHORIZED,
            "no signature"
        );
        assert_eq!(
            unsigned_reply.error, MISSING_SIGNATURE,
            "says what is missing"
        );

        let mut bearer = deliver(&key_id, FIXTURE_PRIVATE_PKCS1_PEM, &body);
        drop_header(&mut bearer, "signature");
        bearer
            .headers
            .push(("authorization", "Bearer tok3n".to_owned()));
        let bearer_reply = send(&app, &bearer).await;
        assert_eq!(
            bearer_reply.error, MISSING_SIGNATURE,
            "a bearer is no signature"
        );

        let mut html = deliver(&key_id, FIXTURE_PRIVATE_PKCS1_PEM, &body);
        replace_header(&mut html, "content-type", "text/html".to_owned());
        let html_reply = send(&app, &html).await;
        assert_eq!(html_reply.status, StatusCode::UNSUPPORTED_MEDIA_TYPE, "415");
        assert_eq!(html_reply.error, UNSUPPORTED_TYPE, "names the type");

        let junk = deliver(&key_id, FIXTURE_PRIVATE_PKCS1_PEM, b"{not json");
        let junk_reply = send(&app, &junk).await;
        assert_eq!(junk_reply.status, StatusCode::BAD_REQUEST, "400");
        assert_eq!(junk_reply.error, INVALID_JSON, "names the parse failure");

        let mut small_config = Config::for_tests();
        small_config.inbox_max_bytes = BODY.len() - 1;
        let small_app = inbox_app(test_state_with_config(pool.clone(), small_config));
        let cap_reply = send(
            &small_app,
            &deliver(&key_id, FIXTURE_PRIVATE_PKCS1_PEM, BODY),
        )
        .await;
        assert_eq!(cap_reply.status, StatusCode::PAYLOAD_TOO_LARGE, "413");
        assert_eq!(cap_reply.error, PAYLOAD_TOO_LARGE, "names the cap");
        server.abort();
    }
}
