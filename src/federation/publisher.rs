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
//!   [`json::references`] normalizes;
//! - the public address (`https://www.w3.org/ns/activitystreams#Public` and
//!   its two shorthands) is never itself a recipient — it says "anyone",
//!   and "anyone" is spelled out as the followers collection and the
//!   directly addressed actors;
//! - the actor's own followers-collection URL expands to the actor's
//!   *remote, accepted, active* followers
//!   ([`remote_followers`](crate::models::follow::remote_followers)), each
//!   reached at their shared inbox when they publish one (one POST per
//!   instance, [`best_inbox`]) and their personal inbox otherwise — this is
//!   the sender-side expansion of the collection;
//! - every other address is looked up by `ap_id`, and a REMOTE user it
//!   finds contributes their *personal* inbox: a directly addressed peer
//!   gets the activity at the endpoint that names them (mentions and direct
//!   recipients; unknown addresses are skipped, deactivated or suspended
//!   mirrors and local rows are not delivery targets, and neither the actor
//!   nor this instance's own actor ever receives a copy);
//! - relay inboxes ([`relay_inboxes`](crate::models::instance::relay_inboxes),
//!   the empty placeholder until the relay ticket lands) join the set for
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
//! no followers and no mentions simply computes the empty set and enqueues
//! nothing, which is also what happens to a `local` status published by
//! mistake while the relay hook is still empty).

use std::collections::HashSet;

use futures::future::BoxFuture;
use serde::Deserialize;
use serde_json::{Value, json};
use sqlx::PgPool;
use tracing::debug;
use url::Url;
use uuid::Uuid;

use crate::{
    error::AppResult,
    federation::{
        delivery::{best_inbox, enqueue_delivery},
        json,
        mrf::{self, MrfDecision},
        note_parser::PUBLIC_RECIPIENTS,
    },
    jobs::{AttemptInfo, HandlerFuture, JobError, QUEUE_FEDERATOR_OUTGOING},
    models::{
        follow, instance, instance_policy_domain,
        job::{NewJob, enqueue},
        user::{INSTANCE_ACTOR_NICKNAME, User, get_by_ap_id, get_by_id},
    },
    state::AppState,
};

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

/// The [`KIND_PUBLISH`] handler, registered by [`crate::serve`]: loads the
/// actor and runs the ordinary [`publish`] with default options.
pub fn handle(state: AppState, _attempt: AttemptInfo, args: Value) -> HandlerFuture {
    Box::pin(async move { run_publish(state, args).await })
}

/// Runs one background publish. What can never work (malformed arguments,
/// an activity with no id, an actor that is gone or not a local one)
/// discards; what may work later (a database hiccup in the fan-out)
/// retries.
async fn run_publish(state: AppState, args: Value) -> Result<(), JobError> {
    let args = serde_json::from_value::<PublishArgs>(args)
        .map_err(|err| JobError::Discard(format!("malformed publish arguments: {err}")))?;
    if !activity_has_id(&args.activity) {
        return Err(JobError::Discard(
            "the activity carries no string `id`".to_owned(),
        ));
    }
    let actor = publishing_actor(&state, args.actor_id).await?;
    let opts = PublishOpts {
        extra_inboxes: args.extra_inboxes,
        only_to: None,
    };
    let count = publish(&state, &actor, &args.activity, opts)
        .await
        .map_err(|err| {
            JobError::Retry(format!(
                "could not publish an activity as {}: {err}",
                args.actor_id
            ))
        })?;
    debug!(actor_id = %args.actor_id, count, "background publish enqueued deliveries");
    Ok(())
}

/// Reads the actor to publish as, by id: only a row this instance owns can
/// address its own followers. Gone and remote rows are permanent failures;
/// a database that will not answer is transient.
async fn publishing_actor(state: &AppState, actor_id: Uuid) -> Result<User, JobError> {
    let found = get_by_id(&state.db, actor_id)
        .await
        .map_err(|err| JobError::Retry(format!("could not read the publishing actor: {err}")))?;
    found
        .filter(|candidate| candidate.local.get())
        .ok_or_else(|| JobError::Discard(format!("actor {actor_id} is not a local publisher")))
}

/// Whether `activity` carries the string `id` every federating document
/// must have — the condition [`enqueue_delivery`] refuses on, checked here
/// so a doomed job discards before any fan-out query runs.
fn activity_has_id(activity: &Value) -> bool {
    activity.get("id").and_then(Value::as_str).is_some()
}

/// Every address the activity's addressing properties name, each in the
/// shapes [`json::references`] normalizes; absent and unreadable
/// properties contribute nothing.
fn addressed_to(activity: &Value) -> Vec<String> {
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
async fn expand_recipients(
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
/// [`deliverable_inboxes`].
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

/// Whether any address in `addresses` is the public address — the
/// condition that makes relays eligible for the activity.
fn includes_public(addresses: &[String]) -> bool {
    addresses.iter().any(|address| is_public(address))
}

/// Whether `address` is the public address in any of its three
/// [`PUBLIC_RECIPIENTS`] spellings.
fn is_public(address: &str) -> bool {
    PUBLIC_RECIPIENTS.contains(&address)
}

/// Whether `activity` is the actor's own `Delete`, `Undo` or `Update` — the
/// self-broadcasts relays must see beside public activities.
fn relays_should_see(activity: &Value, actor_ap_id: &str) -> bool {
    let is_self_broadcast = activity
        .get("type")
        .and_then(json::first_type)
        .is_some_and(|kind| SELF_BROADCAST_TYPES.contains(&kind.as_str()));
    is_self_broadcast && names_reference(activity.get("actor"), actor_ap_id)
}

/// Whether `value` references `ap_id` — a string equal to it or an object
/// whose `id` (or `href`/`url`) is.
fn names_reference(value: Option<&Value>, ap_id: &str) -> bool {
    value
        .and_then(json::value_or_id)
        .is_some_and(|reference| reference == ap_id)
}

/// The final target set: candidates deduplicated by URL, each kept only
/// when it is an `http(s)` inbox on a foreign authority whose host neither
/// instance policy rejects — by table row nor by the [`mrf`] domain lists
/// ([`MrfConfig::rejected`]) — nor the unreachable ledger marks past `grace`.
///
/// [`mrf`]: crate::federation::mrf
/// [`MrfConfig::rejected`]: crate::federation::mrf::MrfConfig::rejected
async fn deliverable_inboxes(
    pool: &PgPool,
    policy: &mrf::MrfConfig,
    actor: &User,
    candidates: Vec<String>,
    grace: chrono::Duration,
) -> AppResult<Vec<String>> {
    let mut seen: HashSet<String> = HashSet::new();
    let mut inboxes = Vec::new();
    for candidate in candidates {
        if !seen.insert(candidate.clone()) {
            continue;
        }
        let Some(host) = deliverable_host(&candidate, &actor.ap_id) else {
            continue;
        };
        if instance_policy_domain::is_rejected(pool, &host).await?
            || policy.rejected(&host)
            || instance::is_host_unreachable(pool, &host, grace).await?
        {
            debug!(inbox = %candidate, "delivery target dropped: the host is refused");
            continue;
        }
        inboxes.push(candidate);
    }
    Ok(inboxes)
}

/// The configured [`UNREACHABLE_GRACE_SECS`](crate::config::UNREACHABLE_GRACE_SECS_VAR)
/// as the span [`is_host_unreachable`](crate::models::instance::is_host_unreachable)
/// compares a mark against: `secs` seconds, and [`Duration::MAX`](chrono::Duration::MAX)
/// for a value too large to represent as a span — the literal reading of a
/// window that outlasts every mark is a gate that never closes, which is what
/// such a value asks for.
///
/// The one conversion from the setting's `usize` seconds to the interval the
/// lookup binds, done once per publish so the per-inbox loop passes a span.
fn unreachable_grace(secs: usize) -> chrono::Duration {
    chrono::Duration::try_seconds(i64::try_from(secs).unwrap_or(i64::MAX))
        .unwrap_or(chrono::Duration::MAX)
}

/// The host of `inbox`, when it is a deliverable `http(s)` URL on an
/// authority other than the publishing actor's own — an inbox on this
/// instance's origin holds local recipients, whom a delivery would only
/// ask this process to `POST` to itself.
fn deliverable_host(inbox: &str, actor_ap_id: &str) -> Option<String> {
    let parsed = Url::parse(inbox).ok()?;
    if !matches!(parsed.scheme(), "http" | "https") {
        return None;
    }
    let host = parsed.host_str()?;
    let local = Url::parse(actor_ap_id).is_ok_and(|actor_id| {
        actor_id
            .authority()
            .eq_ignore_ascii_case(parsed.authority())
    });
    (!local).then(|| host.to_owned())
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;

    use chrono::{Duration, Utc};
    use serde_json::json;
    use sqlx::PgPool;

    use super::*;
    use crate::{
        config::{Config, TEST_BASE_URL, Toggle},
        federation::delivery::KIND_DELIVER,
        http_client::HttpClient,
        models::{
            follow::FollowState,
            instance_setting,
            job::Job,
            user::{NewUser, insert, local_ap_id},
        },
        services::instance_actor::ensure_instance_actor,
        uploads::UploaderBackend,
    };

    /// The full spelling of the public address.
    const PUBLIC: &str = "https://www.w3.org/ns/activitystreams#Public";
    /// The activity id every fixture carries.
    const ACTIVITY_ID: &str = "https://test.example/activities/1";
    /// The personal inbox the `other.example` mirror publishes.
    const ERIN_INBOX: &str = "https://other.example/users/erin/inbox";
    /// The id of the second activity a test publishes, so a re-publish of the
    /// first one's inbox cannot be swallowed by the delivery's deduplication.
    const CORRECTION_ID: &str = "https://test.example/activities/2";
    /// The grace window [`Config::for_tests`] carries: zero seconds, so the
    /// publisher skips a host the moment the ledger marks it.
    const NO_GRACE: Duration = Duration::zero();
    /// How far in the past a test's mark goes, so the comparison with the
    /// database's clock is settled rather than a race.
    const ONE_MINUTE_AGO: Duration = Duration::minutes(1);

    /// The state under test over `pool` and `config`.
    fn state_with(pool: &PgPool, config: Config) -> AppState {
        let config = Arc::new(config);
        AppState {
            db: pool.clone(),
            hub: crate::streaming::hub::Hub::new(pool),
            config: Arc::clone(&config),
            http_client: HttpClient::from(config.as_ref()),
            uploader: UploaderBackend::from(config.as_ref()),
            instance_stats_cache: Arc::default(),
            mrf_config_cache: Arc::default(),
            translation_languages_cache: Arc::default(),
            rate_limiter: Arc::new(crate::rate_limit::RateLimiter::from_config(config.as_ref())),
            mailer: Arc::new(crate::mailer::MemoryTransport::new()),
        }
    }

    /// The default test state: federation on, the standard test config.
    fn test_state(pool: &PgPool) -> AppState {
        state_with(pool, Config::for_tests())
    }

    /// The publishing actor: local `alice` on the test origin, with the
    /// inbox and followers-collection URL a real actor row carries.
    async fn alice(pool: &PgPool) -> User {
        let ap_id = local_ap_id(TEST_BASE_URL, "alice");
        insert(
            pool,
            &NewUser {
                inbox: Some(format!("{ap_id}/inbox")),
                followers_url: Some(format!("{ap_id}/followers")),
                ..NewUser::local("alice", &ap_id)
            },
        )
        .await
        .unwrap()
    }

    /// Inserts a remote mirror `nickname@host` with the given inbox
    /// endpoints, for the rows fan-out resolves addresses to.
    async fn mirror(
        pool: &PgPool,
        nickname: &str,
        host: &str,
        inbox: &str,
        shared: Option<&str>,
    ) -> User {
        insert(
            pool,
            &NewUser {
                nickname: format!("{nickname}@{host}"),
                ap_id: format!("https://{host}/users/{nickname}"),
                domain: Some(host.to_owned()),
                inbox: Some(inbox.to_owned()),
                shared_inbox: shared.map(str::to_owned),
                ..NewUser::default()
            },
        )
        .await
        .unwrap()
    }

    /// Grants `follower` an accepted follow of `target` (a remote follower
    /// of a local, unlocked account is granted on the spot).
    async fn followed(pool: &PgPool, follower: &User, target: &User) {
        let row = follow::follow(pool, TEST_BASE_URL, follower, target)
            .await
            .unwrap();
        assert_eq!(
            row.state,
            FollowState::Accepted,
            "a follow of an unlocked local account is granted at once"
        );
    }

    /// Inserts one instance-policy `reject` row.
    async fn policy_reject(pool: &PgPool, domain: &str) {
        let inserted =
            sqlx::query("INSERT INTO instance_policy_domains (domain, action) VALUES ($1, $2)")
                .bind(domain)
                .bind("reject")
                .execute(pool)
                .await
                .unwrap();
        assert_eq!(inserted.rows_affected(), 1, "the policy row is there");
    }

    /// Deactivates a stored mirror the way a suspend or deactivation would.
    async fn deactivate(pool: &PgPool, user_id: Uuid) {
        let updated = sqlx::query("UPDATE users SET is_active = false WHERE id = $1")
            .bind(user_id)
            .execute(pool)
            .await
            .unwrap();
        assert_eq!(
            updated.rows_affected(),
            1,
            "the mirror is there to deactivate"
        );
    }

    /// The fixture `Create` note by `actor`, addressed to the public and to
    /// `actor`'s followers collection.
    fn public_activity(actor: &User) -> Value {
        json!({
            "@context": "https://www.w3.org/ns/activitystreams",
            "id": ACTIVITY_ID,
            "type": "Create",
            "actor": actor.ap_id,
            "to": [PUBLIC],
            "cc": [actor.followers_url],
        })
    }

    /// Runs [`publish`] and fails the test on any error.
    async fn publish_ok(
        state: &AppState,
        actor: &User,
        activity: &Value,
        opts: PublishOpts,
    ) -> usize {
        publish(state, actor, activity, opts).await.unwrap()
    }

    /// [`publish_ok`] with the options every plain fan-out test uses.
    async fn publish_default(state: &AppState, actor: &User, activity: &Value) -> usize {
        publish_ok(state, actor, activity, PublishOpts::default()).await
    }

    /// The inboxes the `federator.deliver` jobs queued so far ask for,
    /// sorted, so fan-out sets compare directly.
    async fn delivered_inboxes(pool: &PgPool) -> Vec<String> {
        let rows = sqlx::query_as::<_, (String,)>(
            "SELECT args->>'inbox' FROM jobs WHERE kind = $1 ORDER BY 1",
        )
        .bind(KIND_DELIVER)
        .fetch_all(pool)
        .await
        .unwrap();
        rows.into_iter().map(|(inbox,)| inbox).collect()
    }

    /// How many jobs the database holds, of any kind.
    async fn job_count(pool: &PgPool) -> i64 {
        let (count,): (i64,) = sqlx::query_as("SELECT count(*) FROM jobs")
            .fetch_one(pool)
            .await
            .unwrap();
        count
    }

    /// The first attempt a background handler runs on.
    const fn first_attempt() -> AttemptInfo {
        AttemptInfo {
            attempts: 1,
            max_attempts: 5,
        }
    }

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

    /// How a [`handle`] run reads in a test message: the handler's own
    /// `Display`, since a `Result` has none.
    fn outcome_text(outcome: &Result<(), JobError>) -> String {
        match outcome {
            Ok(()) => "completed".to_owned(),
            Err(err) => err.to_string(),
        }
    }

    /// A public note fans out to the followers' remote inboxes, and two
    /// followers sharing one instance share one delivery.
    #[tokio::test]
    async fn a_public_note_fans_out_to_the_remote_followers() {
        let db = crate::test_db::test_db().await.expect("test database slot");
        let pool = db.pool().clone();
        let alice = alice(&pool).await;
        let carol = mirror(
            &pool,
            "carol",
            "peer.example",
            "https://peer.example/users/carol/inbox",
            Some("https://peer.example/inbox"),
        )
        .await;
        let dave = mirror(
            &pool,
            "dave",
            "peer.example",
            "https://peer.example/users/dave/inbox",
            Some("https://peer.example/inbox"),
        )
        .await;
        let erin = mirror(
            &pool,
            "erin",
            "other.example",
            "https://other.example/users/erin/inbox",
            None,
        )
        .await;
        let frank = mirror(
            &pool,
            "frank",
            "third.example",
            "https://third.example/inbox",
            None,
        )
        .await;
        for follower in [&carol, &dave, &erin, &frank] {
            followed(&pool, follower, &alice).await;
        }
        let state = test_state(&pool);

        let count = publish_ok(
            &state,
            &alice,
            &public_activity(&alice),
            PublishOpts::default(),
        )
        .await;

        assert_eq!(count, 3, "three hosts, three deliveries");
        assert_eq!(
            delivered_inboxes(&pool).await,
            vec![
                "https://other.example/users/erin/inbox",
                "https://peer.example/inbox",
                "https://third.example/inbox",
            ],
            "the two peer.example followers share their instance's POST"
        );
    }

    /// A directly addressed remote user is reached at their personal
    /// inbox even when they publish a shared one.
    #[tokio::test]
    async fn a_mentioned_remote_user_gets_their_personal_inbox() {
        let db = crate::test_db::test_db().await.expect("test database slot");
        let pool = db.pool().clone();
        let alice = alice(&pool).await;
        let bob = mirror(
            &pool,
            "bob",
            "peer.example",
            "https://peer.example/users/bob/inbox",
            Some("https://peer.example/inbox"),
        )
        .await;
        let state = test_state(&pool);
        let activity = json!({
            "id": ACTIVITY_ID,
            "type": "Create",
            "actor": alice.ap_id,
            "to": [bob.ap_id, PUBLIC],
            "cc": [alice.followers_url],
        });

        let count = publish_ok(&state, &alice, &activity, PublishOpts::default()).await;

        assert_eq!(count, 1, "the mention is the only resolved recipient");
        assert_eq!(
            delivered_inboxes(&pool).await,
            vec!["https://peer.example/users/bob/inbox"],
            "a direct address delivers to the personal inbox, not the shared one"
        );
    }

    /// With `only_to`, followers expansion does not happen: the named
    /// recipients alone receive the activity.
    #[tokio::test]
    async fn a_direct_message_goes_only_to_the_mentioned() {
        let db = crate::test_db::test_db().await.expect("test database slot");
        let pool = db.pool().clone();
        let alice = alice(&pool).await;
        let carol = mirror(
            &pool,
            "carol",
            "peer.example",
            "https://peer.example/users/carol/inbox",
            Some("https://peer.example/inbox"),
        )
        .await;
        followed(&pool, &carol, &alice).await;
        let bob = mirror(
            &pool,
            "bob",
            "other.example",
            "https://other.example/users/bob/inbox",
            None,
        )
        .await;
        let state = test_state(&pool);
        let opts = PublishOpts {
            extra_inboxes: Vec::new(),
            only_to: Some(vec![bob.ap_id.clone()]),
        };

        let count = publish_ok(&state, &alice, &public_activity(&alice), opts).await;

        assert_eq!(count, 1, "the addressed follower set is replaced wholesale");
        assert_eq!(
            delivered_inboxes(&pool).await,
            vec!["https://other.example/users/bob/inbox"],
            "only the mentioned actor receives the direct message"
        );
    }

    /// Local followers are never delivery targets — their timelines are
    /// the delivery.
    #[tokio::test]
    async fn local_followers_never_receive_a_delivery() {
        let db = crate::test_db::test_db().await.expect("test database slot");
        let pool = db.pool().clone();
        let alice = alice(&pool).await;
        let local_bob = insert(
            &pool,
            &NewUser {
                inbox: Some(format!("{TEST_BASE_URL}/users/bob/inbox")),
                followers_url: Some(format!("{TEST_BASE_URL}/users/bob/followers")),
                ..NewUser::local("bob", &local_ap_id(TEST_BASE_URL, "bob"))
            },
        )
        .await
        .unwrap();
        followed(&pool, &local_bob, &alice).await;
        let remote = mirror(
            &pool,
            "carol",
            "peer.example",
            "https://peer.example/users/carol/inbox",
            None,
        )
        .await;
        followed(&pool, &remote, &alice).await;
        let state = test_state(&pool);

        let count = publish_ok(
            &state,
            &alice,
            &public_activity(&alice),
            PublishOpts::default(),
        )
        .await;

        assert_eq!(count, 1, "only the remote follower is a target");
        assert_eq!(
            delivered_inboxes(&pool).await,
            vec!["https://peer.example/users/carol/inbox"],
            "no delivery is queued to this instance's own origin"
        );
    }

    /// A follower on a policy-rejected domain drops out of the set.
    #[tokio::test]
    async fn a_rejected_domain_is_excluded() {
        let db = crate::test_db::test_db().await.expect("test database slot");
        let pool = db.pool().clone();
        let alice = alice(&pool).await;
        policy_reject(&pool, "spam.example").await;
        let spammer = mirror(
            &pool,
            "spammer",
            "spam.example",
            "https://spam.example/inbox",
            None,
        )
        .await;
        followed(&pool, &spammer, &alice).await;
        let clean = mirror(
            &pool,
            "erin",
            "other.example",
            "https://other.example/users/erin/inbox",
            None,
        )
        .await;
        followed(&pool, &clean, &alice).await;
        let state = test_state(&pool);

        let count = publish_ok(
            &state,
            &alice,
            &public_activity(&alice),
            PublishOpts::default(),
        )
        .await;

        assert_eq!(count, 1, "the rejected host is dropped from the set");
        assert_eq!(
            delivered_inboxes(&pool).await,
            vec!["https://other.example/users/erin/inbox"],
            "only the reachable follower receives a job"
        );
    }

    /// A follower whose host the MRF domain lists reject is dropped the
    /// same way: allowlist mode means this instance fans out only to the
    /// hosts it chose, and the enqueue would refuse the others anyway.
    #[tokio::test]
    async fn a_domain_the_mrf_lists_reject_is_excluded() {
        let db = crate::test_db::test_db().await.expect("test database slot");
        let pool = db.pool().clone();
        instance_setting::set(
            &pool,
            "mrf",
            &json!({
                "policies": ["simple"],
                "simple": {"accept": [["other.example", "the one we talk to"]]},
            }),
        )
        .await
        .unwrap();
        let alice = alice(&pool).await;
        let stranger = mirror(
            &pool,
            "stranger",
            "spam.example",
            "https://spam.example/inbox",
            None,
        )
        .await;
        followed(&pool, &stranger, &alice).await;
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

        assert_eq!(count, 1, "the allowlisted host is the only target");
        assert_eq!(
            delivered_inboxes(&pool).await,
            vec!["https://other.example/users/erin/inbox"],
            "the local publisher itself still federates: the chain exempts its own domain"
        );
    }

    /// The same inbox reached by three different routes — as a follower's
    /// inbox, as a mention's inbox and as an extra — costs one delivery.
    #[tokio::test]
    async fn one_inbox_is_enqueued_once() {
        let db = crate::test_db::test_db().await.expect("test database slot");
        let pool = db.pool().clone();
        let alice = alice(&pool).await;
        let personal = "https://peer.example/users/erin/inbox";
        let erin = mirror(&pool, "erin", "peer.example", personal, None).await;
        followed(&pool, &erin, &alice).await;
        let state = test_state(&pool);
        let activity = json!({
            "id": ACTIVITY_ID,
            "type": "Create",
            "actor": alice.ap_id,
            "to": [erin.ap_id, PUBLIC],
            "cc": [alice.followers_url],
        });
        let opts = PublishOpts {
            extra_inboxes: vec![personal.to_owned(), personal.to_owned()],
            only_to: None,
        };

        let count = publish_ok(&state, &alice, &activity, opts).await;

        assert_eq!(count, 1, "mention, follower and extra collapse to one");
        assert_eq!(
            delivered_inboxes(&pool).await,
            vec![personal],
            "and the queue holds one job for it"
        );
    }

    /// The instance actor and the publishing actor are never recipients,
    /// and neither is a deactivated mirror.
    #[tokio::test]
    async fn the_instance_actor_the_actor_and_the_suspended_are_excluded() {
        let db = crate::test_db::test_db().await.expect("test database slot");
        let pool = db.pool().clone();
        let config = Config::for_tests();
        let instance_actor = ensure_instance_actor(&pool, &config).await.unwrap();
        let alice = alice(&pool).await;
        let gone = mirror(
            &pool,
            "gone",
            "other.example",
            "https://other.example/users/gone/inbox",
            None,
        )
        .await;
        followed(&pool, &gone, &alice).await;
        deactivate(&pool, gone.id).await;
        let live = mirror(
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
            "to": [
                instance_actor.ap_id,
                alice.ap_id,
                live.ap_id,
                PUBLIC,
            ],
            "cc": [alice.followers_url],
        });

        let count = publish_ok(&state, &alice, &activity, PublishOpts::default()).await;

        assert_eq!(count, 1, "only the live remote mention remains");
        assert_eq!(
            delivered_inboxes(&pool).await,
            vec!["https://other.example/users/erin/inbox"],
            "the instance actor, the actor and the deactivated mirror are skipped"
        );
    }

    /// Unknown addresses contribute nothing; an activity addressed only to
    /// the public, from an actor with no followers, enqueues nothing.
    #[tokio::test]
    async fn unknown_recipients_are_skipped() {
        let db = crate::test_db::test_db().await.expect("test database slot");
        let pool = db.pool().clone();
        let alice = alice(&pool).await;
        let state = test_state(&pool);
        let activity = json!({
            "id": ACTIVITY_ID,
            "type": "Create",
            "actor": alice.ap_id,
            "to": [PUBLIC, "https://ghost.example/users/nobody"],
            "cc": [alice.followers_url],
        });

        let count = publish_ok(&state, &alice, &activity, PublishOpts::default()).await;

        assert_eq!(count, 0, "a public activity with nobody behind it");
        assert_eq!(job_count(&pool).await, 0, "and no job for anyone");
    }

    /// A second publish of the same activity finds the queue's per-pair
    /// deduplication: the job count stays at one per inbox.
    #[tokio::test]
    async fn a_repeated_publish_reuses_the_queued_delivery() {
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
        let activity = public_activity(&alice);

        let first = publish_ok(&state, &alice, &activity, PublishOpts::default()).await;
        let second = publish_ok(&state, &alice, &activity, PublishOpts::default()).await;

        assert_eq!((first, second), (1, 1), "both calls compute one target");
        assert_eq!(
            job_count(&pool).await,
            1,
            "the delivery's unique key swallows the repeat"
        );
    }

    /// With federation disabled nothing is computed or queued.
    #[tokio::test]
    async fn a_disabled_federation_publishes_nothing() {
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
        let mut config = Config::for_tests();
        config.federation_enabled = Toggle::new(false);
        let state = state_with(&pool, config);

        let count = publish_ok(
            &state,
            &alice,
            &public_activity(&alice),
            PublishOpts::default(),
        )
        .await;

        assert_eq!(count, 0, "an instance that does not federate sends nothing");
        assert_eq!(job_count(&pool).await, 0, "and queues nothing");
    }

    /// The unreachable hook is the `instances` ledger and the relay hook is
    /// still the empty placeholder: with no row for a host its inboxes are
    /// targets, a mark under the default zero grace drops them out of the set,
    /// and clearing the mark targets them again.
    #[tokio::test]
    async fn the_relay_and_unreachable_hooks_answer_the_placeholders() {
        let db = crate::test_db::test_db().await.expect("test database slot");
        let pool = db.pool().clone();
        assert!(
            instance::relay_inboxes(&pool).await.unwrap().is_empty(),
            "no relay is subscribed until the relay ticket lands"
        );
        assert!(
            !instance::is_host_unreachable(&pool, "peer.example", NO_GRACE)
                .await
                .unwrap(),
            "a host with no instances row is reachable"
        );

        let alice = alice(&pool).await;
        let erin = mirror(&pool, "erin", "other.example", ERIN_INBOX, None).await;
        followed(&pool, &erin, &alice).await;
        let state = test_state(&pool);

        let first_targets = publish_default(&state, &alice, &public_activity(&alice)).await;
        assert_eq!(first_targets, 1, "the follower's host is a target at first");
        assert_eq!(
            delivered_inboxes(&pool).await,
            vec![ERIN_INBOX],
            "and it is queued"
        );

        // A second activity: the queue's per-(activity, inbox) deduplication
        // would otherwise hide whether the skipped fan-out found a target.
        let correction = json!({
            "id": CORRECTION_ID,
            "type": "Create",
            "actor": alice.ap_id,
            "to": [PUBLIC],
            "cc": [alice.followers_url],
        });
        instance::set_unreachable(&pool, "other.example", Utc::now() - ONE_MINUTE_AGO)
            .await
            .unwrap();
        let marked_targets = publish_default(&state, &alice, &correction).await;
        assert_eq!(
            marked_targets, 0,
            "the marked host drops out of the target set"
        );
        assert_eq!(
            delivered_inboxes(&pool).await,
            vec![ERIN_INBOX],
            "and nothing new is queued for it"
        );

        instance::set_reachable(&pool, "other.example")
            .await
            .unwrap();
        let recovered_targets = publish_default(&state, &alice, &correction).await;
        assert_eq!(
            recovered_targets, 1,
            "and the cleared host is a target again"
        );
        assert_eq!(
            delivered_inboxes(&pool).await,
            vec![ERIN_INBOX, ERIN_INBOX],
            "queued once more, for the activity the skip missed"
        );
    }

    /// A self-broadcast runs the relay arm without changing the fan-out: the
    /// hook still answers the empty list, so the actor's own `Update` reaches
    /// its followers and nobody else.
    #[tokio::test]
    async fn an_actor_self_broadcast_runs_the_relay_arm() {
        let db = crate::test_db::test_db().await.expect("test database slot");
        let pool = db.pool().clone();
        let alice = alice(&pool).await;
        let erin = mirror(&pool, "erin", "other.example", ERIN_INBOX, None).await;
        followed(&pool, &erin, &alice).await;
        let state = test_state(&pool);
        let update = json!({
            "id": ACTIVITY_ID,
            "type": "Update",
            "actor": alice.ap_id,
            "object": alice.ap_id,
            "to": [alice.followers_url],
        });

        assert_eq!(
            publish_default(&state, &alice, &update).await,
            1,
            "the actor's Update fans out to followers and consults the empty relay hook"
        );
    }

    /// The background entry point queues one publish job, and its handler
    /// loads the actor and fans the activity out exactly as a direct
    /// publish would.
    #[tokio::test]
    async fn the_background_job_publishes_when_it_runs() {
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
        let activity = public_activity(&alice);

        publish_in_background(&state, alice.id, &activity, &[], None)
            .await
            .unwrap();
        let job = sqlx::query_as::<_, Job>("SELECT * FROM jobs WHERE kind = $1 LIMIT 1")
            .bind(KIND_PUBLISH)
            .fetch_one(&pool)
            .await
            .unwrap();
        assert_eq!(
            job.queue, QUEUE_FEDERATOR_OUTGOING,
            "the publish job rides the outgoing queue"
        );
        assert_eq!(job.args["actor_id"], json!(alice.id), "the actor argument");
        assert_eq!(job.args["activity"], activity, "the activity argument");

        let outcome = handle(state.clone(), first_attempt(), job.args.clone()).await;
        assert_eq!(outcome, Ok(()), "the handler completes");
        assert_eq!(
            delivered_inboxes(&pool).await,
            vec!["https://other.example/users/erin/inbox"],
            "the handler's fan-out queued the follower's delivery"
        );
    }

    /// The background job also carries the recipients the activity's own
    /// addressing does not name: the `extra_inboxes` argument reaches
    /// [`publish`] through [`handle`], and a second enqueue under the same
    /// `unique_key` is swallowed by the queue's dedup — the debounced
    /// publisher's contract.
    #[tokio::test]
    async fn the_background_job_carries_extra_recipients_and_debounces() {
        let db = crate::test_db::test_db().await.expect("test database slot");
        let pool = db.pool().clone();
        let alice = alice(&pool).await;
        let state = test_state(&pool);
        let activity = json!({
            "id": ACTIVITY_ID,
            "type": "Update",
            "actor": alice.ap_id,
            "object": alice.ap_id,
            "to": [PUBLIC],
        });
        let extra = vec!["https://extra.example/inbox".to_owned()];

        publish_in_background(&state, alice.id, &activity, &extra, Some("debounce/1"))
            .await
            .unwrap();
        publish_in_background(&state, alice.id, &activity, &extra, Some("debounce/1"))
            .await
            .unwrap();

        let jobs = sqlx::query_as::<_, Job>("SELECT * FROM jobs WHERE kind = $1")
            .bind(KIND_PUBLISH)
            .fetch_all(&pool)
            .await
            .unwrap();
        assert_eq!(jobs.len(), 1, "the repeat hits the live job's key");
        assert_eq!(jobs[0].args["extra_inboxes"], json!(extra));

        let outcome = handle(state.clone(), first_attempt(), jobs[0].args.clone()).await;
        assert_eq!(outcome, Ok(()), "the handler completes");
        assert_eq!(
            delivered_inboxes(&pool).await,
            vec!["https://extra.example/inbox"],
            "and delivered what the caller named beside the addressing"
        );
    }

    /// What the background handler can never publish — malformed
    /// arguments, an activity with no id, an actor that is gone or not a
    /// local one — discards instead of retrying.
    #[tokio::test]
    async fn the_background_handler_discards_undeliverable_work() {
        let db = crate::test_db::test_db().await.expect("test database slot");
        let pool = db.pool().clone();
        let alice = alice(&pool).await;
        let remote = mirror(
            &pool,
            "bob",
            "peer.example",
            "https://peer.example/users/bob/inbox",
            None,
        )
        .await;
        let state = test_state(&pool);
        let activity = public_activity(&alice);

        let cases = [
            (
                "junk arguments",
                json!({"inbox": "https://peer.example/inbox"}),
            ),
            (
                "no activity id",
                json!({"actor_id": alice.id, "activity": {"type": "Create"}}),
            ),
            (
                "a gone actor",
                json!({"actor_id": Uuid::now_v7(), "activity": activity}),
            ),
            (
                "a remote actor",
                json!({"actor_id": remote.id, "activity": activity}),
            ),
        ];
        for (what, args) in cases {
            let outcome = handle(state.clone(), first_attempt(), args).await;
            assert!(
                matches!(outcome, Err(JobError::Discard(_))),
                "{what} can never publish, got {}",
                outcome_text(&outcome)
            );
        }
        assert_eq!(job_count(&pool).await, 0, "no delivery was queued");
    }

    /// The worker knows the background publish kind.
    #[test]
    fn the_kind_is_registered_with_its_handler() {
        let registry = crate::job_registry();
        assert!(
            registry.get(KIND_PUBLISH).is_some(),
            "the worker knows `{KIND_PUBLISH}`"
        );
    }

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

    /// Relays qualify on the actor's own `Delete`/`Undo`/`Update` and on
    /// nothing else: another actor's self-broadcast and ordinary creates
    /// stay out.
    #[test]
    fn the_relay_rule_covers_self_broadcasts() {
        const ME: &str = "https://test.example/users/alice";
        for kind in SELF_BROADCAST_TYPES {
            assert!(
                relays_should_see(&json!({"type": kind, "actor": ME}), ME),
                "the actor's own `{kind}` reaches relays"
            );
        }
        assert!(
            relays_should_see(&json!({"type": ["Update"], "actor": {"id": ME}}), ME),
            "array types and object actors read the same"
        );
        assert!(
            !relays_should_see(&json!({"type": "Create", "actor": ME}), ME),
            "a Create is not a self-broadcast"
        );
        assert!(
            !relays_should_see(
                &json!({"type": "Update", "actor": "https://peer.example/users/bob"}),
                ME
            ),
            "another actor's Update is not alice's"
        );
        assert!(
            !relays_should_see(&json!({"type": "Delete"}), ME),
            "an activity naming no actor broadcasts nothing"
        );
    }

    /// Only foreign `http(s)` inboxes are deliverable targets; this
    /// instance's own origin and unparseable URLs are not.
    #[test]
    fn only_foreign_http_inboxes_are_deliverable() {
        let alice = "https://test.example/users/alice";
        assert_eq!(
            deliverable_host("https://peer.example/inbox", alice).as_deref(),
            Some("peer.example"),
            "a foreign https inbox"
        );
        assert_eq!(
            deliverable_host("http://peer.example:8080/inbox", alice).as_deref(),
            Some("peer.example"),
            "a port does not change the host"
        );
        assert_eq!(
            deliverable_host("https://test.example.evil.example/inbox", alice).as_deref(),
            Some("test.example.evil.example"),
            "a look-alike prefix host is foreign"
        );
        for own in [
            "https://test.example/users/bob/inbox",
            "https://test.example:443/inbox",
        ] {
            assert_eq!(
                deliverable_host(own, alice),
                None,
                "`{own}` is this instance's own origin"
            );
        }
        for undeliverable in ["ftp://peer.example/inbox", "not a url", ""] {
            assert_eq!(
                deliverable_host(undeliverable, alice),
                None,
                "`{undeliverable}` is no deliverable inbox"
            );
        }
    }
}
