//! The quoted author's answers to a `QuoteRequest` this instance sent
//! (FEP-044f): an `Accept` that names the `QuoteAuthorization`, a `Reject`,
//! and the later `Delete` of the authorization.
//!
//! The reference is Mastodon's `AcceptQuoteRequestSerializer` and
//! `RejectQuoteRequestSerializer`: the answer embeds the request (or names
//! it by id), and the `Accept` carries the authorization address as its
//! `result`. The answers ride the plain `Accept`, `Reject` and `Delete`
//! routes, which hand the activity to `fold_accept`, `fold_reject` and
//! `fold_revocation` first; each answers `None` for an activity that does
//! not concern a quote request, so the follow handshake and the status
//! withdrawal keep their routes.
//!
//! Only the author of the quoted post may answer: an answer signed by
//! anybody else is ignored. A grant also has to live on the author's own
//! host, as Mastodon's verifier requires. Whenever a grant appears or
//! disappears, the quoting note is re-published as an `Update`, so the
//! peers that hold it see (or lose) its `quoteAuthorization`.

use serde_json::Value;
use tracing::debug;

use crate::{
    federation::{
        dispatch::{ActivityView, FederationError, Outcome, TransientExt as _},
        json,
    },
    models::{
        quote_request::{self, QuoteRequest, QuoteRequestState},
        status,
        user::User,
    },
    services::federation_out,
    state::AppState,
};

#[cfg(test)]
mod tests;

/// The type of the activity an answer embeds.
const QUOTE_REQUEST: &str = "QuoteRequest";

/// The request the answer `view` names, with how the answer spoke of it.
enum Answered {
    /// A request this instance sent.
    Ours(Box<QuoteRequest>),
    /// An embedded `QuoteRequest` this instance never sent.
    Unknown,
    /// Not a `QuoteRequest` at all: the follow handshake's business.
    Other,
}

/// Which request, if any, the `Accept` or `Reject` `view` answers: its
/// `object` is the request's id, or the request itself, embedded.
async fn answered_request(
    state: &AppState,
    view: &ActivityView,
) -> Result<Answered, FederationError> {
    let found = match view.object.id() {
        Some(id) => quote_request::by_request_ap_id(&state.db, id)
            .await
            .transient("could not read the quote request")?,
        None => None,
    };
    if let Some(row) = found {
        return Ok(Answered::Ours(Box::new(row)));
    }
    let embedded_type = view
        .object
        .embedded()
        .and_then(|object| object.get("type"))
        .and_then(Value::as_str);
    Ok(if embedded_type == Some(QUOTE_REQUEST) {
        Answered::Unknown
    } else {
        Answered::Other
    })
}

/// The authorization address the `Accept` `view` grants: its `result`,
/// provided it lives on `signer`'s own host, as Mastodon's verifier
/// requires of an authorization.
fn granted_authorization(view: &ActivityView, signer: &User) -> Option<String> {
    let result = view.raw.get("result").and_then(json::value_or_id)?;
    let host = json::http_host(&result)?;
    (json::http_host(&signer.ap_id)? == host).then_some(result)
}

/// Folds the `Accept` `view` when it answers a quote request.
///
/// The grant is stored and the quoting note re-published with it. `None`
/// when the `Accept` is about something else.
///
/// # Errors
///
/// [`FederationError::Transient`] for a read or write the retry may
/// outlive; everything the shape or the standing refuses is
/// [`Outcome::Ignored`].
pub(super) async fn fold_accept(
    state: &AppState,
    view: &ActivityView,
    signer: &User,
) -> Result<Option<Outcome>, FederationError> {
    let row = match answered_request(state, view).await? {
        Answered::Ours(row) => row,
        Answered::Unknown => {
            return Ok(Some(Outcome::Ignored(
                "an Accept of a QuoteRequest this instance never sent",
            )));
        }
        Answered::Other => return Ok(None),
    };
    if row.quoted_user_id != signer.id {
        return Ok(Some(Outcome::Ignored(
            "an Accept of a quote request from an account that is not the quoted author",
        )));
    }
    let Some(authorization) = granted_authorization(view, signer) else {
        return Ok(Some(Outcome::Ignored(
            "an Accept of a quote request with no authorization on the quoted author's host",
        )));
    };
    if row.state() == QuoteRequestState::Accepted
        && row.authorization_uri.as_deref() == Some(authorization.as_str())
    {
        return Ok(Some(Outcome::Ignored("an Accept that restates the grant")));
    }
    quote_request::accept(&state.db, row.id, &authorization)
        .await
        .transient("could not store the quote grant")?;
    debug!(quoting = %row.quoting_status_id, "a quote request was granted");
    republish(state, &row).await?;
    Ok(Some(Outcome::Handled))
}

/// Folds the `Reject` `view` when it answers a quote request.
///
/// The quote is marked rejected and the post stays. `None` when the
/// `Reject` is about something else.
///
/// # Errors
///
/// Those of [`fold_accept`].
pub(super) async fn fold_reject(
    state: &AppState,
    view: &ActivityView,
    signer: &User,
) -> Result<Option<Outcome>, FederationError> {
    let row = match answered_request(state, view).await? {
        Answered::Ours(row) => row,
        Answered::Unknown => {
            return Ok(Some(Outcome::Ignored(
                "a Reject of a QuoteRequest this instance never sent",
            )));
        }
        Answered::Other => return Ok(None),
    };
    if row.quoted_user_id != signer.id {
        return Ok(Some(Outcome::Ignored(
            "a Reject of a quote request from an account that is not the quoted author",
        )));
    }
    withdraw(state, &row, QuoteRequestState::Rejected)
        .await
        .map(Some)
}

/// Folds the `Delete` of `object_id` when it names a quote authorization.
///
/// The quote is marked revoked and the quoting note re-published without
/// the grant. `None` when `object_id` is no authorization of ours.
///
/// # Errors
///
/// Those of [`fold_accept`].
pub(super) async fn fold_revocation(
    state: &AppState,
    object_id: &str,
    signer: &User,
) -> Result<Option<Outcome>, FederationError> {
    let found = quote_request::by_authorization_uri(&state.db, object_id)
        .await
        .transient("could not read the quote grant")?;
    let Some(row) = found else {
        return Ok(None);
    };
    if row.quoted_user_id != signer.id {
        return Ok(Some(Outcome::Ignored(
            "a Delete of a quote authorization by an account that did not grant it",
        )));
    }
    withdraw(state, &row, QuoteRequestState::Revoked)
        .await
        .map(Some)
}

/// Moves `row` to the refusal `next` (`rejected` or `revoked`).
///
/// A grant the row held is withdrawn from the quoting note by a re-publish;
/// a request that already stands in `next` is left alone.
async fn withdraw(
    state: &AppState,
    row: &QuoteRequest,
    next: QuoteRequestState,
) -> Result<Outcome, FederationError> {
    if row.state() == next {
        return Ok(Outcome::Ignored(
            "an answer that restates the state the quote already has",
        ));
    }
    quote_request::set_state(&state.db, row.id, next)
        .await
        .transient("could not store the quote answer")?;
    debug!(quoting = %row.quoting_status_id, state = next.as_str(), "a quote request was answered");
    if row.state() == QuoteRequestState::Accepted {
        republish(state, row).await?;
    }
    Ok(Outcome::Handled)
}

/// Re-publishes the quoting note of `row` as an `Update`, which renders the
/// authorization the row holds now (or none). A quoting status that is gone
/// has nothing to announce.
async fn republish(state: &AppState, row: &QuoteRequest) -> Result<(), FederationError> {
    let quoting = status::get_by_id(&state.db, row.quoting_status_id)
        .await
        .transient("could not read the quoting status")?;
    if let Some(quoting) = quoting {
        federation_out::after_status_edited(state, &quoting).await;
    }
    Ok(())
}
