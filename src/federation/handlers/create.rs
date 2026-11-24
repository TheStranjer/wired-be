//! Inbound `Create` of a `Note`-family object: what a peer announcing a
//! fresh post does to this instance.
//!
//! The reference is Pleroma's `Pleroma.Web.ActivityPub.Transmogrifier`
//! `Create` clause of `handle_incoming` feeding
//! `Pleroma.Web.ActivityPub.SideEffects.handle_create`
//! (`lib/pleroma/web/activity_pub/transmogrifier.ex`,
//! `lib/pleroma/web/activity_pub/side_effects.ex`): a delivered `Create`
//! whose object is embedded is validated against its sender and folded
//! into a stored activity, and the same `Create` naming only an id has
//! that id dereferenced and folded the same way — here through
//! `resolve_status_as_actor` (in [`fetch_object`]).
//! The ticket rules are authoritative where the two differ:
//!
//! * [`create_note`], registered in the dispatcher's
//!   `create_by_object_type` table for every object type
//!   [`crate::federation::note_parser`] accepts (and as the dispatcher's
//!   one `create_by_reference` route), hands the embedded document to
//!   [`ingest::ingest_note`]. Before that it enforces the two
//!   facts the ticket names the handler's own: an *embedded* object's
//!   `attributedTo` must name the activity's `actor` (containment: a peer
//!   may create its own posts, never somebody else's), and a `Create`
//!   whose object is only an id is fetched — and folded only when the
//!   fetched document names that same delivering actor as its author.
//!   The type check the ticket states is
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
use uuid::Uuid;

use crate::{
    federation::{
        dispatch::{ActivityView, FederationError, Handlers, Outcome},
        fetch_object, json,
        note_parser::OBJECT_TYPES,
    },
    models::user::{self, User},
    services::ingest::{self, IngestOutcome},
    state::AppState,
};

/// Folds this module's note-create routes into the dispatcher's registry.
///
/// One `create_by_object_type` entry per object type the note parser
/// accepts, all bound to [`create_note`], and this module as the one
/// `create_by_reference` route — a `Create` whose object is only an id
/// reaches the same handler, which fetches the id instead of folding an
/// embedded document.
///
/// `crate::federation::handlers::registry` calls it, which is the whole
/// composition of the live registry.
pub fn register(handlers: &mut Handlers) {
    for object_type in OBJECT_TYPES {
        handlers.register_create(object_type, create_note);
    }
    handlers.register_create_reference(create_note);
}

/// The `Create` handler of the `Note`-family object types.
///
/// Folds the embedded document after checking the sender authored it, or
/// dereferences the bare id and folds what answers — again only when the
/// sender authored it.
///
/// # Errors
///
/// Those of [`ingest::ingest_note`] and
/// `resolve_status_as_actor` (in [`fetch_object`]):
/// [`FederationError::Permanent`] for a document the parser refuses,
/// [`FederationError::Transient`] for a database or network failure that
/// may pass on a retry.
pub fn create_note(
    state: AppState,
    view: ActivityView,
    signer: User,
) -> BoxFuture<'static, Result<Outcome, FederationError>> {
    Box::pin(async move { apply_create(state, view, signer).await })
}

/// The body of [`create_note`]: the embedded arm runs the handler's two
/// checks and the fold; the reference arm dereferences the id through the
/// fetch resolver and folds only what the delivering actor authored.
async fn apply_create(
    state: AppState,
    view: ActivityView,
    signer: User,
) -> Result<Outcome, FederationError> {
    let Some(object) = view.object.embedded() else {
        return apply_referenced_create(&state, &view, &signer).await;
    };
    if !names_actor(object, &view.actor) {
        debug!(
            actor = %view.actor,
            "a Create whose object is attributed to someone else; discarding"
        );
        return Ok(Outcome::Ignored("attributedTo is not the activity actor"));
    }
    let owner = load_inbox_owner(&state, view.inbox_owner).await?;
    match ingest::ingest_note_delivered(&state, &signer, object, owner.as_ref()).await? {
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

/// Loads the local account whose personal inbox received the delivery. A
/// gone row means the account was deleted after the delivery was queued: the
/// post is folded as a shared-inbox delivery would be.
async fn load_inbox_owner(
    state: &AppState,
    owner_id: Option<Uuid>,
) -> Result<Option<User>, FederationError> {
    let Some(id) = owner_id else {
        return Ok(None);
    };
    user::get_by_id(&state.db, id)
        .await
        .map_err(|err| FederationError::Transient(format!("the inbox owner read failed: {err}")))
}

/// The reference arm of [`apply_create`]: a `Create` whose object is only
/// an id names nothing to route on, so the id is fetched and folded under
/// the rule of the embedded arm's author check — the fetched document
/// belongs to the delivering actor or it is not folded at all
/// (see [`resolve_status_as_actor`](crate::federation::fetch_object::resolve_status_as_actor)).
async fn apply_referenced_create(
    state: &AppState,
    view: &ActivityView,
    signer: &User,
) -> Result<Outcome, FederationError> {
    let Some(id) = view.object.id() else {
        debug!("a Create whose object reference names nothing; discarding");
        return Ok(Outcome::Ignored("object reference without an id"));
    };
    if let Some(row) = fetch_object::resolve_status_as_actor(state, id, signer).await? {
        debug!(status = %row.id, "a Create's referenced object was fetched and folded");
        return Ok(Outcome::Handled);
    }
    debug!(object = %id, "a Create's referenced object resolved to nothing foldable");
    Ok(Outcome::Ignored("referenced object not ingestable"))
}

/// The handler's containment: the first actor reference reachable in the
/// object's `attributedTo` (string, object or array — [`json::as_array`]
/// and [`json::value_or_id`] settle the cardinality) must equal the
/// activity's `actor` id. A document without the field names nobody, and
/// fails the same check.
///
/// The chat handler shares the rule — a peer may create its own objects, of
/// whatever family, never somebody else's — so it reaches this through the
/// module boundary rather than restating it.
pub(crate) fn names_actor(object: &Value, actor: &str) -> bool {
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
    use axum::{
        Router,
        extract::State,
        http::header,
        response::{IntoResponse as _, Response},
        routing::get,
    };
    use serde_json::json;
    use sqlx::PgPool;

    use super::*;
    use crate::{
        config::Config,
        models::{
            status,
            user::{self, Flag, NewUser, User},
        },
        test_server::{bind, origin, spawn},
    };

    /// The actor every fixture here is delivered by.
    const BOB: &str = "https://remote.example/users/bob";

    /// Test state: `pool` and the fixed test config, collaborators inert.
    fn app_state(pool: &PgPool) -> AppState {
        AppState::for_tests(pool.clone(), Config::for_tests())
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

    /// The mirror of `bob` living on the mock peer's origin `base`: the
    /// delivering actor of the bare-id `Create` fixtures, and the author
    /// their documents name — origin containment only passes on a host
    /// that can actually answer.
    async fn loopback_signer(pool: &PgPool, base: &str) -> User {
        let host = base
            .strip_prefix("http://")
            .unwrap_or(base)
            .strip_prefix("https://")
            .unwrap_or(base)
            .to_owned();
        user::insert(
            pool,
            &NewUser {
                nickname: format!("bob@{host}"),
                ap_id: format!("{base}/users/bob"),
                local: Flag::new(false),
                domain: Some(host),
                ..NewUser::default()
            },
        )
        .await
        .expect("the loopback signer fixture inserts")
    }

    /// A mock peer that answers every path with `document`, labelled the
    /// way a federation peer labels an object.
    fn note_peer(document: Value) -> Router {
        Router::new()
            .route("/{*path}", get(serve_document))
            .with_state(document)
    }

    /// [`note_peer`]'s handler: the state document, `ActivityPub`-typed.
    async fn serve_document(State(document): State<Value>) -> Response {
        (
            [(header::CONTENT_TYPE, "application/activity+json")],
            document.to_string(),
        )
            .into_response()
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
            "and this module's table alone stays the note family: the chat handler claims `ChatMessage` beside it"
        );
        assert!(
            handlers.takes_create_reference(),
            "and the bare-id Create route belongs to this handler too"
        );
    }

    #[tokio::test]
    async fn a_create_whose_object_is_only_an_id_is_fetched_and_folded() {
        let db = crate::test_db::test_db().await.expect("test database slot");
        let pool = db.pool().clone();
        let state = app_state(&pool);
        let listener = bind().await;
        let base = origin(&listener);
        let bob = loopback_signer(&pool, &base).await;
        let note = json!({
            "id": format!("{base}/objects/bare"),
            "type": "Note",
            "attributedTo": bob.ap_id,
            "to": ["https://www.w3.org/ns/activitystreams#Public"],
            "content": "<p>fetched by reference</p>",
        });
        let server = spawn(listener, note_peer(note));
        let activity = json!({
            "id": format!("{base}/activities/bare"),
            "type": "Create",
            "actor": bob.ap_id.clone(),
            "object": format!("{base}/objects/bare"),
        });

        let outcome = create_note(state, view_of(&activity), bob)
            .await
            .expect("the fetch folds the object");

        assert_eq!(
            outcome,
            Outcome::Handled,
            "the referenced note becomes a status"
        );
        assert_eq!(
            status::get_by_uri(&pool, &format!("{base}/objects/bare"))
                .await
                .expect("the uri read")
                .expect("the fetched status is stored")
                .content,
            "<p>fetched by reference</p>",
            "as the document the peer served"
        );
        server.abort();
    }

    #[tokio::test]
    async fn a_referenced_note_authored_by_somebody_else_is_not_folded() {
        let db = crate::test_db::test_db().await.expect("test database slot");
        let pool = db.pool().clone();
        let state = app_state(&pool);
        let listener = bind().await;
        let base = origin(&listener);
        let bob = loopback_signer(&pool, &base).await;
        let note = json!({
            "id": format!("{base}/objects/stolen"),
            "type": "Note",
            "attributedTo": format!("{base}/users/mallory"),
            "to": ["https://www.w3.org/ns/activitystreams#Public"],
            "content": "<p>not bob's</p>",
        });
        let server = spawn(listener, note_peer(note));
        let activity = json!({
            "id": format!("{base}/activities/stolen"),
            "type": "Create",
            "actor": bob.ap_id.clone(),
            "object": format!("{base}/objects/stolen"),
        });

        let outcome = create_note(state, view_of(&activity), bob)
            .await
            .expect("a refused fetch is an ignore, not a failure");

        assert_eq!(
            outcome,
            Outcome::Ignored("referenced object not ingestable"),
            "a peer may create its own posts over a bare id too, never somebody else's"
        );
        assert!(
            status::get_by_uri(&pool, &format!("{base}/objects/stolen"))
                .await
                .expect("the uri read")
                .is_none(),
            "and nothing was stored under the foreign object id"
        );
        server.abort();
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
