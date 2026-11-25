//! Inbound poll votes: what a peer announcing somebody's ballot on this
//! instance's own poll does to it.
//!
//! The reference is Pleroma's `Transmogrifier.fix_type` and the `Answer` arm
//! of `SideEffects.handle_object_creation`
//! (`lib/pleroma/web/activity_pub/transmogrifier.ex`,
//! `lib/pleroma/web/activity_pub/side_effects.ex`) feeding
//! `Object.increase_vote_count` (`lib/pleroma/object.ex`), with
//! `AnswerValidator`
//! (`lib/pleroma/web/activity_pub/object_validators/answer_validator.ex`) as
//! the document's own gate: a `Create` whose `Note` names an option and
//! answers a `Question` is not a post at all — it is one mark on that poll,
//! and what it moves is the option's count, the poll's total, and, once per
//! voter, the voter count. The ticket rules are authoritative where the two
//! differ:
//!
//! * [`is_poll_vote`] is the [`Create`](super::create) sub-registry's
//!   pre-check for a `Note`, and Pleroma's `fix_type` clause restated: the
//!   document carries a non-empty `name` (Mastodon's and Pleroma's option
//!   title), it carries no `content` of its own, and its `inReplyTo`
//!   resolves to a *local*, live status this instance stores that carries a
//!   poll. A `Note` failing any of the three is an ordinary post and reaches
//!   the note handler's entry beside this guard — the guard, never the
//!   handler, is what answers "is this a vote?", and a vote the guard claims
//!   is never folded into a status even when its own gates refuse it.
//! * [`create_poll_vote`] is the handler a claimed document routes to. The
//!   voter is the activity's `actor` — the delivery's authenticated signer,
//!   which [`crate::federation::dispatch`]'s containment has already proved
//!   — and the ballot marks the option whose title the document names. The
//!   handler ignores (the dispatcher's static-reason vocabulary, never a
//!   failure) an answer attributed to somebody else, a question the voter may
//!   not see, a poll whose deadline has run out, the poll's own author, a
//!   second ballot on a poll that takes one, and an answer naming no option of
//!   its poll. What it accepts it records through [`poll::cast_vote`] — the
//!   one write `POST /api/v1/polls/{id}/votes` runs — and finishes with the
//!   vote endpoint's `after_vote_cast` hook
//!   ([`crate::api::polls`]). No ballot notifies the author; `poll` is
//!   raised only when the poll ends.
//!
//! A `Create` naming one option of a [`multiple`](Poll::multiple) poll is
//! therefore one more mark on its voter's ballot rather than a refused second
//! vote: a remote multiple-choice ballot arrives as one activity per chosen
//! option, and it is [`poll::cast_vote`] that keeps the voter counted once
//! across them.
//!
//! The other halves of a federated poll are elsewhere by the ticket's own
//! hand: this instance's own votes travelling out as answer activities is the
//! outbound poll ticket, and the `Update` of a remote `Question`'s totals is
//! the note-update ticket. The one thing sent from here is the debounced
//! `Update` of the local `Question`'s new tally
//! ([`after_local_poll_changed`]).

use chrono::Utc;
use futures::future::BoxFuture;
use serde_json::Value;
use tracing::debug;

use crate::{
    api::polls::{after_vote_cast, poll_closed},
    error::AppError,
    federation::{
        dispatch::{ActivityView, FederationError, Handlers, Outcome, TransientExt as _},
        json,
    },
    models::{
        poll::{self, Poll, PollOption},
        status::{self, Status},
        status_visibility,
        user::User,
    },
    services::federation_out::after_local_poll_changed,
    state::AppState,
};

/// The object type a poll vote arrives as: a plain `Note`, the convention
/// Mastodon and Pleroma share. Pleroma rewrites it into an `Answer` only
/// after resolving the question behind it — the same test [`is_poll_vote`]
/// makes here, ahead of the note handler.
const NOTE_TYPE: &str = "Note";

/// The document keys an answer may name its voter under: Mastodon spells it
/// `attributedTo`, Pleroma's `fix_actor` keeps an `actor` beside it.
const VOTER_KEYS: [&str; 2] = ["attributedTo", "actor"];

/// Folds this module's poll-vote route into the dispatcher's registry: the
/// `Note` guard and its handler, registered ahead of the note ticket's
/// unguarded entry under the same key.
///
/// `crate::federation::handlers::registry` calls it, which is the whole
/// composition of the live registry.
pub fn register(handlers: &mut Handlers) {
    handlers.register_create_prechecked(NOTE_TYPE, is_poll_vote, create_poll_vote);
}

/// The `Create` pre-check: whether an embedded `Note` is one answer to one
/// option of a poll this instance owns rather than a post wearing the `Note`
/// shape. The [module docs](self) state the three clauses.
///
/// Nothing here refuses anything: a document that is not a vote is not
/// claimed and folds through the object type's own handler, so the answer to
/// a plain post is a status and the answer to a remote poll is whatever the
/// note ingest makes of it. A database failure reads as "not claimed" for
/// the same reason — the fall-through's own reads will fail the same way, and
/// the dispatcher's retry folds the activity once the database is back.
pub fn is_poll_vote(state: AppState, object: &Value) -> BoxFuture<'static, bool> {
    let Some(answer) = answer_of(object) else {
        return Box::pin(async { false });
    };
    Box::pin(async move {
        match question_of(&state, &answer.question_uri).await {
            Ok(Some(_)) => true,
            Ok(None) => false,
            Err(err) => {
                debug!(
                    %err,
                    "the read behind an answer's question failed; the document is not claimed"
                );
                false
            }
        }
    })
}

/// The `Create` handler of a claimed answer: re-resolve what the guard
/// resolved, walk the ballot's gates, and record the mark through the vote
/// endpoint's own write.
///
/// # Errors
///
/// [`FederationError::Transient`] for a database failure the retry may
/// outlive; every refusal of shape, standing or visibility arrives as
/// [`Outcome::Ignored`] with the reason the [module docs](self) list.
pub fn create_poll_vote(
    state: AppState,
    view: ActivityView,
    voter: User,
) -> BoxFuture<'static, Result<Outcome, FederationError>> {
    Box::pin(async move { apply_vote(state, view, voter).await })
}

/// One delivered answer, in the parts of the document the fold needs.
#[derive(Debug)]
struct IncomingAnswer {
    /// The option the answer chose: its `name`, trimmed.
    option: String,
    /// The question it answers: the object's `inReplyTo` reference.
    question_uri: String,
}

/// The question an answer names, resolved against stored rows.
#[derive(Debug)]
struct Question {
    /// The status asking it: local, live, and named by the answer.
    posted: Status,
    /// The poll standing on that status.
    stand: Poll,
}

/// What one answer's gates decided.
enum Decision {
    /// The ballot may be recorded: the option its title names.
    Mark(PollOption),
    /// Why nothing is marked; the reason is static and rides the dispatcher's
    /// debug log.
    Refuse(&'static str),
}

/// The body of [`create_poll_vote`]: the document's shape, its authorship,
/// the question behind it, the ballot's gates, and the write.
async fn apply_vote(
    state: AppState,
    view: ActivityView,
    voter: User,
) -> Result<Outcome, FederationError> {
    let Some(object) = view.object.embedded() else {
        debug!(
            object_id = view.object.id().unwrap_or_default(),
            "a vote Create whose object is only a reference; this instance does not fetch it"
        );
        return Ok(Outcome::Ignored("object not embedded"));
    };
    let Some(answer) = answer_of(object) else {
        return Ok(Outcome::Ignored("the claimed note names no answer"));
    };
    if names_somebody_else(object, &view.actor) {
        return Ok(Outcome::Ignored(
            "the answer is attributed to somebody other than the activity actor",
        ));
    }
    let Some(question) = question_of(&state, &answer.question_uri).await? else {
        return Ok(Outcome::Ignored(
            "the answer questions no poll of this instance",
        ));
    };
    match gate(&state, &question, &answer.option, &voter).await? {
        Decision::Refuse(reason) => {
            debug!(reason, "the delivered poll vote is refused");
            Ok(Outcome::Ignored(reason))
        }
        Decision::Mark(option) => mark(&state, &question, &option, &voter).await,
    }
}

/// Reads the answer's shape off the embedded document: a non-empty `name` is
/// the chosen option, and the document says nothing else — no `content`, or
/// one that is empty once its whitespace is gone. `None` is not a poll vote;
/// a document carrying a body of its own is the post its object type's own
/// handler folds.
fn answer_of(object: &Value) -> Option<IncomingAnswer> {
    let option = object
        .get("name")
        .and_then(Value::as_str)
        .map(str::trim)
        .filter(|title| !title.is_empty())?
        .to_owned();
    if carries_body(object) {
        return None;
    }
    Some(IncomingAnswer {
        option,
        question_uri: object.get("inReplyTo").and_then(json::value_or_id)?,
    })
}

/// Whether the document carries a post of its own: any `content` that is not
/// absent, null, or blank text. Pleroma's answer validator asks the same
/// question of the fields it keeps — an answer names an option and says
/// nothing.
fn carries_body(object: &Value) -> bool {
    object
        .get("content")
        .is_some_and(|value| !declares_nothing(value))
}

/// Whether a `content` value states nothing: the JSON `null`, an empty or
/// whitespace-only string, or an empty array or object. Anything else —
/// including a shape this fold cannot read — is content.
fn declares_nothing(value: &Value) -> bool {
    match value {
        Value::Null => true,
        Value::String(text) => text.trim().is_empty(),
        Value::Array(items) => items.is_empty(),
        Value::Object(map) => map.is_empty(),
        Value::Bool(_) | Value::Number(_) => false,
    }
}

/// Whether the answer attributes itself to an account other than the one that
/// delivered it: every voter key that is present must name the activity's
/// `actor`, which is what a peer voting as another account would break. A
/// document that names neither is its sender's own, and the dispatcher's
/// containment has already proved that sender is the signer.
fn names_somebody_else(object: &Value, actor: &str) -> bool {
    VOTER_KEYS
        .iter()
        .filter_map(|key| object.get(*key))
        .filter_map(json::value_or_id)
        .any(|who| who != actor)
}

/// The stored, live, *local* status behind `question_uri` and the poll
/// standing on it: `None` when the answer names something this instance does
/// not own — an unstored or tombstoned post, a remote one, or a plain status
/// with no poll.
///
/// # Errors
///
/// [`FederationError::Transient`] for either read's own database failure.
async fn question_of(
    state: &AppState,
    question_uri: &str,
) -> Result<Option<Question>, FederationError> {
    let stored = status::get_by_uri(&state.db, question_uri)
        .await
        .transient("the question read failed")?;
    let Some(posted) = stored.filter(|row| row.local && row.deleted_at.is_none()) else {
        return Ok(None);
    };
    let stand = poll::get_for_status(&state.db, posted.id)
        .await
        .transient("the poll read failed")?;
    Ok(stand.map(|stand| Question { posted, stand }))
}

/// The answer's gates, in the order the ticket names them: the question's
/// visibility to the voter, the poll's deadline, the author's standing, a
/// second ballot on a poll that takes one, and the title's option. The
/// visibility rule is the instance's one [`status_visibility`] read, so an
/// answer from an account that may not see its question is refused the same
/// way a timeline would hide the question from it.
///
/// # Errors
///
/// [`FederationError::Transient`] for a visibility, `has_voted` or options
/// read the database could not answer.
async fn gate(
    state: &AppState,
    question: &Question,
    option: &str,
    voter: &User,
) -> Result<Decision, FederationError> {
    if !status_visibility::can_view(&state.db, &question.posted, Some(voter))
        .await
        .transient("the visibility read failed")?
    {
        return Ok(Decision::Refuse(
            "the question is not visible to the account that answered it",
        ));
    }
    if poll_closed(&question.stand, Utc::now()) {
        return Ok(Decision::Refuse("the poll has closed"));
    }
    if question.posted.user_id == voter.id {
        return Ok(Decision::Refuse("the poll's author cannot vote on it"));
    }
    if !question.stand.multiple
        && poll::has_voted(&state.db, question.stand.id, voter.id)
            .await
            .transient("the voter's marks read failed")?
    {
        return Ok(Decision::Refuse("the voter has already voted on this poll"));
    }
    let options = poll::options(&state.db, question.stand.id)
        .await
        .transient("the options read failed")?;
    Ok(options
        .into_iter()
        .find(|opt| opt.title == option)
        .map_or_else(
            || Decision::Refuse("no option of this poll carries that title"),
            Decision::Mark,
        ))
}

/// The ballot's write: [`poll::cast_vote`] for the mark, then the vote
/// endpoint's documented [`after_vote_cast`] hook over the row as the
/// transaction left it. A ballot (one `Create` per option of a
/// [`multiple`](Poll::multiple) poll) raises no notification: the author
/// is told `poll` only when the poll ends.
///
/// # Errors
///
/// [`FederationError::Transient`] for a ballot write the retry may outlive,
/// [`FederationError::Permanent`] for a poll deleted under it.
async fn mark(
    state: &AppState,
    question: &Question,
    option: &PollOption,
    voter: &User,
) -> Result<Outcome, FederationError> {
    let stand = &question.stand;
    if !poll::cast_vote(&state.db, stand.id, voter.id, &[option.id])
        .await
        .map_err(|err| cast_failure(&err, stand.id))?
    {
        debug!(
            poll = %stand.id,
            voter = %voter.id,
            option = option.position,
            "that voter's mark on this option already stands; the ballot counted nothing"
        );
        return Ok(Outcome::Ignored("the voter has already marked that option"));
    }
    let updated = poll::get_by_id(&state.db, stand.id)
        .await
        .transient("the poll read failed")?;
    if let Some(updated) = updated {
        after_vote_cast(state, &updated, voter, &[option.id]).await;
        // the tally moved on a poll this instance owns: tell its audience
        after_local_poll_changed(state, &question.posted).await;
    }
    debug!(
        poll = %stand.id,
        voter = %voter.id,
        option = option.position,
        "a federated ballot marked a poll"
    );
    Ok(Outcome::Handled)
}

/// Maps the ballot write's failure onto the dispatcher's vocabulary: a poll
/// gone at the lock is nothing a retry can fix, anything else is the database
/// being unreachable.
fn cast_failure(err: &AppError, poll_id: uuid::Uuid) -> FederationError {
    if matches!(err, AppError::NotFound) {
        return FederationError::Permanent(format!(
            "the poll {poll_id} was deleted under the ballot"
        ));
    }
    FederationError::Transient(format!("the ballot write failed: {err}"))
}

#[cfg(test)]
mod tests {
    use serde_json::json;

    use super::*;
    use crate::{
        models::user::{self, Flag, NewUser},
        test_support::app_state,
    };

    /// The actor every fixture here is delivered by.
    const BOB: &str = "https://remote.example/users/bob";

    /// The stored mirror of the fixture voter.
    async fn voter(pool: &sqlx::PgPool) -> User {
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
        .expect("the voter fixture inserts")
    }

    /// The question fixture: a local status of `alice` carrying a
    /// two-option poll closed `mins` minutes from now (`None`: a poll with no
    /// deadline), answered as `(uri, poll id)`.
    async fn question(
        pool: &sqlx::PgPool,
        author: &User,
        options: &[&str],
        multiple: bool,
        mins: Option<i64>,
    ) -> (String, Poll) {
        let posted = status::insert(
            pool,
            crate::config::TEST_BASE_URL,
            &status::NewStatus::local(author, "tea or coffee?"),
        )
        .await
        .expect("the question status inserts");
        let stand = poll::create(
            &mut pool.acquire().await.expect("a slot connection"),
            posted.id,
            &poll::PollParams {
                options: options.iter().map(|title| (*title).to_owned()).collect(),
                multiple,
                expires_at: mins.map(|count| Utc::now() + chrono::TimeDelta::minutes(count)),
                ..poll::PollParams::default()
            },
        )
        .await
        .expect("the question poll creates");
        (posted.uri, stand)
    }

    /// The dispatcher's normalized view of a `Create` of `object`.
    fn view_of(object: &Value) -> ActivityView {
        ActivityView::parse(&json!({
            "id": "https://remote.example/activities/vote-1",
            "type": "Create",
            "actor": BOB,
            "object": object,
        }))
        .expect("the fixture is a dispatchable activity")
    }

    /// The answer document this module folds: `option` chosen on the question
    /// `question_uri`, with every extra field `extra` merges over it.
    fn answer(option: &str, question_uri: &str, extra: Value) -> Value {
        let mut object = json!({
            "id": "https://remote.example/objects/vote-1",
            "type": "Note",
            "attributedTo": BOB,
            "to": ["https://www.w3.org/ns/activitystreams#Public"],
            "inReplyTo": question_uri,
            "name": option,
        });
        if let Value::Object(fields) = extra {
            let map = object.as_object_mut().expect("the fixture is a map");
            for (key, value) in fields {
                let _previous = map.insert(key, value);
            }
        }
        object
    }

    #[test]
    fn register_binds_the_answer_guard_ahead_of_the_note() {
        let mut handlers = Handlers::new();
        register(&mut handlers);
        assert!(
            handlers.pre_checks_create("Note"),
            "a `Create` of a `Note` is put to the answer guard first"
        );
        assert!(
            !handlers.handles_create("Note"),
            "and this module claims no object type of its own: it shares `Note` with the note handler"
        );
        assert!(
            !handlers.pre_checks_create("Question"),
            "a `Question` is a poll, not an answer, and reaches the note handler untouched"
        );
    }

    #[test]
    fn the_answer_shape_is_a_title_with_no_body() {
        let plain = answer("tea", "https://test.example/objects/q", json!({}));
        let read = answer_of(&plain).expect("a title and no content is an answer");
        assert_eq!(read.option, "tea", "the `name` is the chosen option");
        assert_eq!(
            read.question_uri, "https://test.example/objects/q",
            "and `inReplyTo` is the question it marks"
        );

        for body in [json!("<p>with a body</p>"), json!("   x"), json!(["x"])] {
            let object = answer(
                "tea",
                "https://test.example/objects/q",
                json!({"content": body}),
            );
            assert!(
                answer_of(&object).is_none(),
                "content of any shape that says something is a post, not a ballot: {object}"
            );
        }
        for empty in [json!(""), json!("   "), json!([]), json!({})] {
            let object = answer(
                "tea",
                "https://test.example/objects/q",
                json!({"content": empty}),
            );
            assert!(
                answer_of(&object).is_some(),
                "content that says nothing still leaves an answer: {object}"
            );
        }
        for missing in [json!(null), json!(""), json!("   ")] {
            let object = answer(
                "",
                "https://test.example/objects/q",
                json!({"name": missing}),
            );
            assert!(
                answer_of(&object).is_none(),
                "no option title is no answer: {object}"
            );
        }
        let orphan = json!({
            "id": "https://remote.example/objects/vote-2",
            "type": "Note",
            "name": "tea",
        });
        assert!(
            answer_of(&orphan).is_none(),
            "an answer with no question behind it is not one"
        );
    }

    #[test]
    fn the_voter_claim_is_the_activity_actor() {
        let own = answer("tea", "q", json!({"attributedTo": BOB}));
        assert!(
            !names_somebody_else(&own, BOB),
            "an answer attributed to the actor is theirs"
        );
        let spaced = answer("tea", "q", json!({"actor": BOB, "attributedTo": null}));
        assert!(
            !names_somebody_else(&spaced, BOB),
            "the `actor` spelling counts, and a null is no claim"
        );
        let silent = json!({"type": "Note", "name": "tea"});
        assert!(
            !names_somebody_else(&silent, BOB),
            "and a document that names nobody is its sender's own"
        );
        for stolen in [
            json!({"attributedTo": "https://remote.example/users/carol"}),
            json!({"actor": "https://remote.example/users/carol"}),
        ] {
            let object = answer("tea", "q", stolen);
            assert!(
                names_somebody_else(&object, BOB),
                "an answer naming somebody else is not the actor's ballot: {object}"
            );
        }
    }

    #[tokio::test]
    async fn the_guard_demands_a_local_question_carrying_a_poll() {
        let db = crate::test_db::test_db().await.expect("test database slot");
        let pool = db.pool().clone();
        let state = app_state(&pool);
        let alice = user::insert(
            &pool,
            &NewUser::local(
                "alice",
                &user::local_ap_id(crate::config::TEST_BASE_URL, "alice"),
            ),
        )
        .await
        .expect("the author fixture inserts");
        let (uri, _stand) = question(&pool, &alice, &["tea", "coffee"], false, None).await;
        let plain = status::insert(
            &pool,
            crate::config::TEST_BASE_URL,
            &status::NewStatus::local(&alice, "no poll here"),
        )
        .await
        .expect("the plain status inserts");
        let remote = json!({
            "id": "https://remote.example/objects/vote-remote",
            "type": "Note",
            "name": "tea",
            "inReplyTo": "https://remote.example/objects/not-stored",
        });

        assert!(
            is_poll_vote(state.clone(), &answer("tea", &uri, json!({}))).await,
            "a title, no body, and a local poll behind it is a vote"
        );
        assert!(
            !is_poll_vote(state.clone(), &answer("tea", &plain.uri, json!({}))).await,
            "a local status with no poll is a plain reply"
        );
        assert!(
            !is_poll_vote(state, &remote).await,
            "a question this instance does not store is nobody's ballot"
        );
    }

    #[tokio::test]
    async fn a_vote_claimed_by_the_guard_is_still_refused_on_its_own_gates() {
        let db = crate::test_db::test_db().await.expect("test database slot");
        let pool = db.pool().clone();
        let state = app_state(&pool);
        let alice = user::insert(
            &pool,
            &NewUser::local(
                "alice",
                &user::local_ap_id(crate::config::TEST_BASE_URL, "alice"),
            ),
        )
        .await
        .expect("the author fixture inserts");
        let bob = voter(&pool).await;
        let (uri, stand) = question(&pool, &alice, &["tea", "coffee"], false, None).await;

        let unknown = create_poll_vote(
            state.clone(),
            view_of(&answer("sandwich", &uri, json!({}))),
            user::get_by_ap_id(&pool, BOB)
                .await
                .expect("the voter fixture reads")
                .expect("the voter fixture is stored"),
        )
        .await
        .expect("an ignore is not a failure");
        assert_eq!(
            unknown,
            Outcome::Ignored("no option of this poll carries that title"),
            "a title no option carries marks nothing"
        );
        let stolen = create_poll_vote(
            state,
            view_of(&answer(
                "tea",
                &uri,
                json!({"attributedTo": "https://remote.example/users/carol"}),
            )),
            bob,
        )
        .await
        .expect("an ignore is not a failure");
        assert_eq!(
            stolen,
            Outcome::Ignored("the answer is attributed to somebody other than the activity actor"),
            "and a peer cannot vote as another account"
        );
        assert_eq!(
            poll::get_by_id(&pool, stand.id)
                .await
                .expect("the poll reads")
                .expect("the poll stands")
                .votes_count,
            0,
            "neither refusal counted anything"
        );
    }
}
