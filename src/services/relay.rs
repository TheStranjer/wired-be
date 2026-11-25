//! Relay subscriptions: following a remote relay so its re-broadcasts
//! reach this instance.
//!
//! The reference is Pleroma's `Pleroma.Web.ActivityPub.Relay`
//! (`lib/pleroma/web/activity_pub/relay.ex`), whose `follow/1` has the
//! service actor follow a relay actor and whose `unfollow/2` takes that
//! back. Like Pleroma, this instance federates its relay follows *as the
//! instance actor* (see [`crate::services::instance_actor`]): the
//! `follows` row belongs to it, the `Follow` it sends is signed with its
//! key, and the [`relays`](crate::models::relay) table carries the
//! subscription state the rest of this feature reads — the publisher's
//! fan-out targets ([`crate::models::instance::relay_inboxes`]) and the
//! `Announce` handler's relay arm
//! ([`crate::federation::handlers::interactions`]).
//!
//! What differs from an ordinary follow, per the ticket: the `Follow` is
//! addressed straight to the relay's *inbox* (`to` names the endpoint,
//! not the actor), and it travels there alone — the relay is not a
//! follower of this instance and nothing fans out beyond that one POST.
//! The relay's `Accept` grants the `follows` row through the existing
//! [`Accept` handler](crate::federation::handlers::follow), which also
//! turns the relay row accepted — but only when the granted follow is the
//! instance actor's own; a local user's follow of the relay actor leaves
//! the subscription alone. A relay that never answers stays `pending` and
//! its inbox joins no fan-out; a relay that answers with a `Reject` (how
//! relays revoke a subscription) has the instance actor's `follows` row
//! removed and the relay row put back to `pending`, so its inbox leaves
//! the fan-out again.
//!
//! [`follow_relay`] accepts the relay actor's URL directly or a bare
//! host, which names the conventional relay actor at
//! `https://{host}/relay`; the fetched actor must be an `Application`,
//! `Service` or `Person` — the kinds relays are served as.

use serde_json::Value;
use sqlx::PgPool;
use tracing::debug;

use crate::{
    config::Config,
    error::{AppError, AppResult},
    federation::{
        activities::{AnsweredActivity, FollowActivity, follow_json_to, undo_json},
        fetch_actor::get_or_fetch_by_ap_id,
        publisher::{self, PublishOpts},
    },
    models::{
        activitypub_actor,
        follow::{self, Follow},
        relay::{self, Relay},
        user::{User, get_by_ap_id},
    },
    services::instance_actor::ensure_instance_actor,
    state::AppState,
    util::id::new_id,
};

/// The actor types a relay may be served as: the machine actors relays
/// are (`Application`, `Service`) and, for hand-run relay accounts, a
/// `Person`. Anything else — a `Group`, an `Organization` — is not a
/// relay candidate and [`follow_relay`] refuses it.
const RELAY_ACTOR_TYPES: [&str; 3] = ["Application", "Service", "Person"];

/// Follow the relay named by `relay_url_or_actor` as the instance actor.
///
/// The one argument accepts the relay actor's `ActivityPub` URL
/// (anything scheme-qualified, the usual `https://{host}/relay` shape
/// included) or a bare host, which resolves to `https://{host}/relay`
/// (see `relay_actor_url` below). The steps, in order: the instance actor is
/// provisioned; the relay actor is fetched or read from the mirror and
/// checked for a relay's [`actor_type`](User::actor_type) and a stored
/// `inbox`; the `follows` row instance-actor → relay is written with its
/// minted activity id (decision 6); the [`relays`](crate::models::relay)
/// row is stated `pending`; and the `Follow` — `to` the relay's inbox,
/// `object` its actor id — is queued to that inbox alone.
///
/// The write is idempotent like an ordinary follow: a relay already
/// followed keeps its row and its state (re-following an accepted relay
/// does not re-pend it), and the restated `Follow` carries the stored
/// activity id, so the delivery queue deduplicates the repeat.
///
/// Returns the stored subscription.
///
/// # Errors
///
/// [`AppError::Unprocessable`] for an address that is neither a URL nor
/// a host, an actor type no relay wears, or an actor with no inbox; the
/// fetch's own refusals (404, 410, a document that cannot become a
/// mirror) as [`AppError::NotFound`], [`AppError::Gone`] or
/// [`AppError::Unprocessable`]; [`follow::follow`]'s block refusals; and
/// any database failure as [`AppError::Database`](crate::error::AppError).
pub async fn follow_relay(state: &AppState, relay_url_or_actor: &str) -> AppResult<Relay> {
    let ap_id = relay_actor_url(relay_url_or_actor)?;
    let instance = ensure_instance_actor(&state.db, &state.config).await?;
    let relay_user = get_or_fetch_by_ap_id(state, &ap_id).await?;
    ensure_relay_actor_type(&relay_user)?;
    let relay_actor = activitypub_actor::get(&state.db, relay_user.id).await?;
    let inbox = relay_actor.and_then(|row| row.inbox).ok_or_else(|| {
        AppError::Unprocessable(format!(
            "the relay actor {} carries no inbox",
            relay_user.ap_id
        ))
    })?;
    let row = follow::follow(&state.db, &state.config.base_url, &instance, &relay_user).await?;
    let subscription = relay::upsert(&state.db, &relay_user.ap_id, &inbox).await?;
    let Some(document) = relay_follow_document(
        &state.config,
        &row,
        &instance,
        &relay_user,
        &subscription.inbox,
    ) else {
        // Unreachable for a row this instance's own follow minted; an
        // id-less row has no document to send, and the row stands anyway.
        return Ok(subscription);
    };
    send_to_relay(state, &instance, &document, &subscription.inbox).await?;
    debug!(
        relay = %subscription.actor_ap_id,
        "queued the instance actor's Follow of a relay"
    );
    Ok(subscription)
}

/// Take back the follow of the relay named by `relay_url_or_actor`.
///
/// The mirror image of [`follow_relay`], on the same address forms: the
/// `Undo` wrapping the stored `Follow` document is queued to the
/// subscription's inbox, and the `follows` row and the
/// [`relays`](crate::models::relay) row go with it — a relay that later
/// announces to a closed subscription folds like any other remote
/// actor's `Announce` again.
///
/// # Errors
///
/// [`AppError::NotFound`] when this instance follows no relay by that
/// address; those of [`follow_relay`] for the address form, plus any
/// database failure as [`AppError::Database`](crate::error::AppError).
pub async fn unfollow_relay(state: &AppState, relay_url_or_actor: &str) -> AppResult<()> {
    let ap_id = relay_actor_url(relay_url_or_actor)?;
    let subscription = relay::get(&state.db, &ap_id)
        .await?
        .ok_or(AppError::NotFound)?;
    let instance = ensure_instance_actor(&state.db, &state.config).await?;
    if let Some(relay_user) = get_by_ap_id(&state.db, &ap_id).await? {
        withdraw_follow(state, &instance, &relay_user, &subscription).await?;
        let _withdrawn = follow::unfollow(&state.db, &instance, &relay_user).await?;
    }
    let _gone = relay::delete(&state.db, &ap_id).await?;
    debug!(relay = %ap_id, "unfollowed a relay");
    Ok(())
}

/// Every relay this instance follows, pending and accepted, oldest
/// subscription first — the list the admin API serves.
///
/// # Errors
///
/// Any database failure arrives as [`AppError::Database`](crate::error::AppError).
pub async fn list_relays(pool: &PgPool) -> AppResult<Vec<Relay>> {
    relay::list(pool).await
}

/// The `Undo` of the instance actor's stored follow of `relay_user`, sent
/// to the subscription's inbox through the same one-endpoint path as the
/// `Follow` itself. A pair with no `follows` row (a subscription written
/// without its follow, or one already withdrawn) sends nothing.
async fn withdraw_follow(
    state: &AppState,
    instance: &User,
    relay_user: &User,
    subscription: &Relay,
) -> AppResult<()> {
    let Some(row) = follow::get(&state.db, instance.id, relay_user.id).await? else {
        return Ok(());
    };
    let Some(follow) = relay_follow_document(
        &state.config,
        &row,
        instance,
        relay_user,
        &subscription.inbox,
    ) else {
        return Ok(()); // an id-less row: there is nothing to withdraw
    };
    let now = chrono::Utc::now();
    let to = [subscription.inbox.as_str()];
    let undo = undo_json(&AnsweredActivity {
        config: &state.config,
        id: new_id(),
        actor_ap_id: &instance.ap_id,
        to: &to,
        cc: &[],
        answered: &follow,
        published: &now,
    });
    send_to_relay(state, instance, &undo, &subscription.inbox).await
}

/// The `Follow` behind the instance actor's relay subscription: the
/// document [`follow_json_to`] renders for the pair, under the row's stored
/// activity id (decision 6 — the id the relay's `Accept` names back and
/// the `Undo` embeds) and, per the ticket, addressed to the relay's
/// *inbox* — the endpoint that must receive it, which is also why
/// [`send_to_relay`] names that inbox directly instead of letting the
/// publisher resolve an actor address.
fn relay_follow_document(
    config: &Config,
    row: &Follow,
    follower: &User,
    target: &User,
    relay_inbox: &str,
) -> Option<Value> {
    let stored_id = row.ap_id.as_deref()?;
    let follow = FollowActivity {
        id: stored_id,
        follower_ap_id: &follower.ap_id,
        target_ap_id: &target.ap_id,
        published: &row.inserted_at,
    };
    Some(follow_json_to(config, &follow, &[relay_inbox]))
}

/// Hand one built handshake document to the publisher addressed to the
/// relay's inbox alone: `only_to` replaces the document's addressing so
/// no follower expansion runs and no other relay joins in, and
/// `extra_inboxes` names the one endpoint. The delivery queue signs with
/// the instance actor's key.
async fn send_to_relay(
    state: &AppState,
    instance: &User,
    document: &Value,
    inbox: &str,
) -> AppResult<()> {
    let opts = PublishOpts {
        extra_inboxes: vec![inbox.to_owned()],
        only_to: Some(Vec::new()),
    };
    let count = publisher::publish(state, instance, document, opts).await?;
    debug!(
        relay_inbox = inbox,
        count, "queued a relay handshake delivery"
    );
    Ok(())
}

/// The relay actor's address `relay_url_or_actor` names: a
/// scheme-qualified string is an actor URL verbatim; anything else must
/// read as a bare host (no path separator, whitespace or authority
/// markers) and names the conventional relay actor
/// `https://{host}/relay`.
///
/// # Errors
///
/// [`AppError::Unprocessable`] for the empty string and for text that is
/// neither a URL nor a host.
fn relay_actor_url(relay_url_or_actor: &str) -> AppResult<String> {
    let trimmed = relay_url_or_actor.trim();
    if trimmed.is_empty() {
        return Err(AppError::Unprocessable(
            "no relay address was given".to_owned(),
        ));
    }
    let folded = trimmed.to_ascii_lowercase();
    if folded.starts_with("http://") || folded.starts_with("https://") {
        return Ok(trimmed.to_owned());
    }
    if trimmed.contains('/') || trimmed.contains('@') || trimmed.contains(char::is_whitespace) {
        return Err(AppError::Unprocessable(format!(
            "`{trimmed}` is neither a relay actor URL nor a host"
        )));
    }
    Ok(format!("https://{trimmed}/relay"))
}

/// The gate on the fetched actor's kind: [`RELAY_ACTOR_TYPES`] wear a
/// relay's documents; anything else is a refusal, not a misdelivery.
fn ensure_relay_actor_type(relay_user: &User) -> AppResult<()> {
    if RELAY_ACTOR_TYPES.contains(&relay_user.actor_type.as_str()) {
        return Ok(());
    }
    Err(AppError::Unprocessable(format!(
        "the relay actor is a {}, not one of Application, Service or Person",
        relay_user.actor_type
    )))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::models::user::{NewUser, insert};

    /// A scheme-qualified address passes verbatim, outer whitespace
    /// trimmed; a bare host names the conventional `/relay` actor; and
    /// the empty and malformed forms are refused without touching
    /// anything.
    #[test]
    fn the_relay_address_reads_as_an_actor_url_or_a_host() {
        for url in [
            "https://relay.example/relay",
            "http://127.0.0.1:4000/actor",
            "https://relay.example",
            " HTTPS://Relay.Example/Relay ",
        ] {
            assert_eq!(
                relay_actor_url(url).expect("a scheme-qualified address is an actor URL"),
                url.trim(),
                "`{url}` passes through"
            );
        }
        assert_eq!(
            relay_actor_url("relay.example").expect("a bare host resolves"),
            "https://relay.example/relay",
            "the conventional relay actor on that host"
        );
        assert_eq!(
            relay_actor_url("relay.example:8090").expect("a host with a port resolves too"),
            "https://relay.example:8090/relay"
        );
        for bad in ["", "   ", "relay.example/path", "user@relay.example", "a b"] {
            assert!(
                relay_actor_url(bad).is_err(),
                "`{bad}` names neither a URL nor a host"
            );
        }
    }

    /// The relay `Follow` carries the row's stored `ap_id` verbatim and is
    /// addressed to the relay's inbox alone; an id-less row has no document.
    #[tokio::test]
    async fn the_relay_follow_carries_the_stored_id_and_the_relay_inbox() {
        let db = crate::test_db::test_db().await.expect("test database slot");
        let pool = db.pool().clone();
        let [follower, target] = ["instance", "relay"].map(|name| NewUser {
            nickname: format!("{name}@doc.example"),
            ap_id: format!("https://doc.example/{name}"),
            domain: Some("doc.example".to_owned()),
            actor_type: "Service".to_owned(),
            ..NewUser::default()
        });
        let follower = insert(&pool, &follower).await.expect("follower inserts");
        let target = insert(&pool, &target).await.expect("target inserts");
        let stored = "https://doc.example/activities/stored-follow";
        let relay_inbox = "https://doc.example/relay/inbox";
        let now = chrono::Utc::now();
        let mut row = Follow {
            id: uuid::Uuid::nil(),
            follower_id: follower.id,
            followee_id: target.id,
            state: follow::FollowState::Accepted,
            ap_id: Some(stored.to_owned()),
            show_reblogs: true,
            notify: false,
            inserted_at: now,
            updated_at: now,
        };
        let config = Config::for_tests();

        let document = relay_follow_document(&config, &row, &follower, &target, relay_inbox)
            .expect("a row with an id has a document");
        assert_eq!(document["id"], stored, "the stored ap_id, not a minted one");
        assert_eq!(document["to"], serde_json::json!([relay_inbox]));
        assert_eq!(document["actor"], follower.ap_id.as_str());
        assert_eq!(document["object"], target.ap_id.as_str());

        row.ap_id = None;
        assert!(
            relay_follow_document(&config, &row, &follower, &target, relay_inbox).is_none(),
            "an id-less row has no document"
        );
    }

    /// The type gate: the three kinds a relay is served as pass, the
    /// other mirrored kinds are refused.
    #[tokio::test]
    async fn only_an_application_service_or_person_is_a_relay() {
        let db = crate::test_db::test_db().await.expect("test database slot");
        let pool = db.pool().clone();
        for (n, kind) in ["Application", "Service", "Person", "Group", "Organization"]
            .into_iter()
            .enumerate()
        {
            let user = insert(
                &pool,
                &NewUser {
                    nickname: format!("type-{n}@types.example"),
                    ap_id: format!("https://types.example/{kind}"),
                    domain: Some("types.example".to_owned()),
                    actor_type: kind.to_owned(),
                    ..NewUser::default()
                },
            )
            .await
            .expect("the fixture user inserts");
            let outcome = ensure_relay_actor_type(&user);
            assert_eq!(
                outcome.is_ok(),
                RELAY_ACTOR_TYPES.contains(&kind),
                "test {n}: a {kind} actor {}",
                if RELAY_ACTOR_TYPES.contains(&kind) {
                    "passes"
                } else {
                    "is refused"
                }
            );
        }
    }
}
