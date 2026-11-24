//! The document's audience: the visibility derived from `to` and `cc`,
//! with the recipient flattening and the public and followers tests they
//! rest on.

use serde_json::Value;

use crate::{
    federation::json::{as_array, value_or_id},
    models::{status::Visibility, user::User},
};

use super::PUBLIC_RECIPIENTS;

/// The visibility from `to` and `cc`: the public recipient in `to` is
/// public, in `cc` alone unlisted, the author's followers collection (and
/// nobody public) private, anything else direct.
///
/// `pub(crate)` because the C2S outbox in [`crate::api::activitypub`]
/// derives a posted `Create`'s audience with exactly this rule.
pub(crate) fn parse_visibility(json: &Value, author: &User) -> Visibility {
    let to = recipients(json, "to");
    let cc = recipients(json, "cc");
    if has_public(&to) {
        Visibility::Public
    } else if has_public(&cc) {
        Visibility::Unlisted
    } else if addresses_followers(&to, &cc, author.followers_url.as_deref()) {
        Visibility::Private
    } else {
        Visibility::Direct
    }
}

/// The actor ids a document addresses in `to` and `cc`: every recipient
/// that is neither a [`PUBLIC_RECIPIENTS`] spelling nor `author`'s
/// followers collection, each once, `to` first. Mastodon's
/// `process_audience` and Pleroma's `to` check both grant a known local
/// account named here access without a `Mention` tag; resolving which of
/// these are local accounts is the ingest's job.
pub(crate) fn addressed_actor_ids(json: &Value, author: &User) -> Vec<String> {
    let followers_url = author.followers_url.as_deref();
    let mut ids: Vec<String> = Vec::new();
    for recipient in recipients(json, "to")
        .into_iter()
        .chain(recipients(json, "cc"))
    {
        let is_public = PUBLIC_RECIPIENTS.contains(&recipient.as_str());
        let is_followers = followers_url == Some(recipient.as_str());
        if !is_public && !is_followers && !ids.contains(&recipient) {
            ids.push(recipient);
        }
    }
    ids
}

/// The recipient strings of an addressing field: every string, reference
/// object and array of either, flattened through [`as_array`] and
/// [`value_or_id`].
fn recipients(json: &Value, field: &str) -> Vec<String> {
    json.get(field)
        .map_or_default(as_array)
        .iter()
        .filter_map(value_or_id)
        .collect()
}

/// True when any recipient is the public address, in any of its
/// [`PUBLIC_RECIPIENTS`] spellings.
fn has_public(recipients: &[String]) -> bool {
    recipients
        .iter()
        .any(|recipient| PUBLIC_RECIPIENTS.contains(&recipient.as_str()))
}

/// True when either list names the author's followers collection.
fn addresses_followers(to: &[String], cc: &[String], followers_url: Option<&str>) -> bool {
    followers_url.is_some_and(|url| {
        to.iter().any(|recipient| recipient == url) || cc.iter().any(|recipient| recipient == url)
    })
}

#[cfg(test)]
mod tests {
    use serde_json::json;

    use super::*;
    use crate::federation::note_parser::test_support::*;

    /// One visibility case: the `to` and `cc` values and the outcome.
    type AddressingCase = (&'static str, Value, Value, Visibility);

    /// The addressing shapes of the ticket, string vs array vs object.
    fn addressing_cases() -> Vec<AddressingCase> {
        vec![
            (
                "public iri in to",
                json!([PUBLIC]),
                json!([]),
                Visibility::Public,
            ),
            (
                "as:Public in to",
                json!(["as:Public"]),
                json!([]),
                Visibility::Public,
            ),
            (
                "bare Public in to",
                json!(["Public"]),
                json!([]),
                Visibility::Public,
            ),
            (
                "public in cc",
                json!([FOLLOWERS]),
                json!([PUBLIC]),
                Visibility::Unlisted,
            ),
            (
                "to as a bare string",
                json!(PUBLIC),
                Value::Null,
                Visibility::Public,
            ),
            (
                "recipient as an object",
                json!([{"id": PUBLIC, "type": "Collection"}]),
                json!([]),
                Visibility::Public,
            ),
            (
                "followers in to",
                json!([FOLLOWERS]),
                json!([]),
                Visibility::Private,
            ),
            (
                "followers in cc",
                json!([AUTHOR_AP_ID]),
                json!([FOLLOWERS]),
                Visibility::Private,
            ),
            (
                "only a mention",
                json!([AUTHOR_AP_ID]),
                json!([]),
                Visibility::Direct,
            ),
            ("empty lists", json!([]), json!([]), Visibility::Direct),
            ("no lists", Value::Null, Value::Null, Visibility::Direct),
            (
                "public outranks followers",
                json!([PUBLIC, FOLLOWERS]),
                json!([]),
                Visibility::Public,
            ),
            (
                "a foreign followers list is not private",
                json!(["https://other.example/users/x/followers"]),
                json!([]),
                Visibility::Direct,
            ),
        ]
    }

    #[test]
    fn audience_keeps_only_actor_ids() {
        let alice = "https://us.example/users/alice";
        let mut document = minimal_note();
        document["to"] = json!([alice, "as:Public", PUBLIC, FOLLOWERS]);
        document["cc"] = json!([{"id": alice}, "https://us.example/users/carol", FOLLOWERS]);

        assert_eq!(
            addressed_actor_ids(&document, &author()),
            [alice, "https://us.example/users/carol"],
            "public spellings and the followers url drop; duplicates collapse"
        );
    }

    #[test]
    fn audience_is_empty_without_recipients() {
        let mut document = minimal_note();
        remove(&mut document, "to");
        remove(&mut document, "cc");

        assert!(
            addressed_actor_ids(&document, &author()).is_empty(),
            "no lists, no audience"
        );
    }

    #[test]
    fn audience_keeps_a_foreign_followers_collection() {
        let foreign = "https://other.example/users/x/followers";
        let mut document = minimal_note();
        document["to"] = json!([foreign]);

        assert_eq!(
            addressed_actor_ids(&document, &author()),
            [foreign],
            "only the author's own followers url is filtered"
        );
    }

    #[test]
    fn visibility_follows_to_and_cc() {
        for (name, to, cc, expected) in addressing_cases() {
            let mut document = minimal_note();
            if to == Value::Null {
                remove(&mut document, "to");
            } else {
                document["to"] = to;
            }
            if cc == Value::Null {
                remove(&mut document, "cc");
            } else {
                document["cc"] = cc;
            }
            let parsed = parse(&document);
            assert_eq!(parsed.status.visibility, expected, "{name}");
        }
    }
}
