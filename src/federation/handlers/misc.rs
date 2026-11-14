//! Inbound `Block` (+ its `Undo`), `Flag`, `Add`, `Remove` and `Move`: the
//! routes for activities about a relationship or an account's lifecycle,
//! not about a post or a profile document.
//!
//! The reference is Pleroma's `Pleroma.Web.ActivityPub.SideEffects`
//! (`lib/pleroma/web/activity_pub/side_effects.ex`) — its `Block` clause
//! calls `User.block`, its `Undo`-of-`Block` clause `User.unblock`, its
//! `Add`/`Remove` clauses `User.add_pinned_object_id` /
//! `User.remove_pinned_object_id` — together with the `Flag` and `Move`
//! clauses of `Transmogrifier.handle_incoming`
//! (`lib/pleroma/web/activity_pub/transmogrifier.ex`) and the containment
//! checks of its `object_validators/`. The ticket rules are authoritative
//! where the two differ:
//!
//! * [`handle_block`] mirrors a remote account's block of a *local* user:
//!   [`block::block`] writes the pair and severs the follows between them
//!   (its documented side effect), and [`block::set_ap_id`] records the
//!   delivered activity id on the row so the duplicate delivery stands as
//!   one fact and the `Undo` below can find it. An object naming no local
//!   account is somebody else's block — ignored.
//! * [`handle_undo_block`] is the `undo_by_type["Block"]` route: it lifts
//!   exactly the block *its actor placed* — [`block::unblock`] is keyed on
//!   the pair, so an `Undo` signed by anyone else removes nothing and is
//!   ignored. The blocked account comes from the embedded inner activity
//!   when the `Undo` carries one, and from the stored block row when it
//!   names the activity id alone ([`block::get_by_ap_id`]).
//! * [`handle_flag`] files the forwarded report: the object is a list of
//!   ids whose local-account entry names the target (a `Flag` about a
//!   non-local account is ignored, per Pleroma's `get_reported` reduced to
//!   the local audience), the rest are cited status uris matched by
//!   [`status::get_by_uri`] when stored, and `content` is the comment. The
//!   row is written with [`report::create`] as *forwarded* — a remote
//!   instance filing with ours is exactly that — and given the activity id
//!   through [`report::set_ap_id`], before the shared
//!   `after_report_created` hook ([`crate::api::reports`]) runs.
//! * [`handle_add`] and [`handle_remove`] fold the featured-collection
//!   pins: only a `target` equal to the actor's own `featured_url` means
//!   anything here (Pleroma's `Add`/`Remove` are that one collection for
//!   a remote actor), and only a stored status the actor authored can be
//!   pinned ([`pinned_status::pin`] / [`pinned_status::unpin`], idempotent
//!   both ways). An unknown status is ignored — fetching it is the later
//!   inbox-fetch ticket's job.
//! * [`handle_move`] is the account-migration announcement: the object
//!   must be the actor's own id, the target is fetched
//!   ([`get_or_fetch_by_ap_id`]) and must have claimed the old actor in
//!   its `alsoKnownAs` (the validator Pleroma's `Move` clause runs), and
//!   the origin's `moved_to_ap_id` is mirrored through
//!   [`user::set_moved_to`] — no [`mark_moved`] cooldown stamp, because
//!   this instance performed nothing. Then every local follower Pleroma
//!   would migrate ([`follow::move_followers_of`]) follows the new account
//!   and unfollows the old — skipped whole for a blocked pair — and is
//!   told about the move ([`notify::on_move`]).
//!
//! Every route runs behind the dispatcher's own gates: containment has
//! already proved the activity's `actor` to be the signer's `ap_id`
//! ([`crate::federation::dispatch`]), so the [`User`] a handler is handed
//! *is* the remote account the activity speaks for; the local-signer guard
//! each one keeps is the belt to that bracer.
//!
//! [`mark_moved`]: crate::models::user::mark_moved

use futures::future::BoxFuture;
use serde_json::Value;
use sqlx::PgPool;
use tracing::debug;
use uuid::Uuid;

use crate::{
    api::reports::after_report_created,
    error::{AppError, AppResult},
    federation::{
        dispatch::{ActivityView, FederationError, Handlers, Outcome},
        fetch_actor::{FetchError, get_or_fetch_by_ap_id},
        json,
    },
    models::{
        block, follow, pinned_status, report, status,
        user::{self, User},
    },
    services::notify,
    state::AppState,
};

/// Folds this module's routes into the dispatcher's registry.
///
/// The five types by their own names, plus the `Undo` of a `Block` in the
/// `Undo` table. `crate::federation::handlers::registry` calls it, which is
/// the whole composition of the live registry.
pub fn register(handlers: &mut Handlers) {
    handlers.register("Block", handle_block);
    handlers.register_undo("Block", handle_undo_block);
    handlers.register("Flag", handle_flag);
    handlers.register("Add", handle_add);
    handlers.register("Remove", handle_remove);
    handlers.register("Move", handle_move);
}

/// The `Block` route: mirror a remote account's block of a local user,
/// severing the pair's follows and recording the activity id.
///
/// # Errors
///
/// [`FederationError::Transient`] when a database failure stopped the
/// block from being written or its id recorded; anything wrong with the
/// *shape* of the activity is reported as [`Outcome::Ignored`].
pub fn handle_block(
    state: AppState,
    view: ActivityView,
    signer: User,
) -> BoxFuture<'static, Result<Outcome, FederationError>> {
    Box::pin(async move { apply_block(state, view, signer).await })
}

/// The `undo_by_type["Block"]` route: lift the block the signer placed.
///
/// # Errors
///
/// [`FederationError::Transient`] when a database failure stopped the
/// block from being read or removed; an unresolvable or unmatched `Undo`
/// is [`Outcome::Ignored`].
pub fn handle_undo_block(
    state: AppState,
    view: ActivityView,
    signer: User,
) -> BoxFuture<'static, Result<Outcome, FederationError>> {
    Box::pin(async move { apply_undo_block(state, view, signer).await })
}

/// The `Flag` route: file the forwarded report a remote instance sends
/// about a local account.
///
/// # Errors
///
/// [`FederationError::Transient`] when a database failure stopped the
/// report from being filed or its activity id recorded; a `Flag` naming no
/// local account is [`Outcome::Ignored`].
pub fn handle_flag(
    state: AppState,
    view: ActivityView,
    signer: User,
) -> BoxFuture<'static, Result<Outcome, FederationError>> {
    Box::pin(async move { apply_flag(state, view, signer).await })
}

/// The `Add` route: pin the actor's own stored status to its featured
/// collection.
///
/// # Errors
///
/// [`FederationError::Transient`] when a database failure stopped the pin
/// from being written; anything else has nothing to fold and is
/// [`Outcome::Ignored`].
pub fn handle_add(
    state: AppState,
    view: ActivityView,
    signer: User,
) -> BoxFuture<'static, Result<Outcome, FederationError>> {
    Box::pin(async move { apply_featured_change(state, view, signer, true).await })
}

/// The `Remove` route: take the actor's pin of its own stored status back.
///
/// # Errors
///
/// [`FederationError::Transient`] when a database failure stopped the pin
/// from being removed; anything else has nothing to fold and is
/// [`Outcome::Ignored`].
pub fn handle_remove(
    state: AppState,
    view: ActivityView,
    signer: User,
) -> BoxFuture<'static, Result<Outcome, FederationError>> {
    Box::pin(async move { apply_featured_change(state, view, signer, false).await })
}

/// The `Move` route: an account moved to a new actor; mirror the target
/// and migrate this instance's local followers.
///
/// # Errors
///
/// [`FederationError::Transient`] when the target fetch or any migration
/// write failed the way a retry can fix; a `Move` the validators refuse
/// — a mismatched object, an unfetchable target, a missing alias claim —
/// is [`Outcome::Ignored`].
pub fn handle_move(
    state: AppState,
    view: ActivityView,
    signer: User,
) -> BoxFuture<'static, Result<Outcome, FederationError>> {
    Box::pin(async move { apply_move(state, view, signer).await })
}

/// The body of [`handle_block`]: the local-signer guard, the local-target
/// resolution, the model's block-and-sever write, and the activity id that
/// makes the pair's row *this* activity's mirror.
async fn apply_block(
    state: AppState,
    view: ActivityView,
    signer: User,
) -> Result<Outcome, FederationError> {
    if signer.local.get() {
        return Ok(Outcome::Ignored(
            "a Block signed by an account this instance owns",
        ));
    }
    let target = resolve_local_target(&state.db, &view)
        .await
        .map_err(|err| transient("block target", &err))?;
    let Some(target) = target else {
        return Ok(Outcome::Ignored(
            "a Block whose object is not a stored local account",
        ));
    };
    let row = block::block(&state.db, &state.config.base_url, &signer, &target)
        .await
        .map_err(|err| transient("block", &err))?;
    if row.ap_id.as_deref() != Some(view.id.as_str()) {
        block::set_ap_id(&state.db, row.id, &view.id)
            .await
            .map_err(|err| transient("block activity id", &err))?;
    }
    debug!(
        blocker = %signer.nickname,
        blocked = %target.nickname,
        "an inbound Block mirrored a remote account's block of a local one"
    );
    Ok(Outcome::Handled)
}

/// The body of [`handle_undo_block`]: resolve which account the withdrawn
/// block was about, then lift exactly the signer's own block of it.
async fn apply_undo_block(
    state: AppState,
    view: ActivityView,
    signer: User,
) -> Result<Outcome, FederationError> {
    if signer.local.get() {
        return Ok(Outcome::Ignored(
            "an Undo of a Block signed by an account this instance owns",
        ));
    }
    let target = undo_block_target(&state.db, &view)
        .await
        .map_err(|err| transient("undone block", &err))?;
    let Some(target) = target else {
        return Ok(Outcome::Ignored(
            "an Undo of a Block this instance cannot resolve to a blocked account",
        ));
    };
    let lifted = block::unblock(&state.db, &signer, &target)
        .await
        .map_err(|err| transient("block lift", &err))?;
    if lifted.is_none() {
        return Ok(Outcome::Ignored(
            "an Undo of a Block its actor never placed",
        ));
    }
    debug!(
        blocker = %signer.nickname,
        blocked = %target.nickname,
        "an inbound Undo lifted a remote account's block"
    );
    Ok(Outcome::Handled)
}

/// The body of [`handle_flag`]: find the local account among the object
/// ids, match the rest against stored statuses, file the forwarded report
/// under the activity's id, and run the filing's shared hook.
async fn apply_flag(
    state: AppState,
    view: ActivityView,
    signer: User,
) -> Result<Outcome, FederationError> {
    if signer.local.get() {
        return Ok(Outcome::Ignored(
            "a Flag signed by an account this instance owns",
        ));
    }
    let objects = view
        .raw
        .get("object")
        .map_or_else(Vec::new, json::references);
    let account = find_local_account(&state.db, &objects)
        .await
        .map_err(|err| transient("report target", &err))?;
    let Some((account_index, target)) = account else {
        return Ok(Outcome::Ignored(
            "a Flag whose object names no local account",
        ));
    };
    let status_ids = cited_statuses(&state.db, &objects, account_index)
        .await
        .map_err(|err| transient("cited statuses", &err))?;
    let comment = view
        .raw
        .get("content")
        .and_then(Value::as_str)
        .unwrap_or_default()
        .to_owned();
    let new = report::NewReport {
        comment,
        category: report::Category::Other,
        forwarded: true,
        rule_ids: Vec::new(),
        status_ids: status_ids.clone(),
    };
    let row = report::create(&state.db, &state.config.base_url, &signer, &target, &new)
        .await
        .map_err(|err| transient("report", &err))?;
    report::set_ap_id(&state.db, row.id, &view.id)
        .await
        .map_err(|err| transient("report activity id", &err))?;
    after_report_created(&state, &row).await;
    debug!(
        report = %row.id,
        target = %target.nickname,
        cited = status_ids.len(),
        "an inbound Flag filed the forwarded report of a local account"
    );
    Ok(Outcome::Handled)
}

/// The shared body of [`handle_add`] and [`handle_remove`]: the featured
/// collection's pins, folded for the signer alone.
async fn apply_featured_change(
    state: AppState,
    view: ActivityView,
    signer: User,
    pinning: bool,
) -> Result<Outcome, FederationError> {
    if signer.local.get() {
        return Ok(Outcome::Ignored(
            "an Add or Remove signed by an account this instance owns",
        ));
    }
    let featured = view
        .target
        .as_deref()
        .is_some_and(|target| signer.featured_url.as_deref() == Some(target));
    if !featured {
        debug!(
            target = view.target.as_deref().unwrap_or_default(),
            "an Add or Remove about a collection other than the actor's featured one; ignoring"
        );
        return Ok(Outcome::Ignored(
            "an Add or Remove whose target is not the actor's featured collection",
        ));
    }
    let Some(uri) = view.object.id() else {
        return Ok(Outcome::Ignored(
            "an Add or Remove whose object names no id",
        ));
    };
    let stored = status::get_by_uri(&state.db, uri)
        .await
        .map_err(|err| transient("featured status", &err))?;
    let Some(stored) = stored else {
        debug!(
            uri,
            "an Add or Remove of a status this instance does not store; ignoring"
        );
        return Ok(Outcome::Ignored(
            "an Add or Remove whose object is not a stored status",
        ));
    };
    if stored.user_id != signer.id {
        return Ok(Outcome::Ignored(
            "an Add or Remove of a status its actor did not author",
        ));
    }
    if pinning {
        let _pinned = pinned_status::pin(&state.db, signer.id, stored.id)
            .await
            .map_err(|err| transient("featured pin", &err))?;
    } else {
        let _unpinned = pinned_status::unpin(&state.db, signer.id, stored.id)
            .await
            .map_err(|err| transient("featured pin", &err))?;
    }
    debug!(
        actor = %signer.nickname,
        status = %stored.id,
        pinning,
        "an inbound Add/Remove folded the actor's featured pin"
    );
    Ok(Outcome::Handled)
}

/// The body of [`handle_move`]: the two validators (self-object and alias
/// claim) around the target fetch, the mirror write, and the follower
/// migration.
async fn apply_move(
    state: AppState,
    view: ActivityView,
    signer: User,
) -> Result<Outcome, FederationError> {
    if signer.local.get() {
        return Ok(Outcome::Ignored(
            "a Move signed by an account this instance owns",
        ));
    }
    if view.object.id() != Some(view.actor.as_str()) {
        return Ok(Outcome::Ignored(
            "a Move whose object is not the actor's own id",
        ));
    }
    let Some(target_id) = view.target.as_deref() else {
        return Ok(Outcome::Ignored("a Move with no target actor"));
    };
    let target = match get_or_fetch_by_ap_id(&state, target_id).await {
        Ok(target) => target,
        Err(FetchError::NotFound | FetchError::Gone) => {
            debug!(target_id, "a Move to an actor that is not there; ignoring");
            return Ok(Outcome::Ignored(
                "a Move whose target actor cannot be resolved",
            ));
        }
        Err(err) => {
            return Err(FederationError::Transient(format!(
                "the move target could not be fetched: {err}"
            )));
        }
    };
    if !target.also_known_as.contains(&view.actor) {
        debug!(
            target = %target.nickname,
            "a Move to a target that has not claimed the old account; ignoring"
        );
        return Ok(Outcome::Ignored(
            "a Move whose target does not list the old account among its aliases",
        ));
    }
    user::set_moved_to(&state.db, signer.id, Some(&target.ap_id))
        .await
        .map_err(|err| transient("move target", &err))?;
    migrate_followers(&state, &signer, &target).await?;
    debug!(
        from = %signer.nickname,
        to = %target.nickname,
        "an inbound Move mirrored the account's new home and migrated its local followers"
    );
    Ok(Outcome::Handled)
}

/// The local user the activity's object names: the object's own id (bare
/// or embedded) resolved against `users`, kept only when the account is
/// one this instance owns — the `Block` and `Flag` targets alike.
async fn resolve_local_target(pool: &PgPool, view: &ActivityView) -> AppResult<Option<User>> {
    let Some(id) = view.object.id() else {
        return Ok(None);
    };
    let found = user::get_by_ap_id(pool, id).await?;
    Ok(found.filter(|found| found.local.get()))
}

/// The blocked account an `Undo` of a `Block` is about: the embedded inner
/// activity states it in its own `object`, and an `Undo` naming the
/// activity id alone learns it from the stored block row that id keys —
/// which is also why only the blocker's own `Undo` can find its block.
async fn undo_block_target(pool: &PgPool, view: &ActivityView) -> AppResult<Option<User>> {
    if let Some(inner) = view.object.embedded() {
        let Some(id) = inner.get("object").and_then(json::value_or_id) else {
            return Ok(None);
        };
        return user::get_by_ap_id(pool, &id).await;
    }
    let Some(id) = view.object.id() else {
        return Ok(None);
    };
    match block::get_by_ap_id(pool, id).await? {
        Some(row) => user::get_by_id(pool, row.blocked_id).await,
        None => Ok(None),
    }
}

/// The first of `ids` that names a stored *local* account, with its
/// position so the status pass can skip it: Pleroma's `get_reported`
/// reduced to the local audience, since a report about another instance's
/// account is that instance's to file.
async fn find_local_account(pool: &PgPool, ids: &[String]) -> AppResult<Option<(usize, User)>> {
    for (index, id) in ids.iter().enumerate() {
        if let Some(found) = user::get_by_ap_id(pool, id).await?
            && found.local.get()
        {
            return Ok(Some((index, found)));
        }
    }
    Ok(None)
}

/// The stored statuses a `Flag` cites: every object id but the reported
/// account's that is the `uri` of a status this instance keeps
/// ([`status::get_by_uri`], the uri-as-identity rule of decision 6). An id
/// nothing stores is silently dropped — the fetch ticket gives unknown
/// statuses a home later; a report's context only ever names what is here.
async fn cited_statuses(
    pool: &PgPool,
    ids: &[String],
    account_index: usize,
) -> AppResult<Vec<Uuid>> {
    let mut cited = Vec::new();
    for (index, id) in ids.iter().enumerate() {
        if index == account_index {
            continue;
        }
        if let Some(stored) = status::get_by_uri(pool, id).await? {
            cited.push(stored.id);
        }
    }
    Ok(cited)
}

/// The follower migration [`apply_move`] runs: every candidate from
/// [`follow::move_followers_of`] (local, accepted, still allowing its
/// following list to move) takes [`migrate_one_follower`] and then the
/// `move` telling ([`notify::on_move`]) — remote followers are untouched,
/// their instance re-follows on its own once the `Move` reaches it.
async fn migrate_followers(
    state: &AppState,
    origin: &User,
    target: &User,
) -> Result<(), FederationError> {
    let followers = follow::move_followers_of(&state.db, origin.id)
        .await
        .map_err(|err| transient("follower list", &err))?;
    for follower in &followers {
        migrate_one_follower(state, follower, origin, target).await?;
        notify::on_move(state, follower, origin).await;
    }
    Ok(())
}

/// The pair of writes one local follower takes from a move: follow the new
/// account, then unfollow the old — and neither for a pair that has
/// blocked the target, which keeps its relationship whole (the same
/// nothing-halfway rule the account-move endpoint's transfer states, see
/// [`crate::api::pleroma_account_settings`]).
async fn migrate_one_follower(
    state: &AppState,
    follower: &User,
    origin: &User,
    target: &User,
) -> Result<(), FederationError> {
    if block::blocked_between(&state.db, follower.id, target.id)
        .await
        .map_err(|err| transient("block check", &err))?
    {
        debug!(
            follower = %follower.nickname,
            "a blocked pair keeps both its follows through a move"
        );
        return Ok(());
    }
    let _opened = follow::follow(&state.db, &state.config.base_url, follower, target)
        .await
        .map_err(|err| transient("migrated follow", &err))?;
    let _lifted = follow::unfollow(&state.db, follower, origin)
        .await
        .map_err(|err| transient("migrated unfollow", &err))?;
    Ok(())
}

/// The retry arm every database failure in these handlers maps onto: the
/// writes here are idempotent, so the queue's next attempt can run the
/// same activity again.
fn transient(what: &str, err: &AppError) -> FederationError {
    FederationError::Transient(format!(
        "could not fold the remote activity's {what}: {err}"
    ))
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;

    use serde_json::{Map, Value, json};
    use sqlx::PgPool;
    use sqlx::postgres::PgPoolOptions;
    use uuid::Uuid;

    use super::*;
    use crate::{
        config::Config,
        http_client::HttpClient,
        models::{
            follow::FollowState,
            status::{NewStatus, Status},
            user::{NewUser, insert},
        },
        uploads::UploaderBackend,
    };

    /// The mirror every fixture here is sent by: alice on `peer.example`.
    const ALICE: &str = "https://peer.example/users/alice";
    /// MALLORY, the other `peer.example` mirror — the wrong-actor fixture.
    const MALLORY: &str = "https://peer.example/users/mallory";
    /// The move target: carol on `new.example`.
    const CAROL: &str = "https://new.example/users/carol";
    /// The instance origin the local fixtures are built on.
    const BASE: &str = "https://test.example";
    /// The featured collection alice's `Add`/`Remove` fixtures target.
    const FEATURED: &str = "https://peer.example/users/alice/collections/featured";

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

    /// Normalizes a fixture activity: `kind` named
    /// `https://peer.example/activities/{n}`, sent by `actor`, carrying the
    /// fields of `fields` (`object` among them, `target` and `content` when
    /// the test gives them). A fixture that stops parsing says so.
    fn view(kind: &str, n: &str, actor: &str, fields: Value) -> ActivityView {
        let Value::Object(entries) = fields else {
            panic!("the fixture fields are an object");
        };
        let mut activity = json!({
            "id": format!("https://peer.example/activities/{n}"),
            "type": kind,
            "actor": actor,
        });
        let map = activity
            .as_object_mut()
            .expect("the fixture skeleton is an object");
        for (key, value) in entries {
            let _set = map.insert(key, value);
        }
        ActivityView::parse(&activity).expect("the fixture activity parses")
    }

    /// The `Block`/`Flag`/`Move`/`Undo` fixture with the object alone.
    fn object_view(kind: &str, n: &str, actor: &str, object: Value) -> ActivityView {
        let mut fields = Map::new();
        let _object = fields.insert("object".to_owned(), object);
        view(kind, n, actor, Value::Object(fields))
    }

    /// A local account named `nickname` at `{BASE}/users/{nickname}`.
    async fn local_user(pool: &PgPool, nickname: &str) -> User {
        insert(
            pool,
            &NewUser::local(nickname, &user::local_ap_id(BASE, nickname)),
        )
        .await
        .expect("the local fixture inserts")
    }

    /// A remote mirror at `ap_id` named `nickname`, with no other profile.
    async fn remote_at(pool: &PgPool, nickname: &str, ap_id: &str) -> User {
        let host = ap_id
            .split('/')
            .nth(2)
            .expect("the fixture id is a url")
            .to_owned();
        insert(
            pool,
            &NewUser {
                nickname: format!("{nickname}@{host}"),
                ap_id: ap_id.to_owned(),
                domain: Some(host),
                ..NewUser::default()
            },
        )
        .await
        .expect("the remote fixture inserts")
    }

    /// The signer mirror: [`ALICE`], with no featured url.
    async fn remote_signer(pool: &PgPool) -> User {
        remote_at(pool, "alice", ALICE).await
    }

    /// The signer mirror with alice's featured url stored.
    async fn featured_signer(pool: &PgPool) -> User {
        insert(
            pool,
            &NewUser {
                nickname: "alice@peer.example".to_owned(),
                ap_id: ALICE.to_owned(),
                domain: Some("peer.example".to_owned()),
                featured_url: Some(FEATURED.to_owned()),
                ..NewUser::default()
            },
        )
        .await
        .expect("the featured fixture inserts")
    }

    /// [`CAROL`] as a freshly fetched mirror claiming `aliases`: the state
    /// [`get_or_fetch_by_ap_id`] answers from the database, without the
    /// network.
    async fn move_target(pool: &PgPool, aliases: &[&str]) -> User {
        let stored = insert(
            pool,
            &NewUser {
                nickname: "carol@new.example".to_owned(),
                ap_id: CAROL.to_owned(),
                domain: Some("new.example".to_owned()),
                also_known_as: aliases.iter().map(|alias| (*alias).to_owned()).collect(),
                ..NewUser::default()
            },
        )
        .await
        .expect("the target fixture inserts");
        let _stamped = sqlx::query("UPDATE users SET last_fetched_at = now() WHERE id = $1")
            .bind(stored.id)
            .execute(pool)
            .await
            .expect("the fixture stamps the fetch");
        stored
    }

    /// User `id` as it stands now.
    async fn fresh_user(pool: &PgPool, id: Uuid) -> User {
        user::get_by_id(pool, id)
            .await
            .expect("the user reads")
            .expect("the user exists")
    }

    /// A post by the remote `author` at a peer-named uri.
    async fn remote_post(pool: &PgPool, author: &User, tag: &str) -> Status {
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

    /// A public post by the local `author`.
    async fn local_post(pool: &PgPool, author: &User, text: &str) -> Status {
        status::insert(pool, BASE, &NewStatus::local(author, text))
            .await
            .expect("the local status fixture inserts")
    }

    /// `follower` holds an accepted follow of `target` — granted through the
    /// model's own write, so the counters moved with it.
    async fn accepted_follow(pool: &PgPool, follower: &User, target: &User) {
        let requested = follow::follow(pool, BASE, follower, target)
            .await
            .expect("the follow fixture is recorded");
        let _granted = follow::accept(pool, requested.id)
            .await
            .expect("the fixture grants the request");
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

    /// The recipient's stored notifications, oldest first, as
    /// `(type, from_account_id, status_id)` triples.
    async fn told(pool: &PgPool, recipient: Uuid) -> Vec<(String, Uuid, Option<Uuid>)> {
        sqlx::query_as::<_, (String, Uuid, Option<Uuid>)>(
            "SELECT type, from_account_id, status_id FROM notifications \
              WHERE user_id = $1 ORDER BY id",
        )
        .bind(recipient)
        .fetch_all(pool)
        .await
        .expect("the notifications read")
    }

    /// Every stored report, oldest filing first.
    async fn stored_reports(pool: &PgPool) -> Vec<report::Report> {
        sqlx::query_as::<_, report::Report>("SELECT * FROM reports ORDER BY id")
            .fetch_all(pool)
            .await
            .expect("the reports read")
    }

    /// The block mirrored behind an activity id, expecting it to exist.
    async fn mirrored_block(pool: &PgPool, ap_id: &str) -> block::Block {
        block::get_by_ap_id(pool, ap_id)
            .await
            .expect("the block reads")
            .unwrap_or_else(|| panic!("{ap_id} mirrors a stored block"))
    }

    /// The block route: the pair's row lands with the delivered activity id
    /// as its own, the follows in both directions are severed with their
    /// counters, and the same activity delivered twice stands as one row
    /// with the same id.
    #[tokio::test]
    async fn a_block_of_a_local_account_mirrors_the_activity_and_severs_follows() {
        let db = crate::test_db::test_db().await.expect("test database slot");
        let pool = db.pool().clone();
        let state = app_state(&pool);
        let alice = remote_signer(&pool).await;
        let alice_id = alice.id;
        let bob = local_user(&pool, "block-bob").await;
        accepted_follow(&pool, &bob, &alice).await;
        accepted_follow(&pool, &alice, &bob).await;

        let folded = handle_block(
            state.clone(),
            object_view("Block", "block-1", ALICE, json!(bob.ap_id.clone())),
            alice,
        )
        .await;
        assert_handled(&folded);

        let row = mirrored_block(&pool, "https://peer.example/activities/block-1").await;
        assert_eq!(row.blocker_id, alice_id, "the remote actor blocks");
        assert_eq!(row.blocked_id, bob.id, "the local account is blocked");
        assert!(
            block::is_blocking(&pool, alice_id, bob.id).await.unwrap(),
            "the fact reads back"
        );
        assert!(
            follow::get(&pool, bob.id, alice_id)
                .await
                .unwrap()
                .is_none()
                && follow::get(&pool, alice_id, bob.id)
                    .await
                    .unwrap()
                    .is_none(),
            "both directions' follows went with the block"
        );
        assert_eq!(
            fresh_user(&pool, bob.id).await.followers_count,
            0,
            "and the counters stepped down"
        );

        let again = handle_block(
            state,
            object_view("Block", "block-1", ALICE, json!(bob.ap_id.clone())),
            fresh_user(&pool, alice_id).await,
        )
        .await;
        assert_handled(&again);
        assert_eq!(
            block::blocker_ids(&pool, bob.id).await.unwrap(),
            vec![alice_id],
            "the repeat found the same pair's row and wrote no second one"
        );
        assert_eq!(
            block::blocked_ids(&pool, alice_id).await.unwrap(),
            vec![bob.id],
            "and that one row is alice's block of bob"
        );
        assert_eq!(
            mirrored_block(&pool, "https://peer.example/activities/block-1")
                .await
                .id,
            row.id,
            "the row itself did not move"
        );
    }

    /// A `Block` whose object names a remote mirror, or no account at all,
    /// describes somebody else's business: ignored, and nothing blocked.
    #[tokio::test]
    async fn a_block_about_anything_but_a_local_account_is_ignored() {
        let db = crate::test_db::test_db().await.expect("test database slot");
        let pool = db.pool().clone();
        let state = app_state(&pool);
        let alice = remote_signer(&pool).await;
        let alice_id = alice.id;
        let mallory = remote_at(&pool, "mallory", MALLORY).await;

        let remote_target = handle_block(
            state.clone(),
            object_view("Block", "block-2", ALICE, json!(mallory.ap_id.clone())),
            alice,
        )
        .await;
        assert!(
            ignored(remote_target).contains("local account"),
            "a block of a remote account is not ours to mirror"
        );

        let unknown = handle_block(
            state.clone(),
            object_view(
                "Block",
                "block-3",
                ALICE,
                json!("https://test.example/users/nobody"),
            ),
            fresh_user(&pool, alice_id).await,
        )
        .await;
        assert!(
            ignored(unknown).contains("local account"),
            "and an object naming no stored account"
        );

        let idless = handle_block(
            state,
            object_view("Block", "block-6", ALICE, json!({"type": "Person"})),
            fresh_user(&pool, alice_id).await,
        )
        .await;
        assert!(
            ignored(idless).contains("local account"),
            "and an embedded object that names no id at all"
        );
        assert!(
            block::blocked_ids(&pool, mallory.id)
                .await
                .unwrap()
                .is_empty()
                && sqlx::query_scalar::<_, i64>("SELECT count(*) FROM blocks")
                    .fetch_one(&pool)
                    .await
                    .unwrap()
                    == 0,
            "neither delivery wrote a block"
        );
    }

    /// The `Undo` of a `Block`, both shapes: the bare activity id resolves
    /// the pair through the stored block row, the embedded inner activity
    /// through its own object. Both lift the block; withdrawing a gone
    /// block's id resolves to nothing and is ignored.
    #[tokio::test]
    async fn an_undo_by_the_same_actor_lifts_the_block_it_names() {
        let db = crate::test_db::test_db().await.expect("test database slot");
        let pool = db.pool().clone();
        let state = app_state(&pool);
        let alice = remote_signer(&pool).await;
        let alice_id = alice.id;
        let bob = local_user(&pool, "undo-bob").await;
        let carol = local_user(&pool, "undo-carol").await;
        let _bob_blocked = handle_block(
            state.clone(),
            object_view("Block", "block-bare", ALICE, json!(bob.ap_id.clone())),
            fresh_user(&pool, alice_id).await,
        )
        .await;
        let _carol_blocked = handle_block(
            state.clone(),
            object_view("Block", "block-embed", ALICE, json!(carol.ap_id.clone())),
            fresh_user(&pool, alice_id).await,
        )
        .await;
        assert!(
            block::is_blocking(&pool, alice_id, bob.id).await.unwrap()
                && block::is_blocking(&pool, alice_id, carol.id).await.unwrap(),
            "both fixture blocks stand"
        );

        let bare = handle_undo_block(
            state.clone(),
            object_view(
                "Undo",
                "undo-bare-1",
                ALICE,
                json!("https://peer.example/activities/block-bare"),
            ),
            fresh_user(&pool, alice_id).await,
        )
        .await;
        assert_handled(&bare);
        assert!(
            !block::is_blocking(&pool, alice_id, bob.id).await.unwrap(),
            "the bare-id Undo found the pair through the stored row"
        );

        let embedded = handle_undo_block(
            state.clone(),
            object_view(
                "Undo",
                "undo-embed-1",
                ALICE,
                json!({
                    "id": "https://peer.example/activities/block-embed",
                    "type": "Block",
                    "actor": ALICE,
                    "object": carol.ap_id,
                }),
            ),
            fresh_user(&pool, alice_id).await,
        )
        .await;
        assert_handled(&embedded);
        assert!(
            !block::is_blocking(&pool, alice_id, carol.id).await.unwrap(),
            "and the embedded inner activity named the blocked account itself"
        );

        let twice = handle_undo_block(
            state.clone(),
            object_view(
                "Undo",
                "undo-bare-2",
                ALICE,
                json!("https://peer.example/activities/block-bare"),
            ),
            fresh_user(&pool, alice_id).await,
        )
        .await;
        assert!(
            ignored(twice).contains("cannot resolve"),
            "a withdrawn block's id keys nothing any more"
        );
    }

    /// The unresolvable `Undo` shapes: an id that never stored a block and
    /// an embedded inner activity that names no blocked account both
    /// resolve to nothing and are ignored.
    #[tokio::test]
    async fn an_undo_that_resolves_to_no_blocked_account_is_ignored() {
        let db = crate::test_db::test_db().await.expect("test database slot");
        let pool = db.pool().clone();
        let state = app_state(&pool);
        let alice = remote_signer(&pool).await;
        let alice_id = alice.id;

        let unknown = handle_undo_block(
            state.clone(),
            object_view(
                "Undo",
                "undo-none-1",
                ALICE,
                json!("https://peer.example/activities/never-blocked"),
            ),
            alice,
        )
        .await;
        assert!(
            ignored(unknown).contains("cannot resolve"),
            "an id no block carries resolves to nothing"
        );

        let innerless = handle_undo_block(
            state,
            object_view("Undo", "undo-none-2", ALICE, json!({"type": "Block"})),
            fresh_user(&pool, alice_id).await,
        )
        .await;
        assert!(
            ignored(innerless).contains("cannot resolve"),
            "and an embedded Block that names no blocked account resolves to nothing"
        );
    }

    /// An `Undo` of a `Block` signed by an account that never placed it —
    /// with the inner activity embedded or the id named bare — lifts
    /// nothing: the lift is keyed to the pair, so another actor's block of
    /// the same local account stands untouched.
    #[tokio::test]
    async fn an_undo_by_a_different_actor_lifts_nothing() {
        let db = crate::test_db::test_db().await.expect("test database slot");
        let pool = db.pool().clone();
        let state = app_state(&pool);
        let alice = remote_signer(&pool).await;
        let alice_id = alice.id;
        let bob = local_user(&pool, "wrong-bob").await;
        let mallory = remote_at(&pool, "mallory", MALLORY).await;
        let mallory_id = mallory.id;
        let _blocked = handle_block(
            state.clone(),
            object_view("Block", "block-4", ALICE, json!(bob.ap_id.clone())),
            alice,
        )
        .await;

        let embedded = handle_undo_block(
            state.clone(),
            object_view(
                "Undo",
                "undo-2",
                MALLORY,
                json!({"type": "Block", "actor": ALICE, "object": bob.ap_id.clone()}),
            ),
            mallory,
        )
        .await;
        assert!(
            ignored(embedded).contains("never placed"),
            "mallory holds no block of bob to lift"
        );

        let bare = handle_undo_block(
            state,
            object_view(
                "Undo",
                "undo-3",
                MALLORY,
                json!("https://peer.example/activities/block-4"),
            ),
            fresh_user(&pool, mallory_id).await,
        )
        .await;
        assert!(
            ignored(bare).contains("never placed"),
            "and the row found through alice's activity id is not mallory's"
        );
        assert!(
            block::is_blocking(&pool, alice_id, bob.id).await.unwrap(),
            "alice's block of bob stands untouched through both"
        );
    }

    /// The `Flag` route, full shape: the local account among the object ids
    /// is the target, the stored statuses among the rest are cited (an
    /// unknown uri drops out), and the row lands as the forwarded report of
    /// the delivered activity — reporter the remote signer, comment the
    /// `content`, the activity id mirrored onto it.
    #[tokio::test]
    async fn a_flag_files_the_forwarded_report_with_its_statuses() {
        let db = crate::test_db::test_db().await.expect("test database slot");
        let pool = db.pool().clone();
        let state = app_state(&pool);
        let alice = remote_signer(&pool).await;
        let alice_id = alice.id;
        let bob = local_user(&pool, "flag-bob").await;
        let _mallory = remote_at(&pool, "mallory", MALLORY).await;
        let cited_local = local_post(&pool, &bob, "the reported post").await;
        let cited_remote = remote_post(&pool, &alice, "flagged").await;

        let filed = handle_flag(
            state,
            view(
                "Flag",
                "flag-1",
                ALICE,
                json!({
                    "object": [
                        MALLORY,
                        bob.ap_id.clone(),
                        cited_local.uri.clone(),
                        "https://peer.example/posts/never-stored",
                        cited_remote.uri.clone(),
                    ],
                    "content": "harassment, please look",
                }),
            ),
            alice,
        )
        .await;
        assert_handled(&filed);

        let rows = stored_reports(&pool).await;
        assert_eq!(rows.len(), 1, "the Flag filed exactly one report");
        let row = &rows[0];
        assert_eq!(row.reporter_id, alice_id, "the remote actor reported");
        assert_eq!(row.target_id, bob.id, "about the local account");
        assert_eq!(row.comment, "harassment, please look", "with its words");
        assert!(row.forwarded, "a Flag delivered here is a forward");
        assert_eq!(row.category, report::Category::Other, "no category named");
        assert_eq!(
            row.ap_id.as_deref(),
            Some("https://peer.example/activities/flag-1"),
            "the row stands for the delivered activity"
        );
        let mut want = vec![cited_local.id, cited_remote.id];
        want.sort_unstable();
        let mut cited = report::statuses_for(&pool, row.id).await.unwrap();
        cited.sort_unstable();
        assert_eq!(cited, want, "both stored statuses matched by uri");
    }

    /// The `Flag` route, bare shape: an object that is a lone account id and
    /// no `content` still files the report — with nothing cited and the
    /// empty comment.
    #[tokio::test]
    async fn a_flag_of_a_lone_local_account_files_a_bare_report() {
        let db = crate::test_db::test_db().await.expect("test database slot");
        let pool = db.pool().clone();
        let state = app_state(&pool);
        let alice = remote_signer(&pool).await;
        let bob = local_user(&pool, "bare-bob").await;

        let filed = handle_flag(
            state,
            object_view("Flag", "flag-2", ALICE, json!(bob.ap_id.clone())),
            alice,
        )
        .await;
        assert_handled(&filed);

        let rows = stored_reports(&pool).await;
        assert_eq!(rows.len(), 1, "one filing");
        assert_eq!(rows[0].comment, "", "no content is the empty comment");
        assert!(
            report::statuses_for(&pool, rows[0].id)
                .await
                .unwrap()
                .is_empty(),
            "and nothing cited"
        );
    }

    /// A `Flag` about a non-local account files nothing: neither a stored
    /// remote account nor unknown ids in the object list name a target this
    /// instance moderates.
    #[tokio::test]
    async fn a_flag_about_a_non_local_account_files_nothing() {
        let db = crate::test_db::test_db().await.expect("test database slot");
        let pool = db.pool().clone();
        let state = app_state(&pool);
        let alice = remote_signer(&pool).await;
        let alice_id = alice.id;
        let mallory = remote_at(&pool, "mallory", MALLORY).await;
        let their_post = remote_post(&pool, &mallory, "subject").await;

        let remote_subject = handle_flag(
            state.clone(),
            object_view(
                "Flag",
                "flag-3",
                ALICE,
                json!([mallory.ap_id.clone(), their_post.uri.clone()]),
            ),
            alice,
        )
        .await;
        assert!(
            ignored(remote_subject).contains("no local account"),
            "a report about another instance's account is that instance's"
        );

        let statuses_only = handle_flag(
            state,
            object_view("Flag", "flag-4", ALICE, json!([their_post.uri.clone()])),
            fresh_user(&pool, alice_id).await,
        )
        .await;
        assert!(
            ignored(statuses_only).contains("no local account"),
            "and a Flag that names no account at all"
        );
        assert!(
            stored_reports(&pool).await.is_empty(),
            "neither delivery filed a report"
        );
    }

    /// The featured-collection route: an `Add` of the actor's own stored
    /// status pins it (restating the pin is one row), a `Remove` takes it
    /// back, and a restated `Remove` of the gone pin is the same settled
    /// fact.
    #[tokio::test]
    async fn an_add_pins_and_a_remove_unpins_the_actors_status() {
        let db = crate::test_db::test_db().await.expect("test database slot");
        let pool = db.pool().clone();
        let state = app_state(&pool);
        let alice = featured_signer(&pool).await;
        let alice_id = alice.id;
        let posted = remote_post(&pool, &alice, "featured").await;

        let first = handle_add(
            state.clone(),
            view(
                "Add",
                "add-1",
                ALICE,
                json!({"object": posted.uri.clone(), "target": FEATURED}),
            ),
            alice,
        )
        .await;
        assert_handled(&first);
        assert_eq!(
            pinned_status::list_for_user(&pool, alice_id)
                .await
                .unwrap()
                .into_iter()
                .map(|pinned| pinned.id)
                .collect::<Vec<Uuid>>(),
            vec![posted.id],
            "the pin landed on the actor's own status"
        );

        let restated = handle_add(
            state.clone(),
            view(
                "Add",
                "add-2",
                ALICE,
                json!({"object": posted.uri.clone(), "target": FEATURED}),
            ),
            fresh_user(&pool, alice_id).await,
        )
        .await;
        assert_handled(&restated);
        assert_eq!(
            pinned_status::count(&pool, alice_id).await.unwrap(),
            1,
            "and the second Add stands as the one pin it restates"
        );

        for n in ["remove-1", "remove-2"] {
            let withdrawn = handle_remove(
                state.clone(),
                view(
                    "Remove",
                    n,
                    ALICE,
                    json!({"object": posted.uri.clone(), "target": FEATURED}),
                ),
                fresh_user(&pool, alice_id).await,
            )
            .await;
            assert_handled(&withdrawn);
            assert!(
                pinned_status::list_for_user(&pool, alice_id)
                    .await
                    .unwrap()
                    .is_empty(),
                "{n} leaves nothing pinned"
            );
        }
    }

    /// The `Add`/`Remove` target arm: a `target` that is not the actor's
    /// featured collection — a different collection, none at all, or one an
    /// actor with no featured url cannot match — is ignored before anything
    /// is read.
    #[tokio::test]
    async fn an_add_or_remove_on_another_collection_is_ignored() {
        let db = crate::test_db::test_db().await.expect("test database slot");
        let pool = db.pool().clone();
        let state = app_state(&pool);
        let alice = featured_signer(&pool).await;
        let alice_id = alice.id;
        let mallory = remote_at(&pool, "mallory", MALLORY).await;
        let bob = local_user(&pool, "pin-bob").await;
        let bobs_post = local_post(&pool, &bob, "not alice's").await;

        let wrong_collection = handle_add(
            state.clone(),
            view(
                "Add",
                "add-3",
                ALICE,
                json!({
                    "object": bobs_post.uri.clone(),
                    "target": "https://peer.example/users/alice/collections/followers",
                }),
            ),
            alice,
        )
        .await;
        assert!(
            ignored(wrong_collection).contains("featured collection"),
            "only the featured collection pins anything"
        );

        let no_target = handle_add(
            state.clone(),
            object_view("Add", "add-4", ALICE, json!(bobs_post.uri.clone())),
            fresh_user(&pool, alice_id).await,
        )
        .await;
        assert!(
            ignored(no_target).contains("featured collection"),
            "and an activity with no target names none"
        );

        let no_featured_url = handle_add(
            state,
            view(
                "Add",
                "add-5",
                MALLORY,
                json!({"object": bobs_post.uri.clone(), "target": FEATURED}),
            ),
            mallory,
        )
        .await;
        assert!(
            ignored(no_featured_url).contains("featured collection"),
            "an actor whose document named no featured url matches no target"
        );
    }

    /// The `Add`/`Remove` object arm: an unknown status, an object that
    /// names no id, and somebody else's post are all ignored — on both
    /// routes — and nothing is pinned.
    #[tokio::test]
    async fn an_add_or_remove_of_a_status_it_may_not_pin_is_ignored() {
        let db = crate::test_db::test_db().await.expect("test database slot");
        let pool = db.pool().clone();
        let state = app_state(&pool);
        let alice = featured_signer(&pool).await;
        let alice_id = alice.id;
        let bob = local_user(&pool, "pin-bob").await;
        let bobs_post = local_post(&pool, &bob, "not alice's").await;

        let unknown_status = handle_add(
            state.clone(),
            view(
                "Add",
                "add-6",
                ALICE,
                json!({"object": "https://peer.example/posts/never", "target": FEATURED}),
            ),
            alice,
        )
        .await;
        assert!(
            ignored(unknown_status).contains("not a stored status"),
            "the fetch ticket gives unknown statuses a home later"
        );

        let idless_object = handle_add(
            state.clone(),
            view(
                "Add",
                "add-8",
                ALICE,
                json!({"object": {"type": "Note", "content": "no id"}, "target": FEATURED}),
            ),
            fresh_user(&pool, alice_id).await,
        )
        .await;
        assert!(
            ignored(idless_object).contains("names no id"),
            "an Add whose embedded object carries no id resolves to nothing to pin"
        );

        let foreign_post = handle_add(
            state.clone(),
            view(
                "Add",
                "add-7",
                ALICE,
                json!({"object": bobs_post.uri.clone(), "target": FEATURED}),
            ),
            fresh_user(&pool, alice_id).await,
        )
        .await;
        assert!(
            ignored(foreign_post).contains("did not author"),
            "an actor pins its own posts only"
        );

        let unpin_foreign = handle_remove(
            state,
            view(
                "Remove",
                "remove-3",
                ALICE,
                json!({"object": bobs_post.uri.clone(), "target": FEATURED}),
            ),
            fresh_user(&pool, alice_id).await,
        )
        .await;
        assert!(
            ignored(unpin_foreign).contains("did not author"),
            "and the same authorship rule guards the Remove"
        );
        assert_eq!(
            sqlx::query_scalar::<_, i64>("SELECT count(*) FROM pinned_statuses")
                .fetch_one(&pool)
                .await
                .unwrap(),
            0,
            "none of them pinned anything"
        );
    }

    /// The `Move` fixture cast, ids and the stored target.
    #[derive(Debug)]
    struct MoveFixture {
        /// The departing remote account.
        alice: Uuid,
        /// The claimed new account: a fresh mirror listing alice.
        carol: User,
        /// The plain local follower, whose follow moves.
        bob: Uuid,
        /// The local follower blocked with `carol`.
        erin: Uuid,
        /// The remote follower nobody migrates from here.
        dave: Uuid,
    }

    /// The `Move` scenario: [`ALICE`] moving to [`CAROL`], accepted as a
    /// follow by the local `bob`, by the local `erin` — which blocks carol
    /// — and by the remote `dave`.
    async fn move_scenario(pool: &PgPool) -> MoveFixture {
        let alice = remote_signer(pool).await;
        let carol = move_target(pool, &[ALICE]).await;
        let bob = local_user(pool, "move-bob").await;
        let erin = local_user(pool, "move-erin").await;
        let dave = remote_at(pool, "dave", "https://other.example/users/dave").await;
        for follower in [&bob, &erin, &dave] {
            accepted_follow(pool, follower, &alice).await;
        }
        let _erin_blocked_carol = block::block(pool, BASE, &erin, &carol)
            .await
            .expect("the blocked-pair fixture stores");
        MoveFixture {
            alice: alice.id,
            carol,
            bob: bob.id,
            erin: erin.id,
            dave: dave.id,
        }
    }

    /// The `Move` activity named `n` both halves of the happy path deliver.
    fn move_activity(n: &str) -> ActivityView {
        view("Move", n, ALICE, json!({"object": ALICE, "target": CAROL}))
    }

    /// The `Move` happy path, first half: the target's alias claim is
    /// honoured, the old account's `moved_to_ap_id` mirrors the new one
    /// with no local-move cooldown stamp, and the unblocked local follower
    /// re-follows the target, drops the old account, and is told `move`
    /// with the old account as actor.
    #[tokio::test]
    async fn a_move_mirrors_the_target_and_migrates_the_local_followers() {
        let db = crate::test_db::test_db().await.expect("test database slot");
        let pool = db.pool().clone();
        let state = app_state(&pool);
        let scene = move_scenario(&pool).await;

        let moved = handle_move(
            state,
            move_activity("move-1"),
            fresh_user(&pool, scene.alice).await,
        )
        .await;
        assert_handled(&moved);

        let departed = fresh_user(&pool, scene.alice).await;
        assert_eq!(
            departed.moved_to_ap_id.as_deref(),
            Some(CAROL),
            "the old account points at the new one"
        );
        assert!(
            departed.last_move_at.is_none(),
            "and this instance stamps no move cooldown for a peer's move"
        );

        let bob_to_carol = follow::get(&pool, scene.bob, scene.carol.id)
            .await
            .unwrap()
            .expect("bob follows the new account now");
        assert_eq!(
            bob_to_carol.state,
            FollowState::Pending,
            "the re-follow of a remote account waits for its answer, like any other"
        );
        assert!(
            follow::get(&pool, scene.bob, scene.alice)
                .await
                .unwrap()
                .is_none(),
            "and no longer the old one"
        );
        assert_eq!(
            told(&pool, scene.bob).await,
            vec![("move".to_owned(), scene.alice, None)],
            "the migrated follower is told the move, from the old account"
        );
    }

    /// The `Move` happy path, second half: the follower blocked with the
    /// target keeps both its relationships (and is told anyway), the remote
    /// follower and the target itself hear nothing, and a redelivered
    /// `Move` — after its follow has already moved — writes and tells
    /// nothing new.
    #[tokio::test]
    async fn a_move_leaves_blocked_and_remote_followers_and_retells_nobody() {
        let db = crate::test_db::test_db().await.expect("test database slot");
        let pool = db.pool().clone();
        let state = app_state(&pool);
        let scene = move_scenario(&pool).await;

        let moved = handle_move(
            state.clone(),
            move_activity("move-1"),
            fresh_user(&pool, scene.alice).await,
        )
        .await;
        assert_handled(&moved);

        assert!(
            follow::get(&pool, scene.erin, scene.alice)
                .await
                .unwrap()
                .is_some(),
            "the follower blocked with the target keeps its old follow"
        );
        assert!(
            follow::get(&pool, scene.erin, scene.carol.id)
                .await
                .unwrap()
                .is_none(),
            "and gains no follow of the account it blocked"
        );
        assert_eq!(
            told(&pool, scene.erin).await,
            vec![("move".to_owned(), scene.alice, None)],
            "and is told anyway — the move happened whether or not its follow moved"
        );
        assert!(
            follow::get(&pool, scene.dave, scene.alice)
                .await
                .unwrap()
                .is_some(),
            "the remote follower's relationship is its own instance's to migrate"
        );
        assert!(
            told(&pool, scene.dave).await.is_empty()
                && told(&pool, scene.carol.id).await.is_empty(),
            "and nobody else is told from here"
        );

        let again = handle_move(
            state,
            move_activity("move-2"),
            fresh_user(&pool, scene.alice).await,
        )
        .await;
        assert_handled(&again);
        assert_eq!(
            told(&pool, scene.bob).await.len(),
            1,
            "the redelivered Move tells bob once"
        );
    }

    /// The alias validator: a target that has not claimed the old actor in
    /// its `alsoKnownAs` moves nothing — no `moved_to_ap_id`, no migrated
    /// follower, no telling.
    #[tokio::test]
    async fn a_move_the_target_has_not_claimed_is_ignored() {
        let db = crate::test_db::test_db().await.expect("test database slot");
        let pool = db.pool().clone();
        let state = app_state(&pool);
        let alice = remote_signer(&pool).await;
        let alice_id = alice.id;
        let _carol = move_target(&pool, &["https://other.example/users/someone"]).await;
        let bob = local_user(&pool, "alias-bob").await;
        accepted_follow(&pool, &bob, &alice).await;

        let refused = handle_move(
            state,
            view(
                "Move",
                "move-3",
                ALICE,
                json!({"object": ALICE, "target": CAROL}),
            ),
            alice,
        )
        .await;
        assert!(
            ignored(refused).contains("aliases"),
            "the unclaimed origin answers for nothing"
        );
        assert!(
            fresh_user(&pool, alice_id).await.moved_to_ap_id.is_none(),
            "the account is not marked moved"
        );
        assert!(
            follow::get(&pool, bob.id, alice_id)
                .await
                .unwrap()
                .is_some()
                && told(&pool, bob.id).await.is_empty(),
            "and its follower keeps the relationship and hears nothing"
        );
    }

    /// The other refusals: an object that is not the actor's own id, no
    /// target at all, and a target this instance resolves to no actor — an
    /// unknown id on this instance's own origin, answered without the
    /// network — are all ignored.
    #[tokio::test]
    async fn a_malformed_or_unresolvable_move_is_ignored() {
        let db = crate::test_db::test_db().await.expect("test database slot");
        let pool = db.pool().clone();
        let state = app_state(&pool);
        let alice = remote_signer(&pool).await;
        let alice_id = alice.id;
        let _carol = move_target(&pool, &[ALICE]).await;

        let wrong_object = handle_move(
            state.clone(),
            view(
                "Move",
                "move-4",
                ALICE,
                json!({"object": MALLORY, "target": CAROL}),
            ),
            alice,
        )
        .await;
        assert!(
            ignored(wrong_object).contains("not the actor's own id"),
            "a Move of somebody else's account is nobody's"
        );

        let no_target = handle_move(
            state.clone(),
            object_view("Move", "move-5", ALICE, json!(ALICE)),
            fresh_user(&pool, alice_id).await,
        )
        .await;
        assert!(
            ignored(no_target).contains("no target"),
            "and a Move with no destination"
        );

        let gone_target = handle_move(
            state,
            view(
                "Move",
                "move-6",
                ALICE,
                json!({"object": ALICE, "target": "https://test.example/users/ghost"}),
            ),
            fresh_user(&pool, alice_id).await,
        )
        .await;
        assert!(
            ignored(gone_target).contains("cannot be resolved"),
            "a target with no actor behind it moves nobody"
        );
    }

    /// The local-signer guard on every route: an account this instance owns
    /// does not file inbound federation against itself — each handler
    /// ignores the same activity it would fold for a remote one, and
    /// nothing is written.
    #[tokio::test]
    async fn a_local_signer_folds_nothing() {
        let db = crate::test_db::test_db().await.expect("test database slot");
        let pool = db.pool().clone();
        let state = app_state(&pool);
        let ours = local_user(&pool, "ours").await;
        let ours_id = ours.id;
        let bob = local_user(&pool, "guarded-bob").await;
        let posted = local_post(&pool, &ours, "mine").await;
        let who = ours.ap_id.clone();
        let featured = format!("{who}/collections/featured");

        let outcomes = [
            handle_block(
                state.clone(),
                object_view("Block", "guard-1", &who, json!(bob.ap_id.clone())),
                fresh_user(&pool, ours_id).await,
            )
            .await,
            handle_undo_block(
                state.clone(),
                object_view("Undo", "guard-2", &who, json!(bob.ap_id.clone())),
                fresh_user(&pool, ours_id).await,
            )
            .await,
            handle_flag(
                state.clone(),
                object_view("Flag", "guard-3", &who, json!([bob.ap_id.clone()])),
                fresh_user(&pool, ours_id).await,
            )
            .await,
            handle_add(
                state.clone(),
                view(
                    "Add",
                    "guard-4",
                    &who,
                    json!({"object": posted.uri.clone(), "target": featured.clone()}),
                ),
                fresh_user(&pool, ours_id).await,
            )
            .await,
            handle_remove(
                state.clone(),
                view(
                    "Remove",
                    "guard-5",
                    &who,
                    json!({"object": posted.uri.clone(), "target": featured.clone()}),
                ),
                fresh_user(&pool, ours_id).await,
            )
            .await,
            handle_move(
                state,
                view(
                    "Move",
                    "guard-6",
                    &who,
                    json!({"object": who.clone(), "target": CAROL}),
                ),
                fresh_user(&pool, ours_id).await,
            )
            .await,
        ];
        for (label, outcome) in ["Block", "Undo", "Flag", "Add", "Remove", "Move"]
            .into_iter()
            .zip(outcomes)
        {
            assert!(
                ignored(outcome).contains("this instance owns"),
                "a {label} by a local account is the client API's business"
            );
        }
        assert!(
            block::blocked_ids(&pool, ours_id).await.unwrap().is_empty()
                && stored_reports(&pool).await.is_empty()
                && fresh_user(&pool, ours_id).await.moved_to_ap_id.is_none()
                && pinned_status::count(&pool, ours_id).await.unwrap() == 0,
            "and none of the six touched anything"
        );
    }

    /// Every route against a database that cannot answer: the reads fail
    /// first, the failure is transient, and the queue's retry finds the
    /// same idempotent writes waiting for it.
    #[tokio::test]
    async fn a_dead_database_sends_every_route_back_for_a_retry() {
        let db = crate::test_db::test_db().await.expect("test database slot");
        let pool = db.pool().clone();
        let alice = featured_signer(&pool).await;
        let alice_id = alice.id;
        let bob = local_user(&pool, "dead-bob").await;
        let state = broken_state().await;

        let outcomes = [
            handle_block(
                state.clone(),
                object_view("Block", "dead-1", ALICE, json!(bob.ap_id.clone())),
                fresh_user(&pool, alice_id).await,
            )
            .await,
            handle_undo_block(
                state.clone(),
                object_view(
                    "Undo",
                    "dead-2",
                    ALICE,
                    json!("https://peer.example/activities/dead-block"),
                ),
                fresh_user(&pool, alice_id).await,
            )
            .await,
            handle_flag(
                state.clone(),
                object_view("Flag", "dead-3", ALICE, json!([bob.ap_id.clone()])),
                fresh_user(&pool, alice_id).await,
            )
            .await,
            handle_add(
                state.clone(),
                view(
                    "Add",
                    "dead-4",
                    ALICE,
                    json!({"object": "https://peer.example/objects/any", "target": FEATURED}),
                ),
                fresh_user(&pool, alice_id).await,
            )
            .await,
            handle_move(
                state,
                view(
                    "Move",
                    "dead-5",
                    ALICE,
                    json!({"object": ALICE, "target": CAROL}),
                ),
                fresh_user(&pool, alice_id).await,
            )
            .await,
        ];
        for outcome in outcomes {
            assert!(
                matches!(outcome, Err(FederationError::Transient(_))),
                "the dead database is a retry, not a verdict: {outcome:?}"
            );
        }
    }
}
