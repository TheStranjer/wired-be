//! The consuming half of delivery: [`handle`] runs one attempt, and
//! [`deliver`] reads the signing actor, strips the blind recipients, signs
//! the exact bytes it `POST`s and hands the peer's answer to the
//! [`outcome`](super::outcome) module. The address helpers beside them
//! decide what is deliverable and who signs.

use chrono::Utc;
use serde_json::Value;
use tracing::debug;
use url::Url;
use uuid::Uuid;

use super::{
    DeliveryArgs,
    outcome::{delivery_outcome, note_answer, note_transport_failure, transport_error},
};
use crate::{
    error::{AppError, AppResult},
    federation::{
        actor::MAIN_KEY_FRAGMENT,
        signature::{SignedHeaders, SigningKey, sign_request},
        vocab::ACTIVITY_JSON,
    },
    jobs::{AttemptInfo, HandlerFuture, JobError},
    models::user::{self, User},
    state::AppState,
    util::url::same_origin,
};

/// Most response-body bytes a delivery buffers from the peer's inbox: only
/// the status and headers matter, so 64 KiB is ample.
const DELIVERY_RESPONSE_BYTES: usize = 64 * 1024;

/// The [`KIND_DELIVER`] handler, registered by [`crate::serve`]: runs one
/// delivery attempt and reports its outcome to the worker.
///
/// An attempt reads the signing actor, strips the blind recipients, signs the
/// bytes it sends and classifies the peer's answer, as this module's docs
/// describe.
///
/// [`KIND_DELIVER`]: super::KIND_DELIVER
pub fn handle(state: AppState, attempt: AttemptInfo, args: Value) -> HandlerFuture {
    Box::pin(async move { deliver(state, attempt, args).await })
}

/// Runs one delivery attempt: read the signing actor, prepare the activity,
/// sign, `POST`, classify the answer, and keep the reachability ledger of
/// the inbox's host straight about what the answer said, as this module's
/// docs describe.
///
/// Failures are the job's, not the caller's: what cannot ever work (a
/// malformed payload, an actor that is not a local signer, a URL or key that
/// cannot be signed, a peer that refuses for good) is a
/// [`JobError::Discard`], what may work later (a database hiccup, a timeout,
/// a `5xx`, a throttle) a [`JobError::Retry`] or
/// [`JobError::RetryAfter`]. Ledger writes never feed back into any of
/// that: they only log.
async fn deliver(state: AppState, attempt: AttemptInfo, args: Value) -> Result<(), JobError> {
    let args = serde_json::from_value::<DeliveryArgs>(args)
        .map_err(|err| JobError::Discard(format!("malformed delivery arguments: {err}")))?;
    debug!(
        activity_id = %args.activity_id,
        inbox = %args.inbox,
        attempts = attempt.attempts,
        "delivering activity"
    );
    let actor = signing_actor(&state, args.actor_id).await?;
    let private_pem = actor.private_key.as_deref().ok_or_else(|| {
        JobError::Discard(format!(
            "the local actor {} has no private key to sign with",
            actor.ap_id
        ))
    })?;
    let body = serde_json::to_vec(&strip_bto_bcc(&args.body))
        .map_err(|err| JobError::Discard(format!("the activity is not serializable: {err}")))?;
    let key_id = format!("{}{MAIN_KEY_FRAGMENT}", actor.ap_id);
    let signed = sign_request(
        "POST",
        &args.inbox,
        Some(&body),
        &SigningKey {
            key_id: &key_id,
            private_pem,
        },
        Utc::now(),
    )
    .map_err(|err| {
        JobError::Discard(format!("cannot sign the delivery to {}: {err}", args.inbox))
    })?;
    let response = match state
        .http_client
        .post_bytes(
            &args.inbox,
            &delivery_headers(&signed),
            body,
            DELIVERY_RESPONSE_BYTES,
        )
        .await
    {
        Ok(response) => response,
        Err(err) => {
            note_transport_failure(&state, &args.inbox, &err).await;
            return Err(transport_error(&err));
        }
    };
    let outcome = delivery_outcome(response.status, &response.headers, attempt);
    note_answer(&state, &args.inbox, response.status, attempt).await;
    outcome
}

/// Whether `inbox` is served by this very instance.
///
/// Its origin (host, plus the port when it is not the scheme's default) is
/// the origin of the local signing actor's own id, which lives on this
/// instance's origin (`docs/ARCHITECTURE.md` decision 6). Delivering to it
/// would mean asking this process to `POST` an activity to itself.
#[must_use]
pub fn is_local_inbox(inbox: &Url, actor_ap_id: &str) -> bool {
    Url::parse(actor_ap_id).is_ok_and(|id| same_origin(&id, inbox))
}

/// The `ActivityPub` id an activity carries: the identity a delivery is for,
/// and the deduplication key's other half.
pub(super) fn activity_id_of(activity: &Value) -> AppResult<&str> {
    activity
        .get("id")
        .and_then(Value::as_str)
        .ok_or_else(|| AppError::Unprocessable("the activity carries no string `id`".to_owned()))
}

/// Reads the actor to sign as, by id. A row that is gone, or that belongs to
/// another instance, can never sign anything for this one, so those are
/// permanent; a database that will not answer is transient.
async fn signing_actor(state: &AppState, actor_id: Uuid) -> Result<User, JobError> {
    let found = user::get_by_id(&state.db, actor_id)
        .await
        .map_err(|err| JobError::Retry(format!("could not read the signing actor: {err}")))?;
    found
        .filter(|actor| actor.local.get())
        .ok_or_else(|| JobError::Discard(format!("actor {actor_id} is not a local signer")))
}

/// The activity as it goes on the wire: [`bto`] and [`bcc`] removed, because
/// blind recipients are addressed by the sender's own followers list and
/// must never be leaked to the instances that receive the activity (the
/// `PublicAddressing` rule both Mastodon and Pleroma apply before sending).
/// Anything else — including `to` and `cc`, which the signature covers too —
/// is untouched.
///
/// [`bto`]: https://www.w3.org/TR/activitystreams-vocabulary/#dfn-bto
/// [`bcc`]: https://www.w3.org/TR/activitystreams-vocabulary/#dfn-bcc
fn strip_bto_bcc(activity: &Value) -> Value {
    let mut stripped = activity.clone();
    if let Some(object) = stripped.as_object_mut() {
        let _bto = object.remove("bto");
        let _bcc = object.remove("bcc");
    }
    stripped
}

/// The headers of a signed delivery: the activity media type in both
/// directions, then the headers [`sign_request`] signed (see
/// [`SignedHeaders::header_pairs`]), `Host` included so the peer sees
/// exactly the authority that was signed.
fn delivery_headers(signed: &SignedHeaders) -> Vec<(&str, &str)> {
    let mut headers = vec![("content-type", ACTIVITY_JSON), ("accept", ACTIVITY_JSON)];
    headers.extend(signed.header_pairs());
    headers
}

#[cfg(test)]
mod tests {

    use std::time::Duration;

    use axum::{Router, http::StatusCode, routing::post};
    use serde_json::json;

    use super::*;
    use crate::{
        config::TEST_BASE_URL,
        federation::delivery::test_support::{
            INBOX_PATH, LOOPBACK_HOST, activity, actor, attempt_to, bind, first_attempt, mark_of,
            mirror, origin, outcome_text, second_attempt, spawn, status_only, test_state,
            throttled,
        },
        models::user::{NewUser, insert, local_ap_id},
        util::url::parse_http_url,
    };

    #[test]
    fn blind_recipients_are_stripped_and_nothing_else() {
        let stripped = strip_bto_bcc(&activity());
        assert!(
            stripped.get("bto").is_none() && stripped.get("bcc").is_none(),
            "the blind recipient lists are gone: {stripped}"
        );
        assert_eq!(
            stripped["to"],
            json!(["https://www.w3.org/ns/activitystreams#Public"]),
            "the public addressees stay"
        );
        assert_eq!(
            stripped["cc"],
            json!(["https://test.example/users/alice/followers"]),
            "and so do the followers"
        );
        assert_eq!(
            stripped["id"],
            activity()["id"],
            "the document is otherwise untouched"
        );
        let not_an_object = json!("a string is not an object");
        assert_eq!(
            strip_bto_bcc(&not_an_object),
            not_an_object,
            "a body that is not an object passes through"
        );
    }

    #[test]
    fn the_headers_label_the_activity_and_carry_the_signature() {
        let signed = SignedHeaders {
            date: "Tue, 07 Jun 2022 20:51:35 GMT".to_owned(),
            host: "peer.example".to_owned(),
            digest: Some("SHA-256=abc".to_owned()),
            signature: r#"keyId="k",signature="s""#.to_owned(),
        };
        let headers = delivery_headers(&signed);
        assert_eq!(
            headers,
            vec![
                ("content-type", ACTIVITY_JSON),
                ("accept", ACTIVITY_JSON),
                ("host", "peer.example"),
                ("date", "Tue, 07 Jun 2022 20:51:35 GMT"),
                ("digest", "SHA-256=abc"),
                ("signature", r#"keyId="k",signature="s""#),
            ],
            "both media types and the signed values, Host included"
        );
        let bodyless = SignedHeaders {
            digest: None,
            ..signed
        };
        assert!(
            !delivery_headers(&bodyless)
                .iter()
                .any(|(name, _)| *name == "digest"),
            "a signature over no digest sends no Digest header"
        );
    }

    #[test]
    fn only_http_inboxes_are_deliverable() {
        for inbox in [
            "ftp://peer.example/inbox",
            "file:///etc/passwd",
            "not a url",
            "",
        ] {
            assert!(
                parse_http_url(inbox).is_none(),
                "`{inbox}` is not an http(s) inbox"
            );
        }
        for inbox in ["http://peer.example/inbox", "https://peer.example/inbox"] {
            assert!(
                parse_http_url(inbox).is_some(),
                "`{inbox}` is an http(s) inbox"
            );
        }
    }

    #[test]
    fn an_inbox_on_the_actors_own_origin_is_local() {
        let alice = local_ap_id(TEST_BASE_URL, "alice");
        let own = Url::parse("https://test.example/users/bob/inbox").unwrap();
        let lookalike = Url::parse("https://test.example.evil.example/inbox").unwrap();
        let other = Url::parse("https://peer.example/inbox").unwrap();
        let ported = Url::parse("https://test.example:443/inbox").unwrap();
        assert!(is_local_inbox(&own, &alice), "this instance's own origin");
        assert!(
            is_local_inbox(&ported, &alice),
            "the default port spelled out is the same authority"
        );
        assert!(
            !is_local_inbox(&lookalike, &alice),
            "a look-alike prefix host is not this instance"
        );
        assert!(!is_local_inbox(&other, &alice), "another host is not local");
        assert!(
            !is_local_inbox(&own, "not an actor id"),
            "an actor id that is no URL vouches for nothing"
        );
    }

    #[tokio::test]
    async fn a_gone_inbox_discards_the_delivery() {
        let db = crate::test_db::test_db().await.expect("test database slot");
        let pool = db.pool().clone();
        let alice = actor(&pool).await;
        let state = test_state(&pool);
        let listener = bind().await;
        let inbox = format!("{}{INBOX_PATH}", origin(&listener));
        let server = spawn(
            listener,
            Router::new()
                .route(INBOX_PATH, post(status_only))
                .with_state(StatusCode::GONE),
        );

        let outcome = attempt_to(&state, &inbox, alice.id, first_attempt()).await;
        server.abort();

        match outcome {
            Err(JobError::Discard(detail)) => {
                assert!(detail.contains("410"), "the discard names it: {detail}");
            }
            other => panic!(
                "a 410 inbox must discard the delivery, got {}",
                outcome_text(&other)
            ),
        }
        assert!(
            mark_of(&pool, LOOPBACK_HOST).await.is_some(),
            "the 410 also marks the host: an address gone for good closes \
             the ledger entry against every later fan-out"
        );
    }

    #[tokio::test]
    async fn a_server_error_retries_the_delivery() {
        let db = crate::test_db::test_db().await.expect("test database slot");
        let pool = db.pool().clone();
        let alice = actor(&pool).await;
        let state = test_state(&pool);
        let listener = bind().await;
        let inbox = format!("{}{INBOX_PATH}", origin(&listener));
        let server = spawn(
            listener,
            Router::new()
                .route(INBOX_PATH, post(status_only))
                .with_state(StatusCode::INTERNAL_SERVER_ERROR),
        );

        let outcome = attempt_to(&state, &inbox, alice.id, first_attempt()).await;
        server.abort();

        match outcome {
            Err(JobError::Retry(detail)) => {
                assert!(detail.contains("500"), "the retry names it: {detail}");
            }
            other => panic!(
                "a 500 inbox must retry the delivery, got {}",
                outcome_text(&other)
            ),
        }
    }

    #[tokio::test]
    async fn a_first_unauthorized_answer_retries_and_a_second_discards() {
        let db = crate::test_db::test_db().await.expect("test database slot");
        let pool = db.pool().clone();
        let alice = actor(&pool).await;
        let state = test_state(&pool);
        let listener = bind().await;
        let inbox = format!("{}{INBOX_PATH}", origin(&listener));
        let server = spawn(
            listener,
            Router::new()
                .route(INBOX_PATH, post(status_only))
                .with_state(StatusCode::UNAUTHORIZED),
        );

        let first = attempt_to(&state, &inbox, alice.id, first_attempt()).await;
        let second = attempt_to(&state, &inbox, alice.id, second_attempt()).await;
        server.abort();

        assert!(
            matches!(first, Err(JobError::Retry(_))),
            "a 401 the first time may be a key that just rotated, got {}",
            outcome_text(&first)
        );
        assert!(
            matches!(second, Err(JobError::Discard(_))),
            "the same 401 twice is a real refusal, got {}",
            outcome_text(&second)
        );
    }

    #[tokio::test]
    async fn a_throttled_inbox_sets_the_retry_delay() {
        let db = crate::test_db::test_db().await.expect("test database slot");
        let pool = db.pool().clone();
        let alice = actor(&pool).await;
        let state = test_state(&pool);
        let listener = bind().await;
        let inbox = format!("{}{INBOX_PATH}", origin(&listener));
        let server = spawn(
            listener,
            Router::new()
                .route(INBOX_PATH, post(throttled))
                .with_state(7),
        );

        let outcome = attempt_to(&state, &inbox, alice.id, first_attempt()).await;
        server.abort();

        assert_eq!(
            outcome,
            Err(JobError::RetryAfter(
                Duration::from_secs(7),
                "peer answered 429, the peer asked to retry in 7s".to_owned()
            )),
            "the job waits out the peer's own seven seconds"
        );
    }

    #[tokio::test]
    async fn an_unreachable_inbox_retries() {
        let db = crate::test_db::test_db().await.expect("test database slot");
        let pool = db.pool().clone();
        let alice = actor(&pool).await;
        let state = test_state(&pool);
        let listener = bind().await;
        let inbox = format!("{}{INBOX_PATH}", origin(&listener));
        drop(listener);

        let outcome = attempt_to(&state, &inbox, alice.id, first_attempt()).await;

        match outcome {
            Err(JobError::Retry(detail)) => assert!(
                !detail.is_empty(),
                "the retry states the transport failure: {detail}"
            ),
            other => panic!("a dead inbox must retry, got {}", outcome_text(&other)),
        }
    }

    #[tokio::test]
    async fn a_delivery_that_can_never_be_signed_is_discarded() {
        let db = crate::test_db::test_db().await.expect("test database slot");
        let pool = db.pool().clone();
        let alice = actor(&pool).await;
        let state = test_state(&pool);

        let junk_args = handle(state.clone(), first_attempt(), json!({"inbox": "x"})).await;
        assert!(
            matches!(junk_args, Err(JobError::Discard(_))),
            "arguments that do not describe a delivery discard, got {}",
            outcome_text(&junk_args)
        );

        let bad_inbox = attempt_to(&state, "not a url", alice.id, first_attempt()).await;
        assert!(
            matches!(bad_inbox, Err(JobError::Discard(_))),
            "an inbox that cannot be signed discards, got {}",
            outcome_text(&bad_inbox)
        );
    }

    #[tokio::test]
    async fn an_actor_that_cannot_sign_is_discarded() {
        let db = crate::test_db::test_db().await.expect("test database slot");
        let pool = db.pool().clone();
        let remote = mirror(&pool, "bob", None, None).await;
        let keyless = insert(
            &pool,
            &NewUser::local("carol", &local_ap_id(TEST_BASE_URL, "carol")),
        )
        .await
        .unwrap();
        let state = test_state(&pool);
        let inbox = "https://peer.example/inbox";

        for (what, actor_id) in [
            ("a mirror of a remote actor", remote.id),
            ("a local actor with no private key", keyless.id),
            ("a row that is gone", Uuid::now_v7()),
        ] {
            let outcome = attempt_to(&state, inbox, actor_id, first_attempt()).await;
            assert!(
                matches!(outcome, Err(JobError::Discard(_))),
                "{what} can never sign a delivery, got {}",
                outcome_text(&outcome)
            );
        }
    }
}
