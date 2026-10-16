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
//! 3. the `Digest` header must describe the bytes actually received
//!    (401 `Invalid digest`, [`verify_digest`] and its body rule);
//! 4. the `keyId` minus its fragment is the signer's actor URL (Mastodon's
//!    `#main-key`): the stored mirror answers, else the actor is fetched
//!    (401 `Unknown key` when neither works); a `keyId` naming a standalone
//!    key object is resolved by fetching that document and following its
//!    `owner`;
//! 5. the signature verifies against the stored `public_key` (the
//!    [`verify`] rules, `Date` skew included); on failure the actor is
//!    refetched at most once per 60 s and the check retried, then 401
//!    `Invalid signature`;
//! 6. the activity's `actor` must equal the signer's `ap_id` — forwarded
//!    activities authenticated by an `LD` signature are not supported and
//!    are rejected the same way (401 `Signature actor does not match
//!    activity actor`). A `Delete` of an actor that answers `410 Gone` and
//!    is about itself is answered `202 Accepted` and dropped;
//! 7. a signer whose host is on the instance-policy `reject` list (see
//!    [`crate::models::instance_policy_domain`]) is refused (403
//!    `Rejected`).
//!
//! Every failure is an [`AppError`], rendered by [`InboxAuthError`]; the
//! request body is never logged.

use std::{
    collections::BTreeMap,
    error::Error,
    fmt,
    sync::{Mutex, MutexGuard, PoisonError},
    time::{Duration, Instant},
};

use axum::{
    body::Body,
    extract::{FromRequest, Request},
    http::{HeaderMap, HeaderName, StatusCode, Uri, header},
    response::{IntoResponse, Response},
};
use bytes::Bytes;
use chrono::Utc;
use http_body_util::{BodyExt as _, LengthLimitError, Limited};
use serde_json::Value;
use tracing::debug;
use url::Url;

use crate::{
    error::AppError,
    federation::{
        fetch_actor::{FetchError, fetch_actor_json, get_or_fetch_by_ap_id, refresh_user},
        json::{as_array, value_or_id},
        signature::verify::{
            ParsedSignature, SignedRequestParts, parse_signature_header, verify, verify_digest,
        },
    },
    models::{
        instance_policy_domain,
        user::{User, get_by_ap_id},
    },
    state::AppState,
};

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
/// 403 body when the signer's host is on the instance-policy reject list.
const REJECTED: &str = "Rejected";
/// The `ActivityStreams` type of the one activity accepted while unsigned:
/// an actor's own deletion (see the [module docs](self) step 6).
const DELETE_TYPE: &str = "Delete";
/// The `Authorization` scheme an inbox `Signature` header may hide under.
const SIGNATURE_SCHEME: &str = "Signature";
/// The bare `Signature` request header.
const SIGNATURE_HEADER: HeaderName = HeaderName::from_static("signature");
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

/// Actor URLs whose key was refetched within [`KEY_REFETCH_WINDOW`].
/// Process-wide by nature: the throttle protects the remote actor, not one
/// worker's state. A poisoned lock keeps its map (entries are plain data),
/// the same recovery as [`crate::util::cache::TimedCache`].
static KEY_REFETCH_STAMPS: Mutex<BTreeMap<String, Instant>> = Mutex::new(BTreeMap::new());

/// A request rejected or dropped by the [`SignedActivity`] extractor.
///
/// Every authentication failure is an [`AppError`] under
/// [`Rejected`](Self::Rejected), rendered with its own status and
/// `{"error": …}` body (`docs/ARCHITECTURE.md` decision 8);
/// [`Discarded`](Self::Discarded) is the one non-failure: the activity is
/// answered `202 Accepted` and dropped without processing.
#[derive(Debug)]
pub enum InboxAuthError {
    /// The request failed one authentication step; its status and JSON
    /// error body come from the wrapped [`AppError`].
    Rejected(AppError),
    /// A `Delete` about an actor that answers `410 Gone` to the key
    /// resolution: accepted so the remote stops retrying, then dropped.
    Discarded,
}

impl InboxAuthError {
    /// The 413 [`Rejected`](Self::Rejected) for a body over the cap.
    fn too_large() -> Self {
        Self::Rejected(AppError::PayloadTooLarge(PAYLOAD_TOO_LARGE.to_owned()))
    }

    /// The 415 [`Rejected`](Self::Rejected) for a non-JSON `Content-Type`.
    fn wrong_type() -> Self {
        Self::Rejected(AppError::UnsupportedMediaType(UNSUPPORTED_TYPE.to_owned()))
    }

    /// The 400 [`Rejected`](Self::Rejected) for a body that is not JSON.
    fn invalid_json() -> Self {
        Self::Rejected(AppError::BadRequest(INVALID_JSON.to_owned()))
    }

    /// The 401 [`Rejected`](Self::Rejected) for a missing or unparsable
    /// `Signature` header.
    fn missing_signature() -> Self {
        Self::Rejected(AppError::Unauthorized(MISSING_SIGNATURE.to_owned()))
    }

    /// The 401 [`Rejected`](Self::Rejected) for a `Digest` that does not
    /// describe the received body.
    fn invalid_digest() -> Self {
        Self::Rejected(AppError::Unauthorized(INVALID_DIGEST.to_owned()))
    }

    /// The 401 [`Rejected`](Self::Rejected) for a `keyId` that resolves to
    /// no readable actor key.
    fn unknown_key() -> Self {
        Self::Rejected(AppError::Unauthorized(UNKNOWN_KEY.to_owned()))
    }

    /// The 401 [`Rejected`](Self::Rejected) for a signature that does not
    /// verify against the stored or the refetched key.
    fn invalid_signature() -> Self {
        Self::Rejected(AppError::Unauthorized(INVALID_SIGNATURE.to_owned()))
    }

    /// The 401 [`Rejected`](Self::Rejected) for an activity whose `actor`
    /// is not the authenticated signer.
    fn actor_mismatch() -> Self {
        Self::Rejected(AppError::Unauthorized(ACTOR_MISMATCH.to_owned()))
    }

    /// The 403 [`Rejected`](Self::Rejected) for a signer from a domain the
    /// instance policy rejects.
    fn blocked() -> Self {
        Self::Rejected(AppError::Forbidden(REJECTED.to_owned()))
    }
}

/// Any [`AppError`] is an authentication rejection, so `?` works through
/// the extractor's database and fetch steps.
impl From<AppError> for InboxAuthError {
    fn from(err: AppError) -> Self {
        Self::Rejected(err)
    }
}

/// `Display` states the outcome; every payload is log-safe — no body, no
/// key material.
impl fmt::Display for InboxAuthError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Rejected(err) => write!(f, "inbox authentication failed: {err}"),
            Self::Discarded => f.write_str("the Delete of a gone actor was accepted and dropped"),
        }
    }
}

impl Error for InboxAuthError {
    fn source(&self) -> Option<&(dyn Error + 'static)> {
        match self {
            Self::Rejected(err) => Some(err),
            Self::Discarded => None,
        }
    }
}

impl IntoResponse for InboxAuthError {
    fn into_response(self) -> Response {
        match self {
            Self::Rejected(err) => err.into_response(),
            Self::Discarded => StatusCode::ACCEPTED.into_response(),
        }
    }
}

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
    parsed: &'a ParsedSignature,
    /// The request headers as received.
    headers: &'a HeaderMap,
    /// The method, as received (e.g. `POST`).
    method: &'a str,
    /// The request target: path with its query string.
    target: &'a str,
    /// The parsed activity body.
    json: &'a Value,
}

impl Inbound<'_> {
    /// The [`SignedRequestParts`] [`verify`] rebuilds the signing text
    /// from: the headers cloned, since the parts own their map.
    fn signed_request(&self) -> SignedRequestParts<'_> {
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
    verify_digest(&parts.headers, &body).map_err(|_err| InboxAuthError::invalid_digest())?;
    let signer = resolve_signer(state, &parsed.key_id, &json).await?;
    let target = request_target(&parts.uri);
    let inbound = Inbound {
        parsed: &parsed,
        headers: &parts.headers,
        method: parts.method.as_str(),
        target: &target,
        json: &json,
    };
    let signer = verify_signature(state, &inbound, signer).await?;
    require_containment(&json, &signer)?;
    require_unblocked(state, &parsed.key_id).await?;
    Ok(SignedActivity { body, json, signer })
}

/// Whether the request labels its body one of the JSON `ActivityPub` media
/// types: the `Content-Type` up to its first `;`, trimmed and
/// case-folded, names exactly one of [`JSON_MEDIA_TYPES`].
fn declares_json_type(headers: &HeaderMap) -> bool {
    header_text(headers, &header::CONTENT_TYPE).is_some_and(|value| {
        let media_type = value.split_once(';').map_or(value, |(typed, _)| typed);
        JSON_MEDIA_TYPES
            .iter()
            .any(|wanted| media_type.trim().eq_ignore_ascii_case(wanted))
    })
}

/// The request target [`verify`] signs over: the path with its query
/// string when it has one.
fn request_target(uri: &Uri) -> String {
    uri.query().map_or_else(
        || uri.path().to_owned(),
        |query| format!("{}?{query}", uri.path()),
    )
}

/// Reads the request body into memory, stopping with the 413 as soon as
/// more than `max_bytes` have arrived — the cap is enforced while
/// streaming ([`Limited`]), not after buffering an endless body.
async fn read_limited(stream: Body, max_bytes: usize) -> Result<Bytes, InboxAuthError> {
    Limited::new(stream, max_bytes)
        .collect()
        .await
        .map(http_body_util::Collected::to_bytes)
        .map_err(|err| {
            if err.downcast_ref::<LengthLimitError>().is_some() {
                InboxAuthError::too_large()
            } else {
                debug!("inbox request body could not be read");
                InboxAuthError::Rejected(AppError::BadRequest(
                    "the request body could not be read".to_owned(),
                ))
            }
        })
}

/// Parses the capped body as the JSON activity document.
fn parse_json_body(body: &[u8]) -> Result<Value, InboxAuthError> {
    serde_json::from_slice(body).map_err(|_err| InboxAuthError::invalid_json())
}

/// The parsed `Signature` parameters the request presents: the bare
/// `Signature` header first, then an `Authorization: Signature ...`
/// carrying the same ones. A header that is present but unparsable has
/// failed to present a signature at all, so it reports the same 401.
fn present_signature(headers: &HeaderMap) -> Result<ParsedSignature, InboxAuthError> {
    let raw = header_text(headers, &SIGNATURE_HEADER)
        .or_else(|| authorization_signature_text(headers))
        .ok_or_else(InboxAuthError::missing_signature)?;
    parse_signature_header(raw).map_err(|_err| InboxAuthError::missing_signature())
}

/// The `Authorization` header's value when it carries the `Signature`
/// scheme; [`parse_signature_header`] drops the scheme itself.
fn authorization_signature_text(headers: &HeaderMap) -> Option<&str> {
    let value = header_text(headers, &header::AUTHORIZATION)?;
    value
        .trim_start()
        .split_once(char::is_whitespace)
        .is_some_and(|(scheme, _rest)| scheme.eq_ignore_ascii_case(SIGNATURE_SCHEME))
        .then_some(value)
}

/// The text of header `name`, `None` when absent or not visible ASCII.
fn header_text<'a>(headers: &'a HeaderMap, name: &HeaderName) -> Option<&'a str> {
    headers.get(name).and_then(|value| value.to_str().ok())
}

/// The `keyId` minus its fragment: Mastodon's `#main-key` names a key
/// embedded in the actor document, which is addressed by the bare URL.
fn strip_fragment(key_id: &str) -> &str {
    key_id.split_once('#').map_or(key_id, |(base, _)| base)
}

/// Resolves the `keyId` to the actor whose key must verify the signature
/// (step 4 of the [module docs](self)): the mirror of the fragment-stripped
/// URL answers; a miss fetches that actor; a fetch that fails on a
/// document that is not an actor retries the `keyId` itself as a key
/// object, following its `owner`. A `410 Gone` for a self-`Delete` is
/// [`Discarded`](InboxAuthError::Discarded); anything unresolved is 401
/// `Unknown key`.
async fn resolve_signer(
    state: &AppState,
    key_id: &str,
    json: &Value,
) -> Result<User, InboxAuthError> {
    let actor_url = strip_fragment(key_id);
    if let Some(stored) = get_by_ap_id(&state.db, actor_url).await? {
        return Ok(stored);
    }
    match get_or_fetch_by_ap_id(state, actor_url).await {
        Ok(fetched) => Ok(fetched),
        Err(err) => resolve_key_fallback(state, &err, key_id, actor_url, json).await,
    }
}

/// The fallbacks of a failed actor resolution, told apart by *why* the
/// fetch failed: `Gone` + self-`Delete` is the accepted-and-dropped
/// account deletion; an actor document that never parsed may be a
/// standalone key object whose `owner` names the real actor.
async fn resolve_key_fallback(
    state: &AppState,
    err: &FetchError,
    key_id: &str,
    actor_url: &str,
    json: &Value,
) -> Result<User, InboxAuthError> {
    if matches!(err, FetchError::Gone) && is_self_delete(json, actor_url) {
        debug!(actor = %actor_url, "inbox Delete of a gone actor: accepted and dropped");
        return Err(InboxAuthError::Discarded);
    }
    if matches!(err, FetchError::Invalid(_))
        && let Some(owner) = key_object_owner(state, key_id).await
    {
        return fetch_key_owner(state, &owner, json).await;
    }
    debug!(key_id, %err, "inbox signature keyId resolves to no actor");
    Err(InboxAuthError::unknown_key())
}

/// Dereferences `key_id` as a standalone key document and reads its
/// `owner`: the actor the key belongs to. Any failure reads as "no
/// owner", because every outcome ends in the same 401.
async fn key_object_owner(state: &AppState, key_id: &str) -> Option<String> {
    let document = fetch_actor_json(&state.http_client, key_id, None)
        .await
        .ok()?;
    document.get("owner").and_then(value_or_id)
}

/// Loads the actor a key document's `owner` names; a `410 Gone` for a
/// self-`Delete` of that owner is dropped like the direct path's.
async fn fetch_key_owner(
    state: &AppState,
    owner: &str,
    json: &Value,
) -> Result<User, InboxAuthError> {
    match get_or_fetch_by_ap_id(state, owner).await {
        Ok(fetched) => Ok(fetched),
        Err(FetchError::Gone) if is_self_delete(json, owner) => Err(InboxAuthError::Discarded),
        Err(_err) => Err(InboxAuthError::unknown_key()),
    }
}

/// Whether `json` is a `Delete` by `actor_url` of that same actor: the
/// activity's `actor` and `object` (string or object id) both name it.
/// This is the one activity accepted while its signer's key is
/// unreachable — the account is gone and nothing is done with it.
fn is_self_delete(json: &Value, actor_url: &str) -> bool {
    let names_actor = |key: &str| {
        json.get(key)
            .and_then(value_or_id)
            .is_some_and(|named| named == actor_url)
    };
    json.get("type").is_some_and(|types| {
        as_array(types)
            .iter()
            .any(|entry| entry.as_str() == Some(DELETE_TYPE))
    }) && names_actor("actor")
        && names_actor("object")
}

/// Verifies the signature against the signer's stored key, retrying once
/// after a key refetch on failure (step 5 of the [module docs](self)).
///
/// The retry refetches `signer`'s actor document — the peer may have
/// rotated its key since the mirror was written — but at most once per
/// actor per [`KEY_REFETCH_WINDOW`], so a flood of bad signatures cannot
/// turn into a fetch storm. The retried verification uses the refreshed
/// mirror, which the successful caller then receives.
async fn verify_signature(
    state: &AppState,
    inbound: &Inbound<'_>,
    signer: User,
) -> Result<User, InboxAuthError> {
    let request = inbound.signed_request();
    if let Some(pem) = signer.public_key.as_deref()
        && verifies(inbound, &request, pem)
    {
        return Ok(signer);
    }
    retry_after_refetch(state, inbound, &request, &signer).await
}

/// The retry arm of [`verify_signature`]: refetch once per window, check
/// the fresh key, and map every remaining outcome to 401 `Invalid
/// signature` — or to the dropped 202 when the refetch proved the actor
/// deleted itself.
async fn retry_after_refetch(
    state: &AppState,
    inbound: &Inbound<'_>,
    request: &SignedRequestParts<'_>,
    signer: &User,
) -> Result<User, InboxAuthError> {
    if !key_refetch_allowed(&signer.ap_id) {
        debug!(actor = %signer.ap_id, "inbox signature failed; refetch window still closed");
        return Err(InboxAuthError::invalid_signature());
    }
    match refresh_user(state, signer).await {
        Ok(refreshed) => match refreshed.public_key.as_deref() {
            Some(pem) if verifies(inbound, request, pem) => Ok(refreshed),
            Some(_) | None => Err(InboxAuthError::invalid_signature()),
        },
        Err(FetchError::Gone) if is_self_delete(inbound.json, &signer.ap_id) => {
            Err(InboxAuthError::Discarded)
        }
        Err(_err) => Err(InboxAuthError::invalid_signature()),
    }
}

/// The cryptographic check at the current moment; [`verify`] owns the
/// signed-header and `Date` skew rules.
fn verifies(inbound: &Inbound<'_>, request: &SignedRequestParts<'_>, public_pem: &str) -> bool {
    verify(inbound.parsed, request, public_pem, Utc::now()).is_ok()
}

/// Whether `ap_id`'s key may be refetched now: records the attempt and
/// answers `false` while an earlier one is inside the window. Stale
/// stamps are pruned on every call, which bounds the map to the actors
/// with failures in the last [`KEY_REFETCH_WINDOW`].
fn key_refetch_allowed(ap_id: &str) -> bool {
    let now = Instant::now();
    let mut stamps = refetch_stamps();
    stamps.retain(|_actor, then| now.saturating_duration_since(*then) < KEY_REFETCH_WINDOW);
    if stamps.contains_key(ap_id) {
        return false;
    }
    let _previous: Option<Instant> = stamps.insert(ap_id.to_owned(), now);
    true
}

/// The shared refetch-stamp map; a poisoned lock keeps its intact entries
/// (plain owned data, never half-updated).
fn refetch_stamps() -> MutexGuard<'static, BTreeMap<String, Instant>> {
    KEY_REFETCH_STAMPS
        .lock()
        .unwrap_or_else(PoisonError::into_inner)
}

/// Step 6 of the [module docs](self): the activity's `actor` (string or
/// object id) must name the authenticated signer's `ap_id`. An absent
/// `actor` names nobody and fails the same way; forwarding authenticated
/// by an `LD` signature is not supported yet, so a different server's
/// activity cannot pass either.
fn require_containment(json: &Value, signer: &User) -> Result<(), InboxAuthError> {
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

/// Step 7 of the [module docs](self): the signer's instance may not be on
/// the instance-policy reject list (the `MRF` rejector's table). An
/// unparsable `keyId` names no host to check — such a signer never got
/// past resolution anyway, so it cannot smuggle a blocked delivery in.
async fn require_unblocked(state: &AppState, key_id: &str) -> Result<(), InboxAuthError> {
    if let Some(host) = signer_host(key_id)
        && instance_policy_domain::is_rejected(&state.db, &host).await?
    {
        debug!(key_id, "inbox delivery from a rejected instance");
        return Err(InboxAuthError::blocked());
    }
    Ok(())
}

/// The host of a signer's key URL, lowercase and without the port.
///
/// It is the authority of its `keyId` (or, equivalently, of the actor URL
/// it points at) — the canonical spelling `instance_policy_domains` rows
/// keep. `None` when the URL cannot be parsed.
#[must_use]
pub fn signer_host(key_id: &str) -> Option<String> {
    let parsed = Url::parse(key_id).ok()?;
    let host = parsed.host_str()?;
    (!host.is_empty()).then(|| host.to_ascii_lowercase())
}

#[cfg(test)]
mod tests {
    use std::str::FromStr as _;
    use std::sync::{
        Arc,
        atomic::{AtomicUsize, Ordering},
    };

    use axum::{
        Json, Router,
        body::Body,
        extract::{Request, State},
        http::{HeaderMap, HeaderName, HeaderValue, StatusCode, header},
        routing::{get, post},
    };
    use serde_json::{Value, json};
    use sqlx::PgPool;
    use tokio::{net::TcpListener, task::JoinHandle};
    use tower::ServiceExt as _;

    use super::*;
    use crate::{
        config::Config,
        federation::signature::{SigningKey, sign_request},
        http_client::HttpClient,
        models::user::{NewUser, get_by_ap_id, insert},
        uploads::UploaderBackend,
        util::keys::{FIXTURE_PRIVATE_PKCS1_PEM, FIXTURE_PUBLIC_PKCS1_PEM},
    };

    /// The origin test requests are addressed to and sign their `Host` as:
    /// the [`Config::for_tests`] base url, matching what a real peer signs.
    const INBOX_URL: &str = "https://test.example/inbox";
    /// A five-byte body, for the size-cap test with a four-byte limit.
    const BODY: &[u8] = b"hello";
    /// The `publicKeyPem` a mirror row seeds with when it must not verify.
    const STALE_KEY_PEM: &str = "-----BEGIN PUBLIC KEY-----\nSTALE\n-----END PUBLIC KEY-----\n";

    /// The second pair's private key, PKCS#8 PEM, generated offline like
    /// [`FIXTURE_PRIVATE_PKCS1_PEM`]: the fixture key must not accept it.
    const ROTATED_PRIVATE_PEM: &str = "
-----BEGIN PRIVATE KEY-----
MIICdQIBADANBgkqhkiG9w0BAQEFAASCAl8wggJbAgEAAoGBALDiE29QVeI6UuQV
hAwkdU7CFiDpvovtvIDw9a7hj/csyzFMqOops5OdsyELlWVJDYVbIWsu9YIdOoLf
P8ZDNT2UXbZV355Mb9vNX6WdWdxyw2KNQKwVVTDxNzIC9UQns76BYFUuAk27/0uJ
TKHbq7uoQfVVsfroyNDlV1zZ7TQZAgMBAAECgYBpzNATJNo6Fui9xKDXIneIRE84
IJf42pqGBZrun7yc/A4Nzuy6I3dmNE2Q1nzluGx996RlmBEBYro8zYw36rtMu1Hv
ljOplbJPRzcghTJXT7viCocv/FSaVDb5eFCb+dQSu4cXIrdK0DTkxjILKs2vYAhY
DoekP5AVBCx60fvqAQJBAN0L4kj6rDAI+S8C0Ad/yMS6ctDD6IYo24CpBtsjBZyd
CcfT7UlR0OXP3bxto3rHPJ8xM2jgBG9+KoR1kqrV6N0CQQDM2m4D4xD6kvKLGD0N
5Fb1K+3QR7gK2/AG6Fu/eYEjv3q8TjeOE9+ikbilpJMxSrmrSabRPKD0P3CJ1Qxw
xWZtAkA9Fsuy35Q2skQreduENwyFUgHpMDtS2HfRVuVOKp+iaMQLNcaePL4+xPPC
34oOpIFcfXx3v3F6XqPhd5zRGc0BAkAIUVIoPD8DPZY1iZ1pA/135Pm5/D9/yWBc
l3CqPJ++uNad8LsWvL3W1v3dU+paFQOm7c2lL0YFxNGn9wuTRKCdAkB/CQsvdoEM
Xtk2GaHvtK4bFI0BVjRZConB8iruDzYNaVu0GB4KpK5jiT+4pdRCdoEADMAAJghz
abbY4/vMuKDE
-----END PRIVATE KEY-----
";

    /// The public half of [`ROTATED_PRIVATE_PEM`], legacy PKCS#1 PEM: what a
    /// peer serves as its `publicKeyPem`.
    const ROTATED_PUBLIC_PEM: &str = "
-----BEGIN RSA PUBLIC KEY-----
MIGJAoGBALDiE29QVeI6UuQVhAwkdU7CFiDpvovtvIDw9a7hj/csyzFMqOops5Od
syELlWVJDYVbIWsu9YIdOoLfP8ZDNT2UXbZV355Mb9vNX6WdWdxyw2KNQKwVVTDx
NzIC9UQns76BYFUuAk27/0uJTKHbq7uoQfVVsfroyNDlV1zZ7TQZAgMBAAE=
-----END RSA PUBLIC KEY-----
";

    /// The state under test: `pool`, the test config and an SSRF-guard-off
    /// client that may reach the loopback mocks.
    fn test_state(pool: PgPool) -> AppState {
        test_state_with_config(pool, Config::for_tests())
    }

    /// The state under test over an explicit config (the size-cap test
    /// needs a small `inbox_max_bytes`).
    fn test_state_with_config(pool: PgPool, config: Config) -> AppState {
        let config = Arc::new(config);
        let hub = crate::streaming::hub::Hub::new(&pool);
        AppState {
            db: pool,
            hub,
            config: Arc::clone(&config),
            http_client: HttpClient::from(config.as_ref()),
            uploader: UploaderBackend::from(config.as_ref()),
            instance_stats_cache: Arc::default(),
            mailer: Arc::new(crate::mailer::MemoryTransport::new()),
        }
    }

    /// The router under test: one `POST /inbox` gated by the extractor.
    fn inbox_app(state: AppState) -> Router {
        Router::new()
            .route("/inbox", post(echo_activity))
            .with_state(state)
    }

    /// The success arm: 200 JSON naming the signer and echoing the body,
    /// proving the extractor delivered all three fields.
    async fn echo_activity(activity: SignedActivity) -> Response {
        (
            StatusCode::OK,
            Json(json!({
                "signer": activity.signer.nickname,
                "actor": activity.json.get("actor"),
                "type": activity.json.get("type"),
                "bytes": activity.body.len(),
            })),
        )
            .into_response()
    }

    /// The `Person` actor document a peer serves for `id`, vouching for
    /// `public_pem` as its signing key.
    fn actor_document(id: &str, public_pem: &str) -> Value {
        json!({
            "@context": "https://www.w3.org/ns/activitystreams",
            "type": "Person",
            "id": id,
            "preferredUsername": "alice",
            "name": "Alice",
            "inbox": format!("{id}/inbox"),
            "publicKey": {
                "id": format!("{id}#main-key"),
                "owner": id,
                "publicKeyPem": public_pem,
            },
        })
    }

    /// The `ActivityPub` JSON answer of a mock peer.
    fn actor_response(document: &Value) -> Response {
        (
            [(header::CONTENT_TYPE, "application/activity+json")],
            document.to_string(),
        )
            .into_response()
    }

    /// Mock peer state: its origin, the fetch counter every route bumps,
    /// and the public key the served actor carries.
    #[derive(Clone, Debug)]
    struct Peer {
        /// The `http://{addr}` origin of this mock server.
        base: String,
        /// Requests answered, across all routes.
        hits: Arc<AtomicUsize>,
        /// The public key the actor document carries.
        public_pem: String,
    }

    /// A peer at `base` serving `public_pem`.
    fn peer(base: String, public_pem: String) -> Peer {
        Peer {
            base,
            hits: Arc::new(AtomicUsize::new(0)),
            public_pem,
        }
    }

    /// Counts the hit and answers the self-identifying actor document.
    async fn serve_actor(State(peer): State<Peer>) -> Response {
        let _hits = peer.hits.fetch_add(1, Ordering::SeqCst);
        actor_response(&actor_document(
            &format!("{}/users/alice", peer.base),
            &peer.public_pem,
        ))
    }

    /// Counts the hit and answers the standalone key object whose `owner`
    /// names this peer's actor: the `keyId` shape that is not an actor.
    async fn serve_key_object(State(peer): State<Peer>) -> Response {
        let _hits = peer.hits.fetch_add(1, Ordering::SeqCst);
        actor_response(&json!({
            "@context": "https://w3id.org/security/v1",
            "id": format!("{}/keys/1", peer.base),
            "type": "Key",
            "owner": format!("{}/users/alice", peer.base),
            "publicKeyPem": peer.public_pem,
        }))
    }

    /// Answers the configured status and nothing else (410, 404).
    async fn status_only(State(status): State<StatusCode>) -> StatusCode {
        status
    }

    /// Binds an unused loopback port.
    async fn bind() -> TcpListener {
        TcpListener::bind("127.0.0.1:0").await.unwrap()
    }

    /// The `http://{addr}` origin a listener answers on.
    fn origin(listener: &TcpListener) -> String {
        format!("http://{}", listener.local_addr().unwrap())
    }

    /// Serves `app` on `listener` until the test aborts the task.
    fn spawn(listener: TcpListener, app: Router) -> JoinHandle<()> {
        tokio::spawn(async move {
            if let Err(err) = axum::serve(listener, app).await {
                tracing::debug!("test server stopped: {err}");
            }
        })
    }

    /// A mock peer answering `/users/alice` (actor), `/keys/1` (key
    /// object), `/gone` (410) and `/missing` (404). Routes with different
    /// state types get their state erased and the pieces merged, the
    /// pattern the [`crate::federation::fetch_actor`] tests use.
    fn peer_app(peer: Peer) -> Router {
        let actor_peer = peer.clone();
        Router::new()
            .route("/users/alice", get(serve_actor))
            .with_state(actor_peer)
            .merge(
                Router::new()
                    .route("/keys/1", get(serve_key_object))
                    .with_state(peer),
            )
            .merge(
                Router::new()
                    .route("/gone", get(status_only))
                    .with_state(StatusCode::GONE),
            )
            .merge(
                Router::new()
                    .route("/missing", get(status_only))
                    .with_state(StatusCode::NOT_FOUND),
            )
    }

    /// Inserts a `reject` policy row directly (this table has no writer
    /// until the admin tickets, like its own tests).
    async fn block_domain(pool: &PgPool, domain: &str) {
        let inserted = sqlx::query(
            "INSERT INTO instance_policy_domains (domain, action, reason) VALUES ($1, 'reject', '')",
        )
        .bind(domain)
        .execute(pool)
        .await
        .unwrap();
        assert_eq!(inserted.rows_affected(), 1, "the policy row is there");
    }

    /// Seeds a remote mirror row for actor `ap_id` holding `public_pem`:
    /// the stale-mirror state the rotation and throttle tests start from.
    async fn seed_remote_user(pool: &PgPool, ap_id: &str, public_pem: &str) {
        let new = NewUser {
            nickname: "alice@127.0.0.1".to_owned(),
            ap_id: ap_id.to_owned(),
            domain: Some("127.0.0.1".to_owned()),
            name: "Alice".to_owned(),
            inbox: Some(format!("{ap_id}/inbox")),
            public_key: Some(public_pem.to_owned()),
            ..NewUser::default()
        };
        let _seeded: User = insert(pool, &new).await.unwrap();
    }

    /// A delivering peer's `POST`: the body and every header the fediverse
    /// signs, built by [`sign_request`] so the extractor sees the real
    /// shape. Tests tamper with the fields before [`send`]-ing them.
    #[derive(Debug)]
    struct Delivery {
        /// The body bytes, as sent (not necessarily as signed).
        body: Vec<u8>,
        /// The headers, in send order: content-type, host, date, digest,
        /// signature.
        headers: Vec<(&'static str, String)>,
    }

    /// Signs `body` as a `POST` to [`INBOX_URL`] with `key_id`.
    fn deliver(key_id: &str, private_pem: &str, body: &[u8]) -> Delivery {
        let signed = sign_request(
            "POST",
            INBOX_URL,
            Some(body),
            &SigningKey {
                key_id,
                private_pem,
            },
            Utc::now(),
        )
        .unwrap();
        let digest = signed.digest.clone().unwrap();
        Delivery {
            body: body.to_vec(),
            headers: vec![
                ("content-type", "application/activity+json".to_owned()),
                ("host", signed.host.clone()),
                ("date", signed.date.clone()),
                ("digest", digest),
                ("signature", signed.signature),
            ],
        }
    }

    /// Swaps one header value of a [`Delivery`] (the tampering tests).
    fn replace_header(delivery: &mut Delivery, name: &str, value: String) {
        let slot = delivery
            .headers
            .iter_mut()
            .find(|(existing, _)| *existing == name)
            .unwrap_or_else(|| panic!("the test delivery carries a {name} header"));
        slot.1 = value;
    }

    /// Drops one header from a [`Delivery`].
    fn drop_header(delivery: &mut Delivery, name: &str) {
        let index = delivery
            .headers
            .iter()
            .position(|(existing, _)| *existing == name)
            .unwrap_or_else(|| panic!("the test delivery carries a {name} header"));
        let _removed = delivery.headers.remove(index);
    }

    /// The value of one header of a [`Delivery`].
    fn header_value(delivery: &Delivery, name: &str) -> String {
        for (existing, value) in &delivery.headers {
            if *existing == name {
                return value.clone();
            }
        }
        panic!("the test delivery carries a {name} header");
    }

    /// The Follow activity `actor` posts of `object`.
    fn activity(actor: &str, object: &str) -> Vec<u8> {
        json!({
            "@context": "https://www.w3.org/ns/activitystreams",
            "type": "Follow",
            "actor": actor,
            "object": object,
        })
        .to_string()
        .into_bytes()
    }

    /// The `Delete` by `actor` of that same actor.
    fn self_delete(actor: &str) -> Vec<u8> {
        json!({
            "@context": "https://www.w3.org/ns/activitystreams",
            "type": "Delete",
            "actor": actor,
            "object": actor,
        })
        .to_string()
        .into_bytes()
    }

    /// The extractor's answer, fully buffered.
    #[derive(Debug)]
    struct Answer {
        /// The HTTP status.
        status: StatusCode,
        /// The `{"error": …}` body message; empty unless there is one.
        error: String,
        /// The whole decoded body, `Null` when it is not JSON.
        json: Value,
    }

    /// Drives one [`Delivery`] through the real router over `oneshot`.
    async fn send(app: &Router, delivery: &Delivery) -> Answer {
        let mut builder = Request::post("/inbox");
        for (name, value) in &delivery.headers {
            let parsed = HeaderName::from_str(name).unwrap();
            builder = builder.header(parsed, HeaderValue::from_str(value).unwrap());
        }
        let request = builder.body(Body::from(delivery.body.clone())).unwrap();
        let response = app.clone().oneshot(request).await.unwrap();
        let status = response.status();
        let bytes = axum::body::to_bytes(response.into_body(), usize::MAX)
            .await
            .unwrap();
        let text = String::from_utf8_lossy(&bytes).into_owned();
        let json: Value = serde_json::from_str(&text).unwrap_or(Value::Null);
        let error = json
            .get("error")
            .and_then(Value::as_str)
            .unwrap_or_default()
            .to_owned();
        Answer {
            status,
            error,
            json,
        }
    }

    #[test]
    fn strip_fragment_cuts_the_key_fragment() {
        assert_eq!(
            strip_fragment("https://peer.example/users/alice#main-key"),
            "https://peer.example/users/alice",
            "Mastodon's embedded-key spelling"
        );
        assert_eq!(
            strip_fragment("https://peer.example/users/alice"),
            "https://peer.example/users/alice",
            "a bare actor url as keyId is unchanged"
        );
    }

    #[test]
    fn signer_host_reads_the_lowercase_authority_host() {
        assert_eq!(
            signer_host("https://Peer.Example/users/alice#main-key").as_deref(),
            Some("peer.example"),
            "host only, portless, case-folded"
        );
        assert_eq!(
            signer_host("http://127.0.0.1:4000/users/alice#main-key").as_deref(),
            Some("127.0.0.1"),
            "the port is not part of the host"
        );
        assert_eq!(
            signer_host("not a url"),
            None,
            "an unparsable keyId names no host"
        );
    }

    #[test]
    fn the_json_media_types_are_the_three_peers_send() {
        let typed = |value: &str| {
            let mut headers = HeaderMap::new();
            let previous =
                headers.insert(header::CONTENT_TYPE, HeaderValue::from_str(value).unwrap());
            drop(previous);
            headers
        };
        for value in [
            "application/activity+json",
            "application/ld+json",
            "application/json",
            "application/ld+json; profile=\"https://www.w3.org/ns/activitystreams\"",
            "Application/Activity+JSON; charset=utf-8",
        ] {
            assert!(declares_json_type(&typed(value)), "{value} is accepted");
        }
        for value in ["text/html", "application/xml", "application/jsonld"] {
            assert!(!declares_json_type(&typed(value)), "{value} is refused");
        }
        assert!(
            !declares_json_type(&HeaderMap::new()),
            "a body with no declared type is refused"
        );
    }

    #[test]
    fn the_authorization_signature_scheme_is_recognised() {
        for value in [
            r#"Signature keyId="k""#,
            r#"signature keyId="k""#,
            r#"  SIGNATURE keyId="k""#,
        ] {
            let mut headers = HeaderMap::new();
            let previous =
                headers.insert(header::AUTHORIZATION, HeaderValue::from_str(value).unwrap());
            drop(previous);
            assert_eq!(
                authorization_signature_text(&headers),
                Some(value),
                "`{value}` is a Signature authorization"
            );
        }
        for value in ["Bearer tok3n", "Basic dXNlcjpwYXNz", "Signature"] {
            let mut headers = HeaderMap::new();
            let previous =
                headers.insert(header::AUTHORIZATION, HeaderValue::from_str(value).unwrap());
            drop(previous);
            assert_eq!(
                authorization_signature_text(&headers),
                None,
                "`{value}` presents no Signature parameters"
            );
        }
    }

    #[test]
    fn a_self_delete_names_the_deleted_actor_in_both_places() {
        let plain: Value =
            serde_json::from_slice(&self_delete("https://p.example/users/a")).unwrap();
        assert!(
            is_self_delete(&plain, "https://p.example/users/a"),
            "Delete by the actor of itself"
        );
        let embedded = json!({
            "type": ["Delete"],
            "actor": { "id": "https://p.example/users/a" },
            "object": { "id": "https://p.example/users/a" },
        });
        assert!(
            is_self_delete(&embedded, "https://p.example/users/a"),
            "array type and object spellings read too"
        );
        let note = json!({
            "type": "Delete",
            "actor": "https://p.example/users/a",
            "object": "https://p.example/objects/note",
        });
        assert!(
            !is_self_delete(&note, "https://p.example/users/a"),
            "deleting a Note is not deleting the actor"
        );
        assert!(
            !is_self_delete(&note, "https://p.example/objects/note"),
            "and the signer must be the actor, not the object"
        );
    }

    #[test]
    fn the_refetch_throttle_opens_once_per_actor_per_window() {
        assert!(
            key_refetch_allowed("https://throttle.example/users/a"),
            "a first failure may refetch"
        );
        assert!(
            !key_refetch_allowed("https://throttle.example/users/a"),
            "a second inside the window may not"
        );
        assert!(
            key_refetch_allowed("https://throttle.example/users/b"),
            "another actor has its own budget"
        );
    }

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
            Some("alice@127.0.0.1"),
            "the on-the-fly mirror is the signer"
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

    /// A `keyId` that names no actor — and no key object — is unknown.
    #[tokio::test]
    async fn an_unknown_key_is_rejected() {
        let db = crate::test_db::test_db().await.expect("test database slot");
        let pool = db.pool().clone();
        let listener = bind().await;
        let base = origin(&listener);
        let peer = peer(base.clone(), FIXTURE_PUBLIC_PKCS1_PEM.to_owned());
        let server = spawn(listener, peer_app(peer));
        let app = inbox_app(test_state(pool.clone()));

        let body = activity(
            &format!("{base}/users/ghost"),
            "https://test.example/users/bob",
        );
        let answer = send(
            &app,
            &deliver(
                &format!("{base}/missing#main-key"),
                FIXTURE_PRIVATE_PKCS1_PEM,
                &body,
            ),
        )
        .await;
        assert_eq!(answer.status, StatusCode::UNAUTHORIZED, "unknown key");
        assert_eq!(answer.error, UNKNOWN_KEY, "resolution failure wording");
        server.abort();
    }

    /// A `keyId` whose URL is a standalone key object resolves through
    /// its `owner` to the actor document.
    #[tokio::test]
    async fn a_standalone_key_object_follows_its_owner() {
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
            &deliver(&format!("{base}/keys/1"), FIXTURE_PRIVATE_PKCS1_PEM, &body),
        )
        .await;
        assert_eq!(
            answer.status,
            StatusCode::OK,
            "the owner's key verifies: {}",
            answer.error
        );
        assert_eq!(
            answer.json.get("signer").and_then(Value::as_str),
            Some("alice@127.0.0.1"),
            "the owner is the signer"
        );
        assert!(
            hits.load(Ordering::SeqCst) >= 2,
            "the key document and the actor were both dereferenced"
        );
        let stored = get_by_ap_id(&pool, &ap_id).await.unwrap();
        assert!(stored.is_some(), "the owner landed in the mirror table");
        server.abort();
    }

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
        assert_eq!(
            stored.public_key.unwrap().trim(),
            ROTATED_PUBLIC_PEM.trim(),
            "the mirror row now carries the rotated key"
        );
        server.abort();
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

    /// The `Delete` of an actor that answers `410 Gone` is accepted and
    /// dropped when it deletes itself — and only then.
    #[tokio::test]
    async fn a_gone_actors_self_delete_is_dropped_with_202() {
        let db = crate::test_db::test_db().await.expect("test database slot");
        let pool = db.pool().clone();
        let listener = bind().await;
        let base = origin(&listener);
        let gone = format!("{base}/gone");
        let peer = peer(base.clone(), FIXTURE_PUBLIC_PKCS1_PEM.to_owned());
        let server = spawn(listener, peer_app(peer));
        let app = inbox_app(test_state(pool.clone()));

        let delete_reply = send(
            &app,
            &deliver(
                &format!("{gone}#main-key"),
                FIXTURE_PRIVATE_PKCS1_PEM,
                &self_delete(&gone),
            ),
        )
        .await;
        assert_eq!(
            delete_reply.status,
            StatusCode::ACCEPTED,
            "the self-Delete is accepted"
        );
        assert!(delete_reply.json.is_null(), "and dropped without a body");

        // A live-looking actor deleting something else while gone is not
        // the exception: a Note cannot be dropped as an account deletion.
        let note = json!({
            "@context": "https://www.w3.org/ns/activitystreams",
            "type": "Delete",
            "actor": gone,
            "object": format!("{gone}/objects/1"),
        })
        .to_string()
        .into_bytes();
        let note_reply = send(
            &app,
            &deliver(
                &format!("{gone}#main-key"),
                FIXTURE_PRIVATE_PKCS1_PEM,
                &note,
            ),
        )
        .await;
        assert_eq!(
            note_reply.status,
            StatusCode::UNAUTHORIZED,
            "deleting an object is not self-deletion"
        );
        assert_eq!(note_reply.error, UNKNOWN_KEY, "and the key stays unknown");
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
}
