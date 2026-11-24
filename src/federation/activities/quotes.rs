//! The grant that answers a `QuoteRequest` (FEP-044f): an `Accept` that
//! carries the `QuoteAuthorization` address as its `result`.
//!
//! The refusal is the plain [`reject_json`](super::reject_json) over the
//! embedded request.

use serde_json::{Value, json};

use super::{AnsweredActivity, accept_json};

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
        ALICE_AP, AUTHOR_TO, NO_CC, STATUS_URI, activity_uuid, config, json_keys, published,
    };
    use super::*;

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
