//! Inbound `Update` and `Delete` of an actor: what a peer editing its own
//! profile, or retiring its own account, does to this instance.
//!
//! The reference is Pleroma's `Pleroma.Web.ActivityPub.SideEffects`
//! (`lib/pleroma/web/activity_pub/side_effects.ex`): its `Update` clause
//! splits on whether the object the activity carries is an actor and, when
//! it is, rebuilds the stored user from the document; its `Delete` clause
//! resolves the deleted id to an object or to a `User` and, for a user,
//! calls `User.delete` — `perform :delete` in `lib/pleroma/user.ex`, which
//! unfollows every follower and every followed account and deletes the
//! account's activities and the notifications they caused. The ticket rules
//! are authoritative where the two differ:
//!
//! * [`update_actor`], registered for the five actor object types
//!   [`parse_actor`] accepts, folds the embedded document through the same
//!   parse-and-upsert pair the
//!   fetcher uses, so profile, keys, endpoints and fields move together —
//!   plus the two columns that upsert deliberately leaves alone,
//!   `moved_to_ap_id` and `also_known_as`, whenever the document changed
//!   them. It runs only when the object's `id` *is* the activity's `actor`
//!   (an `Update` of somebody else's document is nothing to fold), and not
//!   more often than once per `MIN_UPDATE_INTERVAL` per account, so a peer
//!   stuck redelivering one profile change cannot make this instance rewrite
//!   the mirror forever.
//! * [`delete_actor`] is the `Delete` route for the objects
//!   `delete_by_object_type` does not claim, which is where a bare id has to
//!   be resolved. When it names the sending actor's own account the account
//!   is gone: the mirror is deactivated with `deleted_at` stamped and the
//!   [`users.delete_remote`](crate::jobs::deletion::KIND_DELETE_REMOTE)
//!   sweep is queued to take its statuses, relationships and interactions
//!   with it, keeping the row. An id that names anything else is somebody
//!   else's business — the deletion of a status is the status-delete
//!   ticket's entry in that same table — and is ignored.
//!
//! Both handlers run behind the dispatcher's own gates: containment has
//! already proved the activity's `actor` to be the signer's `ap_id`
//! ([`crate::federation::dispatch`]), so the [`User`] a handler is handed
//! *is* the account the activity speaks about.

use chrono::{DateTime, TimeDelta, Utc};
use futures::future::BoxFuture;
use sqlx::PgPool;
use tracing::debug;

use crate::{
    error::AppError,
    federation::{
        dispatch::{ActivityView, FederationError, Handlers, Outcome},
        remote_actor::{ActorError, NewRemoteUser, parse_actor, upsert_remote_user},
    },
    jobs::{
        QUEUE_BACKGROUND,
        deletion::{KIND_DELETE_REMOTE, job_args},
    },
    models::{
        job::{NewJob, enqueue},
        user::{self, User},
    },
    state::AppState,
};

/// The object types an `Update` of an actor may carry: the same closed set
/// [`parse_actor`] accepts as actor kinds, which is what makes this
/// module's registrations "actor objects only" and leaves
/// `update_by_object_type` free to gain `Note` with the status-edit ticket.
const ACTOR_OBJECT_TYPES: [&str; 5] = ["Person", "Service", "Group", "Organization", "Application"];

/// The flood guard: how long a mirror written by a fetch or by an earlier
/// `Update` answers for itself. Ten seconds — far shorter than the fetcher's
/// own refresh window, so it stops a peer repeating one profile change in a
/// loop rather than delaying a legitimate edit.
const MIN_UPDATE_INTERVAL: TimeDelta = TimeDelta::seconds(10);

/// Folds this module's actor routes into the dispatcher's registry.
///
/// One `Update` entry per object type in the module's actor-kind list — the
/// five kinds `parse_actor` mirrors — and [`delete_actor`] as the `Delete`
/// route for the objects no type-specific entry claims.
///
/// `crate::federation::handlers::registry` calls it, which is the whole
/// composition of the live registry.
pub fn register(handlers: &mut Handlers) {
    for object_type in ACTOR_OBJECT_TYPES {
        handlers.register_update(object_type, update_actor);
    }
    handlers.register_delete_fallback(delete_actor);
}

/// The `Update` handler of an actor object: mirrors the embedded document
/// into the stored account.
///
/// # Errors
///
/// [`FederationError::Transient`] when a database failure stopped the
/// mirror from being written; anything wrong with the *document* is reported
/// as [`Outcome::Ignored`], not as an error.
pub fn update_actor(
    state: AppState,
    view: ActivityView,
    signer: User,
) -> BoxFuture<'static, Result<Outcome, FederationError>> {
    Box::pin(async move { apply_update(state, view, signer).await })
}

/// The `Delete` fallback: the account deletion a peer announces about
/// itself, resolved from the object's id.
///
/// # Errors
///
/// [`FederationError::Transient`] when a database failure stopped the
/// account from being marked deleted or the sweep from being queued.
pub fn delete_actor(
    state: AppState,
    view: ActivityView,
    signer: User,
) -> BoxFuture<'static, Result<Outcome, FederationError>> {
    Box::pin(async move { apply_delete(state, view, signer).await })
}

/// The body of [`update_actor`], in the order the ticket fixes: the shape
/// checks, the flood guard, the two columns the mirror write leaves alone,
/// then the mirror write itself.
async fn apply_update(
    state: AppState,
    view: ActivityView,
    signer: User,
) -> Result<Outcome, FederationError> {
    let Some(object) = view.object.embedded() else {
        return Ok(Outcome::Ignored(
            "an Update whose object is a bare reference",
        ));
    };
    if signer.local.get() {
        return Ok(Outcome::Ignored(
            "an Update of an account this instance owns",
        ));
    }
    if view.object.id() != Some(view.actor.as_str()) {
        debug!(
            object_id = view.object.id().unwrap_or_default(),
            "an Update of a document that is not the sending actor's own; ignoring"
        );
        return Ok(Outcome::Ignored(
            "an Update whose object id is not the actor's own id",
        ));
    }
    if within_update_interval(signer.last_fetched_at) {
        return Ok(Outcome::Ignored(
            "an Update inside the minimum interval since the last write",
        ));
    }
    let parsed = match parse_actor(object) {
        Ok(parsed) => parsed,
        Err(err) => return refused(err),
    };
    // Compared against the row as it stands before the update: that is what
    // "changed" means here, and the upsert below owns every column it
    // touches, so the two writes cannot disagree.
    store_move_fields(&state.db, &signer, &parsed).await?;
    let updated = match upsert_remote_user(&state.db, parsed).await {
        Ok(stored) => stored,
        Err(err) => return refused(err),
    };
    debug!(account = %updated.nickname, "an Update folded a remote actor's new profile");
    Ok(Outcome::Handled)
}

/// The body of [`delete_actor`]: resolve the object's id, and retire the
/// account it names when that account is the sender itself.
///
/// The mark and the queueing are two statements, so a failure between them
/// leaves an inactive account whose sweep arrives with the retry: the
/// activity's ledger row goes back for a transient failure, and folding the
/// activity again writes the same mark and queues an idempotent sweep.
async fn apply_delete(
    state: AppState,
    view: ActivityView,
    signer: User,
) -> Result<Outcome, FederationError> {
    let Some(object_id) = view.object.id() else {
        return Ok(Outcome::Ignored("a Delete whose object names no id"));
    };
    if object_id != view.actor {
        debug!(
            object_id,
            "a Delete whose object is not the sending actor's own id; ignoring"
        );
        return Ok(Outcome::Ignored(
            "a Delete whose object is not the actor's own id",
        ));
    }
    if signer.local.get() {
        return Ok(Outcome::Ignored(
            "a Delete of an account this instance owns",
        ));
    }
    user::mark_deleted(&state.db, signer.id)
        .await
        .map_err(|err| transient("deletion mark", &err))?;
    let queued = enqueue(
        &state.db,
        NewJob {
            queue: QUEUE_BACKGROUND,
            kind: KIND_DELETE_REMOTE,
            args: job_args(signer.id),
            run_at: None,
            max_attempts: None,
            unique_key: None,
        },
    )
    .await
    .map_err(|err| transient("delete sweep", &err))?;
    debug!(
        account = %signer.nickname,
        queued = queued.is_some(),
        "a remote account deleted itself"
    );
    Ok(Outcome::Handled)
}

/// Whether a mirror written since `stamp` still answers for itself, the
/// flood guard of [`update_actor`]: `true` for a stamp in the future as
/// well, because a peer whose clock runs ahead is no reason to fold its
/// updates more often. `None` — an account stored with no fetch stamp — is
/// not too fresh.
fn within_update_interval(stamp: Option<DateTime<Utc>>) -> bool {
    stamp.is_some_and(|fetched| Utc::now().signed_duration_since(fetched) < MIN_UPDATE_INTERVAL)
}

/// What a refused actor document means for the activity: a document that is
/// not a usable actor, and a nickname another stored account already owns,
/// are nothing to fold — ignored, with the reason in the debug log. (The
/// second is the takeover shape Pleroma renames its stale row for; this
/// instance refuses the write and says so.) A database failure is the one
/// arm a retry can fix.
fn refused(err: ActorError) -> Result<Outcome, FederationError> {
    match err {
        ActorError::Failed(err) => Err(transient("actor mirror", &err)),
        ActorError::Invalid(_) => {
            debug!(%err, "an Update's embedded document is not a usable actor");
            Ok(Outcome::Ignored(
                "an Update whose object is not a valid actor document",
            ))
        }
        ActorError::NicknameConflict => {
            debug!(%err, "an Update's nickname belongs to another stored actor");
            Ok(Outcome::Ignored(
                "an Update whose nickname is owned by a different actor id",
            ))
        }
    }
}

/// Writes the two `users` columns [`upsert_remote_user`] leaves alone — the
/// move target and the alias list — but only the ones whose stored value
/// differs from what the document says, so an `Update` repeating the current
/// profile writes nothing new. A document that drops a field the mirror has
/// stores the clearing: the peer's own document is the authority on its
/// account.
async fn store_move_fields(
    pool: &PgPool,
    stored: &User,
    parsed: &NewRemoteUser,
) -> Result<(), FederationError> {
    if parsed.moved_to != stored.moved_to_ap_id {
        user::set_moved_to(pool, stored.id, parsed.moved_to.as_deref())
            .await
            .map_err(|err| transient("move target", &err))?;
    }
    if parsed.also_known_as != stored.also_known_as {
        user::set_also_known_as(pool, stored.id, &parsed.also_known_as)
            .await
            .map_err(|err| transient("aliases", &err))?;
    }
    Ok(())
}

/// The retry arm every database failure in these handlers maps onto: the
/// writes here are idempotent, so the queue's next attempt can run the same
/// activity again.
fn transient(what: &str, err: &AppError) -> FederationError {
    FederationError::Transient(format!("could not store the remote actor's {what}: {err}"))
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;

    use serde_json::{Value, json};
    use sqlx::PgPool;
    use sqlx::postgres::PgPoolOptions;
    use uuid::Uuid;

    use super::*;
    use crate::{
        config::Config,
        http_client::HttpClient,
        jobs::AttemptInfo,
        models::{
            follow::{self, Follow},
            status::{self, NewStatus, Status},
            user::{Flag, NewUser, insert},
        },
        uploads::UploaderBackend,
    };

    /// The mirror every fixture here speaks about: alice on `peer.example`,
    /// the actor id the signer rows and the activities agree on.
    const ALICE: &str = "https://peer.example/users/alice";
    /// The instance origin the local fixtures and the follow rows are built
    /// on, as `Config::for_tests` names it.
    const BASE: &str = "https://test.example";
    /// A public key the parser accepts: only its being a non-empty string is
    /// what `parse_actor` checks.
    const PEM: &str =
        "-----BEGIN PUBLIC KEY-----\nMIIBIjANBgkqhkiG9w0BAQEFAAOCAQ8A\n-----END PUBLIC KEY-----\n";
    /// The replacement key one update brings.
    const NEW_PEM: &str = "-----BEGIN PUBLIC KEY-----\nNEWKEYMATERIAL\n-----END PUBLIC KEY-----\n";

    /// Test state over `pool` with the fixed test config.
    fn app_state(pool: &PgPool) -> AppState {
        let config = Arc::new(Config::for_tests());
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

    /// Test state over a pool that cannot answer: the database having gone
    /// away, as the handlers see it.
    async fn broken_state() -> AppState {
        let pool = PgPoolOptions::new()
            .connect_lazy("postgres://localhost/unused")
            .expect("a lazy pool needs no database");
        pool.close().await;
        app_state(&pool)
    }

    /// alice's `Person` document as a fetch last saw it: what `parse_actor`
    /// requires, plus the profile the assertions watch change.
    fn person() -> Value {
        json!({
            "id": ALICE,
            "type": "Person",
            "preferredUsername": "alice",
            "name": "Alice",
            "summary": "<p>the old note</p>",
            "inbox": "https://peer.example/users/alice/inbox",
            "outbox": "https://peer.example/users/alice/outbox",
            "followers": "https://peer.example/users/alice/followers",
            "icon": {"type": "Image", "url": "https://peer.example/avatar/old.png"},
            "attachment": [
                {"type": "PropertyValue", "name": "Site", "value": "https://old.example"}
            ],
            "publicKey": {
                "id": "https://peer.example/users/alice#main-key",
                "owner": ALICE,
                "publicKeyPem": PEM,
            },
        })
    }

    /// Normalizes an activity document the way the dispatcher does: the
    /// `Update` or `Delete` named by `id`, sent by `actor`, carrying
    /// `object`. A fixture that stopped parsing says so through `expect`.
    fn view(kind: &str, id: &str, actor: &str, object: Value) -> ActivityView {
        let mut activity = json!({
            "id": format!("https://peer.example/activities/{id}"),
            "type": kind,
            "actor": actor,
        });
        let _object = activity
            .as_object_mut()
            .expect("the fixture skeleton is an object")
            .insert("object".to_owned(), object);
        ActivityView::parse(&activity).expect("the fixture activity parses")
    }

    /// The stored mirror of [`person`], as a completed fetch leaves it and
    /// then aged past the flood guard: the state an `Update` arrives in.
    async fn mirror(pool: &PgPool) -> User {
        let stored = upsert_remote_user(pool, parse_actor(&person()).expect("the fixture parses"))
            .await
            .expect("the mirror fixture stores");
        ready_for_update(pool, stored.id).await
    }

    /// Ages a mirror's fetch stamp and hands back the row as it stands
    /// afterwards: the account an `Update` may fold.
    async fn ready_for_update(pool: &PgPool, id: Uuid) -> User {
        let _aged = sqlx::query(
            "UPDATE users SET last_fetched_at = now() - interval '1 minute' WHERE id = $1",
        )
        .bind(id)
        .execute(pool)
        .await
        .expect("the fixture ages the fetch");
        fresh_user(pool, id).await
    }

    /// User `id` as it stands now.
    async fn fresh_user(pool: &PgPool, id: Uuid) -> User {
        user::get_by_id(pool, id)
            .await
            .expect("the user reads")
            .expect("the user exists")
    }

    /// A local account named `nickname` at `{BASE}/users/{nickname}`, for
    /// the arms that must not let an inbound activity write over one.
    async fn local_user(pool: &PgPool, nickname: &str) -> User {
        let ap_id = user::local_ap_id(BASE, nickname);
        insert(pool, &NewUser::local(nickname, &ap_id))
            .await
            .expect("the local fixture inserts")
    }

    /// A remote mirror named `nickname` on `peer.example`, at `ap_id`.
    async fn remote_user(pool: &PgPool, nickname: &str, ap_id: &str) -> User {
        insert(
            pool,
            &NewUser {
                nickname: format!("{nickname}@peer.example"),
                ap_id: ap_id.to_owned(),
                local: Flag::new(false),
                domain: Some("peer.example".to_owned()),
                ..NewUser::default()
            },
        )
        .await
        .expect("the remote fixture inserts")
    }

    /// One of the mirror's own posts, at a peer-named `uri`.
    async fn mirror_post(pool: &PgPool, author: &User, tag: &str) -> Status {
        let id = Uuid::now_v7();
        status::insert(
            pool,
            BASE,
            &NewStatus {
                user_id: author.id,
                uri: Some(format!("https://peer.example/objects/{tag}-{id}")),
                content: format!("{tag} says hello"),
                raw_text: format!("{tag} says hello"),
                ..NewStatus::default()
            },
        )
        .await
        .expect("the status fixture inserts")
    }

    /// The reason an ignored outcome carries, panicking when the handler did
    /// not ignore the activity at all.
    fn ignored(outcome: Result<Outcome, FederationError>) -> &'static str {
        match outcome {
            Ok(Outcome::Ignored(reason)) => reason,
            other => panic!("expected the activity to be ignored, got {other:?}"),
        }
    }

    /// Asserts a handler folded its activity.
    fn assert_handled(outcome: &Result<Outcome, FederationError>) {
        assert_eq!(
            outcome,
            &Ok(Outcome::Handled),
            "the handler folded the activity"
        );
    }

    /// The row of the `users.delete_remote` job queued for account `id`, as
    /// `(queue, kind, args)`.
    async fn queued_sweep(pool: &PgPool, id: Uuid) -> Option<(String, String, Value)> {
        sqlx::query_as::<_, (String, String, Value)>(
            "SELECT queue, kind, args FROM jobs WHERE kind = $1 AND args->>'user_id' = $2",
        )
        .bind(KIND_DELETE_REMOTE)
        .bind(id.to_string())
        .fetch_optional(pool)
        .await
        .expect("the job queue reads")
    }

    /// `follower` holds an accepted follow of `target` — granted through the
    /// model's own write, so the counters moved with it.
    async fn accepted_follow(pool: &PgPool, follower: &User, target: &User) -> Follow {
        let requested = follow::follow(pool, BASE, follower, target)
            .await
            .expect("the follow fixture is recorded");
        follow::accept(pool, requested.id)
            .await
            .expect("the fixture grants the request")
    }

    /// The profile half of an `Update`: name, note, avatar, fields and the
    /// endpoints the document changed all land on the mirror, and the write
    /// refreshes the stamp the flood guard reads.
    #[tokio::test]
    async fn an_update_folds_the_new_profile_into_the_mirror() {
        let db = crate::test_db::test_db().await.expect("test database slot");
        let pool = db.pool().clone();
        let state = app_state(&pool);
        let alice = mirror(&pool).await;
        let alice_id = alice.id;
        assert_eq!(alice.name, "Alice", "the fixture mirrors the old profile");

        let mut document = person();
        document["name"] = json!("Alice the Second");
        document["summary"] = json!("<p>the <b>new</b> note</p>");
        document["icon"] = json!({"type": "Image", "url": "https://peer.example/avatar/new.png"});
        document["attachment"] = json!([
            {"type": "PropertyValue", "name": "Site", "value": "https://new.example"},
        ]);
        document["inbox"] = json!("https://peer.example/users/alice/inbox2");
        document["endpoints"] = json!({"sharedInbox": "https://peer.example/inbox"});
        document["publicKey"]["publicKeyPem"] = json!(NEW_PEM);

        let folded = update_actor(
            state.clone(),
            view("Update", "profile-1", ALICE, document),
            alice,
        )
        .await;
        assert_handled(&folded);

        let stored = fresh_user(&pool, alice_id).await;
        assert_eq!(stored.name, "Alice the Second", "the display name moved");
        assert_eq!(stored.bio, "<p>the <b>new</b> note</p>", "the note moved");
        assert_eq!(stored.raw_bio, "the new note\n", "with its source text");
        assert_eq!(
            stored.avatar_url.as_deref(),
            Some("https://peer.example/avatar/new.png"),
            "the avatar moved"
        );
        assert_eq!(
            stored.fields,
            json!([{"name": "Site", "value": "https://new.example"}]),
            "the profile fields moved"
        );
        assert_eq!(
            stored.inbox.as_deref(),
            Some("https://peer.example/users/alice/inbox2"),
            "the inbox endpoint moved"
        );
        assert_eq!(
            stored.shared_inbox.as_deref(),
            Some("https://peer.example/inbox"),
            "and the shared inbox arrived"
        );
        assert_eq!(
            stored.public_key.as_deref(),
            Some(NEW_PEM),
            "and so did the signing key"
        );
        assert!(
            within_update_interval(stored.last_fetched_at),
            "the write is stamped as fresh, which is what arms the flood guard"
        );
    }

    /// The flood guard: a mirror a fetch or an earlier `Update` wrote less
    /// than ten seconds ago answers for itself, so neither the delivery that
    /// follows a fetch nor the one that follows a successful fold rewrites
    /// it.
    #[tokio::test]
    async fn an_update_that_arrives_too_soon_is_ignored() {
        let db = crate::test_db::test_db().await.expect("test database slot");
        let pool = db.pool().clone();
        let state = app_state(&pool);
        let just_fetched =
            upsert_remote_user(&pool, parse_actor(&person()).expect("the fixture parses"))
                .await
                .expect("the mirror fixture stores");
        let alice_id = just_fetched.id;
        assert!(
            within_update_interval(just_fetched.last_fetched_at),
            "a fetch that just ran is inside the guard"
        );

        let mut first = person();
        first["name"] = json!("First rewrite");
        let after_fetch = update_actor(
            state.clone(),
            view("Update", "flood-1", ALICE, first.clone()),
            just_fetched,
        )
        .await;
        assert!(
            ignored(after_fetch).contains("minimum interval"),
            "the update right after a fetch is ignored"
        );
        assert_eq!(
            fresh_user(&pool, alice_id).await.name,
            "Alice",
            "nothing landed"
        );

        let aged = ready_for_update(&pool, alice_id).await;
        let folded =
            update_actor(state.clone(), view("Update", "flood-2", ALICE, first), aged).await;
        assert_handled(&folded);
        assert_eq!(
            fresh_user(&pool, alice_id).await.name,
            "First rewrite",
            "out of the guard, the first update folds"
        );

        let mut second = person();
        second["name"] = json!("Second rewrite");
        let redelivered = update_actor(
            state.clone(),
            view("Update", "flood-3", ALICE, second),
            fresh_user(&pool, alice_id).await,
        )
        .await;
        assert!(
            ignored(redelivered).contains("minimum interval"),
            "the delivery right behind a fold is ignored, not folded"
        );
        assert_eq!(
            fresh_user(&pool, alice_id).await.name,
            "First rewrite",
            "and the profile it carried never landed"
        );
    }

    /// An `Update` whose object is not the sender's own document describes an
    /// account the signer has no authority over, and an object that is only a
    /// reference declares nothing to store: both ignored, mirror untouched.
    #[tokio::test]
    async fn an_update_of_another_actors_id_or_of_a_bare_id_is_ignored() {
        let db = crate::test_db::test_db().await.expect("test database slot");
        let pool = db.pool().clone();
        let state = app_state(&pool);
        let alice_id = mirror(&pool).await.id;
        let mallory = remote_user(&pool, "mallory", "https://peer.example/users/mallory").await;

        let mut document = person();
        document["id"] = json!("https://peer.example/users/mallory");
        document["name"] = json!("Mallory wearing alice's profile");
        let foreign = update_actor(
            state.clone(),
            view("Update", "wrong-id-1", ALICE, document),
            fresh_user(&pool, alice_id).await,
        )
        .await;
        assert!(
            ignored(foreign).contains("object id"),
            "a document that is not the sender's own is ignored"
        );
        assert_eq!(
            fresh_user(&pool, alice_id).await.name,
            "Alice",
            "alice's mirror is untouched"
        );
        assert_eq!(
            fresh_user(&pool, mallory.id).await.name,
            "",
            "and so is the account the document names"
        );

        let referenced = update_actor(
            state.clone(),
            view("Update", "bare-id-1", ALICE, json!(ALICE)),
            fresh_user(&pool, alice_id).await,
        )
        .await;
        assert!(
            ignored(referenced).contains("bare reference"),
            "an Update of an id alone declares nothing to store"
        );
        assert_eq!(
            fresh_user(&pool, alice_id).await.name,
            "Alice",
            "and it rewrote nothing"
        );
    }

    /// A document that is not a usable actor, one whose nickname another
    /// stored account owns, and an account this instance owns are all
    /// ignored rather than retried: no redelivery makes any of them foldable.
    #[tokio::test]
    async fn an_unfoldable_update_is_ignored_not_retried() {
        let db = crate::test_db::test_db().await.expect("test database slot");
        let pool = db.pool().clone();
        let state = app_state(&pool);
        let alice = mirror(&pool).await;
        let alice_id = alice.id;
        let squatter = remote_user(&pool, "mallory", "https://other.example/users/mallory").await;
        let ours = local_user(&pool, "alice-here").await;
        let ours_id = ours.id;

        let mut keyless = person();
        keyless["publicKey"] = json!(null);
        let broken = update_actor(
            state.clone(),
            view("Update", "invalid-1", ALICE, keyless),
            alice,
        )
        .await;
        assert!(
            ignored(broken).contains("valid actor document"),
            "a keyless document is refused"
        );

        let mut renamed = person();
        renamed["preferredUsername"] = json!("mallory");
        let takeover = update_actor(
            state.clone(),
            view("Update", "conflict-1", ALICE, renamed),
            ready_for_update(&pool, alice_id).await,
        )
        .await;
        assert!(
            ignored(takeover).contains("nickname is owned"),
            "a takeover-shaped nickname is refused"
        );
        assert_eq!(
            fresh_user(&pool, squatter.id).await.ap_id,
            "https://other.example/users/mallory",
            "the squatter's row was neither renamed nor overwritten"
        );

        let local = update_actor(
            state.clone(),
            view("Update", "local-1", ALICE, person()),
            ours,
        )
        .await;
        assert!(
            ignored(local).contains("this instance owns"),
            "a local account's profile is not an inbound activity's to write"
        );

        let kept = fresh_user(&pool, alice_id).await;
        assert_eq!(kept.name, "Alice", "nothing any of them carried landed");
        assert!(
            fresh_user(&pool, ours_id).await.is_active.get(),
            "and the local row is exactly as it was"
        );
    }

    /// `movedTo` and `alsoKnownAs` land when the document changes them, stay
    /// put when it repeats them, and clear again when the peer takes them
    /// back.
    #[tokio::test]
    async fn an_update_stores_moved_to_and_aliases() {
        let db = crate::test_db::test_db().await.expect("test database slot");
        let pool = db.pool().clone();
        let state = app_state(&pool);
        let alice = mirror(&pool).await;
        let alice_id = alice.id;
        assert!(
            alice.moved_to_ap_id.is_none() && alice.also_known_as.is_empty(),
            "the fixture moved nowhere and has no aliases"
        );

        let mut document = person();
        document["movedTo"] = json!("https://new.example/users/alice");
        document["alsoKnownAs"] = json!(["https://old.example/users/alice"]);
        let folded = update_actor(
            state.clone(),
            view("Update", "moved-1", ALICE, document.clone()),
            alice,
        )
        .await;
        assert_handled(&folded);

        let moved = fresh_user(&pool, alice_id).await;
        assert_eq!(
            moved.moved_to_ap_id.as_deref(),
            Some("https://new.example/users/alice"),
            "the move target is stored"
        );
        assert_eq!(
            moved.also_known_as,
            ["https://old.example/users/alice"],
            "the alias list is stored"
        );
        assert!(
            moved.last_move_at.is_none(),
            "a remote account's move is mirrored, not performed here, so no cooldown stamp"
        );

        let repeated = update_actor(
            state.clone(),
            view("Update", "moved-2", ALICE, document),
            ready_for_update(&pool, alice_id).await,
        )
        .await;
        assert_handled(&repeated);
        let same = fresh_user(&pool, alice_id).await;
        assert_eq!(
            same.moved_to_ap_id.as_deref(),
            Some("https://new.example/users/alice"),
            "repeating the same profile leaves the target alone"
        );
        assert_eq!(
            same.also_known_as,
            ["https://old.example/users/alice"],
            "and the alias list"
        );

        let mut undone = person();
        undone["name"] = json!("Alice is back");
        let cleared = update_actor(
            state.clone(),
            view("Update", "moved-3", ALICE, undone),
            ready_for_update(&pool, alice_id).await,
        )
        .await;
        assert_handled(&cleared);
        let back = fresh_user(&pool, alice_id).await;
        assert_eq!(back.name, "Alice is back", "the profile update landed");
        assert!(
            back.moved_to_ap_id.is_none() && back.also_known_as.is_empty(),
            "a document that no longer names them clears the mirror's copy"
        );
    }

    /// A database that cannot answer is transient: the queue retries, and the
    /// ledger row goes back for the retry to find.
    #[tokio::test]
    async fn an_update_against_a_dead_database_is_transient() {
        let db = crate::test_db::test_db().await.expect("test database slot");
        let pool = db.pool().clone();
        let alice = mirror(&pool).await;

        let outcome = update_actor(
            broken_state().await,
            view("Update", "dead-1", ALICE, person()),
            alice,
        )
        .await;

        assert!(
            matches!(outcome, Err(FederationError::Transient(_))),
            "the mirror write failing asks for another attempt: {outcome:?}"
        );
    }

    /// A `Delete` of the sender's own id retires the mirror — inactive, with
    /// `deleted_at` stamped — and queues the sweep on the `background` queue
    /// the ticket names; running that sweep takes the mirror's statuses and
    /// the local follows of it.
    #[tokio::test]
    async fn a_delete_of_its_own_account_deactivates_the_mirror_and_queues_the_sweep() {
        let db = crate::test_db::test_db().await.expect("test database slot");
        let pool = db.pool().clone();
        let state = app_state(&pool);
        let alice = mirror(&pool).await;
        let alice_id = alice.id;
        let bob = local_user(&pool, "bob").await;
        let _following = accepted_follow(&pool, &bob, &alice).await;
        let posted = mirror_post(&pool, &alice, "farewell").await;

        let gone = delete_actor(
            state.clone(),
            view("Delete", "delete-1", ALICE, json!(ALICE)),
            alice,
        )
        .await;
        assert_handled(&gone);

        let retired = fresh_user(&pool, alice_id).await;
        assert!(!retired.is_active.get(), "the mirror is deactivated");
        assert!(retired.deleted_at.is_some(), "and marked deleted");
        assert_eq!(retired.ap_id, ALICE, "the row itself stays for the refs");
        let job = queued_sweep(&pool, alice_id)
            .await
            .expect("the sweep was queued");
        assert_eq!(job.0, QUEUE_BACKGROUND, "on the background queue");
        assert_eq!(job.1, KIND_DELETE_REMOTE, "as the remote sweep");
        assert_eq!(job.2, json!({"user_id": alice_id}), "carrying the account");

        let sweep = crate::job_registry()
            .get(KIND_DELETE_REMOTE)
            .expect("the sweep kind is registered");
        let attempt = AttemptInfo {
            attempts: 1,
            max_attempts: 5,
        };
        sweep(state, attempt, job_args(alice_id))
            .await
            .expect("the queued sweep runs");

        assert!(
            status::get_by_id(&pool, posted.id)
                .await
                .expect("the status reads")
                .is_none(),
            "the departed account's post is no longer a live status"
        );
        assert!(
            follow::get(&pool, bob.id, alice_id)
                .await
                .expect("the follow reads")
                .is_none(),
            "and bob stops following it"
        );
        assert_eq!(
            fresh_user(&pool, bob.id).await.following_count,
            0,
            "with his counter stepped down"
        );
    }

    /// The other spellings of a self-deletion — a `Tombstone` object, and an
    /// embedded actor document — resolve through the object's id to the same
    /// retirement.
    #[tokio::test]
    async fn a_self_deletion_arrives_as_an_id_a_tombstone_or_a_document() {
        let db = crate::test_db::test_db().await.expect("test database slot");
        let pool = db.pool().clone();
        let state = app_state(&pool);
        let alice = mirror(&pool).await;
        let alice_id = alice.id;

        let tombstoned = delete_actor(
            state.clone(),
            view(
                "Delete",
                "delete-2",
                ALICE,
                json!({"type": "Tombstone", "id": ALICE}),
            ),
            alice,
        )
        .await;
        assert_handled(&tombstoned);
        let gone = fresh_user(&pool, alice_id).await;
        assert!(
            gone.deleted_at.is_some() && !gone.is_active.get(),
            "the tombstone spelled its deletion"
        );

        let embedded = delete_actor(
            state.clone(),
            view("Delete", "delete-3", ALICE, person()),
            gone,
        )
        .await;
        assert_handled(&embedded);
    }

    /// An id that is not the sender's own account is not this handler's
    /// business: a status id is the status-delete ticket's route, another
    /// account's id is nobody's to delete here, an object that names no id
    /// resolves to nothing, and an account this instance owns is deleted by
    /// its own endpoint. All ignored, nothing queued.
    #[tokio::test]
    async fn a_delete_of_anything_else_is_ignored() {
        let db = crate::test_db::test_db().await.expect("test database slot");
        let pool = db.pool().clone();
        let state = app_state(&pool);
        let alice_id = mirror(&pool).await.id;
        let ours = local_user(&pool, "alice-here").await;
        let ours_id = ours.id;

        let status_delete = delete_actor(
            state.clone(),
            view(
                "Delete",
                "other-1",
                ALICE,
                json!("https://peer.example/posts/gone"),
            ),
            fresh_user(&pool, alice_id).await,
        )
        .await;
        assert!(
            ignored(status_delete).contains("not the actor's own id"),
            "a status id is the status ticket's route"
        );

        let foreign_actor = delete_actor(
            state.clone(),
            view(
                "Delete",
                "other-2",
                ALICE,
                json!("https://peer.example/users/mallory"),
            ),
            fresh_user(&pool, alice_id).await,
        )
        .await;
        assert!(
            ignored(foreign_actor).contains("not the actor's own id"),
            "another account's id names no account this delivery may retire"
        );

        let idless = delete_actor(
            state.clone(),
            view("Delete", "other-3", ALICE, json!({"type": "Tombstone"})),
            fresh_user(&pool, alice_id).await,
        )
        .await;
        assert!(
            ignored(idless).contains("names no id"),
            "an object with no id resolves to nothing"
        );

        let local = delete_actor(
            state.clone(),
            view("Delete", "other-4", &ours.ap_id, json!(ours.ap_id.clone())),
            ours,
        )
        .await;
        assert!(
            ignored(local).contains("this instance owns"),
            "a local account is not federation's to delete"
        );

        let kept = fresh_user(&pool, alice_id).await;
        assert!(kept.is_active.get(), "none of them touched the mirror");
        assert!(
            kept.deleted_at.is_none(),
            "and none of them marked anything deleted"
        );
        assert!(
            fresh_user(&pool, ours_id).await.deleted_at.is_none(),
            "the local row stayed live"
        );
        let queued: i64 = sqlx::query_scalar("SELECT count(*) FROM jobs WHERE kind = $1")
            .bind(KIND_DELETE_REMOTE)
            .fetch_one(&pool)
            .await
            .expect("the job queue reads");
        assert_eq!(queued, 0, "an ignored Delete queues nothing");
    }

    /// A `Delete` whose database cannot answer is transient, and the account
    /// stays live for the retry to find.
    #[tokio::test]
    async fn a_delete_against_a_dead_database_is_transient() {
        let db = crate::test_db::test_db().await.expect("test database slot");
        let pool = db.pool().clone();
        let alice_id = mirror(&pool).await.id;

        let outcome = delete_actor(
            broken_state().await,
            view("Delete", "dead-2", ALICE, json!(ALICE)),
            fresh_user(&pool, alice_id).await,
        )
        .await;

        assert!(
            matches!(outcome, Err(FederationError::Transient(_))),
            "the mark failing asks for another attempt: {outcome:?}"
        );
        assert!(
            fresh_user(&pool, alice_id).await.is_active.get(),
            "the healthy pool still has a live account to retry for"
        );
    }
}
