//! Fetching remote actors: the HTTP dereference of an actor document and
//! the get-or-fetch mirrors built on top of it.
//!
//! [`fetch_actor_json`] is the wire half: a JSON-asking, size-capped,
//! optionally signed `GET` that returns only a document which vouches for
//! the URL it was fetched from. The get-or-fetch functions are the policy
//! half: consult the local `users` mirror first (decision 7), refresh a
//! remote mirror no more than once a day, keep serving the stale mirror
//! when a peer misbehaves, and resolve `acct` handles through
//! [`WebFinger`](crate::federation::webfinger_client) when the database
//! knows nothing. The reference behaviour is Pleroma's
//! `Pleroma.Object.Fetcher.fetch_and_contain_remote_object_from_id/1`
//! (`lib/pleroma/object/fetcher.ex`) and `Pleroma.User`'s
//! `get_or_fetch_by_ap_id/1`, `get_or_fetch_by_nickname/1` and the 24-hour
//! `needs_update?/1` window (`lib/pleroma/user.ex`); the ticket rules here
//! are authoritative where the two differ.
//!
//! Two containment rules keep a hostile host from putting words in another
//! instance's mouth (Mastodon's round trip, Pleroma's
//! `Containment.contain_origin_from_id/2`):
//!
//! - A fetched document's `id` must name the URL it was fetched from. The
//!   shared [`HttpClient`] follows
//!   redirects internally and does not report the final URL, so a document
//!   naming anything else is dereferenced exactly once at its own `id` and
//!   accepted only if it names *that* URL — a moved document is re-anchored
//!   to its canonical id, a document that keeps moving is
//!   [`FetchError::Invalid`].
//! - A handle resolved through `WebFinger` must be vouched for by the actor
//!   itself: the fetched document's `preferredUsername@host` has to equal
//!   the requested `acct`, or the lookup fails as
//!   [`FetchError::AcctMismatch`].

use std::{error::Error, fmt};

use chrono::{DateTime, Duration, Utc};
use reqwest::header::{CONTENT_TYPE, HeaderMap};
use serde_json::Value;
use tracing::{debug, error};
use url::Url;

use crate::{
    config::Config,
    error::AppError,
    federation::{
        remote_actor::{ActorError, parse_actor, upsert_remote_user},
        signature::{SignedHeaders, SigningKey, sign_request},
        webfinger_client::{WebfingerError, resolve_acct_with_base, strip_finger_prefixes},
    },
    http_client::{HttpClient, HttpError},
    models::user::{User, get_by_acct, get_by_ap_id, get_by_id},
    state::AppState,
};

/// Cap on one fetched actor document: 1 MiB. Actor documents are profiles;
/// anything larger is an attack or a mistake.
const MAX_BODY_BYTES: usize = 1024 * 1024;
/// The `Accept` header every actor fetch asks for: both `ActivityPub` JSON
/// media types, the `ld+json` one pinned to the `ActivityStreams` profile
/// (Mastodon and Pleroma serve on either spelling).
const ACTOR_ACCEPT: &str = "application/activity+json, application/ld+json; \
                            profile=\"https://www.w3.org/ns/activitystreams\"";
/// How long a stored remote mirror answers without a refetch: Pleroma's
/// `needs_update?/1` 24-hour window.
const REFRESH_AFTER: Duration = Duration::hours(24);
/// Status of an actor that does not exist.
const STATUS_NOT_FOUND: u16 = 404;
/// Status of an actor that is permanently gone (Pleroma's and Mastodon's
/// tombstone answer; the one refresh failure that must not hide behind a
/// stale mirror).
const STATUS_GONE: u16 = 410;
/// Substring every JSON-family media type the peers answer with shares.
const JSON_MARKER: &str = "json";

/// How fetching a remote actor can fail.
#[derive(Debug)]
pub enum FetchError {
    /// The request itself failed or the peer answered a status that is not
    /// one of the actor-specific ones: the transport and status failures of
    /// [`crate::http_client`].
    Http(HttpError),
    /// The peer answered 410: the actor is permanently gone. A stale
    /// mirror must not mask this one.
    Gone,
    /// No actor behind the address: the peer answered 404, or a *local*
    /// actor id names a row this database does not have.
    NotFound,
    /// The answer cannot become a mirror: a non-JSON content type, an
    /// unparsable or non-self-consistent document, a database failure
    /// while storing it, or a fetch that could not be signed. The string
    /// states which.
    Invalid(String),
    /// A handle lookup fetched an actor that does not name the requested
    /// `acct` (the Mastodon round trip of this module's docs).
    AcctMismatch,
    /// The `WebFinger` step of an `acct` lookup failed.
    Webfinger(WebfingerError),
}

impl fmt::Display for FetchError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Http(err) => write!(f, "actor fetch failed: {err}"),
            Self::Gone => f.write_str("the actor is gone"),
            Self::NotFound => f.write_str("the actor was not found"),
            Self::Invalid(detail) => f.write_str(detail),
            Self::AcctMismatch => {
                f.write_str("the fetched actor does not belong to the requested account")
            }
            Self::Webfinger(err) => write!(f, "could not resolve the account: {err}"),
        }
    }
}

impl Error for FetchError {
    fn source(&self) -> Option<&(dyn Error + 'static)> {
        match self {
            Self::Http(err) => Some(err),
            Self::Webfinger(err) => Some(err),
            Self::Gone | Self::NotFound | Self::Invalid(_) | Self::AcctMismatch => None,
        }
    }
}

/// Any transport or status failure of the shared client is an
/// [`FetchError::Http`], so `?` works throughout the fetch.
impl From<HttpError> for FetchError {
    fn from(err: HttpError) -> Self {
        Self::Http(err)
    }
}

/// Any `WebFinger` failure of an `acct` lookup is a
/// [`FetchError::Webfinger`].
impl From<WebfingerError> for FetchError {
    fn from(err: WebfingerError) -> Self {
        Self::Webfinger(err)
    }
}

/// A rejected or unstashable actor document is a [`FetchError::Invalid`]
/// carrying the parser's field detail; the database calls of
/// [`upsert_remote_user`] fold into the same variant through `server_error`.
impl From<ActorError> for FetchError {
    fn from(err: ActorError) -> Self {
        match err {
            ActorError::Failed(server) => server_error(&server),
            ActorError::Invalid(detail) => Self::Invalid(format!("invalid remote actor: {detail}")),
            ActorError::NicknameConflict => Self::Invalid(
                "invalid remote actor: the nickname is owned by a different actor id".to_owned(),
            ),
        }
    }
}

impl From<FetchError> for AppError {
    fn from(err: FetchError) -> Self {
        match err {
            FetchError::Gone | FetchError::NotFound => Self::NotFound,
            FetchError::Invalid(detail) => Self::Unprocessable(detail),
            FetchError::AcctMismatch => {
                Self::Unprocessable("the fetched actor is not the requested account".to_owned())
            }
            FetchError::Http(err) => Self::BadGateway(format!("actor dereference failed: {err}")),
            FetchError::Webfinger(WebfingerError::InvalidAcct) => {
                Self::Unprocessable("not a valid acct:nick@domain handle".to_owned())
            }
            FetchError::Webfinger(err) => {
                Self::BadGateway(format!("webfinger lookup failed: {err}"))
            }
        }
    }
}

/// The identity that signs an actor `GET`: the instance actor's key URI
/// and private key, the pair the instance-actor ticket will pass into
/// [`fetch_actor_json`] for authorized-fetch peers.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct Signer<'key> {
    /// The `keyId` Signature parameter: the instance actor's key id, e.g.
    /// `https://social.example/actor#main-key`.
    pub key_id: &'key str,
    /// The signing key as PEM (see
    /// [`crate::util::keys::parse_private_key`]).
    pub private_pem: &'key str,
}

/// Dereferences an actor document over HTTP.
///
/// The `GET` asks for both `ActivityPub` JSON media types, follows
/// redirects under the shared client's policy, caps the body at 1 MiB and
/// refuses an answer whose content type does not say `json`. With `signer`
/// the request is additionally HTTP-signed (see
/// [`sign_request`]), the shape of an authorized fetch.
///
/// The returned document is contained as this module's docs describe: its
/// `id` either equals `url` or names a document fetched once at its own
/// `id` that names itself.
///
/// # Errors
///
/// [`FetchError::Gone`] for a 410, [`FetchError::NotFound`] for a 404,
/// [`FetchError::Http`] for any other transport or status failure
/// (including a body over the cap), and [`FetchError::Invalid`] for a
/// non-JSON content type, a body that is not a JSON object with a string
/// `id`, a document that moves its `id` on while being refetched, or a
/// `signer` that cannot sign this URL.
pub async fn fetch_actor_json(
    http: &HttpClient,
    url: &str,
    signer: Option<&Signer<'_>>,
) -> Result<Value, FetchError> {
    let document = fetch_document(http, url, signer).await?;
    let claimed = document_id(&document)?;
    if claimed == url {
        return Ok(document);
    }
    // The document names a different id than the URL it was fetched from
    // (or the peer redirected elsewhere): follow the claim exactly once
    // and accept only a document that names itself.
    let refetched = fetch_document(http, claimed, signer).await?;
    let refetched_id = document_id(&refetched)?;
    if refetched_id != claimed {
        return Err(FetchError::Invalid(format!(
            "the actor `{claimed}` identifies itself as `{refetched_id}`"
        )));
    }
    Ok(refetched)
}

/// Looks an actor up by `ActivityPub` id, fetching and mirroring it when
/// this instance does not have it (or has a stale mirror of it).
///
/// The database comes first: a local row answers as stored (this instance
/// owns its actors), a remote mirror younger than 24 hours answers as
/// stored too, and an older one is refetched and upserted. Any refresh
/// failure except [`FetchError::Gone`] falls back to the stale mirror —
/// one flaky peer must not hide an account this instance already knows.
/// An unknown *local* actor id (one on this instance's own origin) is
/// [`FetchError::NotFound`] without touching the network: its document
/// would be served by this very process.
///
/// # Errors
///
/// [`FetchError::NotFound`] for an unknown local id; otherwise, on a first
/// fetch, whatever [`fetch_actor_json`], [`parse_actor`] or the mirror
/// write reports. On a refresh those failures only log and the stale
/// mirror answers.
pub async fn get_or_fetch_by_ap_id(state: &AppState, ap_id: &str) -> Result<User, FetchError> {
    if let Some(stored) = get_by_ap_id(&state.db, ap_id)
        .await
        .map_err(|err| server_error(&err))?
    {
        return serve_or_refresh(state, stored, ap_id).await;
    }
    if is_local_ap_id(&state.config, ap_id) {
        return Err(FetchError::NotFound);
    }
    fetch_and_store(state, ap_id).await
}

/// Looks an account up by `acct` handle, resolving it through `WebFinger`
/// and fetching the actor when this instance does not know it yet.
///
/// The stored nickname comes first — a local account, and any remote
/// mirror this instance already keeps under `nick@domain` (decision 7),
/// answers straight from the database, case-insensitively, with a handle
/// carrying this instance's own domain folded to the bare local nickname
/// (see [`get_by_acct`]). Otherwise the [`WebFinger`] resolver finds the
/// actor URL and [`get_or_fetch_by_ap_id`] mirrors it. The fetched actor
/// must then vouch for the handle: its `preferredUsername@host` has to
/// equal `acct` case-insensitively (the Mastodon round trip), or the
/// lookup reports [`FetchError::AcctMismatch`].
///
/// [`WebFinger`]: crate::federation::webfinger_client
///
/// # Errors
///
/// [`FetchError::Webfinger`] when the handle cannot be resolved,
/// [`FetchError::AcctMismatch`] when the resolved actor denies it, and
/// otherwise whatever [`get_or_fetch_by_ap_id`] reports.
pub async fn get_or_fetch_by_acct(state: &AppState, acct: &str) -> Result<User, FetchError> {
    get_or_fetch_by_acct_with_base(state, acct, None).await
}

/// [`get_or_fetch_by_acct`] with the `WebFinger` origin replaced by
/// `webfinger_base` (`scheme://authority`) when given. `WebFinger` is
/// HTTPS-only by RFC 7033 and resolves against the handle's domain, so
/// only [`resolve_acct_with_base`]'s test override can point it at a
/// loopback mock; production passes [`None`] and this override keeps the
/// account tests able to drive the same full round trip.
async fn get_or_fetch_by_acct_with_base(
    state: &AppState,
    acct: &str,
    webfinger_base: Option<&str>,
) -> Result<User, FetchError> {
    if let Some(stored) = get_by_acct(&state.db, &state.config, acct)
        .await
        .map_err(|err| server_error(&err))?
    {
        return Ok(stored);
    }
    let ap_id = resolve_acct_with_base(&state.http_client, acct, webfinger_base).await?;
    let user = get_or_fetch_by_ap_id(state, &ap_id).await?;
    if !acct_matches(&user.nickname, acct) {
        return Err(FetchError::AcctMismatch);
    }
    Ok(user)
}

/// Refetches `user`'s actor document and updates the mirror right away,
/// ignoring the 24-hour freshness window (Pleroma's
/// `UserRefreshWorker` performs the same forced refresh).
///
/// A local user is this instance's own actor: there is nothing to fetch,
/// so its row is simply re-read and returned as stored.
///
/// # Errors
///
/// Whatever [`fetch_actor_json`] or the mirror write reports (no stale
/// fallback: this is the forced refetch), [`FetchError::NotFound`] when
/// the local row vanished.
pub async fn refresh_user(state: &AppState, user: &User) -> Result<User, FetchError> {
    if user.local.get() {
        return get_by_id(&state.db, user.id)
            .await
            .map_err(|err| server_error(&err))?
            .ok_or(FetchError::NotFound);
    }
    fetch_and_store(state, &user.ap_id).await
}

/// `GET`s `url` once and reads the actor document it answers, applying the
/// status, size, content-type and JSON rules of [`fetch_actor_json`].
async fn fetch_document(
    http: &HttpClient,
    url: &str,
    signer: Option<&Signer<'_>>,
) -> Result<Value, FetchError> {
    let signed_request = signer.map(|key| sign_get(url, key)).transpose()?;
    let mut headers = vec![("accept", ACTOR_ACCEPT)];
    if let Some(signed) = &signed_request {
        // Host is sent explicitly so the peer sees exactly the host that
        // was signed (see `HttpClient::signed_get`).
        headers.push(("host", &signed.host));
        headers.push(("date", &signed.date));
        headers.push(("signature", &signed.signature));
    }
    let response = http.get_limited(url, &headers, MAX_BODY_BYTES).await?;
    if response.status == STATUS_GONE {
        return Err(FetchError::Gone);
    }
    if response.status == STATUS_NOT_FOUND {
        return Err(FetchError::NotFound);
    }
    let response = response.error_for_status()?;
    if !declares_json(&response.headers) {
        return Err(FetchError::Invalid(format!(
            "the response from `{url}` does not declare a JSON content type"
        )));
    }
    let document: Value = response.json().map_err(|err| {
        FetchError::Invalid(format!("the body of `{url}` is not parseable JSON: {err}"))
    })?;
    Ok(document)
}

/// Signs the `GET` of `url`, folding a signing failure into
/// [`FetchError::Invalid`]: the request never left this process because it
/// could not be built.
fn sign_get(url: &str, signer: &Signer<'_>) -> Result<SignedHeaders, FetchError> {
    let key = SigningKey {
        key_id: signer.key_id,
        private_pem: signer.private_pem,
    };
    sign_request("GET", url, None, &key, Utc::now())
        .map_err(|err| FetchError::Invalid(format!("could not sign the fetch of `{url}`: {err}")))
}

/// Whether a response declares a JSON body: its `Content-Type` mentions
/// `json` anywhere, case-insensitively (`application/activity+json`,
/// `application/ld+json; profile=...`, `application/json`, ...). An absent
/// or unreadable header does not declare anything and fails.
fn declares_json(headers: &HeaderMap) -> bool {
    headers
        .get(CONTENT_TYPE)
        .and_then(|value| value.to_str().ok())
        .is_some_and(|value| value.to_ascii_lowercase().contains(JSON_MARKER))
}

/// The document's own `id`: every containment rule compares it against the
/// URL the document was fetched from.
fn document_id(document: &Value) -> Result<&str, FetchError> {
    document
        .get("id")
        .and_then(Value::as_str)
        .ok_or_else(|| FetchError::Invalid("the actor document carries no string `id`".to_owned()))
}

/// Answers from the stored mirror when it can (a local row, or a remote
/// one inside the refresh window), refetches otherwise — and on a failed
/// refetch keeps serving the stale mirror, `Gone` excepted.
async fn serve_or_refresh(state: &AppState, stored: User, ap_id: &str) -> Result<User, FetchError> {
    if stored.local.get() || fresh_since(stored.last_fetched_at) {
        return Ok(stored);
    }
    match fetch_and_store(state, ap_id).await {
        Ok(user) => Ok(user),
        // Gone states that the actor is permanently gone; every other
        // failure only says the peer is unreachable or wrong at this
        // moment, so the mirror this instance already has keeps serving.
        Err(FetchError::Gone) => Err(FetchError::Gone),
        Err(err) => {
            debug!(%err, "remote actor refresh failed; serving the stale mirror");
            Ok(stored)
        }
    }
}

/// Dereferences `ap_id`, parses the actor document and stores the mirror
/// row (see [`crate::federation::remote_actor`]).
async fn fetch_and_store(state: &AppState, ap_id: &str) -> Result<User, FetchError> {
    // The instance-actor ticket passes a Signer here; until then remote
    // actors are dereferenced unsigned, like Pleroma's default.
    let document = fetch_actor_json(&state.http_client, ap_id, None).await?;
    let new_user = parse_actor(&document).map_err(FetchError::from)?;
    let user = upsert_remote_user(&state.db, new_user)
        .await
        .map_err(FetchError::from)?;
    Ok(user)
}

/// Whether `stamp` falls inside the [`REFRESH_AFTER`] window; a mirror
/// that was never fetched is not fresh. A stamp in the future (peer or
/// clock skew) counts as fresh rather than triggering a fetch loop.
fn fresh_since(stamp: Option<DateTime<Utc>>) -> bool {
    stamp.is_some_and(|fetched| Utc::now().signed_duration_since(fetched) < REFRESH_AFTER)
}

/// Whether `ap_id` lives on this instance's own origin: ids here are
/// served from this database, never fetched from the network (a lookup
/// that misses is plain [`FetchError::NotFound`]). The comparison is on
/// the parsed URL authority, so a look-alike prefix host cannot pass.
fn is_local_ap_id(config: &Config, ap_id: &str) -> bool {
    let Ok(base) = Url::parse(&config.base_url) else {
        return false;
    };
    Url::parse(ap_id).is_ok_and(|url| url.authority() == base.authority())
}

/// The Mastodon round trip in handle terms: a mirror's nickname *is*
/// `{preferredUsername}@{host of the actor id}` (decision 7), so its
/// equality with the requested `acct` — case folded, `@`/`acct:` prefixes
/// stripped — is what proves the resolved actor vouches for the same
/// account the resolver was asked about.
fn acct_matches(nickname: &str, acct: &str) -> bool {
    nickname.eq_ignore_ascii_case(strip_finger_prefixes(acct))
}

/// Folds a server-side [`AppError`] (the database calls of the lookups)
/// into [`FetchError::Invalid`]: its real cause is logged, and the detail
/// callers may surface stays a fixed message — server-side causes never
/// ride into a client body, per decision 8.
fn server_error(err: &AppError) -> FetchError {
    if matches!(err, AppError::NotFound) {
        return FetchError::NotFound;
    }
    error!(%err, "remote actor lookup failed server-side");
    FetchError::Invalid("could not store or look up the actor".to_owned())
}

#[cfg(test)]
mod tests {
    use std::sync::{
        Arc, Mutex,
        atomic::{AtomicUsize, Ordering},
    };

    use axum::{
        Router,
        extract::State,
        http::{HeaderMap, StatusCode, header},
        response::{IntoResponse as _, Response},
        routing::get,
    };
    use base64::{Engine as _, engine::general_purpose::STANDARD as BASE64};
    use serde_json::json;
    use sqlx::PgPool;
    use tokio::{net::TcpListener, task::JoinHandle};

    use super::*;
    use crate::{
        config::TEST_BASE_URL,
        federation::signature::signing_string,
        models::user::{NewUser, get_by_nickname, insert, local_ap_id},
        uploads::UploaderBackend,
        util::keys::{FIXTURE_PRIVATE_PKCS1_PEM, FIXTURE_PUBLIC_PKCS1_PEM, verify_sha256},
    };

    /// `User-Agent` for the test clients.
    const TEST_UA: &str = "Wired (https://test.example)";
    /// The key id the signed-fetch test presents.
    const TEST_KEY_ID: &str = "https://test.example/actor#main-key";
    /// The content type a federation peer answers an actor document with.
    const ACTOR_CONTENT_TYPE: &str = "application/activity+json";

    /// The state under test: `pool`, the test config and a client whose
    /// SSRF guard is off, so it may reach the loopback mocks.
    fn test_state(pool: PgPool) -> AppState {
        let config = Arc::new(Config::for_tests());
        let hub = crate::streaming::hub::Hub::new(&pool);
        AppState {
            db: pool,
            hub,
            config: Arc::clone(&config),
            http_client: HttpClient::new_for_tests(TEST_UA),
            uploader: UploaderBackend::from(config.as_ref()),
            instance_stats_cache: Arc::default(),
            mailer: Arc::new(crate::mailer::MemoryTransport::new()),
        }
    }

    /// The test-mode client (guard off, so `127.0.0.1` mocks are allowed).
    fn client() -> HttpClient {
        HttpClient::new_for_tests(TEST_UA)
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

    /// A minimal actor document for `id`, the way a Mastodon peer serves
    /// one: everything [`parse_actor`] requires is present.
    fn actor_document(id: &str) -> Value {
        json!({
            "@context": "https://www.w3.org/ns/activitystreams",
            "type": "Person",
            "id": id,
            "preferredUsername": "alice",
            "name": "First",
            "inbox": format!("{id}/inbox"),
            "publicKey": {
                "id": format!("{id}#main-key"),
                "owner": id,
                "publicKeyPem": "-----BEGIN PUBLIC KEY-----\nPEM\n-----END PUBLIC KEY-----\n",
            },
        })
    }

    /// A body labelled [`ACTOR_CONTENT_TYPE`].
    fn json_response(document: &Value) -> Response {
        ([(CONTENT_TYPE, ACTOR_CONTENT_TYPE)], document.to_string()).into_response()
    }

    /// The state every mock-peer handler runs with: its own origin plus
    /// the counters and captures the test asserts on.
    #[derive(Clone, Debug)]
    struct Peer {
        /// The `http://{addr}` origin of this mock server.
        base: String,
        /// Requests this peer answered, across all its routes.
        hits: Arc<AtomicUsize>,
        /// The headers of the most recent request, when one was recorded.
        captured: Arc<Mutex<Option<HeaderMap>>>,
    }

    /// A mock peer at `base` with zeroed counters.
    fn peer_at(base: &str) -> Peer {
        Peer {
            base: base.to_owned(),
            hits: Arc::new(AtomicUsize::new(0)),
            captured: Arc::new(Mutex::new(None)),
        }
    }

    /// Counts the hit and answers the self-identifying actor document.
    async fn self_actor(State(peer): State<Peer>) -> Response {
        let _hits = peer.hits.fetch_add(1, Ordering::SeqCst);
        let id = format!("{}/users/alice", peer.base);
        json_response(&actor_document(&id))
    }

    /// Like [`self_actor`], but every answer after the first carries a
    /// changed display name, so tests can tell a refetch from a mirror.
    async fn versioned_actor(State(peer): State<Peer>) -> Response {
        let hits = peer.hits.fetch_add(1, Ordering::SeqCst);
        let id = format!("{}/users/alice", peer.base);
        let mut document = actor_document(&id);
        if hits > 0 {
            document["name"] = json!("Second");
        }
        json_response(&document)
    }

    /// Like [`self_actor`], but first recording the request headers for
    /// the signature assertions.
    async fn self_actor_recording(State(peer): State<Peer>, headers: HeaderMap) -> Response {
        let _previous = peer.captured.lock().unwrap().replace(headers.clone());
        self_actor(State(peer)).await
    }

    /// Answers the self-identifying document only when the request asked
    /// with the exact [`ACTOR_ACCEPT`], so reaching the document proves
    /// the header.
    async fn self_actor_checking_accept(State(peer): State<Peer>, headers: HeaderMap) -> Response {
        if headers
            .get(header::ACCEPT)
            .and_then(|value| value.to_str().ok())
            .is_some_and(|accept| accept == ACTOR_ACCEPT)
        {
            return self_actor(State(peer)).await;
        }
        StatusCode::NOT_ACCEPTABLE.into_response()
    }

    /// Answer with the configured status.
    async fn status_only(State(status): State<StatusCode>) -> StatusCode {
        status
    }

    /// The actor document's `id` moved to `{base}/moved/alice`.
    async fn moved_actor(State(peer): State<Peer>) -> Response {
        let _hits = peer.hits.fetch_add(1, Ordering::SeqCst);
        let id = format!("{}/moved/alice", peer.base);
        json_response(&actor_document(&id))
    }

    /// The document at `{base}/moved/alice` naming a third id: the claim
    /// keeps moving and containment must reject it.
    async fn moving_on_actor(State(peer): State<Peer>) -> Response {
        let _hits = peer.hits.fetch_add(1, Ordering::SeqCst);
        let id = format!("{}/elsewhere/alice", peer.base);
        json_response(&actor_document(&id))
    }

    /// The document at `{base}/moved/alice` that names itself: the
    /// refetched anchor a moved document must land on.
    async fn moved_home_actor(State(peer): State<Peer>) -> Response {
        let _hits = peer.hits.fetch_add(1, Ordering::SeqCst);
        let id = format!("{}/moved/alice", peer.base);
        json_response(&actor_document(&id))
    }

    /// The `/users/alice` document a `/actor` request is redirected to,
    /// naming the URL the client asked for originally.
    async fn redirect_target_actor(State(peer): State<Peer>) -> Response {
        let _hits = peer.hits.fetch_add(1, Ordering::SeqCst);
        let id = format!("{}/actor", peer.base);
        json_response(&actor_document(&id))
    }

    /// A 301 to this peer's own `/users/alice`.
    async fn hop(State(peer): State<Peer>) -> Response {
        Response::builder()
            .status(StatusCode::MOVED_PERMANENTLY)
            .header(header::LOCATION, format!("{}/users/alice", peer.base))
            .body(axum::body::Body::empty())
            .unwrap()
    }

    /// Moves a stored mirror's fetch stamp outside the refresh window.
    async fn make_stale(pool: &PgPool, ap_id: &str) {
        sqlx::query(
            "UPDATE users SET last_fetched_at = now() - interval '25 hours' WHERE ap_id = $1",
        )
        .bind(ap_id)
        .execute(pool)
        .await
        .map(drop)
        .unwrap();
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

    #[tokio::test]
    async fn fetches_a_self_identifying_document_asking_with_the_ap_accept() {
        let listener = bind().await;
        let base = origin(&listener);
        let peer = peer_at(&base);
        let hits = Arc::clone(&peer.hits);
        let server = spawn(
            listener,
            Router::new()
                .route("/users/alice", get(self_actor_checking_accept))
                .with_state(peer),
        );

        let document = fetch_actor_json(&client(), &format!("{base}/users/alice"), None)
            .await
            .unwrap();

        assert_eq!(
            document["id"],
            format!("{base}/users/alice"),
            "the document whose id names the requested URL"
        );
        assert_eq!(hits.load(Ordering::SeqCst), 1, "one request answered");
        server.abort();
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
            key_id: TEST_KEY_ID,
            private_pem: FIXTURE_PRIVATE_PKCS1_PEM,
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

    /// A peer on a fresh loopback port answering every `/actor` request
    /// with `status`; returns the error the fetch reported.
    async fn fetch_failing(status: StatusCode) -> FetchError {
        let listener = bind().await;
        let url = format!("{}/actor", origin(&listener));
        let server = spawn(
            listener,
            Router::new()
                .route("/actor", get(status_only))
                .with_state(status),
        );
        let error = fetch_actor_json(&client(), &url, None).await.unwrap_err();
        server.abort();
        error
    }

    /// The `Invalid` detail of `error`, panicking on any other variant.
    fn invalid_detail(error: FetchError) -> String {
        match error {
            FetchError::Invalid(detail) => detail,
            FetchError::Http(_)
            | FetchError::Gone
            | FetchError::NotFound
            | FetchError::AcctMismatch
            | FetchError::Webfinger(_) => {
                panic!("an actor fetch must be rejected as Invalid, got {error:?}")
            }
        }
    }

    #[tokio::test]
    async fn a_410_answer_is_gone() {
        let error = fetch_failing(StatusCode::GONE).await;
        assert!(
            matches!(error, FetchError::Gone),
            "a 410 states the actor is gone: {error:?}"
        );
    }

    #[tokio::test]
    async fn a_404_answer_is_not_found() {
        let error = fetch_failing(StatusCode::NOT_FOUND).await;
        assert!(
            matches!(error, FetchError::NotFound),
            "a 404 states the actor is missing: {error:?}"
        );
    }

    #[tokio::test]
    async fn other_error_statuses_arrive_as_http_errors() {
        let error = fetch_failing(StatusCode::INTERNAL_SERVER_ERROR).await;
        assert!(
            matches!(error, FetchError::Http(HttpError::Status(500))),
            "a plain 5xx keeps its status: {error:?}"
        );
    }

    #[tokio::test]
    async fn bodies_that_are_not_actor_json_are_invalid() {
        /// A real actor document under an HTML content type.
        async fn html_document() -> Response {
            (
                [(CONTENT_TYPE, "text/html; charset=utf-8")],
                r#"{"id":"https://test.example/users/alice"}"#,
            )
                .into_response()
        }
        /// A JSON content type over bytes that do not parse.
        async fn broken_json() -> Response {
            ([(CONTENT_TYPE, ACTOR_CONTENT_TYPE)], "{oops").into_response()
        }
        /// Parseable JSON with no string `id`.
        async fn no_id() -> Response {
            ([(CONTENT_TYPE, ACTOR_CONTENT_TYPE)], r#"{"type":"Person"}"#).into_response()
        }

        for (what, app) in [
            (
                "an HTML content type",
                Router::new().route("/actor", get(html_document)),
            ),
            (
                "a body that is not JSON",
                Router::new().route("/actor", get(broken_json)),
            ),
            (
                "an object without an id",
                Router::new().route("/actor", get(no_id)),
            ),
        ] {
            let listener = bind().await;
            let url = format!("{}/actor", origin(&listener));
            let server = spawn(listener, app);
            let error = fetch_actor_json(&client(), &url, None).await.unwrap_err();
            let detail = invalid_detail(error);
            assert!(
                !detail.is_empty(),
                "{what} reports an Invalid carrying its cause"
            );
            server.abort();
        }
    }

    #[tokio::test]
    async fn a_body_over_the_one_mebibyte_cap_is_too_large() {
        /// A JSON document padded past [`MAX_BODY_BYTES`].
        async fn huge() -> Response {
            let padding = "x".repeat(MAX_BODY_BYTES);
            let body = format!(r#"{{"id":"https://test.example/users/alice","p":"{padding}"}}"#);
            ([(CONTENT_TYPE, ACTOR_CONTENT_TYPE)], body).into_response()
        }

        let listener = bind().await;
        let url = format!("{}/actor", origin(&listener));
        let server = spawn(listener, Router::new().route("/actor", get(huge)));

        let error = fetch_actor_json(&client(), &url, None).await.unwrap_err();

        assert!(
            matches!(error, FetchError::Http(HttpError::TooLarge)),
            "the 1 MiB cap stops an endless document: {error:?}"
        );
        server.abort();
    }

    #[tokio::test]
    async fn redirects_are_followed_and_the_original_id_is_accepted() {
        let listener = bind().await;
        let base = origin(&listener);
        let peer = peer_at(&base);
        let hits = Arc::clone(&peer.hits);
        let app = Router::new()
            .route("/actor", get(hop))
            .with_state(peer.clone())
            .merge(
                Router::new()
                    .route("/users/alice", get(redirect_target_actor))
                    .with_state(peer),
            );
        let server = spawn(listener, app);

        let document = fetch_actor_json(&client(), &format!("{base}/actor"), None)
            .await
            .unwrap();

        assert_eq!(
            document["id"],
            format!("{base}/actor"),
            "the document names the URL that was requested (through the hop)"
        );
        assert_eq!(
            hits.load(Ordering::SeqCst),
            1,
            "the hop plus the target, once each"
        );
        server.abort();
    }

    #[tokio::test]
    async fn a_moved_document_is_refetched_once_and_accepted() {
        let listener = bind().await;
        let base = origin(&listener);
        let peer = peer_at(&base);
        let hits = Arc::clone(&peer.hits);
        let app = Router::new()
            .route("/users/alice", get(moved_actor))
            .with_state(peer.clone())
            .merge(
                Router::new()
                    .route("/moved/alice", get(moved_home_actor))
                    .with_state(peer),
            );
        let server = spawn(listener, app);

        let document = fetch_actor_json(&client(), &format!("{base}/users/alice"), None)
            .await
            .unwrap();

        assert_eq!(
            document["id"],
            format!("{base}/moved/alice"),
            "the document is re-anchored to the id it claims"
        );
        assert_eq!(
            hits.load(Ordering::SeqCst),
            2,
            "the original URL and the claim each answered once"
        );
        server.abort();
    }

    #[tokio::test]
    async fn a_document_that_keeps_moving_is_rejected() {
        let listener = bind().await;
        let base = origin(&listener);
        let peer = peer_at(&base);
        let hits = Arc::clone(&peer.hits);
        let app = Router::new()
            .route("/users/alice", get(moved_actor))
            .with_state(peer.clone())
            .merge(
                Router::new()
                    .route("/moved/alice", get(moving_on_actor))
                    .with_state(peer),
            );
        let server = spawn(listener, app);

        let error = fetch_actor_json(&client(), &format!("{base}/users/alice"), None)
            .await
            .unwrap_err();

        match error {
            FetchError::Invalid(detail) => assert!(
                detail.contains("elsewhere"),
                "the rejection names where the id moved on to: {detail}"
            ),
            FetchError::Http(_)
            | FetchError::Gone
            | FetchError::NotFound
            | FetchError::AcctMismatch
            | FetchError::Webfinger(_) => {
                panic!("a moving id must be rejected as Invalid, got {error:?}")
            }
        }
        assert_eq!(
            hits.load(Ordering::SeqCst),
            2,
            "exactly one refetch: the third id is never asked for"
        );
        server.abort();
    }

    #[tokio::test]
    async fn get_or_fetch_by_ap_id_inserts_then_serves_from_the_database() {
        let db = crate::test_db::test_db().await.expect("test database slot");
        let pool = db.pool().clone();
        let listener = bind().await;
        let base = origin(&listener);
        let ap_id = format!("{base}/users/alice");
        let peer = peer_at(&base);
        let hits = Arc::clone(&peer.hits);
        let server = spawn(
            listener,
            Router::new()
                .route("/users/alice", get(versioned_actor))
                .with_state(peer),
        );
        let state = test_state(pool);

        let first = get_or_fetch_by_ap_id(&state, &ap_id).await.unwrap();
        assert_eq!(
            first.nickname, "alice@127.0.0.1",
            "preferredUsername@host of the id (decision 7)"
        );
        assert_eq!(first.ap_id, ap_id, "the id is stored as fetched");
        assert_eq!(first.domain.as_deref(), Some("127.0.0.1"), "the id's host");
        assert!(!first.local.get(), "the row is a remote mirror");
        assert_eq!(first.name, "First", "the document landed");
        assert_eq!(
            first.inbox.as_deref(),
            Some(format!("{ap_id}/inbox").as_str()),
            "the inbox endpoint"
        );
        assert!(first.last_fetched_at.is_some(), "the fetch is stamped");
        assert_eq!(hits.load(Ordering::SeqCst), 1, "one fetch");

        let second = get_or_fetch_by_ap_id(&state, &ap_id).await.unwrap();
        assert_eq!(second.id, first.id, "the same mirror row");
        assert_eq!(second.name, "First", "the database answer, not a refetch");
        assert_eq!(
            hits.load(Ordering::SeqCst),
            1,
            "the second call did not fetch"
        );
        server.abort();
    }

    #[tokio::test]
    async fn a_stale_mirror_is_refetched_and_updated() {
        let db = crate::test_db::test_db().await.expect("test database slot");
        let pool = db.pool().clone();
        let listener = bind().await;
        let base = origin(&listener);
        let ap_id = format!("{base}/users/alice");
        let peer = peer_at(&base);
        let hits = Arc::clone(&peer.hits);
        let server = spawn(
            listener,
            Router::new()
                .route("/users/alice", get(versioned_actor))
                .with_state(peer),
        );
        let state = test_state(pool);

        let first = get_or_fetch_by_ap_id(&state, &ap_id).await.unwrap();
        make_stale(&state.db, &ap_id).await;

        let refreshed = get_or_fetch_by_ap_id(&state, &ap_id).await.unwrap();
        assert_eq!(refreshed.id, first.id, "the existing row is updated");
        assert_eq!(refreshed.name, "Second", "the refetched document landed");
        assert_eq!(
            hits.load(Ordering::SeqCst),
            2,
            "a stale mirror triggers one fetch"
        );

        let again = get_or_fetch_by_ap_id(&state, &ap_id).await.unwrap();
        assert_eq!(again.name, "Second", "the refreshed stamp is fresh again");
        assert_eq!(hits.load(Ordering::SeqCst), 2, "no third fetch");
        server.abort();
    }

    #[tokio::test]
    async fn a_dead_peer_falls_back_to_the_stale_mirror() {
        let db = crate::test_db::test_db().await.expect("test database slot");
        let pool = db.pool().clone();
        let listener = bind().await;
        let dead = format!("http://{}/users/alice", listener.local_addr().unwrap());
        drop(listener);
        let stored = upsert_remote_user(&pool, parse_actor(&actor_document(&dead)).unwrap())
            .await
            .unwrap();
        make_stale(&pool, &dead).await;
        let state = test_state(pool);

        let served = get_or_fetch_by_ap_id(&state, &dead).await.unwrap();

        assert_eq!(served.id, stored.id, "the stale mirror is served");
        assert_eq!(served.name, "First", "the failed refresh overwrote nothing");
    }

    #[tokio::test]
    async fn a_gone_refresh_survives_the_stale_fallback() {
        let db = crate::test_db::test_db().await.expect("test database slot");
        let pool = db.pool().clone();
        let listener = bind().await;
        let base = origin(&listener);
        let ap_id = format!("{base}/users/alice");
        let stored = upsert_remote_user(&pool, parse_actor(&actor_document(&ap_id)).unwrap())
            .await
            .unwrap();
        make_stale(&pool, &ap_id).await;
        let server = spawn(
            listener,
            Router::new()
                .route("/users/alice", get(status_only))
                .with_state(StatusCode::GONE),
        );
        let state = test_state(pool);

        let error = get_or_fetch_by_ap_id(&state, &ap_id).await.unwrap_err();

        assert!(
            matches!(error, FetchError::Gone),
            "a 410 says the actor is gone; the mirror must not hide it: {error:?}"
        );
        let still = get_by_ap_id(&state.db, &ap_id)
            .await
            .unwrap()
            .expect("the mirror row stands");
        assert_eq!(still.id, stored.id, "and it was left untouched");
        server.abort();
    }

    #[tokio::test]
    async fn an_unknown_local_ap_id_is_not_found_without_network() {
        let db = crate::test_db::test_db().await.expect("test database slot");
        let pool = db.pool().clone();
        let state = test_state(pool);
        let ap_id = local_ap_id(TEST_BASE_URL, "nobody");

        let error = get_or_fetch_by_ap_id(&state, &ap_id).await.unwrap_err();

        assert!(
            matches!(error, FetchError::NotFound),
            "a local id this database does not have names nobody: {error:?}"
        );
    }

    #[tokio::test]
    async fn a_local_row_answers_as_stored() {
        let db = crate::test_db::test_db().await.expect("test database slot");
        let pool = db.pool().clone();
        let alice = insert(
            &pool,
            &NewUser::local("alice", &local_ap_id(TEST_BASE_URL, "alice")),
        )
        .await
        .unwrap();
        let state = test_state(pool);

        let found = get_or_fetch_by_ap_id(&state, &alice.ap_id).await.unwrap();

        assert_eq!(found.id, alice.id, "the local row is returned, not fetched");
        assert!(found.local.get(), "and it stays a local row");
    }

    /// The self-identifying document with the required username removed.
    async fn not_an_actor(State(peer): State<Peer>) -> Response {
        let _hits = peer.hits.fetch_add(1, Ordering::SeqCst);
        let mut document = actor_document(&format!("{}/users/alice", peer.base));
        drop(
            document
                .as_object_mut()
                .unwrap()
                .remove("preferredUsername"),
        );
        json_response(&document)
    }

    #[tokio::test]
    async fn a_first_fetch_of_a_document_that_is_not_an_actor_is_invalid() {
        let db = crate::test_db::test_db().await.expect("test database slot");
        let pool = db.pool().clone();
        let listener = bind().await;
        let base = origin(&listener);
        let ap_id = format!("{base}/users/alice");
        let server = spawn(
            listener,
            Router::new()
                .route("/users/alice", get(not_an_actor))
                .with_state(peer_at(&base)),
        );
        let state = test_state(pool);

        let error = get_or_fetch_by_ap_id(&state, &ap_id).await.unwrap_err();

        assert!(
            matches!(error, FetchError::Invalid(_)),
            "a document the parser rejects never becomes a mirror: {error:?}"
        );
        assert!(
            get_by_ap_id(&state.db, &ap_id).await.unwrap().is_none(),
            "and nothing was stored"
        );
        server.abort();
    }

    #[tokio::test]
    async fn a_stored_nickname_answers_without_webfinger() {
        let db = crate::test_db::test_db().await.expect("test database slot");
        let pool = db.pool().clone();
        let stored = insert(
            &pool,
            &NewUser {
                nickname: "alice@mock.example".to_owned(),
                ap_id: "https://mock.example/users/alice".to_owned(),
                domain: Some("mock.example".to_owned()),
                ..NewUser::default()
            },
        )
        .await
        .unwrap();
        let state = test_state(pool);

        // No override, no mock: `mock.example` is never contacted, the
        // citext nickname lookup answers, case folded.
        let found = get_or_fetch_by_acct(&state, "ALICE@MOCK.Example")
            .await
            .unwrap();

        assert_eq!(found.id, stored.id, "the stored row, however spelled");
    }

    /// The JRD pointing at this peer's actor path.
    async fn webfinger(State(body): State<String>) -> Response {
        ([(CONTENT_TYPE, "application/jrd+json")], body).into_response()
    }

    #[tokio::test]
    async fn an_actor_that_denies_the_requested_handle_is_rejected() {
        let db = crate::test_db::test_db().await.expect("test database slot");
        let pool = db.pool().clone();
        let listener = bind().await;
        let base = origin(&listener);
        let peer = peer_at(&base);
        let hits = Arc::clone(&peer.hits);
        let jrd = json!({
            "subject": "acct:alice@mock.example",
            "links": [{
                "rel": "self",
                "type": "application/activity+json",
                "href": format!("{base}/users/alice"),
            }],
        });
        let app = Router::new()
            .route(
                "/.well-known/webfinger",
                get(webfinger).with_state(jrd.to_string()),
            )
            .merge(
                Router::new()
                    .route("/users/alice", get(self_actor))
                    .with_state(peer),
            );
        let server = spawn(listener, app);
        let state = test_state(pool);

        let error = get_or_fetch_by_acct_with_base(&state, "alice@mock.example", Some(&base))
            .await
            .unwrap_err();

        assert!(
            matches!(error, FetchError::AcctMismatch),
            "the actor's own handle is alice@127.0.0.1, not the asked one: {error:?}"
        );
        assert_eq!(
            hits.load(Ordering::SeqCst),
            1,
            "the actor URL came out of the webfinger answer and was fetched once"
        );
        let mirror = get_by_nickname(&state.db, "alice@127.0.0.1")
            .await
            .unwrap()
            .expect("the fetched actor is mirrored under its own handle");
        assert_eq!(mirror.ap_id, format!("{base}/users/alice"), "by its id");
        assert!(
            get_by_nickname(&state.db, "alice@mock.example")
                .await
                .unwrap()
                .is_none(),
            "the claimed handle was never stored"
        );
        server.abort();
    }

    #[tokio::test]
    async fn an_unresolvable_handle_reports_the_webfinger_error() {
        let db = crate::test_db::test_db().await.expect("test database slot");
        let pool = db.pool().clone();
        let state = test_state(pool);

        let error = get_or_fetch_by_acct(&state, "alice@localhost")
            .await
            .unwrap_err();

        assert!(
            matches!(error, FetchError::Webfinger(WebfingerError::InvalidAcct)),
            "a handle the resolver refuses fails before any request: {error:?}"
        );
    }

    #[tokio::test]
    async fn refresh_user_refetches_despite_a_fresh_mirror() {
        let db = crate::test_db::test_db().await.expect("test database slot");
        let pool = db.pool().clone();
        let listener = bind().await;
        let base = origin(&listener);
        let ap_id = format!("{base}/users/alice");
        let peer = peer_at(&base);
        let hits = Arc::clone(&peer.hits);
        let server = spawn(
            listener,
            Router::new()
                .route("/users/alice", get(versioned_actor))
                .with_state(peer),
        );
        let state = test_state(pool);

        let first = get_or_fetch_by_ap_id(&state, &ap_id).await.unwrap();
        assert_eq!(hits.load(Ordering::SeqCst), 1, "the initial fetch");

        let refreshed = refresh_user(&state, &first).await.unwrap();

        assert_eq!(refreshed.id, first.id, "the same row is updated");
        assert_eq!(refreshed.name, "Second", "the forced refetch landed");
        assert_eq!(hits.load(Ordering::SeqCst), 2, "despite the fresh stamp");
        server.abort();
    }

    #[tokio::test]
    async fn refreshing_a_local_user_rereads_the_row() {
        let db = crate::test_db::test_db().await.expect("test database slot");
        let pool = db.pool().clone();
        let alice = insert(
            &pool,
            &NewUser::local("alice", &local_ap_id(TEST_BASE_URL, "alice")),
        )
        .await
        .unwrap();
        let state = test_state(pool);

        let refreshed = refresh_user(&state, &alice).await.unwrap();
        assert_eq!(refreshed.id, alice.id, "a local actor is never fetched");

        sqlx::query("DELETE FROM users WHERE id = $1")
            .bind(alice.id)
            .execute(&state.db)
            .await
            .map(drop)
            .unwrap();
        let error = refresh_user(&state, &alice).await.unwrap_err();
        assert!(
            matches!(error, FetchError::NotFound),
            "a vanished local row reports NotFound: {error:?}"
        );
    }

    #[test]
    fn freshness_is_the_24_hour_window_on_the_fetch_stamp() {
        let at = |hours_ago: i64| Utc::now() - Duration::hours(hours_ago);
        assert!(!fresh_since(None), "never fetched is not fresh");
        assert!(fresh_since(Some(at(23))), "inside the window");
        assert!(!fresh_since(Some(at(25))), "past the window");
        assert!(
            fresh_since(Some(at(-1))),
            "a stamp from the future (clock skew) counts as fresh"
        );
    }

    #[test]
    fn only_this_origin_counts_as_local() {
        let config = Config::for_tests();
        assert!(
            is_local_ap_id(&config, "https://test.example/users/alice"),
            "this instance's actor path"
        );
        assert!(
            is_local_ap_id(&config, "https://TEST.EXAMPLE/activities/x"),
            "origins fold case"
        );
        assert!(
            !is_local_ap_id(&config, "https://test.example.evil/users/alice"),
            "a look-alike prefix host is somebody else"
        );
        assert!(
            !is_local_ap_id(&config, "http://other.example/users/alice"),
            "another origin is another instance"
        );
        assert!(!is_local_ap_id(&config, "not a url"), "junk is junk");
    }

    #[test]
    fn the_round_trip_compares_the_handle_case_insensitively() {
        assert!(acct_matches("alice@remote.example", "alice@remote.example"));
        assert!(acct_matches("Alice@Remote.Example", "ALICE@remote.example"));
        assert!(acct_matches(
            "alice@remote.example",
            "@alice@REMOTE.example"
        ));
        assert!(acct_matches(
            "alice@remote.example",
            "acct:alice@remote.example"
        ));
        assert!(!acct_matches("bob@remote.example", "alice@remote.example"));
        assert!(!acct_matches("alice@other.example", "alice@remote.example"));
    }

    #[test]
    fn fetch_errors_render_their_causes() {
        assert_eq!(FetchError::Gone.to_string(), "the actor is gone", "Gone");
        assert_eq!(
            FetchError::NotFound.to_string(),
            "the actor was not found",
            "NotFound"
        );
        assert_eq!(
            FetchError::Invalid("bad doc".to_owned()).to_string(),
            "bad doc",
            "Invalid passes its detail through"
        );
        assert_eq!(
            FetchError::AcctMismatch.to_string(),
            "the fetched actor does not belong to the requested account",
            "AcctMismatch"
        );
        assert_eq!(
            FetchError::Http(HttpError::Timeout).to_string(),
            "actor fetch failed: HTTP request timed out",
            "Http keeps the transport message"
        );
        assert_eq!(
            FetchError::Webfinger(WebfingerError::Malformed).to_string(),
            "could not resolve the account: webfinger document is malformed",
            "Webfinger keeps the resolver's message"
        );
        assert!(
            Error::source(&FetchError::Http(HttpError::Timeout)).is_some(),
            "Http keeps its source"
        );
        assert!(
            Error::source(&FetchError::Webfinger(WebfingerError::Malformed)).is_some(),
            "and so does Webfinger"
        );
        assert!(
            Error::source(&FetchError::Gone).is_none(),
            "a plain Gone has no source"
        );
    }

    #[tokio::test]
    async fn fetch_errors_map_to_response_statuses() {
        for (error, status) in [
            (FetchError::Gone, StatusCode::NOT_FOUND),
            (FetchError::NotFound, StatusCode::NOT_FOUND),
            (
                FetchError::Invalid("bad".to_owned()),
                StatusCode::UNPROCESSABLE_ENTITY,
            ),
            (FetchError::AcctMismatch, StatusCode::UNPROCESSABLE_ENTITY),
            (
                FetchError::Http(HttpError::Timeout),
                StatusCode::BAD_GATEWAY,
            ),
            (
                FetchError::Webfinger(WebfingerError::InvalidAcct),
                StatusCode::UNPROCESSABLE_ENTITY,
            ),
            (
                FetchError::Webfinger(WebfingerError::Http(HttpError::Status(404))),
                StatusCode::BAD_GATEWAY,
            ),
        ] {
            let shown = format!("{error:?}");
            let response = AppError::from(error).into_response();
            assert_eq!(response.status(), status, "status of {shown}");
        }
    }
}
