//! Inbound `QuoteRequest` (FEP-044f): a remote account asks to quote a
//! local post, and this instance answers with an `Accept` carrying a
//! `QuoteAuthorization`, or with a `Reject`.
//!
//! The reference is Mastodon's `ActivityPub::QuoteRequestSerializer` and
//! `AcceptQuoteRequestSerializer` (`app/serializers/activitypub/`): the
//! request is `{type: QuoteRequest, actor: quoter, object: quoted post,
//! instrument: quoting post}`, and the quoter shows the quote as accepted
//! only after fetching the `result` of our `Accept`. The ticket rules are
//! authoritative where the two differ:
//!
//! * [`handle_quote_request`] takes the activity's `object` as the post
//!   being quoted. It must be a live status this instance owns, by an
//!   active local author; anything else is ignored, since this instance
//!   has no say over somebody else's post.
//! * The `instrument` — embedded or by uri — must be a post of the signer:
//!   an embedded document must name the signer in `attributedTo` and live
//!   on the signer's host (it is then ingested like a delivered `Create`),
//!   a bare uri is fetched on behalf of the signer
//!   ([`fetch_object::resolve_status_as_actor`]). A request whose
//!   instrument is somebody else's, or cannot be read, is ignored and
//!   answered with nothing.
//! * The post's visibility decides, through the same
//!   [`quote_approval`] rule its `interactionPolicy` advertises: public and
//!   unlisted posts accept, a private one accepts the author's accepted
//!   followers, and everything else — a direct post, a quoter the author
//!   blocks or cannot show the post to — is answered with a `Reject`.
//! * A grant is stored in [`quote_authorization`], links the stored quoting
//!   status to the quoted one when the status has not named a quote yet
//!   ([`status::link_backfilled_quote`]), and is delivered with
//!   [`federation_out::answer_quote_request`]. A redelivered request
//!   restates the standing grant; one whose grant was revoked is rejected.
//!
//! A grant that links the quote also tells the quoted author, through
//! [`notify::on_quote_linked`].
//!
//! Withdrawing a grant (the `Delete` of the authorization when the quoted
//! post goes away) is a follow-up: until it lands a revoked grant only
//! makes its document answer 410.
//!
//! The dispatcher's containment has already proved the activity's `actor`
//! to be the signer, so the local-signer guard here is the belt to that
//! bracer, as in [`crate::federation::handlers::misc`].
//!
//! [`quote_authorization`]: crate::models::quote_authorization

use futures::future::BoxFuture;
use serde_json::Value;
use tracing::debug;

use super::create::names_actor;
use crate::{
    federation::{
        dispatch::{ActivityView, FederationError, Handlers, Outcome},
        fetch_object, json,
        quotes::{QuoteApproval, quote_approval, quote_authorization_uri},
    },
    models::{
        follow,
        quote_authorization::{self, NewQuoteAuthorization},
        status::{self, Status},
        status_visibility,
        user::{self, User},
    },
    services::{
        federation_out::{self, QuoteRequestAnswer},
        ingest::{self, IngestOutcome},
        notify,
    },
    state::AppState,
};

#[cfg(test)]
mod tests;

/// The activity type this module folds.
const QUOTE_REQUEST: &str = "QuoteRequest";
/// The ingest's reason for a post that was deleted before it arrived.
const TOMBSTONED: &str = "tombstoned";

/// Folds this module's route into the dispatcher's registry: the
/// `QuoteRequest` by its own name.
pub fn register(handlers: &mut Handlers) {
    handlers.register(QUOTE_REQUEST, handle_quote_request);
}

/// The `QuoteRequest` route: a grant or a refusal of a remote account's
/// request to quote a local post.
///
/// # Errors
///
/// [`FederationError::Transient`] for a read or write the retry may
/// outlive, and the resolver's own for an instrument that cannot be fetched
/// yet; everything the shape or the standing refuses arrives as
/// [`Outcome::Ignored`] or as the `Reject` it answers.
pub fn handle_quote_request(
    state: AppState,
    view: ActivityView,
    signer: User,
) -> BoxFuture<'static, Result<Outcome, FederationError>> {
    Box::pin(async move { apply_quote_request(state, view, signer).await })
}

/// The quoting post a request names: its object id, and the stored row when
/// this instance holds one.
#[derive(Debug)]
struct Instrument {
    /// The quoting post's object id, as the quoter's instance names it.
    uri: String,
    /// The stored quoting status, `None` when the ingest chose not to keep
    /// the post (the grant stands all the same).
    status: Option<Status>,
}

/// The body of [`handle_quote_request`].
async fn apply_quote_request(
    state: AppState,
    view: ActivityView,
    signer: User,
) -> Result<Outcome, FederationError> {
    if signer.local.get() {
        return Ok(Outcome::Ignored(
            "a QuoteRequest signed by an account this instance owns",
        ));
    }
    let Some((quoted, author)) = quoted_post(&state, &view).await? else {
        return Ok(Outcome::Ignored(
            "a QuoteRequest whose object is no live local status",
        ));
    };
    let Some(instrument) = resolve_instrument(&state, &view, &signer).await? else {
        return Ok(Outcome::Ignored(
            "a QuoteRequest whose instrument is not a readable post of its signer",
        ));
    };
    let request = Request {
        state: &state,
        view: &view,
        signer: &signer,
        quoted: &quoted,
        author: &author,
        instrument: &instrument,
    };
    if may_quote(&state, &quoted, &author, &signer).await? {
        request.grant().await
    } else {
        debug!(quoted = %quoted.id, quoter = %signer.ap_id, "a quote request is refused");
        request.refuse().await
    }
}

/// One request that passed the shape checks, with everything its answer
/// is built from.
#[derive(Debug)]
struct Request<'a> {
    /// The shared state.
    state: &'a AppState,
    /// The delivered `QuoteRequest`.
    view: &'a ActivityView,
    /// The remote account that asked.
    signer: &'a User,
    /// The local post it asks to quote.
    quoted: &'a Status,
    /// The quoted post's author, who answers.
    author: &'a User,
    /// The quoting post.
    instrument: &'a Instrument,
}

impl Request<'_> {
    /// The refusal of this request; a grant adds the authorization.
    fn answer(&self) -> QuoteRequestAnswer<'_> {
        QuoteRequestAnswer {
            request_id: &self.view.id,
            quoter_ap_id: &self.signer.ap_id,
            quoted_uri: &self.quoted.uri,
            instrument_uri: &self.instrument.uri,
            authorization_uri: None,
        }
    }

    /// Sends `answer` to the quoter, mapping a failed write onto the retry
    /// the dispatcher gives a transient failure.
    async fn send(&self, answer: &QuoteRequestAnswer<'_>) -> Result<Outcome, FederationError> {
        let queued = federation_out::answer_quote_request(self.state, self.author, answer)
            .await
            .map_err(|err| FederationError::transient("could not queue the quote answer", &err))?;
        debug!(queued, "the quote answer is queued");
        Ok(Outcome::Handled)
    }

    /// Answers the request with a `Reject`.
    async fn refuse(&self) -> Result<Outcome, FederationError> {
        self.send(&self.answer()).await
    }

    /// Stores the grant, links the quoting status when this instance holds
    /// it, and answers with the `Accept`. A grant that was revoked, or that
    /// another account holds, is refused instead.
    async fn grant(&self) -> Result<Outcome, FederationError> {
        let new = NewQuoteAuthorization {
            quoted_status_id: self.quoted.id,
            quoting_uri: &self.instrument.uri,
            quoter_user_id: self.signer.id,
            request_ap_id: &self.view.id,
        };
        let stored = quote_authorization::grant(&self.state.db, &new)
            .await
            .map_err(|err| {
                FederationError::transient("could not store the quote authorization", &err)
            })?;
        let Some(grant) = stored.filter(|row| row.revoked_at.is_none()) else {
            debug!(quoted = %self.quoted.id, "a withdrawn or foreign grant is refused");
            return self.refuse().await;
        };
        if let Some(quoting) = &self.instrument.status {
            let linked = status::link_backfilled_quote(&self.state.db, quoting.id, self.quoted)
                .await
                .map_err(|err| FederationError::transient("could not link the quote", &err))?;
            if linked {
                notify::on_quote_linked(self.state, quoting.id).await;
            }
        }
        let authorization_uri = quote_authorization_uri(&self.state.config.base_url, grant.id);
        let answer = QuoteRequestAnswer {
            authorization_uri: Some(&authorization_uri),
            ..self.answer()
        };
        debug!(quoted = %self.quoted.id, quoter = %self.signer.ap_id, "a quote request is granted");
        self.send(&answer).await
    }
}

/// The live local status the request's `object` names, with its live local
/// author; `None` for anything else.
async fn quoted_post(
    state: &AppState,
    view: &ActivityView,
) -> Result<Option<(Status, User)>, FederationError> {
    let Some(uri) = view.object.id() else {
        return Ok(None);
    };
    let found = status::get_by_uri(&state.db, uri)
        .await
        .map_err(|err| FederationError::transient("could not read the quoted status", &err))?;
    let Some(quoted) =
        found.filter(|row| row.local && row.deleted_at.is_none() && row.reblog_of_id.is_none())
    else {
        return Ok(None);
    };
    let author = user::get_by_id(&state.db, quoted.user_id)
        .await
        .map_err(|err| FederationError::transient("could not read the quoted author", &err))?;
    Ok(author
        .filter(User::is_local_active)
        .map(|owner| (quoted, owner)))
}

/// Whether the post's policy lets `quoter` quote it right now: public and
/// unlisted posts to anyone who may see them, a private one to its
/// author's accepted followers, nothing else.
async fn may_quote(
    state: &AppState,
    post: &Status,
    author: &User,
    asker: &User,
) -> Result<bool, FederationError> {
    let approval = quote_approval(post.visibility);
    if approval == QuoteApproval::Nobody {
        return Ok(false);
    }
    let visible = status_visibility::can_view(&state.db, post, Some(asker))
        .await
        .map_err(|err| FederationError::transient("could not read the quoter's standing", &err))?;
    if !visible || approval == QuoteApproval::Everyone {
        return Ok(visible);
    }
    follow::has_accepted_follow(&state.db, asker.id, author.id)
        .await
        .map_err(|err| FederationError::transient("could not read the quoter's follow", &err))
}

/// The quoting post the request names in `instrument`, `None` unless it is
/// provably the signer's own.
async fn resolve_instrument(
    state: &AppState,
    view: &ActivityView,
    signer: &User,
) -> Result<Option<Instrument>, FederationError> {
    let Some(raw) = view.raw.get("instrument") else {
        return Ok(None);
    };
    let first = json::as_array(raw).first();
    let instrument = if let Some(uri) = first.and_then(Value::as_str) {
        referenced_instrument(state, uri, signer).await?
    } else if let Some(document) = first.filter(|value| value.is_object()) {
        embedded_instrument(state, document, signer).await?
    } else {
        None
    };
    Ok(instrument.filter(|named| {
        named
            .status
            .as_ref()
            .is_none_or(|stored| stored.user_id == signer.id)
    }))
}

/// An instrument delivered inside the request: it must name the signer as
/// its author and sit on the signer's host, and is then ingested like a
/// delivered `Create`. A post the ingest declines to keep still counts —
/// the grant authorizes the uri — unless it was deleted.
async fn embedded_instrument(
    state: &AppState,
    document: &Value,
    signer: &User,
) -> Result<Option<Instrument>, FederationError> {
    let Some(uri) = document.get("id").and_then(Value::as_str) else {
        return Ok(None);
    };
    if !names_actor(document, &signer.ap_id) || !same_host(uri, &signer.ap_id) {
        debug!(
            uri,
            "a quote request's instrument is not the signer's own post"
        );
        return Ok(None);
    }
    let named = |kept| Instrument {
        uri: uri.to_owned(),
        status: kept,
    };
    match ingest::ingest_note(state, signer, document).await {
        Ok(IngestOutcome::Created(row) | IngestOutcome::Existing(row)) => {
            Ok(Some(named(Some(row))))
        }
        Ok(IngestOutcome::Ignored(TOMBSTONED)) => Ok(None),
        Ok(IngestOutcome::Ignored(reason)) => {
            debug!(uri, reason, "the quoting post is authorized but not stored");
            Ok(Some(named(None)))
        }
        Err(FederationError::Permanent(why)) => {
            debug!(uri, %why, "a quote request's instrument is not a note");
            Ok(None)
        }
        Err(err) => Err(err),
    }
}

/// An instrument named by uri: fetched on behalf of the signer, so a post
/// of anybody else resolves to nothing.
async fn referenced_instrument(
    state: &AppState,
    uri: &str,
    signer: &User,
) -> Result<Option<Instrument>, FederationError> {
    if !same_host(uri, &signer.ap_id) {
        debug!(
            uri,
            "a quote request's instrument is on another host than its signer"
        );
        return Ok(None);
    }
    let fetched = fetch_object::resolve_status_as_actor(state, uri, signer).await?;
    Ok(fetched.map(|stored| Instrument {
        uri: stored.uri.clone(),
        status: Some(stored),
    }))
}

/// Whether `uri` and `actor` are on the same host.
fn same_host(uri: &str, actor: &str) -> bool {
    json::http_host(uri)
        .is_some_and(|host| json::http_host(actor).as_deref() == Some(host.as_str()))
}
