//! Inbound `Update` and `Delete` of the `Note` family: what a peer editing
//! or withdrawing one of its own posts does to this instance.
//!
//! The reference is Pleroma's `Update` and `Delete` arms of
//! `SideEffects.handle` (`lib/pleroma/web/activity_pub/side_effects.ex`)
//! with the `UpdateValidator`
//! (`lib/pleroma/web/activity_pub/object_validators/update_validator.ex`)
//! as the gate: an `Update` is only an edit when its author is the actor
//! who sent it and its `updated` time postdates what this instance already
//! holds, and a `Delete` retires the object the sender owns — of a post
//! never seen, Pleroma's `Object.create_tombstone` leaves the withdrawn
//! shell behind so nothing re-imports it. The ticket rules are authoritative
//! where the two differ:
//!
//! * [`update_note`], the `update_by_object_type` entry for `Note`,
//!   `Question`, `Article` and `Page`, gates the embedded document on
//!   `attributedTo` naming the activity's `actor` (containment, shared with
//!   [`super::create`]) and looks for a *live* stored status under the
//!   object's `uri` authored by that account. What it finds, it edits; what
//!   it does not — no row at all, or a tombstone — it hands to
//!   [`ingest::ingest_note`], so an `Update` for a post this instance never
//!   received lands the post (an edit of a deleted one is refused by the
//!   ingest's own tombstone check, and its answer rides out unchanged). The
//!   edit itself: a document no newer than the stored version — its
//!   `updated` at or before the row's `edited_at`, or its creation time
//!   while the row has never been edited — is ignored; everything else
//!   re-folds the status through [`crate::models::status_edit`], the same
//!   write the edit endpoint runs, which snapshots the superseded version
//!   into `status_edits` (visible at
//!   `GET /api/v1/statuses/{id}/history`) and stamps `edited_at`. Content,
//!   source text, warning, sensitive flag, hashtags and mentions — resolved
//!   through the create path's own resolver (`ingest::resolve_mentions`) —
//!   are its new state; the attachments are replaced wholesale in the
//!   create path's row shape, through its media writer
//!   (`ingest::insert_media`), since
//!   nobody uploaded them. A `Question`'s update additionally restates its
//!   poll: the option counts, voter count and deadline the remote reports
//!   overwrite the mirrored ones through [`poll::record_remote_totals`] (and
//!   a `Question` that arrived without a poll grows one through the create
//!   path's poll writer), and the edit route's `after_status_edited`
//!   hook — the boosters' telling, and the `Update` a local post would
//!   federate (a mirrored row is its origin's to announce, so that half of
//!   the hook stays silent here) — runs from here exactly as it runs from
//!   there.
//! * [`delete_note`], the `delete_by_object_type` entry for those four types
//!   plus `Tombstone` and the registry's `Delete` fallback, resolves the
//!   object's id — embedded document, bare string reference, either; a
//!   fallback whose id *is* the sender's own actor id goes straight back to
//!   [`super::actor`]'s handler, which owns account withdrawals. Otherwise
//!   the sender's live status with that `uri` goes through
//!   [`crate::services::delete_status`], the one deletion the status
//!   endpoint, the expiry job and this fold share: counters step down,
//!   notifications about the post clear, the tombstone stays. A `Delete`
//!   naming a status the sender does not own is ignored — one account does
//!   not withdraw another's post — and one for an object never stored leaves
//!   [`status::insert_tombstone`]'s shell in its place, so the
//!   [`Create`](super::create) of a deleted-before-arrival post is swallowed
//!   by the ingest's tombstone check.
//!
//! A `Delete` of a bare id whose type this module does not claim still
//! reaches the fallback and is folded here as a status withdrawal or a
//! tombstone — the same call Pleroma makes, resolving the id without
//! consulting its type. Reblog rows an `Undo` of an `Announce` withdraws are
//! the announce ticket's, not this module's; nothing here sends anything
//! anywhere.

use chrono::{DateTime, TimeDelta, Utc};
use futures::future::BoxFuture;
use serde_json::Value;
use sqlx::PgPool;
use std::fmt;
use tracing::debug;
use uuid::Uuid;

use crate::{
    api::statuses::after_status_edited,
    error::AppResult,
    federation::{
        dispatch::{ActivityView, FederationError, Handlers, Outcome},
        note_parser::{self, ParsedAttachment, ParsedNote, ParsedPoll},
    },
    models::{
        poll::{self, RemoteTotals},
        status::{self, Status},
        status_edit::{self, EditParams},
        user::User,
    },
    services::{delete_status, ingest},
    state::AppState,
    util::time::parse_time,
};

/// The object types whose `Update` this module folds: the four members of
/// the note family that are posts, and so editable ones. The family's
/// media shapes (`Image`, `Video`, `Audio`, `Document`) arrive as statuses
/// through [`super::create`] but carry no text an edit rewrites — an
/// `Update` of one routes nowhere, exactly as Pleroma's side effects leave
/// it.
const EDITED_OBJECT_TYPES: [&str; 4] = ["Note", "Question", "Article", "Page"];

/// The object types whose `Delete` this module folds: the editable four,
/// plus the `Tombstone` document Pleroma and Mastodon sometimes deliver
/// embedded in the `Delete` in place of a bare id.
const DELETED_OBJECT_TYPES: [&str; 5] = ["Note", "Question", "Article", "Page", "Tombstone"];

/// How far a delivered `updated` may lead this clock before the fold stops
/// trusting it — the same five minutes [`crate::federation::note_parser`]
/// grants a note, so a peer's forward clock cannot mark a stored version
/// stale with a timestamp from next week.
const CLOCK_SKEW: TimeDelta = TimeDelta::minutes(5);

/// Folds this module's routes into the dispatcher's registry.
///
/// The `update_by_object_type` entries name the four editable types, the
/// `delete_by_object_type` entries those four plus the embedded
/// `Tombstone`, and the `Delete` fallback resolves the ids no embedded type
/// claimed.
///
/// `crate::federation::handlers::registry` calls it last, which is what
/// lets the fallback wrap the actor handler's — see its module doc for the
/// order's meaning.
pub fn register(handlers: &mut Handlers) {
    for object_type in EDITED_OBJECT_TYPES {
        handlers.register_update(object_type, update_note);
    }
    for object_type in DELETED_OBJECT_TYPES {
        handlers.register_delete(object_type, delete_note);
    }
    handlers.register_delete_fallback(delete_note);
}

/// The `Update` handler of a note-family object: the author's own edit,
/// folded — or, for a post this instance never stored, its first landing.
///
/// # Errors
///
/// [`FederationError::Transient`] for a database failure the retry may
/// outlive; [`FederationError::Permanent`] (the ingest's own) for a document
/// no edit and no create can parse. Everything wrong with the *shape* — the
/// gate's refusals — arrives as [`Outcome::Ignored`].
pub fn update_note(
    state: AppState,
    view: ActivityView,
    signer: User,
) -> BoxFuture<'static, Result<Outcome, FederationError>> {
    Box::pin(async move { apply_update(state, view, signer).await })
}

/// The `Delete` handler of a note-family object, embedded `Tombstone`, and
/// of every `Delete` no other entry claimed: the sender's own post
/// withdrawn, or its tombstone reserved.
///
/// # Errors
///
/// [`FederationError::Transient`] for a database failure; the fold's two
/// shapes — [`delete_status::delete_status`]'s idempotent soft delete and
/// the `ON CONFLICT DO NOTHING` tombstone stub — both finish what a retry
/// restarts.
pub fn delete_note(
    state: AppState,
    view: ActivityView,
    signer: User,
) -> BoxFuture<'static, Result<Outcome, FederationError>> {
    Box::pin(async move { apply_delete(state, view, signer).await })
}

/// The body of [`update_note`]: the authorship gate, then the edit of the
/// stored post or the ingest of an unstored one.
async fn apply_update(
    state: AppState,
    view: ActivityView,
    signer: User,
) -> Result<Outcome, FederationError> {
    let Some(object) = view.object.embedded() else {
        debug!(
            object_id = view.object.id().unwrap_or_default(),
            "an Update whose object is only a reference; this instance does not fetch it yet"
        );
        return Ok(Outcome::Ignored("object not embedded"));
    };
    if !super::create::names_actor(object, &view.actor) {
        debug!(
            actor = %view.actor,
            "an Update whose object is attributed to someone else; discarding"
        );
        return Ok(Outcome::Ignored("attributedTo is not the activity actor"));
    }
    let Some(uri) = view.object.id() else {
        return Ok(Outcome::Ignored("an Update whose object names no id"));
    };
    let stored = status::get_by_uri(&state.db, uri)
        .await
        .map_err(|err| transient("the status uri read", &err))?;
    match stored.filter(|row| row.user_id == signer.id && row.deleted_at.is_none()) {
        Some(row) => fold_edit(&state, object, &signer, &row).await,
        None => ingest_the_rest(&state, &signer, object).await,
    }
}

/// What an `Update` of an object this instance does not hold live (or does
/// not hold at all) becomes: a `Create`'s fold of the same document. The
/// ingest owns the answers — the tombstone's refusal, a row another account
/// authored restated as `Existing`, an unsolicited or deactivated one
/// ignored — and they ride out of this handler unchanged.
async fn ingest_the_rest(
    state: &AppState,
    signer: &User,
    object: &Value,
) -> Result<Outcome, FederationError> {
    match ingest::ingest_note(state, signer, object).await? {
        ingest::IngestOutcome::Created(row) => {
            debug!(
                status = %row.id,
                "an Update brought a status this instance never stored"
            );
            Ok(Outcome::Handled)
        }
        ingest::IngestOutcome::Existing(row) => {
            debug!(
                status = %row.id,
                "an Update restated a status this instance does not edit from here"
            );
            Ok(Outcome::Handled)
        }
        ingest::IngestOutcome::Ignored(reason) => Ok(Outcome::Ignored(reason)),
    }
}

/// The body of [`delete_note`]: the sender's own id back to the actor
/// handler, everything else folded as a status withdrawal.
async fn apply_delete(
    state: AppState,
    view: ActivityView,
    signer: User,
) -> Result<Outcome, FederationError> {
    let Some(object_id) = view.object.id() else {
        return Ok(Outcome::Ignored("a Delete whose object names no id"));
    };
    if object_id == view.actor {
        return super::actor::delete_actor(state, view, signer).await;
    }
    let stored = status::get_by_uri(&state.db, object_id)
        .await
        .map_err(|err| transient("the status uri read", &err))?;
    withdraw(&state, &signer, object_id, stored).await
}

/// The fold of [`apply_delete`]'s read: the author's live post deleted
/// through the shared service, an already withdrawn one restated, somebody
/// else's post nobody may withdraw, and an unseen object reserved as a
/// tombstone so its late `Create` is swallowed.
async fn withdraw(
    state: &AppState,
    signer: &User,
    uri: &str,
    stored: Option<Status>,
) -> Result<Outcome, FederationError> {
    match stored {
        Some(row) if row.user_id != signer.id => {
            debug!(
                status = %row.id,
                account = %signer.ap_id,
                "a Delete by an account other than the post's author; ignoring"
            );
            Ok(Outcome::Ignored(
                "a Delete by someone other than the author",
            ))
        }
        Some(row) if row.deleted_at.is_some() => Ok(Outcome::Ignored("an already deleted status")),
        Some(row) => {
            let deleted = delete_status::delete_status(state, &row)
                .await
                .map_err(|err| transient("the status delete", &err))?;
            if deleted {
                debug!(status = %row.id, "a Delete folded a remote status withdrawal");
                Ok(Outcome::Handled)
            } else {
                Ok(Outcome::Ignored("an already deleted status"))
            }
        }
        None => {
            let reserved = status::insert_tombstone(&state.db, uri, signer.id)
                .await
                .map_err(|err| transient("the tombstone reservation", &err))?;
            match reserved {
                Some(row) => {
                    debug!(
                        status = %row.id,
                        uri, "a Delete of an unstored status reserved its tombstone"
                    );
                    Ok(Outcome::Handled)
                }
                None => Ok(Outcome::Ignored("the object's tombstone was already there")),
            }
        }
    }
}

/// The edit fold of [`apply_update`], in the order the ticket fixes: the
/// stale gate, the parse, the versioned write, the attachments, the poll,
/// and the edit route's own hook — each step the same code the local edit
/// runs, on state the document restates.
async fn fold_edit(
    state: &AppState,
    object: &Value,
    signer: &User,
    stored: &Status,
) -> Result<Outcome, FederationError> {
    let now = Utc::now();
    if is_stale(object, stored, now) {
        debug!(
            status = %stored.id,
            "an Update no newer than the stored version; ignoring"
        );
        return Ok(Outcome::Ignored(
            "an Update no newer than the stored version",
        ));
    }
    let parsed = note_parser::parse_note(object, signer, now)
        .map_err(|err| permanent("the updated object is not parseable", &err))?;
    let mentioned = ingest::resolve_mentions(state, &parsed.mention_hrefs).await?;
    let media = stored_media(&state.db, stored.id)
        .await
        .map_err(|err| transient("the attachment read", &err))?;
    let media_changed = media_replacement_needed(&media, &parsed.attachments);
    let params = edit_params(&parsed, stored, &mentioned, media_changed);
    let Some(updated) = status_edit::update_status(&state.db, stored.id, params)
        .await
        .map_err(|err| transient("the status edit", &err))?
    else {
        return Ok(Outcome::Ignored("the status was deleted under its edit"));
    };
    if media_changed {
        let old_ids: Vec<Uuid> = media.into_iter().map(|(id, _url)| id).collect();
        replace_media(state, stored.id, &old_ids, &parsed.attachments).await?;
    }
    refresh_poll(state, stored.id, parsed.poll.as_ref()).await?;
    after_status_edited(state, stored).await;
    debug!(
        status = %updated.id,
        "an Update folded a remote status edit"
    );
    Ok(Outcome::Handled)
}

/// The new state [`fold_edit`] writes: the document's restated fields —
/// content, source text, warning, sensitive flag, mentions and hashtags —
/// beside the two the ticket leaves the row's own: the language tag an
/// edit's document is not asked to restate, and everything the update's
/// parse cannot see. `media_changed` is the [module doc](self)'s attachment
/// replacement: an empty submitted set, which [`status_edit::update_status`]
/// reads as "detach everything the post wears now" — the rows the remote
/// named are nobody's uploads, so the fold rewrites them itself beside the
/// versioned write.
fn edit_params(
    parsed: &ParsedNote,
    stored: &Status,
    mentioned: &[User],
    media_changed: bool,
) -> EditParams {
    EditParams {
        content: parsed.status.content.clone(),
        raw_text: parsed.status.raw_text.clone(),
        content_type: parsed.status.content_type.clone(),
        spoiler_text: parsed.status.spoiler_text.clone(),
        sensitive: parsed.status.sensitive,
        language: stored.language.clone(),
        mentioned_user_ids: mentioned.iter().map(|account| account.id).collect(),
        hashtags: parsed.hashtags.clone(),
        media_ids: media_changed.then(Vec::new),
    }
}

/// The stale gate of [`fold_edit`] (see [`update_time`]): an update whose
/// own moment does not postdate the version this instance holds describes
/// an edit the fold already made — or an older peer's restatement of the
/// original — and writes nothing.
fn is_stale(object: &Value, stored: &Status, now: DateTime<Utc>) -> bool {
    let Some(incoming) = update_time(object, now) else {
        return false;
    };
    incoming <= stored.edited_at.unwrap_or(stored.inserted_at)
}

/// The moment an `Update` claims its document was last changed: its
/// `updated`, falling back to its `published` for a document that carries
/// only that, clamped against [`CLOCK_SKEW`] the way the note parse clamps
/// creation times — a stamp from a runaway clock answers `now`, still the
/// freshest the fold can honestly call it. `None` for a document whose
/// stamps are unreadable: with nothing to compare, the fold trusts the
/// sender and edits.
fn update_time(object: &Value, now: DateTime<Utc>) -> Option<DateTime<Utc>> {
    let stamp = object
        .get("updated")
        .and_then(Value::as_str)
        .and_then(parse_time)
        .or_else(|| {
            object
                .get("published")
                .and_then(Value::as_str)
                .and_then(parse_time)
        })?;
    let ceiling = now.checked_add_signed(CLOCK_SKEW).unwrap_or(now);
    Some(if stamp > ceiling { now } else { stamp })
}

/// The post's attachments as stored, in render order: each row's id (what
/// the replacement deletes after the versioned write detaches it) beside
/// the url (what [`media_replacement_needed`] compares).
async fn stored_media(pool: &PgPool, status_id: Uuid) -> AppResult<Vec<(Uuid, String)>> {
    let rows = sqlx::query_as::<_, (Uuid, String)>(
        "SELECT id, url FROM media_attachments
         WHERE status_id = $1 ORDER BY position, id",
    )
    .bind(status_id)
    .fetch_all(pool)
    .await?;
    Ok(rows)
}

/// Whether the document's attachments differ from the post's stored ones:
/// the urls compared in order, so an unchanged restatement replaces nothing
/// — and, because [`status_edit::update_status`] only ever snapshots a
/// version it sees changing, records no revision for it either.
fn media_replacement_needed(stored: &[(Uuid, String)], fresh: &[ParsedAttachment]) -> bool {
    stored.len() != fresh.len()
        || stored
            .iter()
            .zip(fresh)
            .any(|((_id, url), media)| url != &media.url)
}

/// The attachment rewrite beside the versioned write: the detached rows of
/// a remote post are not uploads anybody may re-attach, so they go, and the
/// document's fresh set lands through the create path's own
/// [`ingest::insert_media`] — same row shape, same remote-url semantics,
/// whichever activity brought the media.
async fn replace_media(
    state: &AppState,
    status_id: Uuid,
    old_ids: &[Uuid],
    attachments: &[ParsedAttachment],
) -> Result<(), FederationError> {
    let mut tx = state
        .db
        .begin()
        .await
        .map_err(|err| transient("the attachment rewrite", &err))?;
    sqlx::query("DELETE FROM media_attachments WHERE id = ANY($1)")
        .bind(old_ids)
        .execute(&mut *tx)
        .await
        .map(drop)
        .map_err(|err| transient("the detached attachment delete", &err))?;
    ingest::insert_media(&mut tx, status_id, attachments).await?;
    tx.commit()
        .await
        .map_err(|err| transient("the attachment rewrite commit", &err))?;
    Ok(())
}

/// The poll arm of [`fold_edit`]: a `Question` whose update restates its
/// poll has the remote's numbers written over the mirrored ones — options,
/// voter count, total and deadline — and a `Question` that arrived without
/// a poll grows one exactly the create path would have grown it. A document
/// carrying no poll moves nothing; the stored poll's own votes, cast here,
/// are nobody's to overwrite from there.
async fn refresh_poll(
    state: &AppState,
    status_id: Uuid,
    question: Option<&ParsedPoll>,
) -> Result<(), FederationError> {
    let Some(poll_data) = question else {
        return Ok(());
    };
    let stored = poll::get_for_status(&state.db, status_id)
        .await
        .map_err(|err| transient("the stored poll read", &err))?;
    if let Some(poll) = stored {
        let totals = remote_totals(poll_data);
        return poll::record_remote_totals(&state.db, poll.id, &totals)
            .await
            .map_err(|err| transient("the remote poll totals", &err));
    }
    let mut tx = state
        .db
        .begin()
        .await
        .map_err(|err| transient("the poll insert", &err))?;
    ingest::insert_poll(&mut tx, status_id, Some(poll_data)).await?;
    tx.commit()
        .await
        .map_err(|err| transient("the poll insert commit", &err))?;
    Ok(())
}

/// The remote's report as [`RemoteTotals`]: each option its stated count,
/// the poll its `votersCount` and the sum of the options, the deadline as
/// the document names it (`None` keeps the stored one — see
/// [`poll::record_remote_totals`]).
fn remote_totals(poll_data: &ParsedPoll) -> RemoteTotals {
    let option_counts: Vec<(String, i32)> = poll_data
        .options
        .iter()
        .map(|option| (option.title.clone(), whole(option.votes_count)))
        .collect();
    let votes_count = option_counts.iter().fold(0_i64, |sum, (_title, votes)| {
        sum.saturating_add(i64::from(*votes))
    });
    RemoteTotals {
        options: option_counts,
        voters_count: poll_data.voters_count.map_or(0, whole),
        votes_count: whole(votes_count),
        expires_at: poll_data.expires_at,
    }
}

/// A remote count narrowed to the `integer` column's width — the ingest's
/// same clamping rule: beyond it the column's maximum, never a refused
/// update over a number nobody can vote into being true.
fn whole(count: i64) -> i32 {
    i32::try_from(count).unwrap_or(i32::MAX)
}

/// Shapes a database failure into the retryable arm of the dispatcher's
/// error vocabulary, naming the read or write that failed.
fn transient<E: fmt::Display>(what: &str, err: &E) -> FederationError {
    FederationError::Transient(format!("{what} failed: {err}"))
}

/// Shapes a refusal no retry can outlive into the dispatcher's discardable
/// arm.
fn permanent<E: fmt::Display>(what: &str, err: &E) -> FederationError {
    FederationError::Permanent(format!("{what}: {err}"))
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;

    use serde_json::{Value, json};
    use sqlx::PgPool;

    use super::*;
    use crate::{
        config::TEST_BASE_URL,
        models::{
            poll,
            status::NewStatus,
            user::{self, Flag, NewUser},
        },
    };

    /// The actor every fixture here is delivered by, and the author of
    /// every fixture post.
    const BOB: &str = "https://remote.example/users/bob";

    /// Test state: `pool` and the fixed test config, collaborators inert.
    fn app_state(pool: &PgPool) -> AppState {
        let config = Arc::new(crate::config::Config::for_tests());
        AppState {
            db: pool.clone(),
            hub: crate::streaming::hub::Hub::new(pool),
            config: Arc::clone(&config),
            http_client: crate::http_client::HttpClient::from(config.as_ref()),
            uploader: crate::uploads::UploaderBackend::from(config.as_ref()),
            instance_stats_cache: Arc::default(),
            mrf_config_cache: Arc::default(),
            translation_languages_cache: Arc::default(),
            rate_limiter: Arc::new(crate::rate_limit::RateLimiter::from_config(config.as_ref())),
            mailer: Arc::new(crate::mailer::MemoryTransport::new()),
        }
    }

    /// The stored mirror of the fixture author.
    async fn author(pool: &PgPool) -> User {
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
        .expect("the author fixture inserts")
    }

    /// The author account re-read from storage: the handlers take their
    /// signer by value, so every delivery in a test needs a copy of its own.
    async fn author_again(pool: &PgPool) -> User {
        user::get_by_ap_id(pool, BOB)
            .await
            .expect("the author read")
            .expect("the author is stored")
    }

    /// A second remote account on the same host, so a `Delete` aimed at
    /// somebody else's post survives containment's host rule.
    async fn other(pool: &PgPool) -> User {
        user::insert(
            pool,
            &NewUser {
                nickname: "erin@remote.example".to_owned(),
                ap_id: "https://remote.example/users/erin".to_owned(),
                local: Flag::new(false),
                domain: Some("remote.example".to_owned()),
                ..NewUser::default()
            },
        )
        .await
        .expect("the other account fixture inserts")
    }

    /// The stored mirror of a note-family `Update`/`Delete` of `object`,
    /// delivered by [`BOB`] — or by `actor` where a test names one.
    fn view_of(kind: &str, object: Value, actor: Option<&str>) -> ActivityView {
        let mut envelope = json!({
            "id": format!("https://remote.example/activities/{kind}-{}", Uuid::now_v7()),
            "type": kind,
            "actor": actor.unwrap_or(BOB),
        });
        envelope["object"] = object;
        ActivityView::parse(&envelope).expect("the fixture is a dispatchable activity")
    }

    /// A `Note`-family document under `uri`, attributed to [`BOB`], with
    /// every extra field `extra` merges over it.
    fn note(uri: &str, extra: Value) -> Value {
        let mut object = json!({
            "id": uri,
            "type": "Note",
            "attributedTo": BOB,
            "content": "<p>first words</p>",
            "published": "2024-05-01T10:00:00.000Z",
        });
        json_extend(&mut object, extra);
        object
    }

    /// Merges `extra`'s fields over `target`, dropping what they displace.
    fn json_extend(target: &mut Value, extra: Value) {
        let (Some(map), Value::Object(additions)) = (target.as_object_mut(), extra) else {
            panic!("both documents are JSON objects");
        };
        for (key, value) in additions {
            let previous = map.insert(key, value);
            drop(previous);
        }
    }

    /// The instant the fixture notes are published, and the anchor every
    /// mirrored row is stamped with — a fixed past moment so a fixture
    /// `updated` in 2024 postdates it without depending on the wall clock.
    fn published() -> DateTime<Utc> {
        parse_time("2024-05-01T10:00:00.000Z").expect("the fixture time parses")
    }

    /// The mirrored note `uri` as this instance stores it after a `Create`:
    /// `author`'s remote row of `text`, authored at [`published`].
    async fn mirrored(pool: &PgPool, author: &User, uri: &str, text: &str) -> Status {
        status::insert(
            pool,
            TEST_BASE_URL,
            &NewStatus {
                uri: Some(uri.to_owned()),
                content: format!("<p>{text}</p>"),
                raw_text: text.to_owned(),
                user_id: author.id,
                inserted_at: Some(published()),
                ..NewStatus::default()
            },
        )
        .await
        .expect("the mirrored note inserts")
    }

    /// How many live statuses `uri` names, with their content.
    async fn live_content(pool: &PgPool, uri: &str) -> Option<String> {
        let row = status::get_by_uri(pool, uri).await.expect("the uri read");
        row.filter(|status| status.deleted_at.is_none())
            .map(|status| status.content)
    }

    /// How many `statuses` rows carry object uri `uri`, tombstone included.
    async fn rows_for_uri(pool: &PgPool, uri: &str) -> i64 {
        sqlx::query_scalar::<_, i64>("SELECT count(*) FROM statuses WHERE uri = $1")
            .bind(uri)
            .fetch_one(pool)
            .await
            .expect("the row count reads")
    }

    /// How many rows of `table` hang off the status `id` — an expectation
    /// about a link set the edit rewrote.
    async fn links_for(pool: &PgPool, sql: &'static str, id: Uuid) -> i64 {
        sqlx::query_scalar::<_, i64>(sql)
            .bind(id)
            .fetch_one(pool)
            .await
            .unwrap_or_else(|err| panic!("the link count should read: {err}"))
    }

    /// The superseded versions of `status_id`, oldest first.
    async fn revisions(pool: &PgPool, status_id: Uuid) -> Vec<status_edit::StatusEdit> {
        status_edit::list_for_status(pool, status_id)
            .await
            .expect("the revisions read")
    }

    #[test]
    fn register_claims_the_note_family_and_the_delete_fallback() {
        let mut handlers = Handlers::new();
        register(&mut handlers);
        for object_type in EDITED_OBJECT_TYPES {
            assert!(
                handlers.handles_update(object_type),
                "an Update of a {object_type} folds here"
            );
            assert!(
                handlers.handles_delete(object_type),
                "a Delete of a {object_type} withdraws here"
            );
        }
        assert!(
            handlers.handles_delete("Tombstone"),
            "and a Delete carrying its embedded Tombstone"
        );
        assert!(
            !handlers.handles_update("Person"),
            "an actor stays the actor module's Update key"
        );
    }

    #[tokio::test]
    async fn an_edit_rewrites_the_post_and_records_its_previous_version() {
        let db = crate::test_db::test_db().await.expect("test database slot");
        let pool = db.pool().clone();
        let state = app_state(&pool);
        let author = author(&pool).await;
        let mentioned = other(&pool).await;
        let uri = "https://remote.example/users/bob/statuses/edit-1";
        let row = mirrored(&pool, &author, uri, "first words").await;

        let outcome = update_note(
            state,
            view_of(
                "Update",
                note(
                    uri,
                    json!({
                        "content": "<p>second words</p>",
                        "updated": "2024-05-01T12:00:00.000Z",
                        "tag": [
                            { "type": "Hashtag", "href": "https://remote.example/tags/rust",
                              "name": "#Rust" },
                            { "type": "Mention", "href": mentioned.ap_id, "name": "@erin" },
                        ],
                        "attachment": [{
                            "type": "Document",
                            "mediaType": "image/png",
                            "url": "https://remote.example/media/edit-a.png",
                        }],
                    }),
                ),
                None,
            ),
            author_again(&pool).await,
        )
        .await
        .expect("the fold completes");

        assert_eq!(outcome, Outcome::Handled, "the edit is folded");
        let edited = status::get_by_id(&pool, row.id)
            .await
            .expect("the row read")
            .expect("the post is still live");
        assert_eq!(edited.content, "<p>second words</p>", "the text is new");
        assert!(
            edited.edited_at.is_some(),
            "and the edit is stamped on the row"
        );

        let history = revisions(&pool, row.id).await;
        assert_eq!(history.len(), 1, "the superseded version is one row");
        assert_eq!(history[0].content, "<p>first words</p>");

        let tags: Vec<String> = sqlx::query_scalar(
            "SELECT h.name FROM status_hashtags sh JOIN hashtags h ON h.id = sh.hashtag_id \
              WHERE sh.status_id = $1",
        )
        .bind(row.id)
        .fetch_all(&pool)
        .await
        .expect("the tag links read");
        assert_eq!(tags, ["rust"], "the new hashtag is linked");
        assert_eq!(
            links_for(
                &pool,
                "SELECT count(*) FROM status_mentions WHERE status_id = $1",
                row.id
            )
            .await,
            1,
            "and the resolved mention"
        );
        assert_eq!(
            links_for(
                &pool,
                "SELECT count(*) FROM media_attachments WHERE status_id = $1",
                row.id
            )
            .await,
            1,
            "and the attachment the update named"
        );
    }

    #[tokio::test]
    async fn an_update_no_newer_than_the_stored_version_is_ignored() {
        let db = crate::test_db::test_db().await.expect("test database slot");
        let pool = db.pool().clone();
        let state = app_state(&pool);
        let author = author(&pool).await;
        let uri = "https://remote.example/users/bob/statuses/stale-1";
        drop(mirrored(&pool, &author, uri, "first words").await);

        let fresh = note(
            uri,
            json!({
                "content": "<p>second words</p>",
                "updated": "2024-05-01T12:00:00.000Z",
            }),
        );
        let first = update_note(
            state.clone(),
            view_of("Update", fresh, None),
            author_again(&pool).await,
        )
        .await
        .expect("the first edit folds");
        assert_eq!(first, Outcome::Handled, "the fresh edit is applied");

        let outdated = note(
            uri,
            json!({
                "content": "<p>third words</p>",
                "updated": "2024-05-01T11:00:00.000Z",
            }),
        );
        let outcome = update_note(state, view_of("Update", outdated, None), author)
            .await
            .expect("the second fold completes");

        assert_eq!(
            outcome,
            Outcome::Ignored("an Update no newer than the stored version"),
            "an older update describes an older edit"
        );
        assert_eq!(
            live_content(&pool, uri).await.as_deref(),
            Some("<p>second words</p>"),
            "and rewrote nothing"
        );
    }

    #[tokio::test]
    async fn an_update_for_an_unknown_status_ingests_it() {
        let db = crate::test_db::test_db().await.expect("test database slot");
        let pool = db.pool().clone();
        let state = app_state(&pool);
        let author = author(&pool).await;
        let uri = "https://remote.example/users/bob/statuses/never-created";

        let outcome = update_note(
            state.clone(),
            view_of("Update", note(uri, json!({})), None),
            author_again(&pool).await,
        )
        .await
        .expect("the fold completes");

        assert_eq!(outcome, Outcome::Handled, "the post lands");
        assert_eq!(
            live_content(&pool, uri).await.as_deref(),
            Some("<p>first words</p>"),
            "as the document describes it"
        );

        let again = update_note(state, view_of("Update", note(uri, json!({})), None), author)
            .await
            .expect("the second fold completes");
        assert_eq!(
            again,
            Outcome::Ignored("an Update no newer than the stored version"),
            "a restatement of the same version writes nothing further"
        );
        assert_eq!(
            rows_for_uri(&pool, uri).await,
            1,
            "and the post is stored exactly once"
        );
    }

    #[tokio::test]
    async fn a_question_update_moves_the_poll_totals() {
        let db = crate::test_db::test_db().await.expect("test database slot");
        let pool = db.pool().clone();
        let state = app_state(&pool);
        let author = author(&pool).await;
        let uri = "https://remote.example/users/bob/statuses/poll-1";
        let row = mirrored(&pool, &author, uri, "tea or coffee?").await;
        let mut tx = pool.begin().await.expect("the poll fixture opens");
        ingest::insert_poll(
            &mut tx,
            row.id,
            Some(&json_parsed_poll(
                20,
                DateTime::parse_from_rfc3339("2024-06-01T00:00:00Z")
                    .expect("the deadline is an instant")
                    .with_timezone(&Utc),
            )),
        )
        .await
        .expect("the poll fixture lands");
        tx.commit().await.expect("the poll fixture commits");

        let outcome = update_note(
            state,
            view_of("Update", question_update(uri), None),
            author_again(&pool).await,
        )
        .await
        .expect("the fold completes");

        assert_eq!(outcome, Outcome::Handled, "the totals are folded");
        let stand = poll::get_for_status(&pool, row.id)
            .await
            .expect("the poll read")
            .expect("the post still carries its poll");
        assert_eq!(stand.voters_count, 42, "the remote's voter count wins");
        assert_eq!(stand.votes_count, 45, "and its vote total");
        let options = poll::options(&pool, stand.id)
            .await
            .expect("the options read");
        assert_eq!(
            options.iter().map(|o| o.votes_count).collect::<Vec<_>>(),
            vec![30, 15],
            "option by option, matched by title"
        );
        assert_eq!(
            stand.expires_at,
            Some(end_time()),
            "and the deadline the document named overwrote the stored one"
        );
    }

    /// The further deadline `question_update` names, as an instant.
    fn end_time() -> DateTime<Utc> {
        DateTime::parse_from_rfc3339("2024-06-02T00:00:00Z")
            .expect("the deadline is an instant")
            .with_timezone(&Utc)
    }

    /// The `Question` update: the same poll titles as the fixture, with
    /// the remote's new totals and a further deadline.
    fn question_update(uri: &str) -> Value {
        json!({
            "id": uri,
            "type": "Question",
            "attributedTo": BOB,
            "content": "<p>tea or coffee?</p>",
            "published": "2024-05-01T10:00:00.000Z",
            "updated": "2024-05-02T10:00:00.000Z",
            "endTime": "2024-06-02T00:00:00Z",
            "votersCount": 42,
            "oneOf": [
                {
                    "type": "Note",
                    "name": "Tea",
                    "replies": { "type": "Collection", "totalItems": 30 },
                },
                {
                    "type": "Note",
                    "name": "Coffee",
                    "replies": { "type": "Collection", "totalItems": 15 },
                },
            ],
        })
    }

    /// The ingest-side poll fixture: the options `Tea` and `Coffee` with
    /// `voters` counted, closing at `until`.
    fn json_parsed_poll(voters: i64, until: DateTime<Utc>) -> ParsedPoll {
        ParsedPoll {
            options: vec![
                note_parser::ParsedPollOption {
                    title: "Tea".to_owned(),
                    votes_count: 10,
                },
                note_parser::ParsedPollOption {
                    title: "Coffee".to_owned(),
                    votes_count: 8,
                },
            ],
            multiple: false,
            expires_at: Some(until),
            voters_count: Some(voters),
        }
    }

    #[tokio::test]
    async fn a_question_update_grows_the_poll_it_first_carries() {
        let db = crate::test_db::test_db().await.expect("test database slot");
        let pool = db.pool().clone();
        let state = app_state(&pool);
        let author = author(&pool).await;
        let uri = "https://remote.example/users/bob/statuses/poll-late";
        let row = mirrored(&pool, &author, uri, "a plain question").await;

        let grow = update_note(state, view_of("Update", question_update(uri), None), author)
            .await
            .expect("the fold completes");
        assert_eq!(grow, Outcome::Handled, "the poll-bearing update folds");

        let stand = poll::get_for_status(&pool, row.id)
            .await
            .expect("the poll read")
            .expect("the update's poll landed");
        assert_eq!(stand.voters_count, 42, "as a mirrored create's would");
        assert_eq!(poll::options(&pool, stand.id).await.unwrap().len(), 2);
    }

    #[tokio::test]
    async fn the_delete_soft_deletes_the_authors_post() {
        let db = crate::test_db::test_db().await.expect("test database slot");
        let pool = db.pool().clone();
        let state = app_state(&pool);
        let author = author(&pool).await;
        let uri = "https://remote.example/users/bob/statuses/gone-1";
        drop(mirrored(&pool, &author, uri, "doomed").await);

        let outcome = delete_note(
            state.clone(),
            view_of("Delete", json!(uri), None),
            author_again(&pool).await,
        )
        .await
        .expect("the fold completes");

        assert_eq!(outcome, Outcome::Handled, "the withdrawal is folded");
        assert!(
            live_content(&pool, uri).await.is_none(),
            "the post is no longer live"
        );
        let tombstone = status::get_by_uri(&pool, uri)
            .await
            .expect("the uri read")
            .expect("the row stays");
        assert!(tombstone.deleted_at.is_some(), "as a tombstone");

        let again = delete_note(state, view_of("Delete", json!(uri), None), author)
            .await
            .expect("the second fold completes");
        assert_eq!(
            again,
            Outcome::Ignored("an already deleted status"),
            "and the second sight moves nothing"
        );
    }

    #[tokio::test]
    async fn a_delete_of_an_unknown_object_leaves_a_tombstone() {
        let db = crate::test_db::test_db().await.expect("test database slot");
        let pool = db.pool().clone();
        let state = app_state(&pool);
        let author = author(&pool).await;
        let uri = "https://remote.example/users/bob/statuses/deleted-before-arrival";

        let outcome = delete_note(
            state.clone(),
            view_of("Delete", json!(uri), None),
            author_again(&pool).await,
        )
        .await
        .expect("the fold completes");

        assert_eq!(outcome, Outcome::Handled, "the tombstone is reserved");
        assert_eq!(
            rows_for_uri(&pool, uri).await,
            1,
            "exactly one row carries the uri"
        );

        // A `Create` of the same object now meets the reserved tombstone in
        // the ingest's dedupe read and is swallowed; the update path routes
        // an unknown-to-edit object through that same ingest, so it answers
        // identically here.
        let swallowed = update_note(state, view_of("Create", note(uri, json!({})), None), author)
            .await
            .expect("the late activity completes");
        assert_eq!(
            swallowed,
            Outcome::Ignored("tombstoned"),
            "the ingest's tombstone check ignores the post that arrives after its delete"
        );
        assert_eq!(
            rows_for_uri(&pool, uri).await,
            1,
            "and no live status re-appears"
        );
    }

    #[tokio::test]
    async fn a_delete_by_somebody_else_is_ignored() {
        let db = crate::test_db::test_db().await.expect("test database slot");
        let pool = db.pool().clone();
        let state = app_state(&pool);
        let author = author(&pool).await;
        let erin = other(&pool).await;
        let uri = "https://remote.example/users/bob/statuses/not-yours";
        drop(mirrored(&pool, &author, uri, "still here").await);

        let outcome = delete_note(
            state,
            view_of("Delete", json!(uri), Some(erin.ap_id.as_str())),
            erin,
        )
        .await
        .expect("the fold completes");

        assert_eq!(
            outcome,
            Outcome::Ignored("a Delete by someone other than the author"),
            "one account does not withdraw another's post"
        );
        assert_eq!(
            live_content(&pool, uri).await.as_deref(),
            Some("<p>still here</p>"),
            "and the post stands"
        );
    }

    #[tokio::test]
    async fn the_fallback_hands_the_actors_own_id_to_the_actor_handler() {
        let db = crate::test_db::test_db().await.expect("test database slot");
        let pool = db.pool().clone();
        let state = app_state(&pool);
        let signer = author(&pool).await;
        let signer_id = signer.id;

        let outcome = delete_note(state, view_of("Delete", json!(BOB), None), signer)
            .await
            .expect("the fold completes");

        assert_eq!(outcome, Outcome::Handled, "the self-delete was folded");
        let retired = user::get_by_id(&pool, signer_id)
            .await
            .expect("the account read")
            .expect("the account is still stored");
        assert!(
            !retired.is_active.get(),
            "and the actor handler retired it, not this fold"
        );
        assert_eq!(
            rows_for_uri(&pool, BOB).await,
            0,
            "and no tombstone status shadows the actor id"
        );
    }

    #[tokio::test]
    async fn an_update_attributed_to_somebody_else_never_edits() {
        let db = crate::test_db::test_db().await.expect("test database slot");
        let pool = db.pool().clone();
        let state = app_state(&pool);
        let author = author(&pool).await;
        let uri = "https://remote.example/users/bob/statuses/not-my-edit";
        drop(mirrored(&pool, &author, uri, "first words").await);

        let stolen = json!({
            "id": uri,
            "type": "Note",
            "attributedTo": "https://remote.example/users/erin",
            "content": "<p>overwritten</p>",
            "updated": "2024-05-03T10:00:00.000Z",
        });
        let outcome = update_note(state, view_of("Update", stolen, None), author)
            .await
            .expect("the fold completes");

        assert_eq!(
            outcome,
            Outcome::Ignored("attributedTo is not the activity actor"),
            "the sender may only edit its own objects"
        );
        assert_eq!(
            live_content(&pool, uri).await.as_deref(),
            Some("<p>first words</p>"),
            "and rewrote nothing"
        );
    }
}
