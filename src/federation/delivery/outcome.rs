//! Reading a peer's answer: [`delivery_outcome`] classifies one status into
//! the job's outcome, [`transport_error`] does the same for a request that
//! never turned into an answer, and the two `note_` functions keep the
//! [`instances`] reachability ledger straight about what the answer said.
//!
//! [`instances`]: crate::models::instance

use std::time::Duration;

use chrono::Utc;
use reqwest::header::{HeaderMap, RETRY_AFTER};
use tracing::warn;

use super::{FIRST_ATTEMPT, MAX_RETRY_AFTER, STATUS_GONE};
use crate::{
    federation::json,
    http_client::HttpError,
    jobs::{AttemptInfo, JobError},
    models::instance,
    state::AppState,
};

/// Classifies a peer's answer into the job's outcome.
pub(super) fn delivery_outcome(
    status: u16,
    headers: &HeaderMap,
    attempt: AttemptInfo,
) -> Result<(), JobError> {
    let detail = format!("peer answered {status}");
    if (200..=299).contains(&status) {
        return Ok(());
    }
    if is_permanent_refusal(status, attempt) {
        return Err(JobError::Discard(format!("{detail} for good")));
    }
    Err(retryable(&detail, headers))
}

/// Whether the peer's answer says this inbox can never take the activity: a
/// `4xx` that is not a transient one (`408` request timeout, `429` too many
/// requests), and not a first `401`/`403`, which is worth one retry because
/// the actor's key may have rotated since the peer last saw it. Everything
/// else — `3xx`, `408`, `429`, `5xx` — says only *not now*.
fn is_permanent_refusal(status: u16, attempt: AttemptInfo) -> bool {
    let refused = (400..=499).contains(&status) && !matches!(status, 408 | 429);
    let refreshing_key = matches!(status, 401 | 403) && attempt.attempts <= FIRST_ATTEMPT;
    refused && !refreshing_key
}

/// The retryable failure for `detail`: at the peer's own moment when it
/// asked for one, else on the queue's backoff.
fn retryable(detail: &str, headers: &HeaderMap) -> JobError {
    retry_after(headers).map_or_else(
        || JobError::Retry(detail.to_owned()),
        |delay| {
            JobError::RetryAfter(
                delay,
                format!("{detail}, the peer asked to retry in {}s", delay.as_secs()),
            )
        },
    )
}

/// The delay a peer asked for in `Retry-After`, in the delta-seconds spelling
/// only (the HTTP-date form is nowhere in fediverse delivery), capped at
/// [`MAX_RETRY_AFTER`]. `None` when the header is absent, unreadable or not a
/// whole number of seconds.
fn retry_after(headers: &HeaderMap) -> Option<Duration> {
    let seconds = headers
        .get(RETRY_AFTER)
        .and_then(|value| value.to_str().ok())?
        .trim()
        .parse::<u64>()
        .ok()?;
    Some(Duration::from_secs(seconds.min(MAX_RETRY_AFTER.as_secs())))
}

/// The job outcome of a request that never turned into an answer.
///
/// A peer that is down, slow or gone mid-exchange may answer the next
/// attempt, so those failures retry. A failure the address itself causes —
/// an inbox that is not a deliverable URL, or one the SSRF guard refuses
/// because it resolves to a private or reserved address — cannot be fixed by
/// trying again, so it discards.
pub(super) fn transport_error(err: &HttpError) -> JobError {
    match err {
        HttpError::InvalidUrl | HttpError::Blocked(_) => JobError::Discard(err.to_string()),
        // `Status` and `TooLarge` cannot come out of a delivery `POST`
        // (statuses are classified by `delivery_outcome` and the response
        // body is uncapped), so they are treated like the transport
        // hiccups they would be if they ever did.
        HttpError::Timeout | HttpError::Connect(_) | HttpError::Status(_) | HttpError::TooLarge => {
            JobError::Retry(err.to_string())
        }
    }
}

/// The ledger bookkeeping of a request that never turned into an answer: a
/// refused connection or an elapsed timeout is the host being down, so
/// [`instance::set_unreachable`] marks it on any attempt; the failures the
/// address itself causes ([`HttpError::InvalidUrl`], [`HttpError::Blocked`])
/// and the ones a delivery `POST` does not produce say nothing about the
/// host. A write that fails is a lost ledger entry: logged, never the
/// job's outcome, which [`transport_error`] still decides alone.
pub(super) async fn note_transport_failure(state: &AppState, inbox: &str, err: &HttpError) {
    if !matches!(err, HttpError::Connect(_) | HttpError::Timeout) {
        return;
    }
    let Some(host) = json::http_host(inbox) else {
        return;
    };
    if let Err(write_err) = instance::set_unreachable(&state.db, &host, Utc::now()).await {
        warn!(host = %host, %write_err, "could not mark the delivery target unreachable");
    }
}

/// The ledger bookkeeping of a peer's answer, per its status: any `2xx` is a
/// successful outbound exchange — [`instance::touch`] first-sights the host
/// and [`instance::set_reachable`] clears any mark there; a `5xx` on the
/// attempt that spends [`MAX_ATTEMPTS`] marks the host unreachable (a `5xx`
/// with retries still left says only *not now*, and every other answer —
/// the `4xx` refusals other than [`STATUS_GONE`], `3xx`, `408`, `429` —
/// says nothing); and a [`STATUS_GONE`] marks it on any attempt, because a
/// peer that answers 410 has declared the address gone for good and no
/// retry or later activity will land there. An inbox whose host no
/// `http(s)` URL gives names is skipped, and a write that fails is logged,
/// never the job's outcome, which [`delivery_outcome`] still
/// decides alone.
pub(super) async fn note_answer(state: &AppState, inbox: &str, status: u16, attempt: AttemptInfo) {
    let delivered = (200..=299).contains(&status);
    let spent_5xx = (500..=599).contains(&status) && attempt.attempts >= attempt.max_attempts;
    let gone = status == STATUS_GONE;
    if !delivered && !spent_5xx && !gone {
        return;
    }
    let Some(host) = json::http_host(inbox) else {
        return;
    };
    if delivered {
        if let Err(err) = instance::touch(&state.db, &host).await {
            warn!(host = %host, %err, "could not record the delivery target's instance");
        }
        if let Err(err) = instance::set_reachable(&state.db, &host).await {
            warn!(host = %host, %err, "could not clear the delivery target's unreachable mark");
        }
    } else if let Err(err) = instance::set_unreachable(&state.db, &host, Utc::now()).await {
        warn!(host = %host, %err, "could not mark the delivery target unreachable");
    }
}

#[cfg(test)]
mod tests {

    use axum::http::StatusCode;

    use super::*;
    use crate::federation::delivery::test_support::{
        INBOX_PATH, LOOPBACK_HOST, actor, answering_inbox, attempt_to, bind, first_attempt,
        has_row, last_attempt, mark_of, origin, outcome_text, retry_after_headers, second_attempt,
        stamp, test_state,
    };

    #[test]
    fn answers_are_classified_by_whether_retrying_can_help() {
        let none = HeaderMap::new();
        for status in [200_u16, 201, 204, 299] {
            assert_eq!(
                delivery_outcome(status, &none, first_attempt()),
                Ok(()),
                "a {status} answer completes the delivery"
            );
        }
        for status in [400_u16, 404, 405, 410, 413, 422, 451] {
            assert!(
                matches!(
                    delivery_outcome(status, &none, first_attempt()),
                    Err(JobError::Discard(_))
                ),
                "a permanent {status} refusal discards"
            );
        }
        for status in [401_u16, 403] {
            assert_eq!(
                delivery_outcome(status, &none, first_attempt()),
                Err(JobError::Retry(format!("peer answered {status}"))),
                "a first {status} is retried for the key refresh"
            );
            assert!(
                matches!(
                    delivery_outcome(status, &none, second_attempt()),
                    Err(JobError::Discard(_))
                ),
                "the same {status} again is a real refusal"
            );
        }
        for status in [302_u16, 408, 429, 500, 502, 503] {
            assert!(
                matches!(
                    delivery_outcome(status, &none, second_attempt()),
                    Err(JobError::Retry(_))
                ),
                "a {status} answer only says not now"
            );
        }
    }

    #[test]
    fn the_peer_retry_after_is_honoured_and_capped() {
        assert_eq!(
            retry_after(&retry_after_headers("7")),
            Some(Duration::from_secs(7)),
            "seven delta-seconds"
        );
        assert_eq!(
            retry_after(&retry_after_headers(" 999999 ")),
            Some(MAX_RETRY_AFTER),
            "a request past six hours is capped at six"
        );
        for value in ["", "soon", "-3", "1.5"] {
            assert_eq!(
                retry_after(&retry_after_headers(value)),
                None,
                "`{value}` is no usable delay"
            );
        }
        assert_eq!(
            retry_after(&HeaderMap::new()),
            None,
            "absent means nothing was asked"
        );
        assert_eq!(
            retryable("peer answered 503", &retry_after_headers("120")),
            JobError::RetryAfter(
                Duration::from_secs(120),
                "peer answered 503, the peer asked to retry in 120s".to_owned()
            ),
            "a delayed answer becomes the peer's own schedule"
        );
        assert_eq!(
            retryable("peer answered 503", &HeaderMap::new()),
            JobError::Retry("peer answered 503".to_owned()),
            "without the header the queue's backoff decides"
        );
    }

    #[test]
    fn unreachable_inboxes_are_told_apart_from_undeliverable_ones() {
        for err in [
            HttpError::Timeout,
            HttpError::Connect("refused".to_owned()),
            HttpError::Status(503),
            HttpError::TooLarge,
        ] {
            assert_eq!(
                transport_error(&err),
                JobError::Retry(err.to_string()),
                "a peer that did not answer may answer next time: {err}"
            );
        }
        for err in [
            HttpError::InvalidUrl,
            HttpError::Blocked("private address".to_owned()),
        ] {
            assert_eq!(
                transport_error(&err),
                JobError::Discard(err.to_string()),
                "an inbox this client can never reach is refused for good: {err}"
            );
        }
    }

    /// A `2xx` answer to a host the ledger never saw creates its row, and
    /// the same answer clears a mark a pre-marked host carries: the
    /// successful outbound exchange is [`instance::touch`] plus
    /// [`instance::set_reachable`].
    #[tokio::test]
    async fn a_2xx_answer_creates_the_row_and_clears_a_pre_mark() {
        let db = crate::test_db::test_db().await.expect("test database slot");
        let pool = db.pool().clone();
        let alice = actor(&pool).await;
        let state = test_state(&pool);
        let (inbox, server) = answering_inbox(StatusCode::ACCEPTED).await;

        attempt_to(&state, &inbox, alice.id, first_attempt())
            .await
            .unwrap();
        assert!(
            has_row(&pool, LOOPBACK_HOST).await,
            "the first successful delivery first-sights the host in the ledger"
        );
        assert_eq!(
            mark_of(&pool, LOOPBACK_HOST).await,
            None,
            "and the row it created carries no mark"
        );

        instance::set_unreachable(&pool, LOOPBACK_HOST, stamp(1))
            .await
            .unwrap();
        attempt_to(&state, &inbox, alice.id, first_attempt())
            .await
            .unwrap();
        assert_eq!(
            mark_of(&pool, LOOPBACK_HOST).await,
            None,
            "the next 2xx clears the mark: the host answered again"
        );
        server.abort();
    }

    /// A connection the host refused marks it unreachable on the very first
    /// attempt — and the job's outcome stays the retry [`transport_error`]
    /// decides.
    #[tokio::test]
    async fn a_refused_connection_marks_the_host_unreachable() {
        let db = crate::test_db::test_db().await.expect("test database slot");
        let pool = db.pool().clone();
        let alice = actor(&pool).await;
        let state = test_state(&pool);
        let listener = bind().await;
        let inbox = format!("{}{INBOX_PATH}", origin(&listener));
        drop(listener);

        let outcome = attempt_to(&state, &inbox, alice.id, first_attempt()).await;

        assert!(
            matches!(outcome, Err(JobError::Retry(_))),
            "a refused connection still retries, got {}",
            outcome_text(&outcome)
        );
        assert!(
            mark_of(&pool, LOOPBACK_HOST).await.is_some(),
            "a host that refused the connection is marked unreachable"
        );
    }

    /// A `500` marks the host only on the attempt that spends the budget;
    /// the same answer while retries are left says only *not now* and
    /// writes nothing, and either way the outcome stays the retry.
    #[tokio::test]
    async fn a_5xx_marks_only_on_the_attempt_that_spends_the_budget() {
        let db = crate::test_db::test_db().await.expect("test database slot");
        let pool = db.pool().clone();
        let alice = actor(&pool).await;
        let state = test_state(&pool);
        let (inbox, server) = answering_inbox(StatusCode::INTERNAL_SERVER_ERROR).await;

        let early = attempt_to(&state, &inbox, alice.id, first_attempt()).await;
        assert!(
            matches!(early, Err(JobError::Retry(_))),
            "a 500 retries, got {}",
            outcome_text(&early)
        );
        assert!(
            !has_row(&pool, LOOPBACK_HOST).await,
            "a 500 with retries left writes nothing to the ledger"
        );

        let last = attempt_to(&state, &inbox, alice.id, last_attempt()).await;
        assert!(
            matches!(last, Err(JobError::Retry(_))),
            "the final 500 keeps the retry outcome, got {}",
            outcome_text(&last)
        );
        assert!(
            mark_of(&pool, LOOPBACK_HOST).await.is_some(),
            "the 500 that spent the budget marks the host"
        );
        server.abort();
    }

    /// A permanent `4xx` refusal that is not `410` — even the final
    /// attempt's — discards the delivery and leaves the host untouched in
    /// the ledger: a refusal answers for one inbox's mood about one
    /// activity, not for the host being gone. (`410 Gone` is the exception
    /// that speaks about the address itself; see
    /// [`a_gone_inbox_discards_the_delivery`].)
    #[tokio::test]
    async fn a_permanent_refusal_leaves_the_host_unmarked() {
        let db = crate::test_db::test_db().await.expect("test database slot");
        let pool = db.pool().clone();
        let alice = actor(&pool).await;
        let state = test_state(&pool);
        let (inbox, server) = answering_inbox(StatusCode::NOT_FOUND).await;

        let outcome = attempt_to(&state, &inbox, alice.id, last_attempt()).await;
        server.abort();

        assert!(
            matches!(outcome, Err(JobError::Discard(_))),
            "a 404 discards, got {}",
            outcome_text(&outcome)
        );
        assert!(
            !has_row(&pool, LOOPBACK_HOST).await,
            "and the refusal wrote no ledger row"
        );
    }

    /// The marking runs through [`instance::set_unreachable`]'s
    /// only-if-null rule: a fresh failure against an already-marked host
    /// keeps the earlier since — the column says when the host *went*
    /// quiet.
    #[tokio::test]
    async fn a_fresh_failure_keeps_an_earlier_mark() {
        let db = crate::test_db::test_db().await.expect("test database slot");
        let pool = db.pool().clone();
        let alice = actor(&pool).await;
        let state = test_state(&pool);
        let went_quiet = stamp(1);
        instance::set_unreachable(&pool, LOOPBACK_HOST, went_quiet)
            .await
            .unwrap();
        let listener = bind().await;
        let inbox = format!("{}{INBOX_PATH}", origin(&listener));
        drop(listener);

        let outcome = attempt_to(&state, &inbox, alice.id, first_attempt()).await;

        assert!(
            matches!(outcome, Err(JobError::Retry(_))),
            "the failed connection still retries, got {}",
            outcome_text(&outcome)
        );
        assert_eq!(
            mark_of(&pool, LOOPBACK_HOST).await,
            Some(went_quiet),
            "the later failure does not move the mark that says when the host went quiet"
        );
    }
}
