//! Writing a new status: the [`NewStatus`] shape a caller fills in and the
//! [`insert`] / [`insert_conn`] transaction behind it — the minted id, the
//! derived `uri`, `url` and `conversation_id`, the media claims, the
//! mention and hashtag links, and the counters the fresh row feeds.

use chrono::{DateTime, Utc};
use sqlx::{PgConnection, PgPool};
use uuid::Uuid;

use crate::{
    error::{AppError, AppResult},
    models::{hashtag, media_attachment, mention, user::User},
    util::{entities::extract_hashtags, format::text_to_html, id::new_id},
};

use super::{
    DEFAULT_CONTENT_TYPE, Status, Visibility, counters::count_new_post, reads::notice_url,
    reads::object_uri,
};

/// What to insert: every `statuses` column a caller can decide up front.
///
/// What is left out is what the model owns instead: the `id` [`insert`]
/// mints, the `uri`, `url` and `conversation_id` it derives from that id, the
/// `deleted_at` [`soft_delete`](super::soft_delete) sets, and the four counters those three
/// functions move.
///
/// [`NewStatus::default`] mirrors the column defaults, so a caller sets only
/// what it knows; [`NewStatus::local`] is the usual starting point for a post
/// of a local account.
#[derive(Clone, Debug)]
pub struct NewStatus {
    /// `ActivityPub` object id: required for a remote status, ignored for a
    /// local one, whose id [`insert`] derives from its `UUIDv7` key.
    pub uri: Option<String>,
    /// Human-facing URL: derived for a local status, whatever the remote
    /// named otherwise.
    pub url: Option<String>,
    /// Author; required.
    pub user_id: Uuid,
    /// Sanitized HTML of the post; empty in the default.
    pub content: String,
    /// Source text of the post; empty in the default.
    pub raw_text: String,
    /// `MIME` type of [`raw_text`](Self::raw_text);
    /// [`DEFAULT_CONTENT_TYPE`] in the default.
    pub content_type: String,
    /// Content warning subject line; empty in the default.
    pub spoiler_text: String,
    /// Whether the content warning hides the content; `false` in the default.
    pub sensitive: bool,
    /// Who may see the post; [`Visibility::Public`] in the default.
    pub visibility: Visibility,
    /// BCP 47 language tag.
    pub language: Option<String>,
    /// Status being answered; `None` for a root status.
    pub in_reply_to_id: Option<Uuid>,
    /// Author of the status being answered.
    pub in_reply_to_account_id: Option<Uuid>,
    /// `ActivityPub` id of a status being answered that is not stored here.
    pub in_reply_to_ap_id: Option<String>,
    /// Thread group id; `None` lets [`insert`] take the parent's
    /// conversation, or the new id when there is no parent.
    pub conversation_id: Option<Uuid>,
    /// The `ActivityPub` thread collection a remote status's document
    /// names; [`insert`] stores it verbatim, and `None` — every local
    /// status's answer — leaves the column empty (see
    /// [`crate::services::ingest`] for the read that groups a context's
    /// statuses under one `conversation_id`).
    pub ap_context: Option<String>,
    /// The `conversations` row a `direct` post answers into: only
    /// `POST /api/v1/statuses`'s `in_reply_to_conversation_id` arm names
    /// one; every other direct status gets its thread row from
    /// [`crate::models::conversation::participate`] after the insert.
    pub direct_conversation_id: Option<Uuid>,
    /// Original being reblogged.
    pub reblog_of_id: Option<Uuid>,
    /// Original being quoted.
    pub quote_of_id: Option<Uuid>,
    /// Accounts this status @-mentions, in mention order: [`insert`] links
    /// them to the new row in its transaction (see
    /// [`crate::models::mention`]); empty in the default.
    pub mentioned_user_ids: Vec<Uuid>,
    /// Hashtags this status carries, as written: [`insert`] upserts the
    /// usable ones by name and links them to the new row in its
    /// transaction (see [`crate::models::hashtag`]); empty in the default.
    /// A local post's [`NewStatus::local`] fills them with
    /// [`extract_hashtags`] of the text; a remote status's caller fills
    /// them from the activity's `tag` list.
    pub hashtags: Vec<String>,
    /// Media the poster claims for this status, in the order they want them
    /// to render: [`insert`] attaches these rows on its own transaction
    /// through [`crate::models::media_attachment::attach_to_status`], each
    /// one at its position in this list, and refuses the whole write when
    /// any id names a row the poster may not claim (see
    /// [`media_attachment::MEDIA_NOT_FOUND`]); empty in the default.
    pub media_ids: Vec<Uuid>,
    /// The pending schedule this post is the publish of, `None` otherwise.
    ///
    /// That schedule's row still stands while it publishes,
    /// and its reservation of [`media_ids`](Self::media_ids) must not block
    /// the claim it makes for itself (see
    /// [`crate::models::media_attachment::attach_to_status`]).
    pub schedule_id: Option<Uuid>,
    /// Whether this instance owns the post; `false` in the default, as in
    /// [`NewUser`](crate::models::user::NewUser).
    pub local: bool,
    /// Name of the client posting this status.
    pub application_name: Option<String>,
    /// Website of that client.
    pub application_website: Option<String>,
    /// When the content was last edited; `None` for an unedited post.
    pub edited_at: Option<DateTime<Utc>>,
    /// When the post is due to disappear; `None` means it never does.
    pub expires_at: Option<DateTime<Utc>>,
    /// Creation time — the remote `published` timestamp when mirroring;
    /// `None` lets the database stamp `now()`.
    pub inserted_at: Option<DateTime<Utc>>,
}

/// The column defaults of `statuses`: an empty remote post with nothing it
/// replies to, reblogs or quotes, and no counters of its own.
impl Default for NewStatus {
    fn default() -> Self {
        Self {
            uri: None,
            url: None,
            user_id: Uuid::nil(),
            content: String::new(),
            raw_text: String::new(),
            content_type: DEFAULT_CONTENT_TYPE.to_owned(),
            spoiler_text: String::new(),
            sensitive: false,
            visibility: Visibility::Public,
            language: None,
            in_reply_to_id: None,
            in_reply_to_account_id: None,
            in_reply_to_ap_id: None,
            conversation_id: None,
            ap_context: None,
            direct_conversation_id: None,
            reblog_of_id: None,
            quote_of_id: None,
            mentioned_user_ids: Vec::new(),
            hashtags: Vec::new(),
            media_ids: Vec::new(),
            schedule_id: None,
            local: false,
            application_name: None,
            application_website: None,
            edited_at: None,
            expires_at: None,
            inserted_at: None,
        }
    }
}

impl NewStatus {
    /// A local post by `user` of the plain text `text`, in the shape
    /// `docs/ARCHITECTURE.md` decision 6 stores it: [`local`](Self::local)
    /// set, [`user_id`](Self::user_id) from `user`, [`text_to_html`] in
    /// [`content`](Self::content) with `text` kept in
    /// [`raw_text`](Self::raw_text) as [`DEFAULT_CONTENT_TYPE`],
    /// [`extract_hashtags`] of `text` in [`hashtags`](Self::hashtags), and
    /// [`Visibility::Public`].
    ///
    /// The columns that depend on the primary key — `uri`
    /// (`{BASE_URL}/objects/{uuid}`), `url` (`{BASE_URL}/notice/{uuid}`) and
    /// `conversation_id` — are left empty here for [`insert`] to fill once it
    /// has minted the id and looked up the status being replied to.
    #[must_use]
    pub fn local(user: &User, text: &str) -> Self {
        Self {
            user_id: user.id,
            content: text_to_html(text),
            raw_text: text.to_owned(),
            content_type: DEFAULT_CONTENT_TYPE.to_owned(),
            visibility: Visibility::Public,
            hashtags: extract_hashtags(text),
            local: true,
            ..Self::default()
        }
    }
}

/// The `INSERT` behind [`insert`]: every column a caller (or
/// [`NewStatus::local`]) knows is written from a [`NewStatus`], with the id
/// and the derived `uri`, `url` and `conversation_id` as extra binds and the
/// creation time falling back to the column default. `RETURNING *` brings the
/// row back with the timestamps the database stamped.
const INSERT_SQL: &str = "INSERT INTO statuses (
     id, uri, url, user_id, content, raw_text, content_type, spoiler_text,
     sensitive, visibility, language, in_reply_to_id,
     in_reply_to_account_id, in_reply_to_ap_id, conversation_id,
     direct_conversation_id, reblog_of_id, quote_of_id, local,
     application_name, application_website, edited_at, expires_at,
     ap_context, inserted_at
) VALUES (
     $1, $2, $3, $4, $5, $6, $7, $8, $9, $10, $11, $12, $13, $14, $15, $16,
     $17, $18, $19, $20, $21, $22, $23, $24, COALESCE($25, now())
) RETURNING *";

/// The conversation the status being replied to belongs to, read from the
/// parent before the reply is written.
const PARENT_CONVERSATION_SQL: &str = "SELECT conversation_id FROM statuses WHERE id = $1";

/// Inserts `new_status` and returns the stored row.
///
/// The id is a fresh `UUIDv7` (decision 4) and, for a local status, `uri` and
/// `url` are derived from it as `{base_url}/objects/{id}` and
/// `{base_url}/notice/{id}` (decision 6); a remote status keeps the `uri` and
/// `url` it arrived with, and its `uri` is required, since the column is
/// `NOT NULL`. `base_url` is the instance origin, i.e.
/// [`Config::base_url`](crate::config::Config::base_url), which the model
/// layer is handed rather than reading configuration itself.
///
/// `conversation_id` is the one `new_status` carries, else the parent's
/// conversation when the status answers a stored status, else the new id: a
/// root status is its own conversation and every answer joins it.
///
/// In the same transaction the author's `statuses_count` goes up by one and
/// their `last_status_at` moves forward to this status's creation time (never
/// back, so importing an old remote post leaves a newer one on the books),
/// the parent's `replies_count` goes up for a reply, the original's
/// `reblogs_count` for a reblog and the original's `quotes_count` for a quote
/// — exactly what [`soft_delete`](super::soft_delete) steps back down — and every account in [`mentioned_user_ids`](NewStatus::mentioned_user_ids)
/// is linked to the new row (see [`crate::models::mention`]) and every
/// usable tag in [`hashtags`](NewStatus::hashtags) is upserted and linked
/// (see [`crate::models::hashtag`]), so a status and its mentions and tags
/// appear together or not at all. The rows named by
/// [`media_ids`](NewStatus::media_ids) are claimed for the new status in
/// that same transaction, at their list positions
/// (see [`crate::models::media_attachment`]); a media id the poster may not
/// claim refuses the whole write.
///
/// # Errors
///
/// Any database failure (connection, constraint, encoding) arrives as
/// [`AppError::Database`](crate::error::AppError): a missing `uri` for a
/// remote status, a duplicate `uri`, an unknown author, parent or original,
/// or an unknown mentioned account arrives as the not-null, unique or
/// foreign-key violation the database reports — and the rollback takes the
/// half-written status with it. A media id the poster does not own, that no
/// row holds or that another status already claims is
/// [`AppError::Unprocessable`] — the same rollback, the same refusal the
/// status endpoints answer the id with.
pub async fn insert(pool: &PgPool, base_url: &str, new_status: &NewStatus) -> AppResult<Status> {
    let mut tx = pool.begin().await?;
    let status = insert_conn(&mut tx, base_url, new_status).await?;
    tx.commit().await?;
    Ok(status)
}

/// [`insert`]'s whole write on the caller's connection: the same status
/// row, mention links, hashtag links and counter steps, with the
/// transaction left open for the caller to extend or roll back.
///
/// This is the seam an idempotent create writes through — see
/// [`crate::models::idempotency_key::claim`], whose `ON CONFLICT` insert
/// joins this statement on the same transaction so a status and its key
/// commit together or neither does.
///
/// # Errors
///
/// Those of [`insert`], arriving on the caller's transaction: only a commit
/// this function never sees can make the write stand.
pub async fn insert_conn(
    conn: &mut PgConnection,
    base_url: &str,
    new_status: &NewStatus,
) -> AppResult<Status> {
    let parent = parent_conversation(conn, new_status.in_reply_to_id).await?;
    let status = write_status(conn, base_url, new_status, parent).await?;
    claim_media(conn, new_status, status.id).await?;
    mention::link_conn(conn, status.id, &new_status.mentioned_user_ids).await?;
    hashtag::link_names_conn(conn, status.id, &new_status.hashtags).await?;
    count_new_post(conn, &status).await?;
    Ok(status)
}

/// [`insert_conn`]'s media arm: the rows [`NewStatus::media_ids`] names are
/// claimed for the just-written status on the same transaction, each one at
/// the position its id holds in the list, through
/// [`media_attachment::attach_to_status`].
///
/// The attach's own gate is the validation: an id with no row, one that
/// names somebody else's upload and one a status already claims leave their
/// row untouched, so a short count is a body that asked for media its poster
/// may not post — refused with the 422 [`media_attachment::MEDIA_NOT_FOUND`]
/// wording, and the caller's `?` takes the half-written status down with it.
/// A media id that raced here (claimed between the body's read and this
/// write) answers the same refusal. No ids is no query at all.
///
/// # Errors
///
/// [`AppError::Unprocessable`] for a count the poster's own unclaimed rows
/// cannot fill; any database failure arrives through `?` as
/// [`AppError::Database`].
async fn claim_media(
    conn: &mut PgConnection,
    new_status: &NewStatus,
    status_id: Uuid,
) -> AppResult<()> {
    if new_status.media_ids.is_empty() {
        return Ok(());
    }
    let attached = media_attachment::attach_to_status(
        &mut *conn,
        &new_status.media_ids,
        status_id,
        new_status.user_id,
        new_status.schedule_id,
    )
    .await?;
    let wanted = u64::try_from(new_status.media_ids.len()).unwrap_or(u64::MAX);
    if attached != wanted {
        return Err(AppError::Unprocessable(
            media_attachment::MEDIA_NOT_FOUND.to_owned(),
        ));
    }
    Ok(())
}

/// Reads the conversation the replied-to status belongs to, `None` when this
/// status answers nothing (or, before [`insert`] writes it and the foreign
/// key rejects the row, answers a status that is not stored here).
async fn parent_conversation(conn: &mut PgConnection, id: Option<Uuid>) -> AppResult<Option<Uuid>> {
    let Some(parent_id) = id else {
        return Ok(None);
    };
    let conversation = sqlx::query_as::<_, (Uuid,)>(PARENT_CONVERSATION_SQL)
        .bind(parent_id)
        .fetch_optional(conn)
        .await?;
    Ok(conversation.map(|row| row.0))
}

/// Writes the status row on `conn` and returns it, deriving the columns that
/// need the freshly minted id.
async fn write_status(
    conn: &mut PgConnection,
    base_url: &str,
    new_status: &NewStatus,
    parent: Option<Uuid>,
) -> AppResult<Status> {
    let id = new_id();
    let conversation = new_status.conversation_id.or(parent).unwrap_or(id);
    let status = sqlx::query_as::<_, Status>(INSERT_SQL)
        .bind(id)
        .bind(object_uri(base_url, new_status, id))
        .bind(notice_url(base_url, new_status, id))
        .bind(new_status.user_id)
        .bind(new_status.content.as_str())
        .bind(new_status.raw_text.as_str())
        .bind(new_status.content_type.as_str())
        .bind(new_status.spoiler_text.as_str())
        .bind(new_status.sensitive)
        .bind(new_status.visibility)
        .bind(new_status.language.as_deref())
        .bind(new_status.in_reply_to_id)
        .bind(new_status.in_reply_to_account_id)
        .bind(new_status.in_reply_to_ap_id.as_deref())
        .bind(conversation)
        .bind(new_status.direct_conversation_id)
        .bind(new_status.reblog_of_id)
        .bind(new_status.quote_of_id)
        .bind(new_status.local)
        .bind(new_status.application_name.as_deref())
        .bind(new_status.application_website.as_deref())
        .bind(new_status.edited_at)
        .bind(new_status.expires_at)
        .bind(new_status.ap_context.as_deref())
        .bind(new_status.inserted_at)
        .fetch_one(conn)
        .await?;
    Ok(status)
}

#[cfg(test)]
mod tests;
