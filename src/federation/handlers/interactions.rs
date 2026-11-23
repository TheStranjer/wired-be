//! Inbound `Like`, `EmojiReact`, `Announce` and their `Undo`s: a peer's
//! interactions with a post, folded onto this instance's rows.
//!
//! The reference is Pleroma's `Pleroma.Web.ActivityPub.SideEffects`
//! (`lib/pleroma/web/activity_pub/side_effects.ex`) — its `Like`,
//! `EmojiReact` and `Announce` clauses add the like, the reaction and the
//! announce to the acted-on object and raise the author's notifications, and
//! its `handle_undoing` takes exactly that back — with
//! `object_validators/like_validator.ex`, `emoji_react_validator.ex` and
//! `announce_validator.ex` as the documents' own gates. The ticket rules are
//! authoritative where the two differ:
//!
//! * The shared step, `resolve_target`, is every fold's first move: the
//!   activity's `object` — a bare id or an embedded document's — resolves
//!   through [`fetch_object::resolve_status`], so a local status answers
//!   without the network, an unknown remote one is fetched and ingested, a
//!   `Gone`/`NotFound` answer folds nothing, and a transient failure
//!   releases the activity for a retry. The resolved row is folded to its
//!   original when it names a reblog — nobody likes, reacts to or boosts a
//!   boost, the `action_target_id` rule of the endpoints — and it must
//!   stand visible to the actor: `private` and `direct` pass only for an
//!   accepted follower or a mentioned account, the shared
//!   [`status_visibility::can_view`] read.
//! * [`handle_like`] records a [`favourite::favourite_like`] row under the
//!   delivered activity id — idempotent per actor and status, the counter
//!   and the author's telling moving only with the row — and runs the
//!   favourite endpoint's `after_favourite_changed` hook, so a local author
//!   is told and streaming reacts exactly as of a local like. A `Like`
//!   carrying a `content` that names one emoji is a reaction in disguise —
//!   the Misskey and Fedibird favourite-with-emoji shape, Pleroma's
//!   `Transmogrifier` rewrite — and folds through the reaction arm; a
//!   `content` that names nothing usable is ignored rather than defaulted
//!   into a plain like, Pleroma's posture. Types that merely resemble a
//!   like — `Dislike`, `Zap` — reach no handler at all: the dispatcher's
//!   own unknown-type arm ignores them.
//! * [`handle_emoji_react`] validates the activity's `content` by the same
//!   rules the local reaction endpoint asks of its path segment
//!   ([`Emoji::classify`]: one unicode grapheme, or a `:shortcode:`), and —
//!   for a shortcode — the image URL of a matching `Emoji` entry in the
//!   activity's `tag` list: the remote's own emoji is what the reaction
//!   names, so this instance's `custom_emojis` table is not consulted and
//!   the stored URL comes from the tag. The row lands through
//!   [`emoji_reaction::react_with_ap_id`] under the delivered id, and the
//!   reaction endpoint's `after_reaction_changed` hook tells the author.
//! * [`handle_announce`] folds the boost: the target must be `public` or
//!   `unlisted` (a boost of anything else is ignored), and the row is a
//!   reblog status whose `uri` is the delivered `Announce` id (decision 6:
//!   a reblog row *is* the activity), whose visibility the `Announce`'s own
//!   addressing states exactly as a note's — the `parse_visibility` rule:
//!   the public address in `to`,
//!   public; in `cc`, unlisted; the actor's followers collection, private;
//!   anything else, direct), and whose `inserted_at` is the activity's
//!   `published`. A pair that holds a live boost — or an id a row already
//!   carries — restates a fact and is ignored. No follow relation is asked
//!   of the actor, which is what lets a relay's re-broadcasts through:
//!   when the actor is a relay this instance follows and that has accepted
//!   the follow (the [`crate::models::relay`] table, the relay
//!   ticket's other half of that pairing), the `Announce` folds as ingest
//!   only — the target step's
//!   [`resolve_status`](crate::federation::fetch_object::resolve_status)
//!   fetch has already
//!   mirrored the announced object as an ordinary public remote status —
//!   and no reblog row is created, because a relay re-broadcasts rather
//!   than boosts. A peer whose
//!   domain the instance rejects never reaches this code — the dispatcher's
//!   policy stage applies that, and a blocked author's domain with it.
//! * The `Undo` routes ([`handle_undo_like`], [`handle_undo_emoji_react`],
//!   [`handle_undo_announce`]) take back exactly the row the undone
//!   activity's id names — for its maker only, the same-actor scoping of
//!   the id reads and the reblog row's own authorship — through the models'
//!   idempotent withdrawal writes, and run the same hooks with their
//!   withdrawal arm: the counter steps back and the author's telling goes.
//!   An `Undo` signed by anybody other than the actor of what it undoes
//!   dies at the dispatcher's own gate before any of this runs.
//!
//! Every route runs behind the dispatcher's gates: containment has already
//! proved the activity's `actor` to be the signer's `ap_id`
//! ([`crate::federation::dispatch`]), so the [`User`] a handler is handed
//! *is* the remote account the activity speaks for; the local-signer guard
//! each one keeps is the belt to that bracer, as in
//! [`crate::federation::handlers::misc`].
//!
//! [`fetch_object::resolve_status`]: crate::federation::fetch_object::resolve_status
//! [`status_visibility::can_view`]: crate::models::status_visibility::can_view
//! [`favourite::favourite_like`]: crate::models::favourite::favourite_like
//! [`Emoji::classify`]: crate::util::emoji::Emoji::classify
//! [`emoji_reaction::react_with_ap_id`]: crate::models::emoji_reaction::react_with_ap_id

mod announces;
mod likes;
mod target;

use futures::future::BoxFuture;

use self::{
    announces::{apply_announce, apply_undo_announce},
    likes::{apply_emoji_react, apply_like, apply_undo_emoji_react, apply_undo_like},
};
use crate::{
    error::AppError,
    federation::dispatch::{ActivityView, FederationError, Handlers, Outcome},
    models::user::User,
    state::AppState,
};

/// The shared step's static reason: an activity whose object names nothing
/// to fold — no id, nothing live behind the id, a gone-on-fetch object, or
/// a target its actor may not see.
const NO_TARGET: &str = "an activity whose object names no status visible to its actor";
/// Why an `Undo` whose object carries no activity id at all is refused:
/// the withdrawal is keyed on the id of what it undoes.
const NO_INNER_ID: &str = "an Undo whose object names no activity id";
/// The local-signer guard of the three withdrawal routes, shared wording:
/// containment has already proved the signer the activity's actor, so a
/// local one here is an account interacting with itself through the
/// federation door that only remote peers walk.
const UNDO_LOCAL_SIGNER: &str = "an Undo signed by an account this instance owns";

/// Folds this module's interaction routes into the dispatcher's registry.
///
/// The three activity types by their own names and their three withdrawals
/// in the `Undo` table. `crate::federation::handlers::registry` calls it,
/// which is the whole composition of the live registry.
pub fn register(handlers: &mut Handlers) {
    handlers.register("Like", handle_like);
    handlers.register("EmojiReact", handle_emoji_react);
    handlers.register("Announce", handle_announce);
    handlers.register_undo("Like", handle_undo_like);
    handlers.register_undo("EmojiReact", handle_undo_emoji_react);
    handlers.register_undo("Announce", handle_undo_announce);
}

/// The `Like` route: a favourite, or — `content` naming one emoji — the
/// reaction in disguise Pleroma's `Transmogrifier` rewrite names.
///
/// # Errors
///
/// [`FederationError::Transient`] for a read or write the retry may
/// outlive, and the resolver's own for an object that cannot be fetched
/// yet; everything the shape or the standing refuses arrives as
/// [`Outcome::Ignored`].
pub fn handle_like(
    state: AppState,
    view: ActivityView,
    signer: User,
) -> BoxFuture<'static, Result<Outcome, FederationError>> {
    Box::pin(async move { apply_like(state, view, signer).await })
}

/// The `EmojiReact` route: a reaction stored under the delivered id, with
/// the custom emoji's image read off the activity's own `Emoji` tag.
///
/// # Errors
///
/// Those of [`handle_like`].
pub fn handle_emoji_react(
    state: AppState,
    view: ActivityView,
    signer: User,
) -> BoxFuture<'static, Result<Outcome, FederationError>> {
    Box::pin(async move { apply_emoji_react(state, view, signer).await })
}

/// The `Announce` route: the boost's reblog row, `uri`ed under the
/// delivered activity id and addressed as the `Announce` itself states.
///
/// # Errors
///
/// Those of [`handle_like`].
pub fn handle_announce(
    state: AppState,
    view: ActivityView,
    signer: User,
) -> BoxFuture<'static, Result<Outcome, FederationError>> {
    Box::pin(async move { apply_announce(state, view, signer).await })
}

/// The `undo_by_type["Like"]` route: withdraw the like whose activity id
/// the `Undo` names, for its maker only.
///
/// # Errors
///
/// Those of [`handle_like`].
pub fn handle_undo_like(
    state: AppState,
    view: ActivityView,
    signer: User,
) -> BoxFuture<'static, Result<Outcome, FederationError>> {
    Box::pin(async move { apply_undo_like(state, view, signer).await })
}

/// The `undo_by_type["EmojiReact"]` route: take back the reaction whose
/// activity id the `Undo` names, for its maker only.
///
/// # Errors
///
/// Those of [`handle_like`].
pub fn handle_undo_emoji_react(
    state: AppState,
    view: ActivityView,
    signer: User,
) -> BoxFuture<'static, Result<Outcome, FederationError>> {
    Box::pin(async move { apply_undo_emoji_react(state, view, signer).await })
}

/// The `undo_by_type["Announce"]` route: tombstone the boost whose
/// activity id the `Undo` names, for its maker only.
///
/// # Errors
///
/// Those of [`handle_like`].
pub fn handle_undo_announce(
    state: AppState,
    view: ActivityView,
    signer: User,
) -> BoxFuture<'static, Result<Outcome, FederationError>> {
    Box::pin(async move { apply_undo_announce(state, view, signer).await })
}
/// Shapes a database failure into the dispatcher's retryable arm, naming
/// the read or write that failed.
fn transient(what: &str, err: &AppError) -> FederationError {
    FederationError::Transient(format!(
        "could not fold the remote interaction's {what}: {err}"
    ))
}

#[cfg(test)]
mod tests {
    use serde_json::{Value, json};
    use sqlx::PgPool;

    use super::*;
    use crate::{
        config::TEST_BASE_URL,
        models::{
            status::{self, Status},
            user::{Flag, NewUser, insert, local_ap_id},
        },
    };

    /// The remote signer every direct-call fixture here delivers as.
    pub(super) const BOB: &str = "https://remote.example/users/bob";

    /// Test state: `pool` and the fixed test config, collaborators inert.
    fn app_state(pool: &PgPool) -> AppState {
        AppState::for_tests(pool.clone(), crate::config::Config::for_tests())
    }

    /// The stored mirror of the fixture signer.
    async fn signer(pool: &PgPool) -> User {
        insert(
            pool,
            &NewUser {
                nickname: "bob@remote.example".to_owned(),
                ap_id: BOB.to_owned(),
                local: Flag::new(false),
                domain: Some("remote.example".to_owned()),
                ..NewUser::default()
            },
        )
        .await
        .expect("the signer fixture inserts")
    }

    /// A local author named `nickname` and one public post of theirs.
    async fn local_post(pool: &PgPool, nickname: &str) -> Status {
        let author = insert(
            pool,
            &NewUser::local(nickname, &local_ap_id(TEST_BASE_URL, nickname)),
        )
        .await
        .expect("the author fixture inserts");
        status::insert(
            pool,
            TEST_BASE_URL,
            &status::NewStatus::local(&author, "interaction target"),
        )
        .await
        .expect("the status fixture inserts")
    }

    /// The dispatcher's normalized view of `kind`, named `n` on the
    /// signer's host, delivered by `actor`, carrying `object` and the
    /// fields of `extra`.
    pub(super) fn view(
        kind: &str,
        n: &str,
        actor: &str,
        object: Value,
        extra: Value,
    ) -> ActivityView {
        let mut activity = json!({
            "id": format!("https://remote.example/activities/{n}"),
            "type": kind,
            "actor": actor,
        });
        activity["object"] = object;
        if let Value::Object(fields) = extra {
            let map = activity
                .as_object_mut()
                .expect("the fixture activity is a map");
            for (key, value) in fields {
                let _previous = map.insert(key, value);
            }
        }
        ActivityView::parse(&activity).expect("the fixture is a dispatchable activity")
    }

    #[test]
    fn register_binds_the_three_interactions_and_their_withdrawals() {
        let mut handlers = Handlers::new();
        register(&mut handlers);
        for kind in ["Like", "EmojiReact", "Announce"] {
            assert!(handlers.handles(kind), "a {kind} routes here");
            assert!(handlers.handles_undo(kind), "its Undo routes here");
        }
        assert!(
            !handlers.handles("Dislike") && !handlers.handles("Zap"),
            "types that merely resemble a like stay the dispatcher's unknown-type ignore"
        );
    }

    #[tokio::test]
    async fn an_undo_naming_nothing_and_a_locally_signed_activity_fold_nothing() {
        let db = crate::test_db::test_db().await.expect("test database slot");
        let pool = db.pool().clone();
        let state = app_state(&pool);
        let bob = signer(&pool).await;
        let idless = view(
            "Undo",
            "undo-idless",
            BOB,
            json!({"type": "Like"}),
            json!({}),
        );
        let outcome = handle_undo_like(state.clone(), idless, bob)
            .await
            .expect("an ignore is not a failure");
        assert_eq!(outcome, Outcome::Ignored(NO_INNER_ID));

        let local = insert(
            &pool,
            &NewUser::local("self-bob", &local_ap_id(TEST_BASE_URL, "self-bob")),
        )
        .await
        .expect("the local fixture inserts");
        let posted = local_post(&pool, "guard-author").await;
        let guarded = view(
            "Like",
            "guard-1",
            &local.ap_id,
            json!(posted.uri),
            json!({}),
        );
        let refused = handle_like(state, guarded, local)
            .await
            .expect("an ignore is not a failure");
        assert!(
            matches!(refused, Outcome::Ignored(_)),
            "a local signer is refused"
        );
        assert!(
            sqlx::query_scalar::<_, i64>("SELECT count(*) FROM favourites")
                .fetch_one(&pool)
                .await
                .expect("the table reads")
                == 0,
            "and nothing was recorded"
        );
    }
}
