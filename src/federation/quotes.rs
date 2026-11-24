//! Quote consent (FEP-044f): who may quote a local post, the
//! `interactionPolicy` that advertises it and the `QuoteAuthorization`
//! document that proves one grant.
//!
//! Mastodon 4.5 reads a post's `interactionPolicy.canQuote` before it lets
//! anybody quote it, asks the author with a `QuoteRequest`, and shows the
//! quote as accepted only after an `Accept` whose `result` dereferences to a
//! `QuoteAuthorization` (`app/lib/activitypub/parser/status_parser.rb`,
//! `app/services/activitypub/verify_quote_service.rb`). The rule that
//! decides a request, [`quote_approval`], is the one the policy advertises,
//! so the note and the handler cannot disagree.
//!
//! Everything here is pure: the rows come in as borrows and the documents
//! leave as [`Value`]s.

use serde_json::{Value, json};
use uuid::Uuid;

use crate::{
    federation::vocab::{AS_PUBLIC, AS_VOCABULARY},
    models::{
        quote_authorization::QuoteAuthorization,
        status::Visibility,
        user::{User, followers_address},
    },
};

/// Path prefix of the `QuoteAuthorization` documents, relative to the
/// instance origin: `{BASE_URL}/quote_authorizations/{uuid}`.
pub const QUOTE_AUTHORIZATIONS_PATH: &str = "/quote_authorizations";

/// The `GoToSocial` namespace that FEP-044f's interaction-policy and
/// authorization terms live in.
const GTS_NAMESPACE: &str = "https://gotosocial.org/ns#";
/// The FEP-044f definition of the `QuoteAuthorization` type.
const QUOTE_AUTHORIZATION_TERM: &str = "https://w3id.org/fep/044f#QuoteAuthorization";
/// The `type` of the authorization document.
const TYPE_QUOTE_AUTHORIZATION: &str = "QuoteAuthorization";

/// Who may quote a post without asking its author again.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum QuoteApproval {
    /// Anybody: the post is public or unlisted.
    Everyone,
    /// Only the author's accepted followers.
    Followers,
    /// Nobody: the post is direct or never federates.
    Nobody,
}

/// The [`QuoteApproval`] a post of `visibility` grants.
#[must_use]
pub const fn quote_approval(visibility: Visibility) -> QuoteApproval {
    match visibility {
        Visibility::Public | Visibility::Unlisted => QuoteApproval::Everyone,
        Visibility::Private => QuoteApproval::Followers,
        Visibility::Direct | Visibility::Local => QuoteApproval::Nobody,
    }
}

/// The `interactionPolicy` a note of `visibility` by `author` carries, or
/// `None` for a post nobody may quote.
///
/// Mastodon marks a post with no policy as unquotable, which is exactly what
/// a direct post wants. Manual approval is never offered: the instance
/// either accepts a request on the spot or refuses it.
#[must_use]
pub fn interaction_policy_json(visibility: Visibility, author: &User) -> Option<Value> {
    let approved = match quote_approval(visibility) {
        QuoteApproval::Everyone => json!(AS_PUBLIC),
        QuoteApproval::Followers => json!(followers_address(author)),
        QuoteApproval::Nobody => return None,
    };
    Some(json!({
        "canQuote": {
            "automaticApproval": [approved],
            "manualApproval": [],
        }
    }))
}

/// The address of the authorization `id`: `{base_url}/quote_authorizations/{id}`.
#[must_use]
pub fn quote_authorization_uri(base_url: &str, id: Uuid) -> String {
    format!("{base_url}{QUOTE_AUTHORIZATIONS_PATH}/{id}")
}

/// The `QuoteAuthorization` document of `authorization`: `author_ap_id`
/// lets the post at `quoted_uri` be quoted by the post at the grant's
/// `quoting_uri`.
///
/// Mastodon's verifier fetches this and checks the type, that `attributedTo`
/// sits on the document's own host, that `interactingObject` is the quoting
/// post and that `interactionTarget` is the quoted one.
#[must_use]
pub fn quote_authorization_json(
    base_url: &str,
    authorization: &QuoteAuthorization,
    author_ap_id: &str,
    quoted_uri: &str,
) -> Value {
    json!({
        "@context": [
            AS_VOCABULARY,
            {
                "gts": GTS_NAMESPACE,
                "QuoteAuthorization": QUOTE_AUTHORIZATION_TERM,
                "interactingObject": {"@id": "gts:interactingObject", "@type": "@id"},
                "interactionTarget": {"@id": "gts:interactionTarget", "@type": "@id"},
            },
        ],
        "id": quote_authorization_uri(base_url, authorization.id),
        "type": TYPE_QUOTE_AUTHORIZATION,
        "attributedTo": author_ap_id,
        "interactingObject": authorization.quoting_uri,
        "interactionTarget": quoted_uri,
    })
}

#[cfg(test)]
mod tests {
    use chrono::Utc;

    use super::*;
    use crate::test_fixtures::json_keys;

    /// The instance origin of the fixtures.
    const BASE: &str = "https://test.example";
    /// The quoted post.
    const QUOTED: &str = "https://test.example/objects/quoted";
    /// The quoting post on the peer.
    const QUOTING: &str = "https://remote.example/objects/quoting";
    /// The authoring account.
    const AUTHOR: &str = "https://test.example/users/alice";

    /// A grant row fixture.
    fn grant() -> QuoteAuthorization {
        QuoteAuthorization {
            id: Uuid::now_v7(),
            quoted_status_id: Uuid::now_v7(),
            quoting_uri: QUOTING.to_owned(),
            quoter_user_id: Uuid::now_v7(),
            request_ap_id: "https://remote.example/activities/1".to_owned(),
            inserted_at: Utc::now(),
            revoked_at: None,
        }
    }

    #[test]
    fn visibility_decides_who_may_quote() {
        assert_eq!(
            quote_approval(Visibility::Public),
            QuoteApproval::Everyone,
            "public is open to everyone"
        );
        assert_eq!(
            quote_approval(Visibility::Unlisted),
            QuoteApproval::Everyone,
            "unlisted is as quotable as public"
        );
        assert_eq!(
            quote_approval(Visibility::Private),
            QuoteApproval::Followers,
            "private is for followers"
        );
        assert_eq!(
            quote_approval(Visibility::Direct),
            QuoteApproval::Nobody,
            "direct is nobody's to quote"
        );
        assert_eq!(
            quote_approval(Visibility::Local),
            QuoteApproval::Nobody,
            "local never federates"
        );
    }

    #[test]
    fn the_authorization_document_has_exactly_the_keys_mastodon_reads() {
        let row = grant();
        let document = quote_authorization_json(BASE, &row, AUTHOR, QUOTED);
        assert_eq!(
            json_keys(&document),
            [
                "@context",
                "attributedTo",
                "id",
                "interactingObject",
                "interactionTarget",
                "type"
            ],
            "the FEP-044f key set"
        );
        assert_eq!(
            document["id"],
            format!("{BASE}/quote_authorizations/{}", row.id),
            "the id is the address the document is served at"
        );
        assert_eq!(document["type"], "QuoteAuthorization", "the type");
        assert_eq!(document["attributedTo"], AUTHOR, "the granting author");
        assert_eq!(document["interactingObject"], QUOTING, "the quoting post");
        assert_eq!(document["interactionTarget"], QUOTED, "the quoted post");
        assert_eq!(
            document["@context"][1]["interactingObject"],
            json!({"@id": "gts:interactingObject", "@type": "@id"}),
            "the context defines the terms it uses"
        );
    }
}
