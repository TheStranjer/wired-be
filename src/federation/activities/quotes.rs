//! The documents of the quote handshake (FEP-044f): the `QuoteRequest` a
//! local quote of a remote post sends, and the grant that answers one, an
//! `Accept` that carries the `QuoteAuthorization` address as its `result`.
//!
//! The refusal is the plain [`reject_json`](super::reject_json) over the
//! embedded request.

use serde_json::{Value, json};

use super::{AnsweredActivity, accept_json};
use crate::{config::Config, federation::note::json_ld_context};

/// The activity type of a request to quote a post.
const TYPE_QUOTE_REQUEST: &str = "QuoteRequest";

/// The inputs [`quote_request_json`] renders from.
#[derive(Debug)]
pub struct QuoteRequestContext<'a> {
    /// The instance whose JSON-LD schema frames the document.
    pub config: &'a Config,
    /// The document's `id`, the one the answer embeds.
    pub id: &'a str,
    /// The quoting account's actor id.
    pub actor_ap_id: &'a str,
    /// The quoted author's actor id, the request's only recipient.
    pub quoted_author_ap_id: &'a str,
    /// The object id of the post being quoted.
    pub quoted_uri: &'a str,
    /// The quoting post's note, embedded whole: Mastodon ingests an
    /// embedded instrument instead of fetching it back.
    pub instrument: &'a Value,
}

/// The `QuoteRequest` that asks the quoted author to let `ctx`'s
/// instrument quote `ctx`'s object.
///
/// Mastodon matches the request to the quote by the `instrument`, answers
/// with an `Accept` or `Reject` that embeds this document, and files the
/// quote as pending until then.
#[must_use]
pub fn quote_request_json(ctx: &QuoteRequestContext<'_>) -> Value {
    json!({
        "@context": json_ld_context(ctx.config),
        "id": ctx.id,
        "type": TYPE_QUOTE_REQUEST,
        "actor": ctx.actor_ap_id,
        "to": [ctx.quoted_author_ap_id],
        "cc": [],
        "object": ctx.quoted_uri,
        "instrument": ctx.instrument,
    })
}

/// The `Accept` that grants `answer`'s embedded `QuoteRequest`, with the
/// authorization the quoter will dereference as its `result`.
///
/// Mastodon shows the quote as accepted only once it has fetched
/// `authorization_uri` and found the quoting and the quoted post in it.
#[must_use]
pub fn accept_quote_json(answer: &AnsweredActivity<'_>, authorization_uri: &str) -> Value {
    let mut document = accept_json(answer);
    if let Some(fields) = document.as_object_mut() {
        let _previous: Option<Value> = fields.insert("result".to_owned(), json!(authorization_uri));
    }
    document
}

#[cfg(test)]
mod tests {
    use super::super::test_support::{
        ALICE_AP, AUTHOR_TO, BOB_AP, NO_CC, STATUS_URI, activity_uuid, config, expected_context,
        json_keys, published,
    };
    use super::*;

    #[test]
    fn the_request_embeds_the_quoting_note_and_names_the_quoted_post() {
        let config = config();
        let note = json!({
            "id": "https://test.example/objects/quoting",
            "type": "Note",
            "quote": STATUS_URI,
        });
        let ctx = QuoteRequestContext {
            config: &config,
            id: "https://test.example/activities/req-1",
            actor_ap_id: ALICE_AP,
            quoted_author_ap_id: BOB_AP,
            quoted_uri: STATUS_URI,
            instrument: &note,
        };

        let document = quote_request_json(&ctx);

        assert_eq!(document["type"], "QuoteRequest", "the FEP-044f type");
        assert_eq!(document["id"], ctx.id, "the id the answer embeds");
        assert_eq!(document["actor"], ALICE_AP, "the quoter asks");
        assert_eq!(document["object"], STATUS_URI, "the quoted post");
        assert_eq!(document["instrument"], note, "the note rides inside");
        assert_eq!(document["to"], json!([BOB_AP]), "the quoted author hears");
        assert_eq!(document["@context"], expected_context(), "the context");
        assert_eq!(
            json_keys(&document),
            [
                "@context",
                "actor",
                "cc",
                "id",
                "instrument",
                "object",
                "to",
                "type"
            ],
            "exactly the request's keys"
        );
    }

    #[test]
    fn the_grant_is_an_accept_with_the_authorization_as_its_result() {
        let config = config();
        let published = published();
        let request = json!({
            "id": "https://remote.example/activities/quote-1",
            "type": "QuoteRequest",
            "actor": "https://remote.example/users/bob",
            "object": STATUS_URI,
            "instrument": "https://remote.example/objects/quoting",
        });
        let answer = AnsweredActivity {
            config: &config,
            id: activity_uuid(),
            actor_ap_id: ALICE_AP,
            to: &AUTHOR_TO,
            cc: &NO_CC,
            answered: &request,
            published: &published,
        };

        let document = accept_quote_json(&answer, "https://test.example/quote_authorizations/q1");

        assert_eq!(document["type"], "Accept", "the answer is an Accept");
        assert_eq!(
            document["result"], "https://test.example/quote_authorizations/q1",
            "the result names the authorization to fetch"
        );
        assert_eq!(
            document["object"], request,
            "the request rides inside, embedded"
        );
        assert_eq!(document["actor"], ALICE_AP, "the quoted author answers");
        assert_eq!(
            json_keys(&document),
            [
                "@context",
                "actor",
                "cc",
                "id",
                "object",
                "published",
                "result",
                "to",
                "type"
            ],
            "the activity keys plus the result"
        );
    }
}
