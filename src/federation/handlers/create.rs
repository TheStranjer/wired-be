//! Inbound `Create` of a `Note`-family object: what a peer announcing a
//! fresh post does to this instance.
//!
//! The reference is Pleroma's `Pleroma.Web.ActivityPub.Transmogrifier`
//! `Create` clause of `handle_incoming` feeding
//! `Pleroma.Web.ActivityPub.SideEffects.handle_create`
//! (`lib/pleroma/web/activity_pub/transmogrifier.ex`,
//! `lib/pleroma/web/activity_pub/side_effects.ex`): a delivered `Create`
//! whose object is embedded is validated against its sender and folded
//! into a stored activity, and the same `Create` naming only an id is
//! (Pleroma) fetched over the network or (here, per this ticket) left
//! alone. The ticket rules are authoritative where the two differ:
//!
//! * [`create_note`], registered in the dispatcher's
//!   `create_by_object_type` table for every object type
//!   [`crate::federation::note_parser`] accepts, hands the embedded
//!   document to [`ingest::ingest_note`]. Before that it enforces the two
//!   facts the ticket names the handler's own: the object must be *embedded* (a
//!   `Create` whose object is only an id is ignored — this instance does
//!   not fetch arbitrary objects yet), and its `attributedTo` must name
//!   the activity's `actor` (containment: a peer may create its own
//!   posts, never somebody else's). The type check the ticket states is
//!   the routing itself — only the registered `Note`-family types reach
//!   this handler at all.
//! * [`register`] is the one line [`super::registry`] calls: a later
//!   ticket that teaches this instance another createable object (a
//!   `ChatMessage`, the answer types of a poll vote) registers its own
//!   handler in its own file, beside this one.
//!
//! Both the ingest's refusals and this module's ignorable shapes are
//! reported, never fatal: the dispatcher's dedupe ledger has already
//! claimed the activity, and a discarded job that cannot succeed on retry
//! is the correct treatment of a document that will not change.

use futures::future::BoxFuture;
use serde_json::Value;
use tracing::debug;

use crate::{
    federation::{
        dispatch::{ActivityView, FederationError, Handlers, Outcome},
        json,
        note_parser::OBJECT_TYPES,
    },
    models::user::User,
    services::ingest::{self, IngestOutcome},
    state::AppState,
};

/// Folds this module's note-create routes into the dispatcher's registry:
/// one `create_by_object_type` entry per object type the note parser
/// accepts, all bound to [`create_note`].
///
/// `crate::federation::handlers::registry` calls it, which is the whole
/// composition of the live registry.
pub fn register(handlers: &mut Handlers) {
    for object_type in OBJECT_TYPES {
        handlers.register_create(object_type, create_note);
    }
}

/// The `Create` handler of the `Note`-family object types: check the
/// embedded document is the sender's own, then fold it.
///
/// # Errors
///
/// Those of [`ingest::ingest_note`]: [`FederationError::Permanent`] for a
/// document the parser refuses, [`FederationError::Transient`] for a
/// database failure that may pass on a retry.
pub fn create_note(
    state: AppState,
    view: ActivityView,
    signer: User,
) -> BoxFuture<'static, Result<Outcome, FederationError>> {
    Box::pin(async move { apply_create(state, view, signer).await })
}

/// The body of [`create_note`]: the handler's own two checks, then the
/// fold and the outcome mapping onto the dispatcher's vocabulary.
async fn apply_create(
    state: AppState,
    view: ActivityView,
    signer: User,
) -> Result<Outcome, FederationError> {
    let Some(object) = view.object.embedded() else {
        debug!(
            object_id = view.object.id().unwrap_or_default(),
            "a Create whose object is only a reference; this instance does not fetch it yet"
        );
        return Ok(Outcome::Ignored("object not embedded"));
    };
    if !names_actor(object, &view.actor) {
        debug!(
            actor = %view.actor,
            "a Create whose object is attributed to someone else; discarding"
        );
        return Ok(Outcome::Ignored("attributedTo is not the activity actor"));
    }
    match ingest::ingest_note(&state, &signer, object).await? {
        IngestOutcome::Created(row) => {
            debug!(status = %row.id, "a Create folded a new remote status");
            Ok(Outcome::Handled)
        }
        IngestOutcome::Existing(row) => {
            debug!(status = %row.id, "a Create restated an already stored status");
            Ok(Outcome::Handled)
        }
        IngestOutcome::Ignored(reason) => Ok(Outcome::Ignored(reason)),
    }
}

/// The handler's containment: the first actor reference reachable in the
/// object's `attributedTo` (string, object or array — [`json::as_array`]
/// and [`json::value_or_id`] settle the cardinality) must equal the
/// activity's `actor` id. A document without the field names nobody, and
/// fails the same check.
fn names_actor(object: &Value, actor: &str) -> bool {
    object
        .get("attributedTo")
        .map(json::as_array)
        .is_some_and(|entries| {
            entries
                .iter()
                .find_map(json::value_or_id)
                .is_some_and(|who| who == actor)
        })
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;

    use serde_json::json;
    use sqlx::PgPool;

    use super::*;
    use crate::{
        config::Config,
        http_client::HttpClient,
        models::{
            status,
            user::{self, Flag, NewUser, User},
        },
        uploads::UploaderBackend,
    };

    /// The actor every fixture here is delivered by.
    const BOB: &str = "https://remote.example/users/bob";

    /// Test state: `pool` and the fixed test config, collaborators inert.
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

    /// The stored mirror of the fixture author.
    async fn signer(pool: &PgPool) -> User {
        user::insert(
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

    /// A `Create` by `bob` of an embedded note named `n`.
    fn create_of(n: &str) -> Value {
        json!({
            "id": format!("https://remote.example/activities/{n}"),
            "type": "Create",
            "actor": BOB,
            "to": ["https://www.w3.org/ns/activitystreams#Public"],
            "object": {
                "id": format!("https://remote.example/objects/{n}"),
                "type": "Note",
                "attributedTo": BOB,
                "to": ["https://www.w3.org/ns/activitystreams#Public"],
                "content": format!("<p>handler note {n}</p>"),
            },
        })
    }

    /// Normalizes `activity` the way the dispatcher does.
    fn view_of(activity: &Value) -> ActivityView {
        ActivityView::parse(activity).expect("the fixture is a dispatchable activity")
    }

    #[test]
    fn register_binds_every_object_type_the_parser_accepts() {
        let mut handlers = Handlers::new();
        register(&mut handlers);
        let claimed = OBJECT_TYPES
            .into_iter()
            .filter(|object_type| handlers.handles_create(object_type))
            .count();
        assert_eq!(
            claimed,
            OBJECT_TYPES.len(),
            "one entry per note-family type, nothing else"
        );
        assert!(
            handlers.handles_create("Question"),
            "polls create through the same handler"
        );
        assert!(
            !handlers.handles_create("ChatMessage"),
            "and the chat-create ticket has not claimed its key yet"
        );
    }

    #[tokio::test]
    async fn a_create_whose_object_is_only_an_id_is_ignored() {
        let db = crate::test_db::test_db().await.expect("test database slot");
        let pool = db.pool().clone();
        let state = app_state(&pool);
        let bob = signer(&pool).await;
        let activity = json!({
            "id": "https://remote.example/activities/bare",
            "type": "Create",
            "actor": BOB,
            "object": "https://remote.example/objects/bare",
        });

        let outcome = create_note(state, view_of(&activity), bob)
            .await
            .expect("the ignore is not a failure");

        assert_eq!(
            outcome,
            Outcome::Ignored("object not embedded"),
            "this instance does not fetch referenced objects yet"
        );
    }

    #[tokio::test]
    async fn a_create_of_somebody_elses_note_is_ignored() {
        let db = crate::test_db::test_db().await.expect("test database slot");
        let pool = db.pool().clone();
        let state = app_state(&pool);
        let bob = signer(&pool).await;
        let activity = json!({
            "id": "https://remote.example/activities/impersonation",
            "type": "Create",
            "actor": BOB,
            "object": {
                "id": "https://remote.example/objects/stolen",
                "type": "Note",
                "attributedTo": "https://remote.example/users/mallory",
                "content": "<p>not bob's</p>",
            },
        });

        let outcome = create_note(state, view_of(&activity), bob)
            .await
            .expect("the ignore is not a failure");

        assert_eq!(
            outcome,
            Outcome::Ignored("attributedTo is not the activity actor"),
            "a peer may create its own posts, never somebody else's"
        );
    }

    #[tokio::test]
    async fn an_embedded_note_is_folded_once_and_restated_thereafter() {
        let db = crate::test_db::test_db().await.expect("test database slot");
        let pool = db.pool().clone();
        let state = app_state(&pool);
        let bob = signer(&pool).await;
        let author_id = bob.id;

        let outcome = create_note(state.clone(), view_of(&create_of("folded")), bob)
            .await
            .expect("the create folds");
        assert_eq!(outcome, Outcome::Handled, "the note becomes a status");
        assert_eq!(
            status::get_by_uri(&pool, "https://remote.example/objects/folded")
                .await
                .expect("the uri read")
                .expect("the status is stored")
                .user_id,
            author_id,
            "under the delivering author's row"
        );

        let again = user::get_by_id(&pool, author_id)
            .await
            .expect("the author reads back")
            .expect("the author is stored");
        let restated = create_note(state, view_of(&create_of("folded")), again)
            .await
            .expect("the second create folds too");
        assert_eq!(restated, Outcome::Handled, "as the existing row");
        let rows = sqlx::query_scalar::<_, i64>("SELECT count(*) FROM statuses WHERE uri = $1")
            .bind("https://remote.example/objects/folded")
            .fetch_one(&pool)
            .await
            .expect("the row count reads");
        assert_eq!(rows, 1, "and still exactly one row");
    }

    #[test]
    fn the_actor_check_reads_every_attributed_to_shape() {
        let shapes = [
            json!({"attributedTo": BOB}),
            json!({"attributedTo": [{ "id": BOB, "type": "Person" }]}),
            json!({"attributedTo": { "id": BOB, "type": "Person" }}),
        ];
        for shape in shapes {
            assert!(
                names_actor(&shape, BOB),
                "the {shape} spelling names the actor"
            );
        }
        let refusals = [
            json!({}),
            json!({"attributedTo": null}),
            json!({"attributedTo": "https://remote.example/users/mallory"}),
            json!({"attributedTo": ["https://remote.example/users/mallory"]}),
        ];
        for refusal in refusals {
            assert!(
                !names_actor(&refusal, BOB),
                "the {refusal} spelling does not"
            );
        }
    }
}
