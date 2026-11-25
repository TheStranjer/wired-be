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
    federation::{delivery::best_inboxes, json, vocab::is_public_address},
    models::{
        activitypub_actor, follow,
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

/// Whether `addresses` reach beyond named actors: they carry the public
/// address or the publishing actor's own followers collection,
/// `followers_url`.
///
/// Such an activity is broadcast, so every recipient on a host collapses
/// onto that host's shared inbox; one that names only actors is direct and
/// each is reached at their personal inbox.
fn is_broadcast(followers_url: Option<&str>, addresses: &[String]) -> bool {
    addresses
        .iter()
        .any(|address| is_public_address(address) || followers_url == Some(address.as_str()))
}

/// Resolves recipient addresses to candidate inbox URLs: the followers
/// collection expands to the follower inboxes, every other resolvable
/// address contributes what a single recipient receives, and the public
/// address contributes nothing.
///
/// A broadcast activity (see [`is_broadcast`]) reaches a named recipient at
/// their shared inbox when they publish one, so the host that also holds
/// followers is delivered to once; a direct activity reaches them at their
/// personal inbox.
///
/// The actor's followers collection and every recipient's inbox come from
/// the `activitypub_actors` table. The named recipients are collected first
/// so their rows are read in one statement, not one per recipient.
pub(super) async fn expand_recipients(
    pool: &PgPool,
    actor: &User,
    addresses: &[String],
) -> AppResult<Vec<String>> {
    let followers_url = activitypub_actor::get(pool, actor.id)
        .await?
        .and_then(|row| row.followers_url);
    let broadcast = is_broadcast(followers_url.as_deref(), addresses);
    let mut inboxes = Vec::new();
    let mut recipients = Vec::new();
    for address in addresses {
        if is_public_address(address) {
            continue;
        }
        if followers_url.as_deref() == Some(address.as_str()) {
            inboxes.extend(follower_inboxes(pool, actor.id).await?);
        } else if let Some(recipient) = receiving_account(pool, address, actor).await? {
            recipients.push(recipient);
        }
    }
    inboxes.extend(recipient_inboxes(pool, &recipients, broadcast).await?);
    Ok(inboxes)
}

/// The sender-side expansion of the followers collection: the shared (or,
/// when a follower publishes none, personal) inbox of every remote, active
/// follower of `actor_id`. Deduplication of shared inboxes happens in
/// [`deliverable_inboxes`](super::inboxes::deliverable_inboxes).
async fn follower_inboxes(pool: &PgPool, actor_id: Uuid) -> AppResult<Vec<String>> {
    let followers = follow::remote_followers(pool, actor_id).await?;
    best_inboxes(pool, &followers).await
}

/// The account a directly addressed `address` names, when it may receive a
/// delivery; `None` for an unknown address (never seen, so never stored), a
/// local user (their timeline is the delivery), the actor itself, this
/// instance's own actor, and a deactivated or suspended mirror.
async fn receiving_account(pool: &PgPool, address: &str, actor: &User) -> AppResult<Option<User>> {
    let recipient = get_by_ap_id(pool, address).await?;
    Ok(recipient.filter(|candidate| may_receive(candidate, actor)))
}

/// The inboxes of the directly addressed `recipients`, read from their
/// `activitypub_actors` rows in one statement: each one's
/// [`best_inbox`](crate::federation::delivery::best_inbox) when `broadcast`, their personal inbox otherwise.
///
/// A recipient whose row publishes no usable inbox (or has no row)
/// contributes nothing.
async fn recipient_inboxes(
    pool: &PgPool,
    recipients: &[User],
    broadcast: bool,
) -> AppResult<Vec<String>> {
    if broadcast {
        return best_inboxes(pool, recipients).await;
    }
    let ids: Vec<Uuid> = recipients.iter().map(|recipient| recipient.id).collect();
    let rows = activitypub_actor::get_many(pool, &ids).await?;
    Ok(rows.into_iter().filter_map(|row| row.inbox).collect())
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

/// Whether the public address is in the activity's `to` — the primary
/// audience. An unlisted post carries it in `cc` only, so it is not
/// public in this sense and relays never see it.
pub(super) fn primary_public(activity: &Value) -> bool {
    activity
        .get("to")
        .map_or_else(Vec::new, json::references)
        .iter()
        .any(|address| is_public_address(address))
}

/// Whether relays carry activities of this `type`: the ones that spread a
/// post, its edits and its removal (`Create`, `Announce`, `Update`,
/// `Delete`, `Undo`, `Move`), and nothing else — never a `Like`, an
/// `EmojiReact`, a collection change, a report or a relationship activity.
pub(super) fn relay_type(activity: &Value) -> bool {
    matches!(
        activity.get("type").and_then(Value::as_str),
        Some("Create" | "Announce" | "Update" | "Delete" | "Undo" | "Move")
    )
}

#[cfg(test)]
mod tests {
    use serde_json::json;

    use super::super::test_support::*;
    use super::*;
    use crate::federation::vocab::PUBLIC_RECIPIENTS;

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

    /// The public address is recognized in all three spellings.
    #[test]
    fn the_public_spellings_are_recognised() {
        for spelling in PUBLIC_RECIPIENTS {
            assert!(
                is_public_address(spelling),
                "`{spelling}` is the public address"
            );
        }
        assert!(
            !is_public_address("https://peer.example/users/bob"),
            "an actor id is nobody's public address"
        );
    }

    /// Only the public address in `to` makes an activity relay-eligible,
    /// whichever spelling it uses; in `cc` (unlisted) it does not.
    #[test]
    fn primary_public_reads_only_the_to_field() {
        for spelling in PUBLIC_RECIPIENTS {
            assert!(
                primary_public(&json!({"to": [spelling]})),
                "`{spelling}` in a `to` list is primary public"
            );
            assert!(
                primary_public(&json!({"to": spelling})),
                "`{spelling}` as a bare `to` string is primary public"
            );
        }
        assert!(
            primary_public(&json!({"to": ["as:Public"]})),
            "the compact spelling counts"
        );
        assert!(
            primary_public(&json!({"to": ["Public"]})),
            "the bare spelling counts"
        );
        assert!(
            !primary_public(&json!({
                "to": ["https://test.example/users/alice/followers"],
                "cc": [PUBLIC],
            })),
            "public only in `cc` is unlisted"
        );
        assert!(
            !primary_public(&json!({"cc": ["as:Public"], "bto": ["Public"]})),
            "no `to` at all is not public"
        );
        assert!(
            !primary_public(&json!({"to": ["https://peer.example/users/bob"]})),
            "a private `to` is not public"
        );
    }

    /// The relay-carried activity types, and the ones relays never get.
    #[test]
    fn relay_type_accepts_only_post_lifecycle_activities() {
        for kind in ["Create", "Announce", "Update", "Delete", "Undo", "Move"] {
            assert!(relay_type(&json!({"type": kind})), "`{kind}` is relayed");
        }
        for kind in [
            "Like",
            "EmojiReact",
            "Add",
            "Remove",
            "Flag",
            "Block",
            "Follow",
            "Accept",
            "Reject",
        ] {
            assert!(!relay_type(&json!({"type": kind})), "`{kind}` is not");
        }
        assert!(!relay_type(&json!({})), "an untyped document is not");
        assert!(
            !relay_type(&json!({"type": ["Create"]})),
            "a non-string type is not"
        );
    }
}
