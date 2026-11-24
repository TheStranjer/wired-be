//! The quote handshake, local side (FEP-044f): the `QuoteRequest` a local
//! author sends when their post quotes a remote one ([`request_quote`]),
//! and the answer a local author sends to a remote `QuoteRequest`
//! ([`answer_quote_request`]): an `Accept` carrying the
//! `QuoteAuthorization` as its `result`, or a `Reject`.
//!
//! Mastodon files a quote with no consent as a pending legacy quote and
//! never shows it, so the quoter asks: the request embeds the quoting note
//! as its `instrument`, and the author's `Accept` (folded by
//! [`crate::federation::handlers::quote_answers`]) brings back the
//! authorization the note then names in `quoteAuthorization`.
//!
//! A remote quoter's request is embedded in our answer the way the follow
//! handshake embeds its `Follow`, because Mastodon matches the answer to
//! its pending quote by the request id it finds inside.
//!
//! A withdrawal — the `Delete` of the authorization when the quoted status
//! is deleted or the author revokes — is the follow-up
//! [`crate::models::quote_authorization::revoke`] waits for: until it
//! lands, a revoked grant only makes its document answer 410.

use chrono::Utc;
use serde_json::json;
use tracing::{debug, warn};

use super::{load_status, load_user, publish_only_to};
use crate::{
    error::AppResult,
    federation::{
        activities::{
            AnsweredActivity, QuoteRequestContext, accept_quote_json, activity_id,
            quote_request_json, reject_json,
        },
        note,
        publisher::{self, PublishOpts},
    },
    models::{
        quote_request::{self, NewQuoteRequest},
        status::Status,
        user::User,
    },
    state::AppState,
    util::id::new_id,
};

/// Ask the author of the remote post that `status` quotes for consent.
///
/// The `QuoteRequest` embeds the quoting note and goes to that author's
/// inbox alone, and a `pending` row remembers it.
///
/// Silence is the answer of a quote of a local post (the instance owns the
/// answer, and a local quoted author is never asked), of a post that is no
/// quote, of a quoted post that is gone, and of a quote that already has
/// its request: a retried hook finds the stored row under another request
/// id and sends nothing twice. Like every lifecycle hook it never fails
/// the caller: a miss is logged.
pub(super) async fn request_quote(state: &AppState, status: &Status, poster: &User) {
    let Some((quoted, quoted_author)) = remote_quoted_post(state, status).await else {
        return;
    };
    let request_ap_id = activity_id(&state.config, new_id());
    if !record_request(state, status, &quoted, &quoted_author, &request_ap_id).await {
        return;
    }
    let Some(instrument) = quoting_note(state, status).await else {
        return;
    };
    let document = quote_request_json(&QuoteRequestContext {
        config: &state.config,
        id: &request_ap_id,
        actor_ap_id: &poster.ap_id,
        quoted_author_ap_id: &quoted_author.ap_id,
        quoted_uri: &quoted.uri,
        instrument: &instrument,
    });
    publish_only_to(
        state,
        poster,
        &quoted_author.ap_id,
        &document,
        "quote request",
    )
    .await;
}

/// The live remote post `status` quotes and its remote author, `None` when
/// `status` quotes nothing, quotes a post of this instance, or either row
/// is gone.
async fn remote_quoted_post(state: &AppState, status: &Status) -> Option<(Status, User)> {
    let quoted = load_status(state, status.quote_of_id?).await?;
    if quoted.local {
        return None;
    }
    let author = load_user(state, quoted.user_id).await?;
    (!author.local.get()).then_some((quoted, author))
}

/// Writes the pending row for the request sent under `request_ap_id`.
///
/// `true` when this call wrote it, `false` for a request that already
/// stands (a retried hook) or a failed write (logged).
async fn record_request(
    state: &AppState,
    status: &Status,
    quoted: &Status,
    quoted_author: &User,
    request_ap_id: &str,
) -> bool {
    let stored = quote_request::insert(
        &state.db,
        &NewQuoteRequest {
            quoting_status_id: status.id,
            quoted_status_id: quoted.id,
            quoted_user_id: quoted_author.id,
            request_uri: request_ap_id,
        },
    )
    .await;
    match stored {
        Ok(row) if row.request_ap_id == request_ap_id => true,
        Ok(_) => {
            debug!(status = %status.id, "the quote request was already sent");
            false
        }
        Err(err) => {
            warn!(%err, status = %status.id, "could not record the quote request");
            false
        }
    }
}

/// The note `status` federates as, `None` (logged by the loader) when it
/// cannot be rendered.
async fn quoting_note(state: &AppState, status: &Status) -> Option<serde_json::Value> {
    match note::load_note_context(&state.db, &state.config, status.id).await {
        Ok(ctx) => note::note_json(&ctx),
        Err(err) => {
            warn!(%err, status = %status.id, "the quote request could not read the note");
            None
        }
    }
}

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
    use crate::{
        config::{Config, TEST_BASE_URL},
        models::{
            quote_request::QuoteRequestState,
            status::{self, NewStatus, Visibility},
        },
        services::federation_out::{
            after_status_created,
            test_support::{
                app_state, delivery_jobs, job_count, local, local_post, remote, remote_status,
                state_with, unfederated,
            },
        },
    };

    /// A public post of `author` quoting `quoted`.
    async fn quoting(pool: &sqlx::PgPool, author: &User, quoted: &Status) -> Status {
        status::insert(
            pool,
            TEST_BASE_URL,
            &NewStatus {
                quote_of_id: Some(quoted.id),
                ..NewStatus::local(author, "quoting")
            },
        )
        .await
        .expect("the quoting post inserts")
    }

    /// How many quote requests are stored.
    async fn stored_requests(pool: &sqlx::PgPool) -> i64 {
        sqlx::query_scalar::<_, i64>("SELECT count(*) FROM quote_requests")
            .fetch_one(pool)
            .await
            .expect("the request count reads")
    }

    /// A quote of a remote post queues one `QuoteRequest` for the quoted
    /// author's inbox, embedding the very note the instance serves, and
    /// stores it as pending under the id it was sent with.
    #[tokio::test]
    async fn a_quote_of_a_remote_post_sends_one_request_to_its_author() {
        let db = crate::test_db::test_db().await.expect("test database slot");
        let pool = db.pool().clone();
        let alice = local(&pool, "alice").await;
        let bob = remote(&pool, "bob").await;
        let quoted = remote_status(&pool, &bob, "quoted-post", Visibility::Public).await;
        let post = quoting(&pool, &alice, &quoted).await;

        after_status_created(&app_state(&pool), &post).await;

        let jobs = delivery_jobs(&pool).await;
        assert_eq!(jobs.len(), 1, "the request is the only direct delivery");
        let body = &jobs[0]["body"];
        assert_eq!(body["type"], "QuoteRequest", "the FEP-044f request");
        assert_eq!(body["actor"], alice.ap_id, "the quoter asks");
        assert_eq!(body["object"], quoted.uri, "about the remote post");
        assert_eq!(body["to"], json!([bob.ap_id]), "addressed to its author");
        assert_eq!(
            jobs[0]["inbox"], "https://remote.example/users/bob/inbox",
            "delivered to the author's own inbox"
        );
        let ctx = note::load_note_context(&pool, &Config::for_tests(), post.id)
            .await
            .expect("the note context reads");
        let served = note::note_json(&ctx).expect("a public quote renders");
        assert_eq!(
            body["instrument"], served,
            "the instrument is the served note"
        );
        assert_eq!(body["instrument"]["quote"], quoted.uri, "and it quotes");
        let id = body["id"].as_str().expect("the request has an id");
        let row = quote_request::by_request_ap_id(&pool, id)
            .await
            .expect("the read runs")
            .expect("the request is remembered under its id");
        assert_eq!(row.state(), QuoteRequestState::Pending, "unanswered");
        assert_eq!(row.quoting_status_id, post.id, "for the quote");
        assert_eq!(row.quoted_user_id, bob.id, "of the quoted author");
    }

    /// A hook that runs twice for one quote sends one request.
    #[tokio::test]
    async fn a_retried_hook_does_not_ask_twice() {
        let db = crate::test_db::test_db().await.expect("test database slot");
        let pool = db.pool().clone();
        let alice = local(&pool, "alice").await;
        let bob = remote(&pool, "bob").await;
        let quoted = remote_status(&pool, &bob, "quoted-twice", Visibility::Public).await;
        let post = quoting(&pool, &alice, &quoted).await;
        let state = app_state(&pool);

        request_quote(&state, &post, &alice).await;
        request_quote(&state, &post, &alice).await;

        assert_eq!(delivery_jobs(&pool).await.len(), 1, "one request is sent");
        assert_eq!(stored_requests(&pool).await, 1, "and one is stored");
    }

    /// Posts that quote nothing, or a post of this instance, ask nobody.
    #[tokio::test]
    async fn only_a_quote_of_a_remote_post_asks_for_consent() {
        let db = crate::test_db::test_db().await.expect("test database slot");
        let pool = db.pool().clone();
        let alice = local(&pool, "alice").await;
        let carol = local(&pool, "carol").await;
        let own = local_post(&pool, &alice, "mine", Visibility::Public).await;
        let theirs = local_post(&pool, &carol, "hers", Visibility::Public).await;
        let plain = local_post(&pool, &alice, "no quote", Visibility::Public).await;
        let self_quote = quoting(&pool, &alice, &own).await;
        let local_quote = quoting(&pool, &alice, &theirs).await;
        let state = app_state(&pool);

        for post in [&plain, &self_quote, &local_quote] {
            request_quote(&state, post, &alice).await;
        }

        assert_eq!(job_count(&pool).await, 0, "nothing is queued");
        assert_eq!(stored_requests(&pool).await, 0, "and nothing is stored");
    }

    /// With federation off the hook asks nobody and remembers nothing.
    #[tokio::test]
    async fn a_disabled_instance_sends_no_request() {
        let db = crate::test_db::test_db().await.expect("test database slot");
        let pool = db.pool().clone();
        let alice = local(&pool, "alice").await;
        let bob = remote(&pool, "bob").await;
        let quoted = remote_status(&pool, &bob, "quoted-off", Visibility::Public).await;
        let post = quoting(&pool, &alice, &quoted).await;

        after_status_created(&state_with(&pool, unfederated()), &post).await;

        assert_eq!(job_count(&pool).await, 0, "no job is queued");
        assert_eq!(stored_requests(&pool).await, 0, "no request is stored");
    }

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
