//! The `interactionPolicy.canQuote` a note advertises (FEP-044f): who may
//! quote it without asking and who must ask first.

use serde_json::Value;

use crate::{
    federation::{
        json::references,
        vocab::{AS_PUBLIC, is_public_address},
    },
    models::status::QuotePolicy,
};

/// The keys that list the approvers of an accepted quote, current spelling
/// first, then the older one Mastodon still reads.
const AUTOMATIC_KEYS: [&str; 2] = ["automaticApproval", "always"];
/// The keys that list the approvers of a quote the author must answer.
const MANUAL_KEYS: [&str; 2] = ["manualApproval", "approvalRequired"];

/// The quote policy of `json`, or `None` when the note advertises none.
///
/// A note without `interactionPolicy`, or whose policy has no `canQuote`
/// object, says nothing about quotes. A `canQuote` object with no approvers
/// is a policy: nobody may quote.
pub(super) fn parse_quote_policy(json: &Value) -> Option<QuotePolicy> {
    let can_quote = json
        .get("interactionPolicy")?
        .get("canQuote")
        .filter(|policy| policy.is_object())?;
    Some(QuotePolicy {
        automatic: approvers(can_quote, &AUTOMATIC_KEYS),
        manual: approvers(can_quote, &MANUAL_KEYS),
    })
}

/// The approver addresses `keys` list in `can_quote`, each once, in
/// document order, with every spelling of the public address folded to
/// [`AS_PUBLIC`].
fn approvers(can_quote: &Value, keys: &[&str]) -> Vec<String> {
    let mut found: Vec<String> = Vec::new();
    for key in keys {
        let Some(value) = can_quote.get(*key) else {
            continue;
        };
        for listed in references(value) {
            let folded = if is_public_address(&listed) {
                AS_PUBLIC.to_owned()
            } else {
                listed
            };
            if !found.contains(&folded) {
                found.push(folded);
            }
        }
    }
    found
}

#[cfg(test)]
mod tests {
    use serde_json::json;

    use super::*;

    /// A followers collection address.
    const FOLLOWERS: &str = "https://remote.example/users/bob/followers";

    #[test]
    fn a_note_without_a_policy_advertises_nothing() {
        assert_eq!(parse_quote_policy(&json!({"type": "Note"})), None);
        assert_eq!(
            parse_quote_policy(&json!({"interactionPolicy": {"canReply": {}}})),
            None,
            "a policy about other interactions says nothing about quotes"
        );
        assert_eq!(
            parse_quote_policy(&json!({"interactionPolicy": {"canQuote": null}})),
            None,
            "a null canQuote is no policy"
        );
    }

    #[test]
    fn an_empty_can_quote_is_a_policy_of_nobody() {
        let policy = parse_quote_policy(&json!({"interactionPolicy": {"canQuote": {}}}));
        assert_eq!(policy, Some(QuotePolicy::default()));
    }

    #[test]
    fn automatic_and_manual_approval_are_read_as_strings_or_lists() {
        let note = json!({"interactionPolicy": {"canQuote": {
            "automaticApproval": ["as:Public"],
            "manualApproval": FOLLOWERS,
        }}});
        assert_eq!(
            parse_quote_policy(&note),
            Some(QuotePolicy {
                automatic: vec![AS_PUBLIC.to_owned()],
                manual: vec![FOLLOWERS.to_owned()],
            }),
            "public folds to the IRI; a lone string is a list of one"
        );
    }

    #[test]
    fn the_older_spellings_are_read_and_merged() {
        let note = json!({"interactionPolicy": {"canQuote": {
            "always": [{"id": FOLLOWERS}],
            "approvalRequired": [AS_PUBLIC, "Public"],
            "automaticApproval": [FOLLOWERS],
        }}});
        assert_eq!(
            parse_quote_policy(&note),
            Some(QuotePolicy {
                automatic: vec![FOLLOWERS.to_owned()],
                manual: vec![AS_PUBLIC.to_owned()],
            }),
            "always joins automaticApproval and approvalRequired joins manualApproval, once each"
        );
    }
}
