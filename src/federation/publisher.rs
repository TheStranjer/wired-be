//! The publisher: who an outgoing activity goes to, and the fan-out that
//! sends it there.
//!
//! The reference is Pleroma's publishing pipeline
//! (`lib/pleroma/web/activity_pub/publisher.ex`'s `publish/2` and
//! `determine_inbox/2`, `lib/pleroma/user.ex`'s follower-inbox queries):
//! turn one activity's `ActivityPub` addressing into the set of remote
//! inboxes that should receive it, and queue one delivery per inbox.
//! [`super::delivery`] already owns the per-inbox half — [`enqueue_delivery`]
//! queues the signed `federator.deliver` job — so this module is the
//! producing side: [`publish`] computes the inboxes and calls it once per
//! unique address, answering how many deliveries it queued.
//!
//! The recipient rules mirror what Mastodon and Pleroma do on the sending
//! side:
//!
//! - the addressing properties `to`, `cc`, `bto`, `bcc` and `audience`
//!   carry the recipients, each in any of the JSON-LD shapes
//!   [`json::references`](crate::federation::json::references) normalizes;
//! - the public address (`https://www.w3.org/ns/activitystreams#Public` and
//!   its two shorthands) is never itself a recipient — it says "anyone",
//!   and "anyone" is spelled out as the followers collection and the
//!   directly addressed actors;
//! - the actor's own followers-collection URL expands to the actor's
//!   *remote, accepted, active* followers
//!   ([`remote_followers`](crate::models::follow::remote_followers)), each
//!   reached at their shared inbox when they publish one (one POST per
//!   instance, [`best_inbox`](super::delivery::best_inbox)) and their personal inbox otherwise — this is
//!   the sender-side expansion of the collection;
//! - every other address is looked up by `ap_id`, and a REMOTE user it
//!   finds contributes their *personal* inbox: a directly addressed peer
//!   gets the activity at the endpoint that names them (mentions and direct
//!   recipients; unknown addresses are skipped, deactivated or suspended
//!   mirrors and local rows are not delivery targets, and neither the actor
//!   nor this instance's own actor ever receives a copy);
//! - relay inboxes ([`relay_inboxes`](crate::models::instance::relay_inboxes),
//!   the `inbox` of every relay this instance follows and that has
//!   accepted the follow) join the set for
//!   activities with public addressing and for the actor's own
//!   `Delete`/`Undo`/`Update`, so relays keep their copies fresh;
//! - inboxes whose host instance policy rejects — by its own table or by
//!   the [`mrf`] domain lists — or whose
//!   [`instances`](crate::models::instance) row carries an unreachable mark
//!   older than [`UNREACHABLE_GRACE_SECS`](crate::config::UNREACHABLE_GRACE_SECS_VAR),
//!   are dropped
//!   ([`is_host_unreachable`](crate::models::instance::is_host_unreachable)),
//!   and so is everything that is not an `http(s)` inbox URL on a foreign
//!   authority;
//! - the surviving set is deduplicated by inbox URL, so four followers on
//!   one shared inbox cost one delivery.
//!
//! [`PublishOpts`] extends the computed set the way callers need:
//! `extra_inboxes` adds addresses the activity's own addressing does not
//! name, and `only_to` replaces the addressing entirely — the direct-
//! message case, where a status is for the mentioned actors alone and must
//! not fan out to followers even though the document still carries them.
//!
//! [`publish_in_background`] is the entry point API handlers use: it
//! queues one [`KIND_PUBLISH`] job on [`QUEUE_FEDERATOR_OUTGOING`] so a
//! request never waits on follower queries over a large follower set, and
//! [`handle`] is that job's consumer: it loads the actor by id and runs
//! the ordinary [`publish`].
//!
//! A `local`-visibility status never reaches this module: callers must not
//! call [`publish`] for one (an activity addressed only to `Public` with
//! no followers and no mentions otherwise enqueues nothing but the relay
//! fan-out the accepted subscriptions open — which is exactly what a
//! relay is subscribed to hear, and the reason a `local` status must
//! never reach here by mistake).

use futures::future::BoxFuture;
use serde::Deserialize;
use serde_json::{Value, json};
use tracing::debug;
use uuid::Uuid;

use crate::{
    error::AppResult,
    federation::{
        delivery::enqueue_delivery,
        mrf::{self, MrfDecision},
    },
    jobs::QUEUE_FEDERATOR_OUTGOING,
    models::{
        instance,
        job::{NewJob, enqueue},
        user::User,
    },
    state::AppState,
};

/// The tests of [`publish`]'s fan-out through the database.
#[cfg(test)]
mod fan_out;
/// The tests of the conditions that change a publish: federation off, the
/// relay and unreachable hooks, and an actor's self-broadcasts.
#[cfg(test)]
mod hooks;
/// The last fan-out stage: the deliverable, deduplicated inbox set.
mod inboxes;
/// The background job: the handler that runs a queued publish.
mod job;
/// The recipients an activity addresses and their candidate inboxes.
mod recipients;
/// The fixtures the children's tests share.
#[cfg(test)]
mod test_support;

use inboxes::{deliverable_inboxes, unreachable_grace};
pub use job::handle;
use recipients::{addressed_to, expand_recipients, includes_public, relays_should_see};

/// The job kind of a backgrounded fan-out, on
/// [`QUEUE_FEDERATOR_OUTGOING`], handled by [`handle`].
pub const KIND_PUBLISH: &str = "federator.publish";

/// The addressing properties whose values name recipients, in the order
/// [`publish`] reads them.
const ADDRESSING_FIELDS: [&str; 5] = ["to", "cc", "bto", "bcc", "audience"];

/// The activity types through which an actor corrects, retracts or deletes
/// what peers (and relays) hold about it: its own `Delete`, `Undo` and
/// `Update`. Pleroma's relay flow subscribes relays to exactly these, so
/// they join the target set beside the follower expansion.
const SELF_BROADCAST_TYPES: [&str; 3] = ["Delete", "Undo", "Update"];

/// What a caller adds to [`publish`]'s computation beyond the activity's
/// own addressing.
#[derive(Clone, Debug, Default)]
pub struct PublishOpts {
    /// Inbox URLs to deliver to no matter what the activity addresses —
    /// recipients the caller knows about and the document does not name.
    pub extra_inboxes: Vec<String>,
    /// When set, these addresses *replace* the activity's `to`, `cc`,
    /// `bto`, `bcc` and `audience` fields as the recipient set: the
    /// direct-message form, where only the named recipients receive the
    /// activity. Followers expansion and the public-address rules still
    /// apply to this list, so a caller can name the followers collection
    /// here too.
    pub only_to: Option<Vec<String>>,
}

/// The [`KIND_PUBLISH`] job arguments: the fields
/// [`publish_in_background`] writes and [`handle`] reads.
#[derive(Debug, Deserialize)]
struct PublishArgs {
    /// `users.id` of the local actor to publish as.
    actor_id: Uuid,
    /// The activity document to fan out.
    activity: Value,
    /// Recipients the activity's own addressing does not name, carried
    /// through to [`publish`] as [`PublishOpts::extra_inboxes`]; absent
    /// means none.
    #[serde(default)]
    extra_inboxes: Vec<String>,
}

/// The kind of the reject-stage seam of [`publish`]: a `fn` pointer so a
/// test can drive the reject path with a fake, the same way the
/// dispatcher's pipeline takes its [`mrf`](super::mrf) stage.
type OutgoingFilter = fn(AppState, Value) -> BoxFuture<'static, MrfDecision>;

/// Computes the delivery targets of `activity` as addressed by `actor` and
/// queues one [`enqueue_delivery`] per unique inbox, answering how many
/// deliveries were enqueued.
///
/// The set is the addressing described in the module docs: the recipients
/// of the activity's `to`, `cc`, `bto`, `bcc` and `audience` fields (or
/// [`PublishOpts::only_to`], when set), with the actor's followers
/// collection expanded to their remote followers' shared-or-personal
/// inboxes, every other resolved remote recipient at their personal inbox,
/// relay inboxes for public or self-broadcast activities,
/// [`PublishOpts::extra_inboxes`] added, and everything undeliverable —
/// local, refused, rejected, unreachable, unknown, already counted —
/// dropped.
///
/// Before any of that, the activity passes the outgoing half of the
/// [MRF](super::mrf) chain: a rejection delivers nothing (the answer is
/// `0`, the reason goes to the debug log), and a rewrite is what gets
/// addressed and delivered.
///
/// While `FEDERATION_ENABLED` is off nothing is computed or queued and the
/// answer is `0`, the same no-op [`enqueue_delivery`] performs per inbox.
///
/// # Errors
///
/// Any database failure reading followers or recipients arrives as
/// [`AppError::Database`](crate::error::AppError), and a body with no
/// string `id` as [`AppError::Unprocessable`](crate::error::AppError)
/// through [`enqueue_delivery`].
pub async fn publish(
    state: &AppState,
    actor: &User,
    activity: &Value,
    opts: PublishOpts,
) -> AppResult<usize> {
    publish_filtered(state, actor, activity, opts, mrf::filter_outgoing_boxed).await
}

/// [`publish`] over an injected policy stage — the real one for callers,
/// a fake for the tests of the reject path.
async fn publish_filtered(
    state: &AppState,
    actor: &User,
    activity: &Value,
    opts: PublishOpts,
    mrf_filter: OutgoingFilter,
) -> AppResult<usize> {
    if !state.config.federation_enabled.get() {
        debug!("publish skipped: federation is disabled");
        return Ok(0);
    }
    let activity = match mrf_filter(state.clone(), activity.clone()).await {
        MrfDecision::Reject(reason) => {
            debug!(reason = %reason, "the policy pipeline rejected the outgoing activity; nothing is delivered");
            return Ok(0);
        }
        MrfDecision::Accept(filtered) => filtered,
    };
    let addresses = opts
        .only_to
        .as_ref()
        .map_or_else(|| addressed_to(&activity), Clone::clone);
    let policy = mrf::load_config(state).await;
    let mut candidates = expand_recipients(&state.db, actor, &addresses).await?;
    if includes_public(&addresses) || relays_should_see(&activity, &actor.ap_id) {
        candidates.extend(instance::relay_inboxes(&state.db).await?);
    }
    candidates.extend(opts.extra_inboxes);
    let grace = unreachable_grace(state.config.unreachable_grace_secs);
    let targets = deliverable_inboxes(&state.db, policy.as_ref(), actor, candidates, grace).await?;
    for inbox in &targets {
        enqueue_delivery(state, inbox, actor, &activity).await?;
    }
    Ok(targets.len())
}

/// Queues one [`KIND_PUBLISH`] job on [`QUEUE_FEDERATOR_OUTGOING`] so an
/// API request returns without waiting for the fan-out's DB lookups.
///
/// The job carries `activity`, the id of the local actor to publish it as,
/// and `extra_inboxes` — recipients the activity's own addressing does not
/// name, which [`handle`] hands to [`publish`] through
/// [`PublishOpts::extra_inboxes`] — and runs as soon as a worker claims
/// it. Large follower sets are the reason this exists. Each publish call
/// enqueues its own job; the per-`(activity, inbox)` deduplication
/// [`enqueue_delivery`] applies is what keeps a repeated publish from
/// sending twice.
///
/// `unique_key` becomes the job row's deduplication key (see
/// [`enqueue`]): while a job with the same key is live the new job is
/// dropped, which is what lets a debounced publisher (a profile's
/// `Update`, say) collapse a burst of edits into the one queued fan-out.
///
/// # Errors
///
/// Any database failure inserting the job row arrives as
/// [`AppError::Database`](crate::error::AppError).
pub async fn publish_in_background(
    state: &AppState,
    actor_id: Uuid,
    activity: &Value,
    extra_inboxes: &[String],
    unique_key: Option<&str>,
) -> AppResult<()> {
    let _job = enqueue(
        &state.db,
        NewJob {
            queue: QUEUE_FEDERATOR_OUTGOING,
            kind: KIND_PUBLISH,
            args: json!({
                "actor_id": actor_id,
                "activity": activity,
                "extra_inboxes": extra_inboxes,
            }),
            run_at: None,
            max_attempts: None,
            unique_key,
        },
    )
    .await?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::test_support::*;
    use super::*;
    use crate::federation::delivery::KIND_DELIVER;

    /// The fake rejecting policy stage: the MRF arm answers `Reject`
    /// without looking at the document.
    fn reject_outgoing(_state: AppState, _activity: Value) -> BoxFuture<'static, MrfDecision> {
        Box::pin(async { MrfDecision::Reject("test policy".to_owned()) })
    }

    /// The fake rewriting policy stage: it stamps `content` into the
    /// activity and accepts the rewrite.
    fn rewrite_outgoing(_state: AppState, mut activity: Value) -> BoxFuture<'static, MrfDecision> {
        Box::pin(async move {
            activity["published"] = json!("1970-01-01T00:00:00.000Z");
            MrfDecision::Accept(activity)
        })
    }

    /// The MRF reject path on the way out: a rejected activity queues
    /// nothing at all — not even for a follower that would otherwise
    /// receive it.
    #[tokio::test]
    async fn a_rejected_outgoing_activity_delivers_nothing() {
        let db = crate::test_db::test_db().await.expect("test database slot");
        let pool = db.pool().clone();
        let alice = alice(&pool).await;
        let erin = mirror(
            &pool,
            "erin",
            "other.example",
            "https://other.example/users/erin/inbox",
            None,
        )
        .await;
        followed(&pool, &erin, &alice).await;
        let state = test_state(&pool);

        let count = publish_filtered(
            &state,
            &alice,
            &public_activity(&alice),
            PublishOpts::default(),
            reject_outgoing,
        )
        .await
        .unwrap();

        assert_eq!(count, 0, "the rejection stops the fan-out");
        assert_eq!(job_count(&pool).await, 0, "and no delivery is queued");
    }

    /// The rewritten document is what the fan-out addresses and
    /// delivers: the chain's answer replaces the input.
    #[tokio::test]
    async fn the_accepted_rewrite_is_what_gets_delivered() {
        let db = crate::test_db::test_db().await.expect("test database slot");
        let pool = db.pool().clone();
        let alice = alice(&pool).await;
        let erin = mirror(
            &pool,
            "erin",
            "other.example",
            "https://other.example/users/erin/inbox",
            None,
        )
        .await;
        let state = test_state(&pool);
        let activity = json!({
            "id": ACTIVITY_ID,
            "type": "Create",
            "actor": alice.ap_id,
            "to": [erin.ap_id],
        });

        let count = publish_filtered(
            &state,
            &alice,
            &activity,
            PublishOpts::default(),
            rewrite_outgoing,
        )
        .await
        .unwrap();

        assert_eq!(count, 1, "the accepted rewrite is delivered");
        let delivered: (Value,) = sqlx::query_as("SELECT args->'body' FROM jobs WHERE kind = $1")
            .bind(KIND_DELIVER)
            .fetch_one(&pool)
            .await
            .unwrap();
        assert_eq!(
            delivered.0["published"],
            json!("1970-01-01T00:00:00.000Z"),
            "and the queued delivery carries the rewritten document, not the original"
        );
    }

    /// The real policy stage on a default configuration accepts, so the
    /// ordinary [`publish`] path is unchanged by the pipeline hook.
    #[tokio::test]
    async fn the_default_chain_publishes_everything_unchanged() {
        let db = crate::test_db::test_db().await.expect("test database slot");
        let pool = db.pool().clone();
        let alice = alice(&pool).await;
        let erin = mirror(
            &pool,
            "erin",
            "other.example",
            "https://other.example/users/erin/inbox",
            None,
        )
        .await;
        followed(&pool, &erin, &alice).await;
        let state = test_state(&pool);

        let count = publish_ok(
            &state,
            &alice,
            &public_activity(&alice),
            PublishOpts::default(),
        )
        .await;

        assert_eq!(
            count, 1,
            "the accept-everything default lets the fan-out run"
        );
        assert_eq!(
            delivered_inboxes(&pool).await,
            vec!["https://other.example/users/erin/inbox"],
            "with the document untouched"
        );
    }
}
