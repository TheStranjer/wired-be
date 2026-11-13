//! Remote-status ingestion: folding one received `Note`-family object into
//! a stored status.
//!
//! [`ingest_note`] is the database half of a federated `Create`: given the
//! already-parsed author's object document and the [`User`] mirror it was
//! delivered by, it decides whether this instance gains a post, and writes
//! everything that post owns — the `statuses` row with its reply link and
//! context, its `status_mentions` and hashtag links, its
//! `media_attachments`, its `polls`/`poll_options` rows and the
//! `custom_emojis` its `Emoji` tags announce — in one transaction, so a
//! half-folded remote post never stands visible. The reference is
//! Pleroma's `Pleroma.Web.ActivityPub.Transmogrifier.handle_incoming`
//! `Create` clause feeding `SideEffects.handle_incoming`
//! (`lib/pleroma/web/activity_pub/transmogrifier.ex`,
//! `lib/pleroma/web/activity_pub/side_effects.ex`); the ticket rules here
//! are authoritative where the two differ.
//!
//! The [`crate::federation::note_parser`] did the document half already:
//! [`parse_note`](crate::federation::note_parser::parse_note) answers a
//! [`ParsedNote`] of insert-ready parts. This
//! module resolves what only a live database (and, for unknown mention
//! actors, the network) can settle, in the order the ticket fixes:
//!
//! 1. parse; a malformed or refusing document is a
//!    [`FederationError::Permanent`] the dispatcher discards the job for;
//! 2. the `uri` dedupe: a status with the same object id is nobody's news
//!    twice — a live one answers [`IngestOutcome::Existing`], a tombstone
//!    [`IngestOutcome::Ignored`] with `"tombstoned"` (a deleted remote post
//!    is never re-imported, the reason its `uri` stays reserved);
//! 3. a deactivated author [`IngestOutcome::Ignored`]s the post: the
//!    mirror was retired, its new content is not admitted;
//! 4. the reply link: a stored parent (by `uri`) supplies
//!    `in_reply_to_id`, `in_reply_to_account_id` and its `conversation_id`;
//!    an unstored one keeps the `in_reply_to_ap_id` reference and, when a
//!    stored status shares the object's `context`, that status's
//!    conversation — a thread joins itself together before its root ever
//!    arrives; with neither, [`status::insert`] makes the new row its own
//!    conversation. The quote `uri` resolves locally only: a stored, live
//!    original becomes `quote_of_id`, anything else leaves the quote
//!    unfilled (this ticket does not fetch objects);
//! 5. mentions: each `Mention` tag `href` maps to a stored user by
//!    `ap_id`; an unknown one is fetched through
//!    [`get_or_fetch_by_ap_id`] under `MENTION_LOOKUP_TIMEOUT`, at most
//!    `MAX_MENTION_LOOKUPS` of them per status, and whatever stays
//!    unresolved — a dead peer, an elapsed timeout, a mention past the
//!    cap — is dropped rather than refused;
//! 6. remote emojis upsert `custom_emojis` under the author's domain;
//! 7. attachments become `media_attachments` rows with no `user_id` (nobody
//!    uploaded them) and `remote_url` beside `url` (the media proxy decides
//!    what to mirror), ordered as the document lists them, written on the
//!    status's own transaction; a `Question`'s poll likewise lands its
//!    options and the counts the remote reported;
//! 8. the status itself is inserted with `local = false` and every counter
//!    at zero — Mastodon-family reblog and favourite counts of remote
//!    statuses are learned only from the `Announce`es and `Like`s that
//!    arrive later, never from what the object's own `likes`/`shares`
//!    happened to say when it was fetched;
//! 9. after the commit, the create path's `after_status_created` hook
//!    runs over the row exactly as it does for a local post — mention and
//!    subscriber notifications, the link-card job, DM-thread participation
//!    — every one of which decides on the *recipient's* locality and none
//!    on the author's;
//! 10. [`Config::accept_unsolicited_public`](crate::config::Config::accept_unsolicited_public)
//!     off drops `public` and
//!     `unlisted` statuses from authors no local account follows, unless a
//!     local user is mentioned or the status answers a local one — the
//!     explicit anti-spam gate, evaluated after mentions and the reply
//!     link are known and before anything is written.
//!
//! Database failures are [`FederationError::Transient`]: the dispatcher
//! releases the dedupe row and the retry folds the activity again — safe
//! because the whole write is one transaction and a status committed
//! before a retry's death answers step 2's `Existing` instead of doubling.

use std::{collections::HashSet, fmt, time::Duration};

use chrono::Utc;
use futures::future::join_all;
use serde_json::Value;
use sqlx::PgConnection;
use tracing::debug;
use uuid::Uuid;

use crate::{
    federation::{
        dispatch::FederationError,
        fetch_actor::get_or_fetch_by_ap_id,
        note_parser::{self, ParsedAttachment, ParsedNote, ParsedPoll, RemoteEmoji},
    },
    models::{
        custom_emoji::{self, NewCustomEmoji},
        follow,
        media_attachment::{self, NewMediaAttachment},
        poll::{self, PollParams},
        status::{self, NewStatus, Status, Visibility},
        user::{self, User},
    },
    services::create_status::after_status_created,
    state::AppState,
};

/// The time one unknown-mention actor fetch may take before the mention is
/// dropped: three seconds, the same order as the outbound client's request
/// budgets — a slow peer may delay an ingestion slightly, never stall it.
const MENTION_LOOKUP_TIMEOUT: Duration = Duration::from_secs(3);

/// How many unknown mention actors one status may look up over the
/// network; further mentions of the same document drop out unresolved, the
/// way [`crate::services::mentions`] caps its acct lookups through
/// [`crate::config::Config::max_remote_mention_lookups`].
const MAX_MENTION_LOOKUPS: usize = 10;

/// What [`ingest_note`] decided about one received object.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum IngestOutcome {
    /// The instance gained the post: the fresh row, hook already run.
    Created(Status),
    /// The object's `uri` names a status this instance already stores —
    /// a redelivered `Create`, a second activity about the same object:
    /// the stored row, written again by nobody.
    Existing(Status),
    /// Nothing was written; the reason is static and rides the
    /// dispatcher's debug log.
    Ignored(&'static str),
}

/// The reply facts [`ingest_note`] resolved against stored statuses.
#[derive(Clone, Debug, Default)]
struct ReplyLink {
    /// The stored parent the object answers, when this instance has it.
    parent: Option<Status>,
    /// The answered object's `uri` when no stored row answers it — kept as
    /// the status's `in_reply_to_ap_id` so a later fetch of the parent can
    /// close the link.
    unresolved_ap_id: Option<String>,
    /// The conversation of the oldest stored status sharing the object's
    /// `context`, consulted only when the parent is not stored.
    context_conversation: Option<Uuid>,
}

/// Ingests one received `Note`-family object authored by `author`.
///
/// `object` is the embedded document (the dispatcher's routing already
/// proved its `type` one the create handlers register and, for a `Create`,
/// that its host matches the actor's); `author` is the [`User`] mirror
/// the delivery was authenticated by — the document's `attributedTo` must
/// name exactly that account, which [`note_parser::parse_note`] enforces.
///
/// The function owns the whole fold the [module docs](self) lay out and
/// reports what it decided as an [`IngestOutcome`].
///
/// # Errors
///
/// [`FederationError::Permanent`] when the document refuses parsing — it
/// is no note, carries no usable id, or fails containment; no retry can
/// make it one. [`FederationError::Transient`] for any database failure
/// (or a mention fetch's own database write failing): the dedupe ledger
/// row goes back and the activity is folded again.
pub async fn ingest_note(
    state: &AppState,
    author: &User,
    object: &Value,
) -> Result<IngestOutcome, FederationError> {
    let parsed = note_parser::parse_note(object, author, Utc::now()).map_err(|err| {
        FederationError::Permanent(format!("the object is not ingestable: {err}"))
    })?;
    let uri =
        parsed.status.uri.as_deref().ok_or_else(|| {
            FederationError::Permanent("the parsed object carries no uri".to_owned())
        })?;
    if let Some(decision) = seen_before(state, uri).await? {
        return Ok(decision);
    }
    if !author.is_active.get() {
        debug!(account = %author.ap_id, "a deactivated author's post is not ingested");
        return Ok(IngestOutcome::Ignored("deactivated author"));
    }
    let reply = resolve_reply(state, &parsed).await?;
    let quote_of_id = resolve_quote(state, parsed.quote_uri.as_deref()).await?;
    let mentioned = resolve_mentions(state, &parsed.mention_hrefs).await?;
    upsert_emojis(state, &parsed.emojis).await?;
    if drops_unsolicited(state, &parsed, author, &reply, &mentioned).await? {
        debug!(account = %author.ap_id, "an unsolicited public post from an unfollowed author is dropped");
        return Ok(IngestOutcome::Ignored("unsolicited public"));
    }
    let new = build_new_status(&parsed, &reply, quote_of_id, &mentioned);
    let row = write_ingested(state, &new, &parsed).await?;
    after_status_created(state, &row).await;
    debug!(
        status = %row.id,
        visibility = %row.visibility,
        "a remote note was ingested"
    );
    Ok(IngestOutcome::Created(row))
}

/// Step 2: what a repeat sight of `uri` answers — the tombstone's refusal,
/// the live row's restatement, or [`None`] when this instance has never
/// stored the object.
async fn seen_before(
    state: &AppState,
    uri: &str,
) -> Result<Option<IngestOutcome>, FederationError> {
    let stored = status::get_by_uri(&state.db, uri)
        .await
        .map_err(|err| transient("the status uri read", &err))?;
    Ok(stored.map(|row| {
        if row.deleted_at.is_some() {
            IngestOutcome::Ignored("tombstoned")
        } else {
            IngestOutcome::Existing(row)
        }
    }))
}

/// Step 4, reply arm: resolve the object's `inReplyTo` against the stored
/// statuses, and only when that misses inherit the conversation of a
/// stored status sharing the object's `context` (`None` conversation makes
/// [`status::insert`] mint the new row as its own thread root).
async fn resolve_reply(
    state: &AppState,
    parsed: &ParsedNote,
) -> Result<ReplyLink, FederationError> {
    let Some(parent_uri) = parsed.reply_to_uri.as_deref() else {
        return Ok(ReplyLink::default());
    };
    let parent = status::get_by_uri(&state.db, parent_uri)
        .await
        .map_err(|err| transient("the reply parent read", &err))?;
    if parent.is_some() {
        return Ok(ReplyLink {
            parent,
            ..ReplyLink::default()
        });
    }
    let context_conversation = match parsed.ap_context.as_deref() {
        Some(context) => status::conversation_id_for_context(&state.db, context)
            .await
            .map_err(|err| transient("the context conversation read", &err))?,
        None => None,
    };
    Ok(ReplyLink {
        unresolved_ap_id: Some(parent_uri.to_owned()),
        context_conversation,
        ..ReplyLink::default()
    })
}

/// Step 4, quote arm: `quote_uri` resolves locally only — a stored, live
/// status with that `uri` becomes the quote, an unknown or tombstoned one
/// leaves it unfilled. This ticket never fetches the quoted object.
async fn resolve_quote(
    state: &AppState,
    quote_uri: Option<&str>,
) -> Result<Option<Uuid>, FederationError> {
    let Some(uri) = quote_uri else {
        return Ok(None);
    };
    let found = status::get_by_uri(&state.db, uri)
        .await
        .map_err(|err| transient("the quote uri read", &err))?;
    Ok(found
        .filter(|row| row.deleted_at.is_none())
        .map(|row| row.id))
}

/// Step 5: turn the object's mention `href`s into the accounts it really
/// mentions, in document order, each counted once.
///
/// A `href` naming a stored account (local or a remote mirror — decision 7
/// keeps both in `users`, keyed by `ap_id`) resolves straight from the
/// database. An unknown `href` is fetched through
/// [`get_or_fetch_by_ap_id`] — at most [`MAX_MENTION_LOOKUPS`] of them per
/// status, each under its own [`MENTION_LOOKUP_TIMEOUT`] — and whatever is
/// still unknown afterwards is dropped: the mention stays text, the post
/// stands.
///
/// The inbound edit handler resolves an edited post's mentions through this
/// same function ([`crate::federation::handlers::note`]), so a mention the
/// edit adds lands on exactly the account the create's path would find.
pub(crate) async fn resolve_mentions(
    state: &AppState,
    hrefs: &[String],
) -> Result<Vec<User>, FederationError> {
    let mut seen: HashSet<&str> = HashSet::new();
    let unique: Vec<&str> = hrefs
        .iter()
        .filter(|href| seen.insert(href.as_str()))
        .map(String::as_str)
        .collect();
    let mut found: Vec<User> = Vec::new();
    let mut unknown: Vec<&str> = Vec::new();
    for href in unique {
        let stored = user::get_by_ap_id(&state.db, href)
            .await
            .map_err(|err| transient("the mention read", &err))?;
        match stored {
            Some(account) => found.push(account),
            None if unknown.len() < MAX_MENTION_LOOKUPS => unknown.push(href),
            None => {}
        }
    }
    let fetched = join_all(
        unknown
            .iter()
            .map(|href| fetch_mention(state, (*href).to_owned())),
    )
    .await;
    found.extend(fetched.into_iter().flatten());
    Ok(found)
}

/// One unknown mention's capped fetch: the [`User`] when the actor answers
/// inside [`MENTION_LOOKUP_TIMEOUT`] and parses into a mirror, [`None`] —
/// logged at debug — when it fails, refuses, or takes too long. A mention
/// lookup's failure is never an ingestion failure.
async fn fetch_mention(state: &AppState, href: String) -> Option<User> {
    match tokio::time::timeout(MENTION_LOOKUP_TIMEOUT, get_or_fetch_by_ap_id(state, &href)).await {
        Ok(Ok(account)) => Some(account),
        Ok(Err(err)) => {
            debug!(%err, mention = %href, "a mention actor could not be resolved; dropping the mention");
            None
        }
        Err(_elapsed) => {
            debug!(
                mention = %href,
                timeout_secs = MENTION_LOOKUP_TIMEOUT.as_secs(),
                "a mention actor ran past its lookup timeout; dropping the mention"
            );
            None
        }
    }
}

/// Step 6: upsert the `Emoji` tags the post announces, each under the
/// announcing author's domain — the same row shape the emoji endpoints
/// serve through `GET /api/v1/custom_emojis`, and idempotent, so a
/// redelivered Create rewrites the same images it wrote the first time.
async fn upsert_emojis(state: &AppState, emojis: &[RemoteEmoji]) -> Result<(), FederationError> {
    for emoji in emojis {
        let new = NewCustomEmoji {
            shortcode: emoji.shortcode.clone(),
            domain: emoji.domain.clone(),
            image_url: emoji.url.clone(),
            ..NewCustomEmoji::default()
        };
        custom_emoji::upsert(&state.db, &new)
            .await
            .map(drop)
            .map_err(|err| transient("the custom emoji upsert", &err))?;
    }
    Ok(())
}

/// Step 10: whether the `ACCEPT_UNSOLICITED_PUBLIC` gate drops this post.
///
/// The gate answers `true` — drop — only while the setting is off, for a
/// `public` or `unlisted` status whose author no local account follows and
/// that neither mentions a local user nor answers a local status. Every
/// other post keeps its author's reasons for having arrived here:
/// followers-addressed and direct posts were aimed at somebody on this
/// instance, and a followed author's post is solicited by definition.
async fn drops_unsolicited(
    state: &AppState,
    parsed: &ParsedNote,
    author: &User,
    reply: &ReplyLink,
    mentioned: &[User],
) -> Result<bool, FederationError> {
    if state.config.accept_unsolicited_public.get() {
        return Ok(false);
    }
    if !matches!(
        parsed.status.visibility,
        Visibility::Public | Visibility::Unlisted
    ) {
        return Ok(false);
    }
    if reply.parent.as_ref().is_some_and(|parent| parent.local)
        || mentioned.iter().any(|account| account.local.get())
    {
        return Ok(false);
    }
    let solicited = follow::has_local_followers(&state.db, author.id)
        .await
        .map_err(|err| transient("the follower gate read", &err))?;
    Ok(!solicited)
}

/// Steps 4 to 8 as data: the [`NewStatus`] the parsed document plus the
/// resolved references describe.
///
/// The parse filled everything the document alone states (author, text,
/// `uri`, `url`, visibility, timestamps); this moves the tag lists onto the
/// row's columns, answers, quotes and mentions included, fixes the status
/// as remote, and hands the conversation the inherited or [`None`] answer
/// [`status::insert`] derives. The counters stay at the insert's zero
/// defaults: a remote status' boosts and favourites are learned from the
/// activities this instance receives, never from what the object carried.
fn build_new_status(
    parsed: &ParsedNote,
    reply: &ReplyLink,
    quote_of_id: Option<Uuid>,
    mentioned: &[User],
) -> NewStatus {
    NewStatus {
        ap_context: parsed.ap_context.clone(),
        hashtags: parsed.hashtags.clone(),
        mentioned_user_ids: mentioned.iter().map(|account| account.id).collect(),
        in_reply_to_id: reply.parent.as_ref().map(|parent| parent.id),
        in_reply_to_account_id: reply.parent.as_ref().map(|parent| parent.user_id),
        in_reply_to_ap_id: reply.unresolved_ap_id.clone(),
        conversation_id: reply
            .parent
            .as_ref()
            .map(|parent| parent.conversation_id)
            .or(reply.context_conversation),
        quote_of_id,
        ..parsed.status.clone()
    }
}

/// Step 7 and 8's write: status, remote media and poll on one
/// transaction, so the row goes visible with everything it shows or not at
/// all. The [module docs](self) own the ordering rules each arm keeps.
///
/// # Errors
///
/// [`FederationError::Transient`] for any database failure; the rollback
/// takes the half-written status, media and poll with it.
async fn write_ingested(
    state: &AppState,
    new: &NewStatus,
    parsed: &ParsedNote,
) -> Result<Status, FederationError> {
    let mut tx = state
        .db
        .begin()
        .await
        .map_err(|err| transient("the ingest transaction", &err))?;
    let row = status::insert_conn(&mut tx, &state.config.base_url, new)
        .await
        .map_err(|err| transient("the status insert", &err))?;
    insert_media(&mut tx, row.id, &parsed.attachments).await?;
    insert_poll(&mut tx, row.id, parsed.poll.as_ref()).await?;
    tx.commit()
        .await
        .map_err(|err| transient("the ingest commit", &err))?;
    Ok(row)
}

/// The attachment arm of [`write_ingested`]: one `media_attachments` row
/// per kept document entry, on the status's open transaction.
///
/// `user_id` is `None` — nobody uploaded these, the remote post named them
/// — and the document url goes into both `remote_url` (where the media
/// proxy mirrors from) and `url` (what the entity serves until it has),
/// with the stated content type, description, `blurhash` and dimensions
/// kept as the parse read them. `position` is the document order.
///
/// The inbound edit handler writes an edited post's attachments through this
/// same function ([`crate::federation::handlers::note`]), so a mirror's
/// media looks the same whichever activity brought it.
pub(crate) async fn insert_media(
    conn: &mut PgConnection,
    status_id: Uuid,
    attachments: &[ParsedAttachment],
) -> Result<(), FederationError> {
    for (index, media) in attachments.iter().enumerate() {
        let position = i32::try_from(index).unwrap_or(i32::MAX);
        let new = NewMediaAttachment {
            user_id: None,
            status_id: Some(status_id),
            position,
            remote_url: Some(media.url.clone()),
            url: media.url.clone(),
            content_type: media.content_type.clone(),
            description: media.description.clone(),
            blurhash: media.blurhash.clone(),
            width: media.width,
            height: media.height,
            focus_x: media.focus_x,
            focus_y: media.focus_y,
            ..NewMediaAttachment::default()
        };
        media_attachment::insert(&mut *conn, &new)
            .await
            .map(drop)
            .map_err(|err| transient("the media insert", &err))?;
    }
    Ok(())
}

/// The poll arm of [`write_ingested`]: a `Question`'s [`ParsedPoll`] lands
/// exactly as the polls model defines a create — one `polls` row and one
/// `poll_options` row per option on the status's transaction — except that
/// a mirrored poll carries the counts the remote reported: each option its
/// `replies.totalItems`, the poll its `votersCount` and the options' sum.
/// A document that carried no poll writes nothing here.
///
/// The inbound edit handler grows a poll on a mirrored `Question` that
/// arrived without one through this same arm
/// ([`crate::federation::handlers::note`]).
pub(crate) async fn insert_poll(
    conn: &mut PgConnection,
    status_id: Uuid,
    question: Option<&ParsedPoll>,
) -> Result<(), FederationError> {
    let Some(poll_data) = question else {
        return Ok(());
    };
    let option_votes: Vec<i32> = poll_data
        .options
        .iter()
        .map(|option| whole(option.votes_count))
        .collect();
    let total = option_votes
        .iter()
        .fold(0_i64, |sum, votes| sum.saturating_add(i64::from(*votes)));
    let params = PollParams {
        options: poll_data
            .options
            .iter()
            .map(|option| option.title.clone())
            .collect(),
        expires_at: poll_data.expires_at,
        multiple: poll_data.multiple,
        option_votes,
        voters_count: poll_data.voters_count.map_or(0, whole),
        votes_count: whole(total),
        ..PollParams::default()
    };
    poll::create(conn, status_id, &params)
        .await
        .map_err(|err| transient("the poll insert", &err))
        .map(drop)
}

/// A remote count narrowed to the `integer` column's width: counts beyond
/// it clamp to the column's maximum rather than refuse the whole post for
/// a number nobody can vote into being true.
fn whole(count: i64) -> i32 {
    i32::try_from(count).unwrap_or(i32::MAX)
}

/// Shapes a database failure into the retryable arm of the dispatcher's
/// error vocabulary, naming the read or write that failed.
fn transient<E: fmt::Display>(what: &str, err: &E) -> FederationError {
    FederationError::Transient(format!("{what} failed: {err}"))
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
        models::user::{Flag, NewUser},
        uploads::UploaderBackend,
    };

    /// The test state: `pool` and the test config, collaborators inert.
    fn app_state(pool: &PgPool) -> AppState {
        state_with_config(pool, Config::for_tests())
    }

    /// The test state on `config` instead of the stock test one.
    fn state_with_config(pool: &PgPool, config: Config) -> AppState {
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

    /// The fixture author: remote `bob@remote.example`, active by default.
    async fn author(pool: &PgPool) -> User {
        user::insert(
            pool,
            &NewUser {
                nickname: "bob@remote.example".to_owned(),
                ap_id: "https://remote.example/users/bob".to_owned(),
                local: Flag::new(false),
                domain: Some("remote.example".to_owned()),
                followers_url: Some("https://remote.example/users/bob/followers".to_owned()),
                ..NewUser::default()
            },
        )
        .await
        .expect("the author fixture inserts")
    }

    /// `bob`'s public note object named `n`, replying to nothing.
    fn note(n: &str) -> Value {
        json!({
            "id": format!("https://remote.example/objects/{n}"),
            "type": "Note",
            "attributedTo": "https://remote.example/users/bob",
            "to": ["https://www.w3.org/ns/activitystreams#Public"],
            "cc": ["https://remote.example/users/bob/followers"],
            "content": format!("<p>ingest note {n}</p>"),
            "published": "2024-02-01T10:00:00.000Z",
        })
    }

    /// Counts the `statuses` rows stored under object uri `n`.
    async fn rows_for(pool: &PgPool, n: &str) -> i64 {
        sqlx::query_scalar::<_, i64>("SELECT count(*) FROM statuses WHERE uri = $1")
            .bind(format!("https://remote.example/objects/{n}"))
            .fetch_one(pool)
            .await
            .expect("the row count reads")
    }

    #[tokio::test]
    async fn ingesting_a_note_writes_a_remote_status() {
        let db = crate::test_db::test_db().await.expect("test database slot");
        let pool = db.pool().clone();
        let state = app_state(&pool);
        let bob = author(&pool).await;

        let outcome = ingest_note(&state, &bob, &note("ingest-1"))
            .await
            .expect("the fixture note ingests");

        let IngestOutcome::Created(row) = outcome else {
            panic!("a first-seen note must be created, got {outcome:?}");
        };
        assert!(!row.local, "the row is remote");
        assert_eq!(
            row.uri, "https://remote.example/objects/ingest-1",
            "the object id is the uri"
        );
        assert_eq!(row.user_id, bob.id, "the author owns the row");
        assert_eq!(row.reblogs_count, 0, "the counters stay zero");
        assert_eq!(row.favourites_count, 0, "both of them");
        assert_eq!(
            row.inserted_at.to_rfc3339(),
            "2024-02-01T10:00:00+00:00",
            "the published time is the creation time"
        );
    }

    #[tokio::test]
    async fn a_second_create_of_the_same_object_answers_the_stored_row() {
        let db = crate::test_db::test_db().await.expect("test database slot");
        let pool = db.pool().clone();
        let state = app_state(&pool);
        let bob = author(&pool).await;

        let first = ingest_note(&state, &bob, &note("ingest-dup"))
            .await
            .expect("the first create ingests");
        let second = ingest_note(&state, &bob, &note("ingest-dup"))
            .await
            .expect("the second create is not an error");

        let IngestOutcome::Existing(row) = second else {
            panic!("a repeat create restates the stored row, got {second:?}");
        };
        let IngestOutcome::Created(created) = first else {
            panic!("the first create writes");
        };
        assert_eq!(row.id, created.id, "and it restates the same row");
        assert_eq!(
            rows_for(&pool, "ingest-dup").await,
            1,
            "one row for the two creates"
        );
    }

    #[tokio::test]
    async fn a_create_of_a_tombstoned_uri_is_ignored() {
        let db = crate::test_db::test_db().await.expect("test database slot");
        let pool = db.pool().clone();
        let state = app_state(&pool);
        let bob = author(&pool).await;

        let IngestOutcome::Created(row) = ingest_note(&state, &bob, &note("ingest-dead"))
            .await
            .expect("the first create ingests")
        else {
            panic!("the first create writes");
        };
        let _tombstone = status::soft_delete(&pool, row.id)
            .await
            .expect("the delete tombstones");
        let outcome = ingest_note(&state, &bob, &note("ingest-dead"))
            .await
            .expect("the re-import is not an error");

        assert_eq!(
            outcome,
            IngestOutcome::Ignored("tombstoned"),
            "a deleted post is never re-imported"
        );
    }

    #[tokio::test]
    async fn a_deactivated_authors_post_is_ignored() {
        let db = crate::test_db::test_db().await.expect("test database slot");
        let pool = db.pool().clone();
        let state = app_state(&pool);
        let created = author(&pool).await;
        let _deactivated = sqlx::query("UPDATE users SET is_active = false WHERE id = $1")
            .bind(created.id)
            .execute(&pool)
            .await
            .expect("the deactivation writes");
        // Re-read the row as the dispatcher's `load_signer` hands it: the
        // live one, with `is_active` already false.
        let bob = user::get_by_id(&pool, created.id)
            .await
            .expect("the author reads back")
            .expect("the author is stored");

        let outcome = ingest_note(&state, &bob, &note("ingest-inactive"))
            .await
            .expect("the refusal is not an error");

        assert_eq!(
            outcome,
            IngestOutcome::Ignored("deactivated author"),
            "a retired mirror ingests nothing"
        );
        assert_eq!(
            rows_for(&pool, "ingest-inactive").await,
            0,
            "and no row stands"
        );
    }

    #[tokio::test]
    async fn an_unparseable_object_is_a_permanent_failure() {
        let db = crate::test_db::test_db().await.expect("test database slot");
        let pool = db.pool().clone();
        let state = app_state(&pool);
        let bob = author(&pool).await;

        let err = ingest_note(
            &state,
            &bob,
            &json!({"id": "https://remote.example/objects/x", "type": "ChatMessage"}),
        )
        .await
        .expect_err("a chat message is not this ticket's note");

        assert!(
            matches!(err, FederationError::Permanent(_)),
            "no retry can fix the type, and it must not ask for one: {err}"
        );
    }

    #[tokio::test]
    async fn a_reply_to_a_stored_parent_links_and_groups() {
        let db = crate::test_db::test_db().await.expect("test database slot");
        let pool = db.pool().clone();
        let state = app_state(&pool);
        let bob = author(&pool).await;
        let parent = ingest_note(&state, &bob, &note("ingest-root"))
            .await
            .expect("the root ingests");
        let IngestOutcome::Created(root) = parent else {
            panic!("the root writes");
        };
        let child = json!({
            "id": "https://remote.example/objects/ingest-child",
            "type": "Note",
            "attributedTo": "https://remote.example/users/bob",
            "to": ["https://www.w3.org/ns/activitystreams#Public"],
            "content": "<p>replied</p>",
            "inReplyTo": "https://remote.example/objects/ingest-root",
            "context": "https://remote.example/contexts/ingest-thread",
        });

        let outcome = ingest_note(&state, &bob, &child)
            .await
            .expect("the reply ingests");
        let IngestOutcome::Created(row) = outcome else {
            panic!("the reply writes, got {outcome:?}");
        };

        assert_eq!(
            row.in_reply_to_id,
            Some(root.id),
            "the stored parent is linked by id"
        );
        assert_eq!(row.in_reply_to_account_id, Some(bob.id), "and by author");
        assert_eq!(
            row.in_reply_to_ap_id, None,
            "a linked parent leaves no dangling reference"
        );
        assert_eq!(
            row.conversation_id, root.conversation_id,
            "the reply joins the parent's conversation"
        );
        assert_eq!(
            row.ap_context.as_deref(),
            Some("https://remote.example/contexts/ingest-thread"),
            "and keeps the thread collection it names"
        );
        let replies =
            sqlx::query_scalar::<_, i32>("SELECT replies_count FROM statuses WHERE id = $1")
                .bind(root.id)
                .fetch_one(&pool)
                .await
                .expect("the parent reads back");
        assert_eq!(replies, 1, "and the parent counts the answer");
    }

    #[tokio::test]
    async fn a_reply_to_an_unknown_parent_groups_by_context() {
        let db = crate::test_db::test_db().await.expect("test database slot");
        let pool = db.pool().clone();
        let state = app_state(&pool);
        let bob = author(&pool).await;
        let orphan = |n: &str| {
            json!({
                "id": format!("https://remote.example/objects/{n}"),
                "type": "Note",
                "attributedTo": "https://remote.example/users/bob",
                "to": ["https://www.w3.org/ns/activitystreams#Public"],
                "content": format!("<p>orphan {n}</p>"),
                "inReplyTo": "https://remote.example/objects/never-delivered",
                "context": "https://remote.example/contexts/orphans",
            })
        };

        let first = ingest_note(&state, &bob, &orphan("ingest-orphan-1"))
            .await
            .expect("the first orphan ingests");
        let second = ingest_note(&state, &bob, &orphan("ingest-orphan-2"))
            .await
            .expect("the second orphan ingests");
        let (IngestOutcome::Created(one), IngestOutcome::Created(two)) = (first, second) else {
            panic!("both orphans write");
        };

        assert_eq!(
            one.in_reply_to_ap_id.as_deref(),
            Some("https://remote.example/objects/never-delivered"),
            "an unstored parent stays an ap_id reference"
        );
        assert_eq!(one.in_reply_to_id, None, "and gets no id link");
        assert_eq!(
            two.conversation_id, one.conversation_id,
            "the second status of the same context joins the first's conversation"
        );
    }

    #[tokio::test]
    async fn mentions_resolve_to_stored_accounts_and_unresolved_drop() {
        let db = crate::test_db::test_db().await.expect("test database slot");
        let pool = db.pool().clone();
        let state = app_state(&pool);
        let bob = author(&pool).await;
        let carol = user::insert(
            &pool,
            &NewUser {
                nickname: "carol@remote.example".to_owned(),
                ap_id: "https://remote.example/users/carol".to_owned(),
                local: Flag::new(false),
                domain: Some("remote.example".to_owned()),
                ..NewUser::default()
            },
        )
        .await
        .expect("the mentionee inserts");

        let object = json!({
            "id": "https://remote.example/objects/ingest-mentions",
            "type": "Note",
            "attributedTo": "https://remote.example/users/bob",
            "to": ["https://www.w3.org/ns/activitystreams#Public"],
            "content": "<p>hi carol and nobody</p>",
            "tag": [
                { "type": "Mention", "href": "https://remote.example/users/carol", "name": "@carol" },
                { "type": "Mention", "href": "https://nowhere.invalid/users/gone", "name": "@gone" },
            ],
        });
        let outcome = ingest_note(&state, &bob, &object)
            .await
            .expect("the mentions that resolve are enough");
        let IngestOutcome::Created(row) = outcome else {
            panic!("the post writes, got {outcome:?}");
        };

        let linked = mention_user_ids(&pool, row.id).await;
        assert_eq!(
            linked,
            vec![carol.id],
            "the stored mention links, the unreachable one drops"
        );
    }

    /// The users `status_id` mentions, in `status_mentions` order.
    async fn mention_user_ids(pool: &PgPool, status_id: Uuid) -> Vec<Uuid> {
        sqlx::query_scalar::<_, Uuid>(
            "SELECT user_id FROM status_mentions WHERE status_id = $1 ORDER BY user_id",
        )
        .bind(status_id)
        .fetch_all(pool)
        .await
        .expect("the mentions read")
    }

    #[tokio::test]
    async fn attachments_emojis_and_hashtags_land_with_the_status() {
        let db = crate::test_db::test_db().await.expect("test database slot");
        let pool = db.pool().clone();
        let state = app_state(&pool);
        let bob = author(&pool).await;

        let outcome = ingest_note(&state, &bob, &satellites_object())
            .await
            .expect("the satellites ingest");
        let IngestOutcome::Created(row) = outcome else {
            panic!("the post writes, got {outcome:?}");
        };

        let media = media_attachment::for_statuses(&pool, &[row.id])
            .await
            .expect("the media reads")
            .remove(&row.id)
            .expect("the status carries media");
        assert_media_rows(&media);

        let emoji = custom_emoji::get_by_shortcodes(
            &pool,
            &[String::from("ralph")],
            Some("remote.example"),
        )
        .await
        .expect("the emoji reads");
        assert_eq!(emoji.len(), 1, "the Emoji tag upserted its row");
        assert_eq!(
            emoji[0].domain.as_deref(),
            Some("remote.example"),
            "under the author's domain"
        );
        assert_eq!(
            emoji[0].image_url, "https://remote.example/emoji/ralph.png",
            "with the announced image"
        );

        let tags = sqlx::query_scalar::<_, String>(
            "SELECT t.name FROM hashtags t
               JOIN status_hashtags sh ON sh.hashtag_id = t.id
              WHERE sh.status_id = $1",
        )
        .bind(row.id)
        .fetch_all(&pool)
        .await
        .expect("the tags read");
        assert_eq!(tags, vec!["rust".to_owned()], "the Hashtag tag linked");
    }

    /// The note object carrying one `Hashtag`, one `Emoji` and two
    /// `Document` attachments, the first with the full media column set.
    fn satellites_object() -> Value {
        json!({
            "id": "https://remote.example/objects/ingest-satellites",
            "type": "Note",
            "attributedTo": "https://remote.example/users/bob",
            "to": ["https://www.w3.org/ns/activitystreams#Public"],
            "content": "<p>look</p>",
            "tag": [
                { "type": "Hashtag", "name": "#Rust" },
                {
                    "type": "Emoji",
                    "name": ":ralph:",
                    "icon": { "type": "Image", "url": "https://remote.example/emoji/ralph.png" }
                },
            ],
            "attachment": [
                {
                    "type": "Document",
                    "mediaType": "image/png",
                    "url": "https://remote.example/media/b.png",
                    "name": "Second",
                    "blurhash": "LeHK",
                    "width": 640,
                    "height": 480
                },
                {
                    "type": "Document",
                    "mediaType": "image/jpeg",
                    "url": "https://remote.example/media/a.jpg",
                    "name": "First"
                },
            ],
        })
    }

    /// The attachment rows the ingest wrote: both documents, in document
    /// order, unowned, with the stated url, alt text, placeholder and
    /// dimensions kept.
    fn assert_media_rows(media: &[media_attachment::MediaAttachment]) {
        assert_eq!(media.len(), 2, "both attachments land");
        assert_eq!(
            media[0].url, "https://remote.example/media/b.png",
            "in document order"
        );
        assert_eq!(
            media[0].remote_url.as_deref(),
            Some("https://remote.example/media/b.png"),
            "the document url is the remote url"
        );
        assert_eq!(media[0].user_id, None, "remote media has no uploader");
        assert_eq!(
            media[0].description.as_deref(),
            Some("Second"),
            "and its alt text"
        );
        assert_eq!(
            media[0].blurhash.as_deref(),
            Some("LeHK"),
            "and its placeholder"
        );
        assert_eq!(media[0].width, Some(640), "and its dimensions");
        assert_eq!(media[0].height, Some(480), "both");
    }

    #[tokio::test]
    async fn a_question_lands_its_poll_with_the_reported_counts() {
        let db = crate::test_db::test_db().await.expect("test database slot");
        let pool = db.pool().clone();
        let state = app_state(&pool);
        let bob = author(&pool).await;
        let object = json!({
            "id": "https://remote.example/objects/ingest-poll",
            "type": "Question",
            "attributedTo": "https://remote.example/users/bob",
            "to": ["https://www.w3.org/ns/activitystreams#Public"],
            "content": "<p>which?</p>",
            "oneOf": [
                { "type": "Note", "name": "Red", "replies": { "totalItems": 3 } },
                { "type": "Note", "name": "Blue", "replies": { "totalItems": 7 } }
            ],
            "votersCount": 10
        });

        let outcome = ingest_note(&state, &bob, &object)
            .await
            .expect("the question ingests");
        let IngestOutcome::Created(row) = outcome else {
            panic!("the poll post writes, got {outcome:?}");
        };

        let stand = poll::get_for_status(&pool, row.id)
            .await
            .expect("the poll reads")
            .expect("the status carries a poll");
        assert_eq!(stand.voters_count, 10, "the reported voter count");
        assert_eq!(stand.votes_count, 10, "and the votes total");
        let choices = poll::options(&pool, stand.id)
            .await
            .expect("the options read");
        let votes: Vec<i32> = choices.iter().map(|choice| choice.votes_count).collect();
        assert_eq!(votes, vec![3, 7], "per option, in document order");
    }

    #[tokio::test]
    async fn a_locally_quoted_object_links_the_quote() {
        let db = crate::test_db::test_db().await.expect("test database slot");
        let pool = db.pool().clone();
        let state = app_state(&pool);
        let bob = author(&pool).await;
        let IngestOutcome::Created(quoted) = ingest_note(&state, &bob, &note("ingest-quoted"))
            .await
            .expect("the quote target ingests")
        else {
            panic!("the target writes");
        };
        let quoting = json!({
            "id": "https://remote.example/objects/ingest-quoter",
            "type": "Note",
            "attributedTo": "https://remote.example/users/bob",
            "to": ["https://www.w3.org/ns/activitystreams#Public"],
            "content": "<p>quoting</p>",
            "quoteUrl": "https://remote.example/objects/ingest-quoted",
        });

        let outcome = ingest_note(&state, &bob, &quoting)
            .await
            .expect("the quoting note ingests");
        let IngestOutcome::Created(row) = outcome else {
            panic!("the quoting post writes, got {outcome:?}");
        };
        assert_eq!(
            row.quote_of_id,
            Some(quoted.id),
            "a stored quote target links"
        );

        let stranger = json!({
            "id": "https://remote.example/objects/ingest-quoter-2",
            "type": "Note",
            "attributedTo": "https://remote.example/users/bob",
            "to": ["https://www.w3.org/ns/activitystreams#Public"],
            "content": "<p>quoting nothing</p>",
            "quoteUrl": "https://remote.example/objects/never-quoted",
        });
        let IngestOutcome::Created(unlinked) = ingest_note(&state, &bob, &stranger)
            .await
            .expect("an unstored quote is no failure")
        else {
            panic!("the post writes");
        };
        assert_eq!(
            unlinked.quote_of_id, None,
            "and stays unlinked — this ticket resolves quotes locally only"
        );
    }

    #[tokio::test]
    async fn the_unsolicited_gate_drops_public_posts_from_unfollowed_authors() {
        let db = crate::test_db::test_db().await.expect("test database slot");
        let pool = db.pool().clone();
        let state = gating_state(&pool);
        let bob = author(&pool).await;
        let alice = user::insert(
            &pool,
            &NewUser::local("alice", &user::local_ap_id(&state.config.base_url, "alice")),
        )
        .await
        .expect("the local account inserts");

        let dropped = ingest_note(&state, &bob, &note("ingest-unsolicited"))
            .await
            .expect("the refusal is not an error");
        assert_eq!(
            dropped,
            IngestOutcome::Ignored("unsolicited public"),
            "off, an unfollowed author's public post is dropped"
        );

        let mentioned = json!({
            "id": "https://remote.example/objects/ingest-mentioned-local",
            "type": "Note",
            "attributedTo": "https://remote.example/users/bob",
            "to": ["https://www.w3.org/ns/activitystreams#Public"],
            "content": "<p>hello alice</p>",
            "tag": [{ "type": "Mention", "href": alice.ap_id.clone(), "name": "@alice" }],
        });
        let outcome = ingest_note(&state, &bob, &mentioned)
            .await
            .expect("the local mention resolves");
        assert!(
            matches!(outcome, IngestOutcome::Created(_)),
            "a post that mentions a local user passes the gate anyway, got {outcome:?}"
        );

        let direct = json!({
            "id": "https://remote.example/objects/ingest-gated-private",
            "type": "Note",
            "attributedTo": "https://remote.example/users/bob",
            "to": ["https://remote.example/users/bob/followers"],
            "content": "<p>followers only</p>",
        });
        let to_followers = ingest_note(&state, &bob, &direct)
            .await
            .expect("the followers note ingests");
        assert!(
            matches!(to_followers, IngestOutcome::Created(_)),
            "a followers-addressed post was aimed at somebody here, got {to_followers:?}"
        );

        let open = follow::follow(&pool, &state.config.base_url, &alice, &bob)
            .await
            .expect("the follow writes");
        let _accepted = follow::accept(&pool, open.id)
            .await
            .expect("the follow is accepted");
        let solicited = ingest_note(&state, &bob, &note("ingest-solicited"))
            .await
            .expect("the post-follow fold runs");
        assert!(
            matches!(solicited, IngestOutcome::Created(_)),
            "once a local account follows, the author's public posts pass, got {solicited:?}"
        );
    }

    #[tokio::test]
    async fn a_reply_to_a_local_status_passes_the_unsolicited_gate() {
        let db = crate::test_db::test_db().await.expect("test database slot");
        let pool = db.pool().clone();
        let state = gating_state(&pool);
        let bob = author(&pool).await;
        let alice = user::insert(
            &pool,
            &NewUser::local("alice", &user::local_ap_id(&state.config.base_url, "alice")),
        )
        .await
        .expect("the local account inserts");
        let local_post = status::insert(
            &pool,
            &state.config.base_url,
            &NewStatus::local(&alice, "the local root"),
        )
        .await
        .expect("the local status writes");

        let child = json!({
            "id": "https://remote.example/objects/ingest-reply-local",
            "type": "Note",
            "attributedTo": "https://remote.example/users/bob",
            "to": ["https://www.w3.org/ns/activitystreams#Public"],
            "content": "<p>answering alice</p>",
            "inReplyTo": local_post.uri,
        });
        let outcome = ingest_note(&state, &bob, &child)
            .await
            .expect("the reply ingests");
        let IngestOutcome::Created(row) = outcome else {
            panic!("a post answering a local status passes the gate, got {outcome:?}");
        };
        assert_eq!(
            row.in_reply_to_id,
            Some(local_post.id),
            "and keeps its link to the local parent"
        );
    }

    #[tokio::test]
    async fn a_direct_note_is_its_own_conversation_root() {
        let db = crate::test_db::test_db().await.expect("test database slot");
        let pool = db.pool().clone();
        let state = app_state(&pool);
        let bob = author(&pool).await;
        let object = json!({
            "id": "https://remote.example/objects/ingest-dm",
            "type": "Note",
            "attributedTo": "https://remote.example/users/bob",
            "to": ["https://test.example/users/someone"],
            "content": "<p>a direct note</p>",
        });

        let outcome = ingest_note(&state, &bob, &object)
            .await
            .expect("the direct note ingests");
        let IngestOutcome::Created(row) = outcome else {
            panic!("the post writes, got {outcome:?}");
        };

        assert_eq!(
            row.visibility,
            Visibility::Direct,
            "a non-public, non-followers addressing parses direct"
        );
        assert_eq!(
            row.conversation_id, row.id,
            "a root of nothing mints its own conversation"
        );
        assert!(
            row.in_reply_to_id.is_none() && row.in_reply_to_ap_id.is_none(),
            "and carries no reply reference of either kind"
        );
    }

    /// The state with `ACCEPT_UNSOLICITED_PUBLIC` off: the gate under test.
    fn gating_state(pool: &PgPool) -> AppState {
        let config = Config::from_lookup(|key| match key {
            crate::config::DATABASE_URL_VAR => Some(crate::config::TEST_DATABASE_URL.to_owned()),
            crate::config::BASE_URL_VAR => Some(crate::config::TEST_BASE_URL.to_owned()),
            crate::config::ACCEPT_UNSOLICITED_PUBLIC_VAR => Some("false".to_owned()),
            _ => None,
        })
        .expect("the gating test config is valid");
        state_with_config(pool, config)
    }
}
