//! Who an outgoing activity addresses: the addressing properties it
//! carries, the expansion of the actor's followers collection and of every
//! directly named recipient into candidate inboxes, and the question that
//! decides whether relays see it too — does it address the public.

use serde_json::Value;
use sqlx::PgPool;
use uuid::Uuid;

use super::ADDRESSING_FIELDS;
use crate::{
    error::AppResult,
    federation::{delivery::best_inbox, json, note_parser::PUBLIC_RECIPIENTS},
    models::{
        follow,
        user::{INSTANCE_ACTOR_NICKNAME, User, get_by_ap_id},
    },
};

/// Every address the activity's addressing properties name, each in the
/// shapes [`json::references`] normalizes; absent and unreadable
/// properties contribute nothing.
pub(super) fn addressed_to(activity: &Value) -> Vec<String> {
    let mut addresses = Vec::new();
    for field in ADDRESSING_FIELDS {
        let listed = activity.get(field).map_or_else(Vec::new, json::references);
        addresses.extend(listed);
    }
    addresses
}

/// Resolves recipient addresses to candidate inbox URLs: the followers
/// collection expands to the follower inboxes, every other resolvable
/// address contributes what a single recipient receives, and the public
/// address contributes nothing.
pub(super) async fn expand_recipients(
    pool: &PgPool,
    actor: &User,
    addresses: &[String],
) -> AppResult<Vec<String>> {
    let mut inboxes = Vec::new();
    for address in addresses {
        if is_public(address) {
            continue;
        }
        if actor.followers_url.as_deref() == Some(address.as_str()) {
            inboxes.extend(follower_inboxes(pool, actor.id).await?);
        } else if let Some(inbox) = recipient_inbox(pool, address, actor).await? {
            inboxes.push(inbox);
        }
    }
    Ok(inboxes)
}

/// The sender-side expansion of the followers collection: the shared (or,
/// when a follower publishes none, personal) inbox of every remote, active
/// follower of `actor_id`. Deduplication of shared inboxes happens in
/// [`deliverable_inboxes`](super::inboxes::deliverable_inboxes).
async fn follower_inboxes(pool: &PgPool, actor_id: Uuid) -> AppResult<Vec<String>> {
    let followers = follow::remote_followers(pool, actor_id).await?;
    Ok(followers.iter().filter_map(best_inbox).collect())
}

/// The personal inbox of a directly addressed recipient, or `None` when
/// `address` resolves to nobody who may receive a delivery: an unknown
/// address (never seen, so never stored), a local user (their timeline is
/// the delivery), the actor itself, this instance's own actor, a
/// deactivated or suspended mirror, or a remote row that published no
/// personal inbox at all.
async fn recipient_inbox(pool: &PgPool, address: &str, actor: &User) -> AppResult<Option<String>> {
    let Some(recipient) = get_by_ap_id(pool, address).await? else {
        return Ok(None);
    };
    if !may_receive(&recipient, actor) {
        return Ok(None);
    }
    Ok(recipient.inbox)
}

/// Whether a resolved recipient row may receive a delivery: a remote,
/// active account that is neither the publishing actor nor the instance
/// actor. (Local rows — including both of those — are already excluded by
/// `local`; the named checks stay anyway so the rule survives a future
/// row-shape change.)
fn may_receive(recipient: &User, actor: &User) -> bool {
    !recipient.local.get()
        && recipient.is_active.get()
        && recipient.id != actor.id
        && recipient.nickname != INSTANCE_ACTOR_NICKNAME
}

/// Whether any address in `addresses` is the public address — the only
/// condition that makes relays eligible for the activity.
pub(super) fn includes_public(addresses: &[String]) -> bool {
    addresses.iter().any(|address| is_public(address))
}

/// Whether `address` is the public address in any of its three
/// [`PUBLIC_RECIPIENTS`] spellings.
fn is_public(address: &str) -> bool {
    PUBLIC_RECIPIENTS.contains(&address)
}

#[cfg(test)]
mod tests {
    use serde_json::json;

    use super::super::test_support::*;
    use super::*;

    /// All five addressing properties contribute recipients, in any
    /// JSON-LD shape.
    #[test]
    fn every_addressing_field_names_recipients() {
        let activity = json!({
            "to": [PUBLIC, "https://peer.example/users/bob"],
            "cc": "https://test.example/users/alice/followers",
            "bto": [{"id": "https://peer.example/users/carol"}],
            "bcc": { "type": "Link", "href": "https://peer.example/users/dave" },
            "audience": ["https://peer.example/circles/fans"],
        });

        let addresses = addressed_to(&activity);

        assert_eq!(
            addresses,
            vec![
                PUBLIC.to_owned(),
                "https://peer.example/users/bob".to_owned(),
                "https://test.example/users/alice/followers".to_owned(),
                "https://peer.example/users/carol".to_owned(),
                "https://peer.example/users/dave".to_owned(),
                "https://peer.example/circles/fans".to_owned(),
            ],
            "the fields are read in order and every reference shape resolves"
        );
    }

    /// The public address is recognized in all three spellings and is the
    /// relay condition's trigger.
    #[test]
    fn the_public_spellings_are_recognised() {
        for spelling in PUBLIC_RECIPIENTS {
            assert!(is_public(spelling), "`{spelling}` is the public address");
        }
        assert!(
            !is_public("https://peer.example/users/bob"),
            "an actor id is nobody's public address"
        );
        assert!(
            includes_public(&["as:Public".to_owned()]),
            "one public spelling in the set triggers the rule"
        );
        assert!(
            !includes_public(&["https://peer.example/users/bob".to_owned()]),
            "a private set does not"
        );
    }
}
