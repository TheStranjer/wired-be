//! The answer a local author sends to a remote `QuoteRequest` (FEP-044f):
//! an `Accept` carrying the `QuoteAuthorization` as its `result`, or a
//! `Reject`.
//!
//! The request is the quoter's own activity, embedded in the answer the way
//! the follow handshake embeds its `Follow`, because Mastodon matches the
//! answer to its pending quote by the request id it finds inside.
//!
//! A withdrawal — the `Delete` of the authorization when the quoted status
//! is deleted or the author revokes — is the follow-up
//! [`crate::models::quote_authorization::revoke`] waits for: until it
//! lands, a revoked grant only makes its document answer 410.

use chrono::Utc;
use serde_json::json;

use crate::{
    error::AppResult,
    federation::{
        activities::{AnsweredActivity, accept_quote_json, reject_json},
        publisher::{self, PublishOpts},
    },
    models::user::User,
    state::AppState,
    util::id::new_id,
};

/// The delivered `QuoteRequest` a local author answers.
#[derive(Debug)]
pub struct QuoteRequestAnswer<'a> {
    /// The `QuoteRequest` activity's id, embedded verbatim in the answer.
    pub request_id: &'a str,
    /// The quoter's actor id: the request's `actor` and the answer's sole
    /// recipient.
    pub quoter_ap_id: &'a str,
    /// The object id of the quoted local post.
    pub quoted_uri: &'a str,
    /// The object id of the quoting post, the request's `instrument`.
    pub instrument_uri: &'a str,
    /// The address of the granted `QuoteAuthorization`; `None` refuses the
    /// request.
    pub authorization_uri: Option<&'a str>,
}

/// Answer a remote account's `QuoteRequest` on behalf of `author`: an
/// `Accept` when `answer` carries an authorization, a `Reject` when it does
/// not.
///
/// The document is under a fresh `{BASE_URL}/activities/{uuid}` id
/// (decision 6), with `author` as actor and the quoter as its only
/// recipient; the fan-out runs [`publisher::publish`] right here, `only_to`
/// the quoter, so the answer lands in their inbox alone. Returns how many
/// deliveries were queued: `0` for a quoter nothing can be delivered to and
/// for an instance with federation disabled.
///
/// # Errors
///
/// Any database failure behind the fan-out's recipient reads, or a failed
/// delivery-job insert, arrives as
/// [`AppError::Database`](crate::error::AppError); the dispatcher's retry
/// runs the idempotent fold again.
pub async fn answer_quote_request(
    state: &AppState,
    author: &User,
    answer: &QuoteRequestAnswer<'_>,
) -> AppResult<usize> {
    let request = json!({
        "id": answer.request_id,
        "type": "QuoteRequest",
        "actor": answer.quoter_ap_id,
        "object": answer.quoted_uri,
        "instrument": answer.instrument_uri,
    });
    let published = Utc::now();
    let to = [answer.quoter_ap_id];
    let envelope = AnsweredActivity {
        config: &state.config,
        id: new_id(),
        actor_ap_id: &author.ap_id,
        to: &to,
        cc: &[],
        answered: &request,
        published: &published,
    };
    let document = answer.authorization_uri.map_or_else(
        || reject_json(&envelope),
        |authorization_uri| accept_quote_json(&envelope, authorization_uri),
    );
    let opts = PublishOpts {
        extra_inboxes: Vec::new(),
        only_to: Some(vec![answer.quoter_ap_id.to_owned()]),
    };
    publisher::publish(state, author, &document, opts).await
}

#[cfg(test)]
mod tests {
    use serde_json::Value;

    use super::*;
    use crate::services::federation_out::test_support::{
        app_state, delivery_jobs, local, remote, state_with, unfederated,
    };

    /// The quoted post's object id.
    const QUOTED: &str = "https://test.example/objects/quoted";
    /// The quoting post's object id.
    const QUOTING: &str = "https://remote.example/objects/quoting";
    /// The request's activity id.
    const REQUEST: &str = "https://remote.example/activities/quote-1";

    /// The one delivered document, asserted to be the only delivery.
    async fn only_delivery(pool: &sqlx::PgPool) -> Value {
        let jobs = delivery_jobs(pool).await;
        assert_eq!(jobs.len(), 1, "the answer is delivered once");
        jobs[0]["body"].clone()
    }

    #[tokio::test]
    async fn a_grant_delivers_an_accept_with_the_authorization_result() {
        let db = crate::test_db::test_db().await.expect("test database slot");
        let pool = db.pool().clone();
        let alice = local(&pool, "alice").await;
        let bob = remote(&pool, "bob").await;
        let answer = QuoteRequestAnswer {
            request_id: REQUEST,
            quoter_ap_id: &bob.ap_id,
            quoted_uri: QUOTED,
            instrument_uri: QUOTING,
            authorization_uri: Some("https://test.example/quote_authorizations/q1"),
        };

        let queued = answer_quote_request(&app_state(&pool), &alice, &answer)
            .await
            .expect("the answer queues");

        assert_eq!(queued, 1, "one inbox is reached");
        let body = only_delivery(&pool).await;
        assert_eq!(body["type"], "Accept", "the grant is an Accept");
        assert_eq!(body["actor"], alice.ap_id, "the author answers");
        assert_eq!(
            body["result"], "https://test.example/quote_authorizations/q1",
            "the result is the authorization address"
        );
        assert_eq!(
            body["object"],
            json!({
                "id": REQUEST,
                "type": "QuoteRequest",
                "actor": bob.ap_id,
                "object": QUOTED,
                "instrument": QUOTING,
            }),
            "the request is embedded as received"
        );
    }

    #[tokio::test]
    async fn a_refusal_delivers_a_reject_without_a_result() {
        let db = crate::test_db::test_db().await.expect("test database slot");
        let pool = db.pool().clone();
        let alice = local(&pool, "alice").await;
        let bob = remote(&pool, "bob").await;
        let answer = QuoteRequestAnswer {
            request_id: REQUEST,
            quoter_ap_id: &bob.ap_id,
            quoted_uri: QUOTED,
            instrument_uri: QUOTING,
            authorization_uri: None,
        };

        let queued = answer_quote_request(&app_state(&pool), &alice, &answer)
            .await
            .expect("the answer queues");

        assert_eq!(queued, 1, "one inbox is reached");
        let body = only_delivery(&pool).await;
        assert_eq!(body["type"], "Reject", "the refusal is a Reject");
        assert!(body.get("result").is_none(), "a refusal authorizes nothing");
        assert_eq!(body["object"]["id"], REQUEST, "and names the request");
    }

    #[tokio::test]
    async fn a_disabled_federation_delivers_nothing() {
        let db = crate::test_db::test_db().await.expect("test database slot");
        let pool = db.pool().clone();
        let alice = local(&pool, "alice").await;
        let bob = remote(&pool, "bob").await;
        let answer = QuoteRequestAnswer {
            request_id: REQUEST,
            quoter_ap_id: &bob.ap_id,
            quoted_uri: QUOTED,
            instrument_uri: QUOTING,
            authorization_uri: None,
        };
        let state = state_with(&pool, unfederated());

        let queued = answer_quote_request(&state, &alice, &answer)
            .await
            .expect("the answer is a no-op");

        assert_eq!(queued, 0, "nothing is queued");
        assert!(delivery_jobs(&pool).await.is_empty(), "and nothing is sent");
    }
}
