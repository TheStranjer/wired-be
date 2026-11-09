//! The `statuses` table: posts, replies, reblogs and quotes.
//!
//! Per `docs/ARCHITECTURE.md` decision 6 statuses are relational
//! Mastodon-style rows rather than Pleroma's JSONB activities: Pleroma keeps
//! the same facts in `Pleroma.Activity` wrapping an `Object`
//! (`lib/pleroma/activity.ex`, `lib/pleroma/object.ex`), where a `Create`
//! becomes one row here, its actor becomes [`user_id`](Status::user_id), its
//! `published` timestamp becomes [`inserted_at`](Status::inserted_at), and an
//! `Announce` becomes a row with [`reblog_of_id`](Status::reblog_of_id) and no
//! text of its own.
//!
//! A local status owns the two URLs clients and peers know it by, derived
//! from its id: `{BASE_URL}/objects/{uuid}` in [`uri`](Status::uri) and
//! `{BASE_URL}/notice/{uuid}` in [`url`](Status::url). [`insert`] mints the id
//! (decision 4) and so is the only place those can be computed; it also
//! resolves [`conversation_id`](Status::conversation_id) — a reply inherits
//! its parent's conversation, a root status is its own conversation — and
//! moves the denormalised counters in the same transaction, so a status and
//! the counts that describe it are always written together. It also links
//! the accounts in [`NewStatus::mentioned_user_ids`] in that transaction
//! (see [`crate::models::mention`]), so a status and the mentions it
//! carries appear together or not at all, and the same for the tags in
//! [`NewStatus::hashtags`] (see [`crate::models::hashtag`]).
//!
//! Deletion is soft: [`soft_delete`] keeps the row as a tombstone with its
//! text cleared and its `uri` reserved (so a deleted remote post is never
//! re-imported) and steps the counters back down. Deleting through the API
//! takes the boosts with the post: [`soft_delete_with_reblogs`] tombstones the
//! status and every live reblog of it in one transaction. Everything that
//! reads by id hides tombstones; [`get_by_uri`] and
//! [`get_by_id_including_deleted`] do not, because federation and moderation
//! need to see them. A status created with `expires_in` carries that moment
//! in [`expires_at`](Status::expires_at), and the expiration is just the
//! delete arriving on a schedule: [`fetch_expired`] hands the
//! [`statuses.expire_sweep`](crate::jobs::statuses::KIND_SWEEP) safety job
//! the rows whose time has run out.
//!
//! Reblogs are the same table wearing the `Announce` role (decision 6):
//! [`create_reblog`] writes a live reblog row with no text of its own, the
//! original's conversation, and the activity uri `{BASE_URL}/activities/{id}`
//! rather than the plain status's `/objects/{id}`; the partial unique index
//! on `(user_id, reblog_of_id)` keeps one live reblog per pair, so a repeat
//! restates the row instead of writing a second one. [`remove_reblog`]
//! withdraws it — the same tombstone [`soft_delete`] writes, with the same
//! counter steps — and [`reblogged_status_ids`] and [`reblogger_page`] serve
//! the two reads: the batched `reblogged` flag of the status renderer and
//! the `reblogged_by` account listing.
//!
//! Counts move by whole steps only, through [`insert`], [`soft_delete`] and
//! [`bump_counter`], and are clamped at zero in SQL, so they can neither go
//! negative nor be lost to a concurrent write.

use std::collections::HashSet;
use std::{fmt, str::FromStr};

use chrono::{DateTime, Utc};
use sqlx::{PgConnection, PgPool, Postgres, QueryBuilder};
use uuid::Uuid;

use crate::{
    error::{AppError, AppResult},
    models::{hashtag, mention, user::User},
    pagination::PageParams,
    util::{entities::extract_hashtags, format::text_to_html, id::new_id},
};

/// The `text/plain` value of [`content_type`](NewStatus::content_type), which
/// is also the column default.
pub const DEFAULT_CONTENT_TYPE: &str = "text/plain";

/// Who may see a [`Status`], stored in the `visibility` text column as its
/// lowercase variant name (`public`, `unlisted`, `private`, `direct` or
/// `local`, which is exactly what the column's `CHECK` constraint allows).
///
/// The four Mastodon visibilities keep their Mastodon meaning;
/// [`Visibility::Local`] is Pleroma's extension for a post that is never
/// federated (`lib/pleroma/activity.ex`, the `visibility` field).
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, sqlx::Type)]
#[sqlx(type_name = "text", rename_all = "lowercase")]
pub enum Visibility {
    /// Addressed to everyone and shown in public timelines.
    Public,
    /// Addressed to everyone but left out of public timelines.
    Unlisted,
    /// Addressed to the author's followers only.
    Private,
    /// Addressed to the mentioned accounts only.
    Direct,
    /// Kept on this instance: addressed to everyone, delivered to nobody
    /// else (Pleroma's `local`).
    Local,
}

/// The message [`FromStr`] answers for text the column would not accept.
const UNKNOWN_VISIBILITY: &str = "unknown visibility";

impl Visibility {
    /// All five visibilities, in the order the column's `CHECK` constraint
    /// lists them.
    pub const ALL: [Self; 5] = [
        Self::Public,
        Self::Unlisted,
        Self::Private,
        Self::Direct,
        Self::Local,
    ];

    /// The stored (and wire) text of this visibility: its lowercase name,
    /// which is what the `visibility` column holds and what Mastodon's JSON
    /// reports.
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Public => "public",
            Self::Unlisted => "unlisted",
            Self::Private => "private",
            Self::Direct => "direct",
            Self::Local => "local",
        }
    }
}

/// [`as_str`](Self::as_str) is the text form, so `format!("{visibility}")`
/// and the database agree.
impl fmt::Display for Visibility {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

/// Parses the stored text back, case-sensitively: the column only ever holds
/// these five spellings and clients send them verbatim.
impl FromStr for Visibility {
    type Err = &'static str;

    fn from_str(text: &str) -> Result<Self, Self::Err> {
        match text {
            "public" => Ok(Self::Public),
            "unlisted" => Ok(Self::Unlisted),
            "private" => Ok(Self::Private),
            "direct" => Ok(Self::Direct),
            "local" => Ok(Self::Local),
            _ => Err(UNKNOWN_VISIBILITY),
        }
    }
}

/// One denormalised counter of a [`Status`], for [`bump_counter`].
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Counter {
    /// [`replies_count`](Status::replies_count): live answers to this status.
    Replies,
    /// [`reblogs_count`](Status::reblogs_count): statuses reblogging it.
    Reblogs,
    /// [`favourites_count`](Status::favourites_count): favourites of it.
    Favourites,
    /// [`quotes_count`](Status::quotes_count): statuses quoting it.
    Quotes,
}

/// One row of `statuses`: one post, in any of the shapes decision 6 allows
/// (root, reply, reblog, quote).
#[derive(Clone, Debug, PartialEq, Eq, sqlx::FromRow)]
pub struct Status {
    /// `UUIDv7` primary key, minted by [`insert`] (decision 4).
    pub id: Uuid,
    /// `ActivityPub` object id: `{BASE_URL}/objects/{uuid}` for a local
    /// status, the remote's own id for a mirrored one. Unique, and it stays
    /// reserved across a soft delete.
    pub uri: String,
    /// Human-facing URL of the post (`{BASE_URL}/notice/{uuid}` locally);
    /// `None` when the origin named none.
    pub url: Option<String>,
    /// Author.
    pub user_id: Uuid,
    /// The post as sanitized HTML; emptied by [`soft_delete`].
    pub content: String,
    /// The post as its source text, before formatting.
    pub raw_text: String,
    /// `MIME` type [`raw_text`](Self::raw_text) was written in.
    pub content_type: String,
    /// Content warning subject line; empty means there is none.
    pub spoiler_text: String,
    /// Whether [`spoiler_text`](Self::spoiler_text) hides
    /// [`content`](Self::content) until the reader opens it.
    pub sensitive: bool,
    /// Who may see this post.
    pub visibility: Visibility,
    /// BCP 47 language tag of the content, when known.
    pub language: Option<String>,
    /// Status this one answers; `None` for a root status.
    pub in_reply_to_id: Option<Uuid>,
    /// Author of the answered status, when this instance knows them.
    pub in_reply_to_account_id: Option<Uuid>,
    /// `ActivityPub` id of the answered status while it is not stored yet.
    pub in_reply_to_ap_id: Option<String>,
    /// Thread group id: every status of one conversation shares it, and a
    /// root status's is its own id.
    pub conversation_id: Uuid,
    /// Original this status reblogs; `None` for anything but a reblog.
    pub reblog_of_id: Option<Uuid>,
    /// Original this status quotes; `None` for anything but a quote.
    pub quote_of_id: Option<Uuid>,
    /// Whether this instance owns the post.
    pub local: bool,
    /// Name of the client that posted this status, if it reported one.
    pub application_name: Option<String>,
    /// Website of that client, if it reported one.
    pub application_website: Option<String>,
    /// Live answers to this post; moved by [`insert`], [`soft_delete`] and
    /// [`bump_counter`].
    pub replies_count: i32,
    /// Reblogs of this post; moved by the same three.
    pub reblogs_count: i32,
    /// Favourites of this post; moved by [`bump_counter`].
    pub favourites_count: i32,
    /// Quotes of this post; moved by [`bump_counter`].
    pub quotes_count: i32,
    /// When the content last changed; `None` until an edit.
    pub edited_at: Option<DateTime<Utc>>,
    /// When this post was deleted; `None` while it is live.
    pub deleted_at: Option<DateTime<Utc>>,
    /// When this post is due to disappear; `None` means it never does.
    pub expires_at: Option<DateTime<Utc>>,
    /// Creation time: for a remote status, the `published` timestamp it
    /// arrived with.
    pub inserted_at: DateTime<Utc>,
    /// When the row last changed; refreshed by every update here.
    pub updated_at: DateTime<Utc>,
}

/// One entry of [`reblogger_page`]: the [`User`] behind a reblog row, led
/// by that row's id.
///
/// The id is what the endpoint's keyset cursors and `Link` header carry —
/// the reblog row's id, not the reblogger's — exactly as
/// [`crate::models::favourite`] hands the like id over next to the account.
#[derive(Debug, sqlx::FromRow)]
pub struct ReblogPageEntry {
    /// Primary key of the reblog [`Status`] row behind this entry.
    pub reblog_id: Uuid,
    /// The account that reblogged.
    #[sqlx(flatten)]
    pub user: User,
}

/// What to insert: every `statuses` column a caller can decide up front.
///
/// What is left out is what the model owns instead: the `id` [`insert`]
/// mints, the `uri`, `url` and `conversation_id` it derives from that id, the
/// `deleted_at` [`soft_delete`] sets, and the four counters those three
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
            reblog_of_id: None,
            quote_of_id: None,
            mentioned_user_ids: Vec::new(),
            hashtags: Vec::new(),
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
     reblog_of_id, quote_of_id, local, application_name,
     application_website, edited_at, expires_at, inserted_at
) VALUES (
     $1, $2, $3, $4, $5, $6, $7, $8, $9, $10, $11, $12, $13, $14, $15, $16,
     $17, $18, $19, $20, $21, $22, COALESCE($23, now())
) RETURNING *";

/// The conversation the status being replied to belongs to, read from the
/// parent before the reply is written.
const PARENT_CONVERSATION_SQL: &str = "SELECT conversation_id FROM statuses WHERE id = $1";

/// The author's denormalised post count and newest-post time, moved by one
/// status: `last_status_at` only ever moves forward, so importing an old
/// remote post leaves a newer one on the books.
const AUTHOR_COUNT_SQL: &str = "UPDATE users
      SET statuses_count = GREATEST(0, statuses_count + $2),
          last_status_at = GREATEST(last_status_at, $3),
          updated_at = now()
    WHERE id = $1";

/// The `UPDATE` behind [`bump_counter`] for [`Counter::Replies`]; one literal
/// per counter, so no statement is ever assembled from data.
const STEP_REPLIES_SQL: &str = "UPDATE statuses
      SET replies_count = GREATEST(0, replies_count + $2), updated_at = now()
    WHERE id = $1";

/// The same write for [`Counter::Reblogs`].
const STEP_REBLOGS_SQL: &str = "UPDATE statuses
      SET reblogs_count = GREATEST(0, reblogs_count + $2), updated_at = now()
    WHERE id = $1";

/// The same write for [`Counter::Favourites`].
const STEP_FAVOURITES_SQL: &str = "UPDATE statuses
      SET favourites_count = GREATEST(0, favourites_count + $2), updated_at = now()
    WHERE id = $1";

/// The same write for [`Counter::Quotes`].
const STEP_QUOTES_SQL: &str = "UPDATE statuses
      SET quotes_count = GREATEST(0, quotes_count + $2), updated_at = now()
    WHERE id = $1";

/// [`get_by_id`]: the live row behind a primary key, tombstones excluded.
const LIVE_BY_ID_SQL: &str = "SELECT * FROM statuses WHERE id = $1 AND deleted_at IS NULL";

/// [`get_many`]: the live rows of `ids`, oldest id first (decision 4).
const MANY_SQL: &str =
    "SELECT * FROM statuses WHERE id = ANY($1) AND deleted_at IS NULL ORDER BY id";

/// [`newest_public_posts`]: one author's live `public` root posts (a reply
/// or a reblog is neither), newest id first (decision 4), optionally a
/// keyset page older than the cursor. `$2` is bound `NULL` on a first page,
/// which keeps every row.
const NEWEST_PUBLIC_POSTS_SQL: &str = "SELECT * FROM statuses
   WHERE user_id = $1
     AND visibility = 'public'
     AND deleted_at IS NULL
     AND in_reply_to_id IS NULL
     AND reblog_of_id IS NULL
     AND ($2::uuid IS NULL OR id < $2)
ORDER BY id DESC
    LIMIT $3";

/// [`ancestor_chain`]: the walk *up* from `$1` along `in_reply_to_id`, one
/// hop per row, stopped at depth `$2`. `$3` is again the starting id, which
/// the answer must not carry; the base row sits at depth `0` so a cap of
/// `1` still admits the direct parent. `ORDER BY chain.depth DESC` puts the
/// farthest link — the thread's root — first: a parent was stored before
/// its reply could name it, so id order is creation order (decision 4) and
/// walking up walks back in time. The walk is raw on purpose: a tombstone
/// mid-chain is a link the walk must pass through and keep, and whether any
/// row is *viewable* is the caller's [`filter_visible`] step.
///
/// [`filter_visible`]: crate::models::status_visibility::filter_visible
const ANCESTOR_CHAIN_SQL: &str = "WITH RECURSIVE chain(id, depth) AS (
    SELECT $1::uuid, 0
  UNION ALL
    SELECT s.in_reply_to_id, chain.depth + 1
      FROM chain
      JOIN statuses s ON s.id = chain.id
     WHERE s.in_reply_to_id IS NOT NULL
       AND chain.depth < $2
)
SELECT s.* FROM chain
  JOIN statuses s ON s.id = chain.id
 WHERE chain.id <> $3
 ORDER BY chain.depth DESC";

/// [`descendant_tree`]: the walk *down* from `$1` along replies, one hop per
/// row, stopped at depth `$2` and cut to `$3` rows. A reply was stored after
/// its parent, so `ORDER BY s.id` is the flat chronological thread order
/// (decision 4) Pleroma's context read answers. The base arm is the direct
/// replies at depth `1`, kept only when the cap admits at least one hop;
/// the cut runs before any visibility narrowing, so the caller's filters
/// can only shrink what `$3` kept. As in [`ancestor_chain`], tombstones and
/// unreadable rows stay in the walk — they are links below them the walk
/// must pass — and are the caller's to drop.
const DESCENDANT_TREE_SQL: &str = "WITH RECURSIVE tree(id, depth) AS (
    SELECT s.id, 1
      FROM statuses s
     WHERE s.in_reply_to_id = $1
       AND $2 >= 1
  UNION ALL
    SELECT c.id, tree.depth + 1
      FROM tree
      JOIN statuses c ON c.in_reply_to_id = tree.id
     WHERE tree.depth < $2
)
SELECT s.* FROM tree
  JOIN statuses s ON s.id = tree.id
 ORDER BY s.id
    LIMIT $3";

/// [`get_by_id_including_deleted`]: one row, tombstone or not.
const ANY_BY_ID_SQL: &str = "SELECT * FROM statuses WHERE id = $1";

/// [`get_by_uri`]: one row by its `ActivityPub` id, tombstone or not —
/// federation dedupes on this, and a deleted post's `uri` stays reserved.
const BY_URI_SQL: &str = "SELECT * FROM statuses WHERE uri = $1";

/// The `INSERT` behind [`create_reblog`]: a reblog row is decision 6's
/// `Announce` wearing status clothing — no text of its own (the `content`,
/// `raw_text`, `content_type` and `spoiler_text` column defaults empty it),
/// the original's conversation, `local` set, and the `{BASE_URL}/activities/
/// {id}` uri rather than a plain status's `/objects/{id}`. It rides the
/// partial unique index on `(user_id, reblog_of_id)`: a repeat reblog
/// inserts nothing, and `RETURNING *` answers exactly the insert that
/// happened — which is what tells the caller a counter step is owed.
const INSERT_REBLOG_SQL: &str = "INSERT INTO statuses (
     id, uri, url, user_id, visibility, conversation_id, reblog_of_id, local
) VALUES (
     $1, $2, $3, $4, $5, $6, $7, true
) ON CONFLICT (user_id, reblog_of_id) WHERE deleted_at IS NULL DO NOTHING
 RETURNING *";

/// [`create_reblog`]'s restatement arm: the caller's live reblog of one
/// original — at most one row, because the partial unique index keeps it
/// so, and a withdrawn (tombstoned) reblog is no row at all.
const LIVE_REBLOG_SQL: &str = "SELECT * FROM statuses
      WHERE user_id = $1 AND reblog_of_id = $2 AND deleted_at IS NULL";

/// [`remove_reblog`]: the same tombstone [`soft_delete`] writes, keyed on
/// the live `(user, original)` pair instead of the row id so the withdrawal
/// is one statement. `RETURNING *` hands the caller the row that went,
/// which is what the write hook reports and the counter steps run on; a
/// pair that never reblogged (or already withdrew) matches nothing, which
/// is what makes the write idempotent.
const TOMBSTONE_REBLOG_SQL: &str = "UPDATE statuses
      SET content = '',
          raw_text = '',
          spoiler_text = '',
          deleted_at = now(),
          updated_at = now()
     WHERE user_id = $1 AND reblog_of_id = $2 AND deleted_at IS NULL
   RETURNING *";

/// [`reblogged_status_ids`]: which of a batch of originals the viewer has
/// a live reblog of. Only the ids that *have* a row come back; the caller's
/// flag is membership in that set.
const REBLOGGED_IDS_SQL: &str = "SELECT reblog_of_id FROM statuses
      WHERE user_id = $1 AND reblog_of_id = ANY($2) AND deleted_at IS NULL";

/// [`reblogger_page`]: the accounts that reblogged one original, each
/// entry led by the id of the reblog row behind it — the id the page keys
/// on. The text stops where its one placeholder belongs, because that is
/// where [`reblogger_page`] pushes its bind; the live-row filter and the
/// keyset then run on the reblog row id `s.id`.
const REBLOGGERS_PAGE_SQL: &str = "SELECT s.id AS reblog_id, u.* FROM statuses s
     JOIN users u ON u.id = s.user_id
      WHERE s.reblog_of_id = ";

/// [`soft_delete`]: a live row becomes a tombstone once, which is what makes
/// the counter steps of the same transaction run exactly once per status.
const SOFT_DELETE_SQL: &str = "UPDATE statuses
      SET content = '',
          raw_text = '',
          spoiler_text = '',
          deleted_at = now(),
          updated_at = now()
    WHERE id = $1 AND deleted_at IS NULL
  RETURNING *";

/// [`soft_delete_with_reblogs`]: the same tombstone [`soft_delete`] writes,
/// for every live reblog of `id` at once. `RETURNING *` brings those rows back
/// so their own counters can be stepped down in the same transaction.
const SOFT_DELETE_REBLOGS_SQL: &str = "UPDATE statuses
      SET content = '',
          raw_text = '',
          spoiler_text = '',
          deleted_at = now(),
          updated_at = now()
    WHERE reblog_of_id = $1 AND deleted_at IS NULL
  RETURNING *";

/// [`soft_delete`] and [`soft_delete_with_reblogs`]: every account's pin of a
/// status that has just become a tombstone goes with it. A pin is a promise
/// that a profile can show the post, and a tombstone shows nowhere, so the
/// row would name something no listing answers; this is the soft delete's
/// share of what the foreign key's cascade does for a hard one (see
/// [`crate::models::pinned_status`]).
const CLEAR_PINS_SQL: &str = "DELETE FROM pinned_statuses WHERE status_id = $1";

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
/// the parent's `replies_count` goes up for a reply and the original's
/// `reblogs_count` for a reblog — exactly what [`soft_delete`] steps back
/// down — and every account in [`mentioned_user_ids`](NewStatus::mentioned_user_ids)
/// is linked to the new row (see [`crate::models::mention`]) and every
/// usable tag in [`hashtags`](NewStatus::hashtags) is upserted and linked
/// (see [`crate::models::hashtag`]), so a status and its mentions and tags
/// appear together or not at all.
///
/// # Errors
///
/// Any database failure (connection, constraint, encoding) arrives as
/// [`AppError::Database`](crate::error::AppError): a missing `uri` for a
/// remote status, a duplicate `uri`, an unknown author, parent or original,
/// or an unknown mentioned account arrives as the not-null, unique or
/// foreign-key violation the database reports — and the rollback takes the
/// half-written status with it.
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
    mention::link_conn(conn, status.id, &new_status.mentioned_user_ids).await?;
    hashtag::link_names_conn(conn, status.id, &new_status.hashtags).await?;
    count_new_post(conn, &status).await?;
    Ok(status)
}

/// Fetches the live status whose primary key is `id`, `None` when no row
/// matches or when the row is a tombstone.
///
/// # Errors
///
/// Any database failure (connection, encoding) arrives as
/// [`AppError::Database`](crate::error::AppError).
pub async fn get_by_id(pool: &PgPool, id: Uuid) -> AppResult<Option<Status>> {
    let status = sqlx::query_as::<_, Status>(LIVE_BY_ID_SQL)
        .bind(id)
        .fetch_optional(pool)
        .await?;
    Ok(status)
}

/// Fetches the status whose primary key is `id`, tombstone included: the
/// moderation and federation view of a deleted post.
///
/// # Errors
///
/// Any database failure (connection, encoding) arrives as
/// [`AppError::Database`](crate::error::AppError).
pub async fn get_by_id_including_deleted(pool: &PgPool, id: Uuid) -> AppResult<Option<Status>> {
    let status = sqlx::query_as::<_, Status>(ANY_BY_ID_SQL)
        .bind(id)
        .fetch_optional(pool)
        .await?;
    Ok(status)
}

/// Fetches the status whose `ActivityPub` object id is `uri`, tombstone
/// included: `uri` is unique and stays reserved when a status is deleted, so
/// this is how federation recognises a post it has already seen.
///
/// # Errors
///
/// Any database failure (connection, encoding) arrives as
/// [`AppError::Database`](crate::error::AppError).
pub async fn get_by_uri(pool: &PgPool, uri: &str) -> AppResult<Option<Status>> {
    let status = sqlx::query_as::<_, Status>(BY_URI_SQL)
        .bind(uri)
        .fetch_optional(pool)
        .await?;
    Ok(status)
}

/// Fetches the live statuses whose ids are in `ids`, in `id` order (`UUIDv7`
/// text sorts by creation, decision 4). Ids with no live row — absent or
/// deleted — are skipped; an empty `ids` yields an empty `Vec`.
///
/// # Errors
///
/// Any database failure (connection, encoding) arrives as
/// [`AppError::Database`](crate::error::AppError).
pub async fn get_many(pool: &PgPool, ids: &[Uuid]) -> AppResult<Vec<Status>> {
    let ids = ids.to_vec();
    let statuses = sqlx::query_as::<_, Status>(MANY_SQL)
        .bind(&ids)
        .fetch_all(pool)
        .await?;
    Ok(statuses)
}

/// The newest `limit` live, `public`, root posts of author `user_id`,
/// newest first (decision 4): the page the account's `Atom`/`RSS` feeds
/// ([`crate::api::feeds`]) enumerate.
///
/// A root post is neither a reply nor a reblog — the feed carries what the
/// account itself said, not its side of a conversation or its boosts — and
/// `public` is the only visibility that is (an `unlisted` post is public to
/// timelines but not advertised, `private`, `direct` and `local` are not
/// public at all). `max_id`, when given, is the keyset cursor: only posts
/// strictly older than it are returned.
///
/// # Errors
///
/// Any database failure (connection, encoding) arrives as
/// [`AppError::Database`](crate::error::AppError).
pub async fn newest_public_posts(
    pool: &PgPool,
    user_id: Uuid,
    max_id: Option<Uuid>,
    limit: i64,
) -> AppResult<Vec<Status>> {
    let statuses = sqlx::query_as::<_, Status>(NEWEST_PUBLIC_POSTS_SQL)
        .bind(user_id)
        .bind(max_id)
        .bind(limit)
        .fetch_all(pool)
        .await?;
    Ok(statuses)
}

/// Every status *above* `status_id`: the walk up its
/// [`in_reply_to_id`](Status::in_reply_to_id) chain to the thread's root,
/// oldest first (root first), `status_id` itself excluded and the walk cut
/// at `max_depth` hops.
///
/// The answer is the raw chain, not a visible page: tombstones and rows no
/// viewer may read are kept (they are links the walk passes *through*, and
/// their own visibility is decided by the caller's
/// [`filter_visible`](crate::models::status_visibility::filter_visible)
/// step), and no parent of the chain is dropped when a row beside it falls
/// out. A status that replies to nothing answers the empty `Vec`, as does
/// one whose parent link dangles at its first hop.
///
/// # Errors
///
/// Any database failure (connection, encoding) arrives as
/// [`AppError::Database`](crate::error::AppError).
pub async fn ancestor_chain(
    pool: &PgPool,
    status_id: Uuid,
    max_depth: i32,
) -> AppResult<Vec<Status>> {
    let statuses = sqlx::query_as::<_, Status>(ANCESTOR_CHAIN_SQL)
        .bind(status_id)
        .bind(max_depth)
        .bind(status_id)
        .fetch_all(pool)
        .await?;
    Ok(statuses)
}

/// Every status *below* `status_id`, across all branches, in flat
/// chronological thread order (id ascending, decision 4).
///
/// The walk descends through every reply of `status_id`, of every reply of
/// those, and so on — the order Pleroma's context read answers. `status_id`
/// itself is never in the answer; a status nobody replied to is the empty
/// `Vec`.
///
/// The walk is cut at `max_depth` hops (at least one for any row to come
/// back) and to `max_rows` rows before the caller has said which of them
/// are visible, so a caller that narrows afterwards may answer fewer than
/// `max_rows`.
///
/// The raw-read contract is [`ancestor_chain`]'s: tombstones and unreadable
/// rows stay in the walk so it continues past them, and the caller drops
/// them with `filter_visible`.
///
/// # Errors
///
/// Any database failure (connection, encoding) arrives as
/// [`AppError::Database`](crate::error::AppError).
pub async fn descendant_tree(
    pool: &PgPool,
    status_id: Uuid,
    max_depth: i32,
    max_rows: i64,
) -> AppResult<Vec<Status>> {
    let statuses = sqlx::query_as::<_, Status>(DESCENDANT_TREE_SQL)
        .bind(status_id)
        .bind(max_depth)
        .bind(max_rows)
        .fetch_all(pool)
        .await?;
    Ok(statuses)
}

/// Soft-deletes status `id`: the row stays, as a tombstone.
///
/// Its text is cleared and its `uri` reserved, the counters it fed step
/// back down in the same transaction — its author's `statuses_count`, its
/// parent's `replies_count` and, for a reblog, the original's
/// `reblogs_count` — and every account's pin of it is removed there too,
/// the soft twin of the cascade a hard delete rides (see
/// [`crate::models::pinned_status`]).
///
/// Idempotent: a row that is already a tombstone, and an unknown id, are both
/// answered `None` and move nothing.
///
/// # Errors
///
/// Any database failure (connection, encoding) arrives as
/// [`AppError::Database`](crate::error::AppError).
pub async fn soft_delete(pool: &PgPool, id: Uuid) -> AppResult<Option<Status>> {
    let mut tx = pool.begin().await?;
    let Some(tombstone) = mark_deleted(&mut tx, id).await? else {
        return Ok(None);
    };
    uncount_deleted(&mut tx, &tombstone).await?;
    clear_pins(&mut tx, id).await?;
    tx.commit().await?;
    Ok(Some(tombstone))
}

/// Soft-deletes status `id` **and every reblog of it** in one transaction,
/// answering `id`'s own tombstone.
///
/// A post that is gone takes its boosts with it, so no live row is left
/// pointing at a tombstone: every reblog of `id` is tombstoned exactly as
/// [`soft_delete`] would tombstone it, and each one's counters step down in
/// the same transaction — its author's `statuses_count`, and above all
/// [`reblogs_count`](Status::reblogs_count) of the status being deleted, which
/// lands at zero.
///
/// Idempotent in the same shape as [`soft_delete`]: an id that is already a
/// tombstone, and an unknown id, are answered `None` and move nothing —
/// reblogs included, since an already deleted original has nothing left to
/// take down.
///
/// The media attachments of the deleted rows are left alone: they keep their
/// `status_id` and stay in the table, referenced by no live status, until the
/// cleanup job that removes them exists.
///
/// # Errors
///
/// Any database failure (connection, encoding) arrives as
/// [`AppError::Database`](crate::error::AppError), and the rollback takes the
/// half-written delete with it: a status and its reblogs are tombstoned
/// together or not at all.
pub async fn soft_delete_with_reblogs(pool: &PgPool, id: Uuid) -> AppResult<Option<Status>> {
    let mut tx = pool.begin().await?;
    let Some(tombstone) = mark_deleted(&mut tx, id).await? else {
        return Ok(None);
    };
    uncount_deleted(&mut tx, &tombstone).await?;
    clear_pins(&mut tx, id).await?;
    for reblog in tombstone_reblogs(&mut tx, id).await? {
        uncount_deleted(&mut tx, &reblog).await?;
        clear_pins(&mut tx, reblog.id).await?;
    }
    tx.commit().await?;
    Ok(Some(tombstone))
}

/// How many rows one pass of [`soft_delete_all_for_user`] tombstones: the
/// account-deletion job's batch size, small enough to keep every statement
/// short and large enough that most accounts tombstone in one pass.
const SOFT_DELETE_BATCH: i64 = 100;

/// The `UPDATE` behind [`soft_delete_all_for_user`]: the tombstone
/// [`soft_delete`] writes, applied to at most [`SOFT_DELETE_BATCH`] of the
/// account's live rows per pass, oldest id first.
const SOFT_DELETE_BATCH_FOR_USER_SQL: &str = "UPDATE statuses
       SET content = '',
           raw_text = '',
           spoiler_text = '',
           deleted_at = now(),
           updated_at = now()
     WHERE id IN (
         SELECT id FROM statuses
          WHERE user_id = $1 AND deleted_at IS NULL
          ORDER BY id
          LIMIT $2
     )";

/// Soft-deletes every status authored by user `user_id`: the status step of
/// the [`users.delete`](crate::jobs::deletion::KIND_DELETE) job.
///
/// `SOFT_DELETE_BATCH` rows per statement, repeated until a pass
/// tombstones nothing — the loop ends however many posts the account left,
/// and a rerun over an already tombstoned set is a no-op, so the job can
/// retry the step safely. Returns the total number of rows the passes
/// tombstoned.
///
/// What the per-status [`soft_delete`] does around the tombstone — counter
/// steps and pin clearing — is deliberately left out: the author's row is
/// purged and deactivated by the same job, so its denormalised counters
/// describe nothing anyone will read, and a pin of a tombstone renders
/// nowhere, exactly like the tombstone itself.
///
/// # Errors
///
/// Any database failure (connection, encoding) arrives as
/// [`AppError::Database`](crate::error::AppError).
pub async fn soft_delete_all_for_user(pool: &PgPool, user_id: Uuid) -> AppResult<u64> {
    let mut total = 0_u64;
    loop {
        let tombstoned = sqlx::query(SOFT_DELETE_BATCH_FOR_USER_SQL)
            .bind(user_id)
            .bind(SOFT_DELETE_BATCH)
            .execute(pool)
            .await?
            .rows_affected();
        if tombstoned == 0 {
            return Ok(total);
        }
        total += tombstoned;
    }
}

/// The `SELECT` behind [`fetch_expired`]: live statuses whose expiration has
/// run out, soonest-expiring first, at most `limit` of them.
const EXPIRED_SQL: &str = "SELECT * FROM statuses
     WHERE deleted_at IS NULL
       AND expires_at IS NOT NULL
       AND expires_at <= now()
     ORDER BY expires_at, id
     LIMIT $1";

/// Fetches at most `limit` live statuses whose
/// [`expires_at`](Status::expires_at) has run out, soonest expiry first.
///
/// This is the batch read of the
/// [`statuses.expire_sweep`](crate::jobs::statuses::KIND_SWEEP) safety
/// job. A tombstoned row no longer matches the predicate and a
/// scheduled delete clears the row's own expiry from the queue's reach, so
/// repeated batches of one call drain the backlog in order without ever
/// handing the same row out twice.
///
/// # Errors
///
/// Any database failure (connection, encoding) arrives as
/// [`AppError::Database`](crate::error::AppError).
pub async fn fetch_expired(pool: &PgPool, limit: i64) -> AppResult<Vec<Status>> {
    let rows = sqlx::query_as::<_, Status>(EXPIRED_SQL)
        .bind(limit)
        .fetch_all(pool)
        .await?;
    Ok(rows)
}

/// Adds `delta` to `counter` of status `id`, clamped at zero, and refreshes
/// its `updated_at`. An unknown id matches no row and changes nothing.
///
/// [`insert`] and [`soft_delete`] keep `replies_count` and `reblogs_count` in
/// step with the rows they write; this is how everything else that counts
/// against a status moves its number (favourites, quotes) without reading the
/// row before writing it.
///
/// # Errors
///
/// Any database failure (connection, encoding) arrives as
/// [`AppError::Database`](crate::error::AppError).
pub async fn bump_counter(pool: &PgPool, id: Uuid, counter: Counter, delta: i32) -> AppResult<()> {
    let mut conn = pool.acquire().await?;
    step_counter(&mut conn, id, counter, delta).await
}

/// Records `user`'s reblog of `original` at `visibility` and answers the
/// reblog row with a flag saying whether this call wrote it.
///
/// The row is what decision 6 stores an `Announce` as: empty content, the
/// requested `visibility`, `local` set, the original's
/// [`conversation_id`](Status::conversation_id), and the
/// `{base_url}/activities/{uuid}` uri (the row *is* the activity; a plain
/// local status owns `/objects/{uuid}` instead) with the usual
/// `{base_url}/notice/{uuid}` human url. `base_url` is the instance origin,
/// i.e. [`Config::base_url`](crate::config::Config::base_url), which the
/// model layer is handed rather than reading configuration itself.
///
/// A user reblogs a given status once: the write rides the partial unique
/// index on `(user_id, reblog_of_id)` (`WHERE deleted_at IS NULL`), so a
/// repeat after a live row exists — or after a withdrawal if the pair is
/// reblogged again in between — inserts nothing, steps nothing, and answers
/// the row that is already there with `false`. Only the insert that
/// happened moves the counters, in the same transaction as the row: the
/// original's `reblogs_count` and the reblogger's `statuses_count` (and
/// `last_status_at`, which the author-count step only ever moves forward),
/// exactly as [`insert`] does for the reblog rows federation imports.
///
/// # Errors
///
/// Any database failure (connection, constraint, encoding) arrives as
/// [`AppError::Database`](crate::error::AppError). [`AppError::NotFound`]
/// is the one row this write cannot answer: a reblog some concurrent
/// withdrawal tombstoned between the conflict and the restatement read.
pub async fn create_reblog(
    pool: &PgPool,
    base_url: &str,
    user: &User,
    original: &Status,
    visibility: Visibility,
) -> AppResult<(Status, bool)> {
    let mut tx = pool.begin().await?;
    let id = new_id();
    let written = sqlx::query_as::<_, Status>(INSERT_REBLOG_SQL)
        .bind(id)
        .bind(format!("{base_url}/activities/{id}"))
        .bind(format!("{base_url}/notice/{id}"))
        .bind(user.id)
        .bind(visibility)
        .bind(original.conversation_id)
        .bind(original.id)
        .fetch_optional(&mut *tx)
        .await?;
    let reblog = match written {
        Some(row) => {
            count_new_post(&mut tx, &row).await?;
            (row, true)
        }
        None => (live_reblog(&mut tx, user.id, original.id).await?, false),
    };
    tx.commit().await?;
    Ok(reblog)
}

/// Withdraws `user_id`'s reblog of `original_id`, answering the tombstone
/// of the reblog row, or `None` when the pair has no live reblog to take.
///
/// The withdrawal is [`soft_delete`]'s move — the row stays as a tombstone
/// with its text cleared and its `uri` reserved — keyed on the
/// `(user_id, reblog_of_id)` pair so the caller never names the row, and
/// idempotent for the same reason: a pair that never reblogged (or already
/// withdrew) matches no live row, steps nothing and is not an error. The
/// counters step back in the same transaction the tombstone goes into: the
/// reblogger's `statuses_count` and the original's `reblogs_count` (through
/// the same uncount step `soft_delete` runs, which reads both off the row),
/// and any pin of the reblog row goes with it as it would on a delete.
///
/// # Errors
///
/// Any database failure (connection, encoding) arrives as
/// [`AppError::Database`](crate::error::AppError), and the rollback leaves
/// the pair exactly as it was: a reblog and its counter steps go together
/// or not at all.
pub async fn remove_reblog(
    pool: &PgPool,
    user_id: Uuid,
    original_id: Uuid,
) -> AppResult<Option<Status>> {
    let mut tx = pool.begin().await?;
    let Some(tombstone) = sqlx::query_as::<_, Status>(TOMBSTONE_REBLOG_SQL)
        .bind(user_id)
        .bind(original_id)
        .fetch_optional(&mut *tx)
        .await?
    else {
        return Ok(None);
    };
    uncount_deleted(&mut tx, &tombstone).await?;
    clear_pins(&mut tx, tombstone.id).await?;
    tx.commit().await?;
    Ok(Some(tombstone))
}

/// Which of `original_ids` `user_id` has a live reblog of — the batched
/// `reblogged` read the status renderer runs once per page
/// (`docs/ARCHITECTURE.md` decision 13).
///
/// The answer is the membership set of *original* ids, not of reblog row
/// ids: a caller keys a row's flag to the status the reblog lands on, which
/// for a reblog row on the page is its own original (the same rule the
/// `favourited` flag follows). An id with no live reblog row is simply
/// absent, and an empty `original_ids` asks the database nothing at all.
///
/// # Errors
///
/// Any database failure (connection, encoding) arrives as
/// [`AppError::Database`](crate::error::AppError).
pub async fn reblogged_status_ids(
    pool: &PgPool,
    user_id: Uuid,
    original_ids: &[Uuid],
) -> AppResult<HashSet<Uuid>> {
    if original_ids.is_empty() {
        return Ok(HashSet::new());
    }
    let rows = sqlx::query_as::<_, (Uuid,)>(REBLOGGED_IDS_SQL)
        .bind(user_id)
        .bind(original_ids)
        .fetch_all(pool)
        .await?;
    Ok(rows.into_iter().map(|(id,)| id).collect())
}

/// The accounts that reblogged `original_id`, newest reblog first and
/// `page`'s `limit` at a time, each entry carrying the id of the reblog row
/// behind the account (see [`ReblogPageEntry`]).
///
/// This is the read behind `GET /api/v1/statuses/{id}/reblogged_by`
/// (Pleroma's `reblogged_by`, which walks the original object's
/// `announcements` list): the page keys on the *reblog row* id, so the
/// endpoint's `max_id`/`min_id`/`since_id` cursors and its `Link` header
/// carry reblog row ids, not account ids. A tombstoned reblog is off the
/// page — a withdrawn boost names no reblogger. Who may see the list is the
/// endpoint's gate (the shared status visibility check), not this query's.
///
/// # Errors
///
/// Any database failure (connection, encoding) arrives as
/// [`AppError::Database`](crate::error::AppError).
pub async fn reblogger_page(
    pool: &PgPool,
    original_id: Uuid,
    page: PageParams,
) -> AppResult<Vec<ReblogPageEntry>> {
    let mut builder = QueryBuilder::<Postgres>::new(REBLOGGERS_PAGE_SQL);
    // The builder's push methods hand back `&mut Self` for chaining; binding
    // it keeps the statement from having an unused result (decision 14).
    let _bound: &mut _ = builder.push_bind(original_id);
    let _live: &mut _ = builder.push(" AND s.deleted_at IS NULL");
    page.apply_keyset(&mut builder, "s.id");
    let entries = builder
        .build_query_as::<ReblogPageEntry>()
        .fetch_all(pool)
        .await?;
    Ok(page.finish(entries))
}

/// Reads the restatement arm of [`create_reblog`]: the caller's live reblog
/// row of `original_id`, which the conflict says exists — unless a
/// concurrent withdrawal tombstoned it in the same window, which is this
/// read's only way to answer `None`.
async fn live_reblog(
    conn: &mut PgConnection,
    user_id: Uuid,
    original_id: Uuid,
) -> AppResult<Status> {
    let existing = sqlx::query_as::<_, Status>(LIVE_REBLOG_SQL)
        .bind(user_id)
        .bind(original_id)
        .fetch_optional(conn)
        .await?
        .ok_or(AppError::NotFound)?;
    Ok(existing)
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
        .bind(new_status.reblog_of_id)
        .bind(new_status.quote_of_id)
        .bind(new_status.local)
        .bind(new_status.application_name.as_deref())
        .bind(new_status.application_website.as_deref())
        .bind(new_status.edited_at)
        .bind(new_status.expires_at)
        .bind(new_status.inserted_at)
        .fetch_one(conn)
        .await?;
    Ok(status)
}

/// The `ActivityPub` object id to store: this instance's
/// `{base_url}/objects/{id}` for a local status (decision 6), else the remote
/// id the caller supplied — `None` for a remote status with no id, which the
/// `NOT NULL` column then rejects.
fn object_uri(base_url: &str, new_status: &NewStatus, id: Uuid) -> Option<String> {
    new_status
        .local
        .then(|| format!("{base_url}/objects/{id}"))
        .or_else(|| new_status.uri.clone())
}

/// The human-facing URL to store: this instance's `{base_url}/notice/{id}`
/// for a local status, else the remote's own `url`.
fn notice_url(base_url: &str, new_status: &NewStatus, id: Uuid) -> Option<String> {
    new_status
        .local
        .then(|| format!("{base_url}/notice/{id}"))
        .or_else(|| new_status.url.clone())
}

/// Moves the counters one fresh status feeds on `conn`: the author's
/// `statuses_count` and `last_status_at`, and whatever it counts on another
/// status.
async fn count_new_post(conn: &mut PgConnection, status: &Status) -> AppResult<()> {
    step_author(conn, status, 1).await?;
    bump_linked(conn, status, 1).await
}

/// Steps the counters the deleted `status` had fed back down on `conn`: its
/// author's `statuses_count` and whatever it counted on another status.
async fn uncount_deleted(conn: &mut PgConnection, status: &Status) -> AppResult<()> {
    step_author(conn, status, -1).await?;
    bump_linked(conn, status, -1).await
}

/// Adds `delta` to the post count of `status`'s author and offers its
/// creation time to `last_status_at`, which the SQL only lets move forward.
async fn step_author(conn: &mut PgConnection, status: &Status, delta: i32) -> AppResult<()> {
    sqlx::query(AUTHOR_COUNT_SQL)
        .bind(status.user_id)
        .bind(delta)
        .bind(status.inserted_at)
        .execute(&mut *conn)
        .await
        .map(drop)?;
    Ok(())
}

/// Steps the counter this status feeds on another row: `replies_count` of the
/// status it answers, `reblogs_count` of the one it reblogs. Quotes count
/// themselves through [`bump_counter`], because the row that holds
/// `quotes_count` is written after the quote.
async fn bump_linked(conn: &mut PgConnection, status: &Status, delta: i32) -> AppResult<()> {
    if let Some(parent_id) = status.in_reply_to_id {
        step_counter(conn, parent_id, Counter::Replies, delta).await?;
    }
    if let Some(original_id) = status.reblog_of_id {
        step_counter(conn, original_id, Counter::Reblogs, delta).await?;
    }
    Ok(())
}

/// Turns the live row `id` into a tombstone on `conn`, returning it, or
/// `None` when there was no live row to tombstone.
async fn mark_deleted(conn: &mut PgConnection, id: Uuid) -> AppResult<Option<Status>> {
    let tombstone = sqlx::query_as::<_, Status>(SOFT_DELETE_SQL)
        .bind(id)
        .fetch_optional(conn)
        .await?;
    Ok(tombstone)
}

/// Removes every pin of the deleted status `id` on `conn`: the soft delete's
/// counterpart of the cascade a hard delete rides, and what keeps
/// [`count`](crate::models::pinned_status::count) counting only pins a
/// profile can still show.
async fn clear_pins(conn: &mut PgConnection, id: Uuid) -> AppResult<()> {
    sqlx::query(CLEAR_PINS_SQL)
        .bind(id)
        .execute(&mut *conn)
        .await
        .map(drop)?;
    Ok(())
}

/// Turns every live reblog of `id` into a tombstone on `conn`, answering the
/// rows it turned so their counters can step down with them.
async fn tombstone_reblogs(conn: &mut PgConnection, id: Uuid) -> AppResult<Vec<Status>> {
    let reblogs = sqlx::query_as::<_, Status>(SOFT_DELETE_REBLOGS_SQL)
        .bind(id)
        .fetch_all(conn)
        .await?;
    Ok(reblogs)
}

/// Adds `delta` to one counter of status `id`, in SQL, so the value the row
/// holds is what gets stepped and the result is never below zero.
async fn step_counter(
    conn: &mut PgConnection,
    id: Uuid,
    counter: Counter,
    delta: i32,
) -> AppResult<()> {
    let sql = counter_sql(counter);
    sqlx::query(sql)
        .bind(id)
        .bind(delta)
        .execute(&mut *conn)
        .await
        .map(drop)?;
    Ok(())
}

/// The statement that steps one counter, picked by which [`Counter`] it is:
/// one literal per column instead of a string built from data, which is how
/// `sqlx` wants its SQL.
const fn counter_sql(counter: Counter) -> &'static str {
    match counter {
        Counter::Replies => STEP_REPLIES_SQL,
        Counter::Reblogs => STEP_REBLOGS_SQL,
        Counter::Favourites => STEP_FAVOURITES_SQL,
        Counter::Quotes => STEP_QUOTES_SQL,
    }
}

#[cfg(test)]
mod tests {
    use chrono::SubsecRound as _;
    use sqlx::{Error as SqlxError, PgPool};

    use super::*;
    use crate::{
        config::TEST_BASE_URL,
        error::AppError,
        models::{
            media_attachment::{self, MediaAttachment, NewMediaAttachment},
            mention,
            user::{self, NewUser, insert as insert_user, local_ap_id},
        },
    };

    /// The instance origin the fixtures pretend to serve.
    const BASE: &str = TEST_BASE_URL;

    /// Inserts a local user named `nickname`.
    async fn user(pool: &PgPool, nickname: &str) -> User {
        insert_user(
            pool,
            &NewUser::local(nickname, &local_ap_id(BASE, nickname)),
        )
        .await
        .expect("fixture user inserts")
    }

    /// Inserts `text` as a local public status of `author`.
    async fn post(pool: &PgPool, author: &User, text: &str) -> Status {
        insert(pool, BASE, &NewStatus::local(author, text))
            .await
            .expect("a plain local post inserts")
    }

    /// Inserts `text` as `author`'s answer to `parent`.
    async fn reply(pool: &PgPool, author: &User, parent: &Status, text: &str) -> Status {
        insert(
            pool,
            BASE,
            &NewStatus {
                in_reply_to_id: Some(parent.id),
                ..NewStatus::local(author, text)
            },
        )
        .await
        .expect("a reply to a stored status inserts")
    }

    /// `author`'s reblog of the status `original`, as its row.
    async fn boost(pool: &PgPool, author: &User, original: Uuid) -> Status {
        insert(
            pool,
            BASE,
            &NewStatus {
                reblog_of_id: Some(original),
                ..NewStatus::local(author, "")
            },
        )
        .await
        .expect("a reblog of a stored status inserts")
    }

    /// One media attachment of `author` posted on `status`: the row a
    /// deleted status leaves behind, referenced by nothing live.
    async fn attach(pool: &PgPool, author: &User, status: Uuid) -> MediaAttachment {
        media_attachment::insert(
            pool,
            &NewMediaAttachment {
                user_id: Some(author.id),
                status_id: Some(status),
                url: format!("{BASE}/media/fixture.png"),
                content_type: "image/png".to_owned(),
                ..NewMediaAttachment::default()
            },
        )
        .await
        .expect("a fixture attachment inserts")
    }

    /// The row of user `id` as it stands now, for reading counters back.
    async fn fresh_user(pool: &PgPool, id: Uuid) -> User {
        user::get_by_id(pool, id)
            .await
            .unwrap()
            .expect("the user exists")
    }

    /// The stored status behind `id`, tombstone included.
    async fn stored(pool: &PgPool, id: Uuid) -> Status {
        get_by_id_including_deleted(pool, id)
            .await
            .unwrap()
            .expect("the status exists")
    }

    /// The stored value of one counter of status `id`, read from the column
    /// the counter names.
    async fn counter(pool: &PgPool, id: Uuid, which: Counter) -> i32 {
        let sql = match which {
            Counter::Replies => "SELECT replies_count FROM statuses WHERE id = $1",
            Counter::Reblogs => "SELECT reblogs_count FROM statuses WHERE id = $1",
            Counter::Favourites => "SELECT favourites_count FROM statuses WHERE id = $1",
            Counter::Quotes => "SELECT quotes_count FROM statuses WHERE id = $1",
        };
        let row: (i32,) = sqlx::query_as(sql).bind(id).fetch_one(pool).await.unwrap();
        row.0
    }

    /// The ids of `statuses`, in the order they came back.
    fn ids(statuses: &[Status]) -> Vec<Uuid> {
        statuses.iter().map(|status| status.id).collect()
    }

    /// The SQLSTATE a database failure carries, or an empty string when the
    /// failure is not the database reporting a constraint.
    fn sqlstate_of(err: AppError) -> String {
        if let AppError::Database(SqlxError::Database(db_err)) = err {
            return db_err.code().unwrap_or_default().into_owned();
        }
        String::new()
    }

    /// A remote mirror of `nickname` on `remote.example`, whose posts the
    /// federation-shaped fixtures insert.
    async fn remote_user(pool: &PgPool, nickname: &str) -> User {
        let ap_id = format!("https://remote.example/users/{nickname}");
        insert_user(
            pool,
            &NewUser {
                nickname: format!("{nickname}@remote.example"),
                ap_id,
                domain: Some("remote.example".to_owned()),
                ..NewUser::default()
            },
        )
        .await
        .expect("fixture remote user inserts")
    }

    /// A [`NewStatus`] mirroring the remote post `uri`, by `author`.
    fn mirrored(author: &User, uri: &str) -> NewStatus {
        NewStatus {
            uri: Some(uri.to_owned()),
            user_id: author.id,
            content: "<p>from elsewhere</p>".to_owned(),
            raw_text: "from elsewhere".to_owned(),
            local: false,
            ..NewStatus::default()
        }
    }

    #[tokio::test]
    async fn a_local_root_status_fills_its_urls_and_counts_its_author() {
        let db = crate::test_db::test_db().await.expect("test database slot");
        let pool = db.pool().clone();
        let alice = user(&pool, "alice").await;
        let status = post(&pool, &alice, "hello world").await;

        assert!(status.id.get_version_num() == 7, "the id is a UUIDv7");
        assert_eq!(
            status.uri,
            format!("{BASE}/objects/{}", status.id),
            "a local status owns this instance's objects id (decision 6)"
        );
        assert_eq!(
            status.url,
            Some(format!("{BASE}/notice/{}", status.id)),
            "and the human URL derives from the same id"
        );
        assert_eq!(
            status.conversation_id, status.id,
            "a root status is its own conversation"
        );
        assert!(status.local, "the post is marked local");
        assert_eq!(status.user_id, alice.id, "the author is stored");
        assert_eq!(status.content, "<p>hello world</p>", "content is HTML");
        assert_eq!(status.raw_text, "hello world", "the source is kept");
        assert_eq!(
            status.content_type, DEFAULT_CONTENT_TYPE,
            "as is its content type"
        );
        assert_eq!(
            status.visibility,
            Visibility::Public,
            "a plain post is public"
        );
        assert!(
            status.in_reply_to_id.is_none()
                && status.reblog_of_id.is_none()
                && status.quote_of_id.is_none(),
            "a root status links to nothing"
        );
        assert_eq!(
            (
                status.replies_count,
                status.reblogs_count,
                status.favourites_count,
                status.quotes_count
            ),
            (0, 0, 0, 0),
            "every counter starts at zero"
        );
        assert!(
            status.edited_at.is_none()
                && status.deleted_at.is_none()
                && status.expires_at.is_none(),
            "nothing has happened to it yet"
        );
        assert!(
            status.inserted_at <= status.updated_at,
            "the database stamps both timestamps"
        );

        let counted = fresh_user(&pool, alice.id).await;
        assert_eq!(counted.statuses_count, 1, "the author counts one post");
        assert_eq!(
            counted.last_status_at,
            Some(status.inserted_at),
            "and knows when they last posted"
        );
        assert_eq!(
            get_by_id(&pool, status.id).await.unwrap(),
            Some(status.clone()),
            "get_by_id reads the row back"
        );
    }

    #[tokio::test]
    async fn a_reply_inherits_the_conversation_and_counts_on_its_parent() {
        let db = crate::test_db::test_db().await.expect("test database slot");
        let pool = db.pool().clone();
        let alice = user(&pool, "alice").await;
        let bob = user(&pool, "bob").await;
        let root = post(&pool, &alice, "root").await;

        let answer = reply(&pool, &bob, &root, "an answer").await;

        assert_eq!(
            answer.conversation_id, root.conversation_id,
            "the reply joins the thread it answers"
        );
        assert_eq!(answer.in_reply_to_id, Some(root.id), "and names its parent");
        assert_eq!(
            counter(&pool, root.id, Counter::Replies).await,
            1,
            "the parent counts one answer"
        );
        assert_eq!(
            fresh_user(&pool, bob.id).await.statuses_count,
            1,
            "the reply counts for its own author too"
        );

        let deeper = reply(&pool, &alice, &answer, "deeper").await;
        assert_eq!(
            deeper.conversation_id, root.id,
            "a reply to a reply still belongs to the root's conversation"
        );
        assert_eq!(
            counter(&pool, answer.id, Counter::Replies).await,
            1,
            "each row counts its own answers"
        );
        assert_eq!(
            counter(&pool, root.id, Counter::Replies).await,
            1,
            "and not its grandparent's"
        );
    }

    #[tokio::test]
    async fn a_mirrored_status_round_trips_every_column() {
        let db = crate::test_db::test_db().await.expect("test database slot");
        let pool = db.pool().clone();
        let mallory = remote_user(&pool, "mallory").await;
        let published = (Utc::now() - chrono::Duration::days(3)).trunc_subsecs(6);
        let expires = (Utc::now() + chrono::Duration::days(1)).trunc_subsecs(6);
        let uri = "https://remote.example/posts/42";

        let status = insert(
            &pool,
            BASE,
            &NewStatus {
                url: Some("https://remote.example/notice/42".to_owned()),
                spoiler_text: "cats".to_owned(),
                sensitive: true,
                visibility: Visibility::Local,
                language: Some("de".to_owned()),
                in_reply_to_ap_id: Some("https://remote.example/posts/41".to_owned()),
                conversation_id: Some(new_id()),
                application_name: Some("RemoteClient".to_owned()),
                application_website: Some("https://remote.example/client".to_owned()),
                edited_at: Some(published),
                expires_at: Some(expires),
                inserted_at: Some(published),
                ..mirrored(&mallory, uri)
            },
        )
        .await
        .unwrap();

        assert_eq!(status.uri, uri, "a remote status keeps the remote id");
        assert_eq!(
            status.url.as_deref(),
            Some("https://remote.example/notice/42"),
            "and the remote's human URL"
        );
        assert!(!status.local, "the post stays marked remote");
        assert_eq!(
            status.spoiler_text, "cats",
            "the content warning round trips"
        );
        assert!(status.sensitive, "so does the sensitive flag");
        assert_eq!(
            status.visibility,
            Visibility::Local,
            "Pleroma's local visibility round trips"
        );
        assert_eq!(status.language.as_deref(), Some("de"), "the language tag");
        assert_eq!(
            status.in_reply_to_ap_id.as_deref(),
            Some("https://remote.example/posts/41"),
            "an unsaved reply target is kept by id"
        );
        assert_eq!(
            status.application_name.as_deref(),
            Some("RemoteClient"),
            "the application block round trips"
        );
        assert_eq!(
            status.application_website.as_deref(),
            Some("https://remote.example/client"),
            "both halves of it"
        );
        assert_eq!(status.edited_at, Some(published), "edited_at round trips");
        assert_eq!(status.expires_at, Some(expires), "expires_at round trips");
        assert_eq!(
            status.inserted_at, published,
            "inserted_at is the remote published time"
        );

        let counted = fresh_user(&pool, mallory.id).await;
        assert_eq!(counted.statuses_count, 1, "a mirrored post counts too");
        assert_eq!(
            counted.last_status_at,
            Some(published),
            "and it sets last_status_at to when it was published"
        );
    }

    /// A mirrored post cannot name a quote that is not stored here, so this
    /// covers `quote_of_id` against a real row.
    #[tokio::test]
    async fn a_quote_names_its_original_and_leaves_the_count_alone() {
        let db = crate::test_db::test_db().await.expect("test database slot");
        let pool = db.pool().clone();
        let alice = user(&pool, "alice").await;
        let bob = user(&pool, "bob").await;
        let original = post(&pool, &alice, "quote me").await;

        let quote = insert(
            &pool,
            BASE,
            &NewStatus {
                quote_of_id: Some(original.id),
                ..NewStatus::local(&bob, "I second that")
            },
        )
        .await
        .unwrap();

        assert_eq!(quote.quote_of_id, Some(original.id), "the link is kept");
        assert_eq!(
            counter(&pool, original.id, Counter::Quotes).await,
            0,
            "insert counts no quote: the quoting ticket bumps it itself"
        );
    }

    /// The mentioned ids of `status_id` from the batch read, in order.
    async fn mentioned(pool: &PgPool, status_id: Uuid) -> Vec<Uuid> {
        let map = mention::for_statuses(pool, &[status_id])
            .await
            .expect("the batch read works");
        map.get(&status_id)
            .map(|users| users.iter().map(|user| user.id).collect())
            .unwrap_or_default()
    }

    #[tokio::test]
    async fn insert_links_the_mentioned_accounts_in_order() {
        let db = crate::test_db::test_db().await.expect("test database slot");
        let pool = db.pool().clone();
        let alice = user(&pool, "alice").await;
        let bob = user(&pool, "bob").await;
        let carol = user(&pool, "carol").await;

        let status = insert(
            &pool,
            BASE,
            &NewStatus {
                mentioned_user_ids: vec![carol.id, bob.id],
                ..NewStatus::local(&alice, "@carol and @bob")
            },
        )
        .await
        .unwrap();

        assert_eq!(
            mentioned(&pool, status.id).await,
            vec![carol.id, bob.id],
            "the links went in in mention order, in the status's transaction"
        );
        assert_eq!(
            fresh_user(&pool, alice.id).await.statuses_count,
            1,
            "and the rest of the insert happened as usual"
        );

        let plain = post(&pool, &alice, "no mentions here").await;
        assert!(
            mentioned(&pool, plain.id).await.is_empty(),
            "a status without mentioned_user_ids links nobody"
        );
    }

    #[tokio::test]
    async fn an_unknown_mention_rolls_the_status_back() {
        let db = crate::test_db::test_db().await.expect("test database slot");
        let pool = db.pool().clone();
        let alice = user(&pool, "alice").await;
        let err = insert(
            &pool,
            BASE,
            &NewStatus {
                mentioned_user_ids: vec![new_id()],
                hashtags: vec!["doomed".to_owned()],
                ..NewStatus::local(&alice, "@ghost #doomed")
            },
        )
        .await
        .expect_err("an unknown mentioned account is the foreign key's to refuse");
        assert!(
            matches!(err, AppError::Database(_)),
            "the failure is the database's: {err}"
        );

        let left: (i64,) = sqlx::query_as("SELECT count(*) FROM statuses")
            .fetch_one(&pool)
            .await
            .unwrap();
        assert_eq!(left.0, 0, "the half-written status rolled back with it");
        let links: (i64,) = sqlx::query_as("SELECT count(*) FROM status_mentions")
            .fetch_one(&pool)
            .await
            .unwrap();
        assert_eq!(links.0, 0, "and nothing was linked");
        let tags: (i64,) = sqlx::query_as(
            "SELECT count(*) FROM hashtags h
               JOIN status_hashtags sh ON sh.hashtag_id = h.id",
        )
        .fetch_one(&pool)
        .await
        .unwrap();
        assert_eq!(tags.0, 0, "and the hashtag's link rolled back with it");
        assert_eq!(
            fresh_user(&pool, alice.id).await.statuses_count,
            0,
            "the author's count went back too"
        );
    }

    /// The names of the batch read's entry for `status_id`, in order.
    async fn tag_names(pool: &PgPool, status_id: Uuid) -> Vec<String> {
        let map = hashtag::for_statuses(pool, &[status_id])
            .await
            .expect("the batch read works");
        map.get(&status_id)
            .map(|tags| tags.iter().map(|tag| tag.name.clone()).collect())
            .unwrap_or_default()
    }

    #[tokio::test]
    async fn insert_links_the_hashtags_extracted_from_local_text() {
        let db = crate::test_db::test_db().await.expect("test database slot");
        let pool = db.pool().clone();
        let alice = user(&pool, "alice").await;

        let status = post(&pool, &alice, "hello #World #foo #FOO").await;
        assert_eq!(
            tag_names(&pool, status.id).await,
            vec!["foo", "world"],
            "the local post's tags were extracted on insert, in name order, \
             case folded"
        );

        let twin = post(&pool, &alice, "#foo again").await;
        let shared = post(&pool, &alice, "#bar").await;
        assert_eq!(
            tag_names(&pool, twin.id).await,
            vec!["foo"],
            "a second post with the same tag links the same tag"
        );
        let ids: (i64,) = sqlx::query_as("SELECT count(*) FROM hashtags")
            .fetch_one(&pool)
            .await
            .unwrap();
        assert_eq!(ids.0, 3, "foo, world and bar — one row per name");
        let foos: (i64,) = sqlx::query_as(
            "SELECT count(*) FROM status_hashtags sh
               JOIN hashtags h ON h.id = sh.hashtag_id
              WHERE h.name = 'foo'",
        )
        .fetch_one(&pool)
        .await
        .unwrap();
        assert_eq!(foos.0, 2, "and two statuses point at the one foo row");
        assert_eq!(
            tag_names(&pool, shared.id).await,
            vec!["bar"],
            "an untouched tag keeps its own status"
        );

        let plain = post(&pool, &alice, "no tags here").await;
        assert!(
            tag_names(&pool, plain.id).await.is_empty(),
            "a status without hashtags links none"
        );
    }

    #[tokio::test]
    async fn insert_links_the_explicit_hashtags_of_a_remote_post() {
        let db = crate::test_db::test_db().await.expect("test database slot");
        let pool = db.pool().clone();
        let mallory = remote_user(&pool, "mallory").await;
        let status = insert(
            &pool,
            BASE,
            &NewStatus {
                hashtags: vec![
                    "FromAP".to_owned(),
                    "#dup".to_owned(),
                    "  ".to_owned(),
                    "dup".to_owned(),
                ],
                ..mirrored(&mallory, "https://remote.example/posts/tags")
            },
        )
        .await
        .unwrap();

        assert_eq!(
            tag_names(&pool, status.id).await,
            vec!["dup", "fromap"],
            "the caller's tag list is normalized, folded and linked \
             name-first, and the blank name was ignored"
        );
    }

    #[tokio::test]
    async fn importing_an_old_post_never_moves_last_status_back() {
        let db = crate::test_db::test_db().await.expect("test database slot");
        let pool = db.pool().clone();
        let mallory = remote_user(&pool, "mallory").await;
        let now = Utc::now().trunc_subsecs(6);
        let old = insert(
            &pool,
            BASE,
            &NewStatus {
                inserted_at: Some(now - chrono::Duration::days(10)),
                ..mirrored(&mallory, "https://remote.example/posts/old")
            },
        )
        .await
        .unwrap();
        assert_eq!(
            fresh_user(&pool, mallory.id).await.last_status_at,
            Some(old.inserted_at),
            "the first post names itself"
        );

        let newer = insert(
            &pool,
            BASE,
            &NewStatus {
                inserted_at: Some(now - chrono::Duration::days(1)),
                ..mirrored(&mallory, "https://remote.example/posts/newer")
            },
        )
        .await
        .unwrap();
        assert_eq!(
            fresh_user(&pool, mallory.id).await.last_status_at,
            Some(newer.inserted_at),
            "a newer post moves it forward"
        );

        let oldest = insert(
            &pool,
            BASE,
            &NewStatus {
                inserted_at: Some(now - chrono::Duration::days(20)),
                ..mirrored(&mallory, "https://remote.example/posts/oldest")
            },
        )
        .await
        .unwrap();
        assert_eq!(
            oldest.inserted_at,
            now - chrono::Duration::days(20),
            "the oldest import keeps the time it arrived with"
        );
        assert_eq!(
            fresh_user(&pool, mallory.id).await.last_status_at,
            Some(newer.inserted_at),
            "and an older import leaves it where it was"
        );
        assert_eq!(
            fresh_user(&pool, mallory.id).await.statuses_count,
            3,
            "while the count keeps all three posts"
        );
    }

    #[tokio::test]
    async fn soft_delete_hides_the_status_and_undoes_its_counts() {
        let db = crate::test_db::test_db().await.expect("test database slot");
        let pool = db.pool().clone();
        let alice = user(&pool, "alice").await;
        let bob = user(&pool, "bob").await;
        let root = post(&pool, &alice, "to be deleted").await;
        let answer = reply(&pool, &bob, &root, "answer").await;

        let tombstone = soft_delete(&pool, answer.id)
            .await
            .unwrap()
            .expect("the reply was live");

        assert_eq!(tombstone.id, answer.id, "the tombstone is that row");
        assert!(tombstone.deleted_at.is_some(), "marked deleted");
        assert_eq!(tombstone.content, "", "its content is cleared");
        assert_eq!(tombstone.raw_text, "", "its source text is cleared");
        assert_eq!(tombstone.spoiler_text, "", "and its content warning");
        assert_eq!(
            tombstone.uri, answer.uri,
            "its uri survives, reserved against a re-import"
        );
        assert_eq!(
            tombstone.visibility, answer.visibility,
            "everything else of the row is kept"
        );
        assert!(
            get_by_id(&pool, answer.id).await.unwrap().is_none(),
            "a tombstone is not a status any more"
        );
        assert!(
            get_by_id_including_deleted(&pool, answer.id)
                .await
                .unwrap()
                .is_some(),
            "but the row is still there"
        );
        assert_eq!(
            counter(&pool, root.id, Counter::Replies).await,
            0,
            "the parent stops counting the answer"
        );
        assert_eq!(
            fresh_user(&pool, bob.id).await.statuses_count,
            0,
            "and so does its author"
        );
        assert_eq!(
            fresh_user(&pool, alice.id).await.statuses_count,
            1,
            "the root's author is untouched"
        );

        assert!(
            soft_delete(&pool, answer.id).await.unwrap().is_none(),
            "deleting twice finds nothing to delete"
        );
        assert_eq!(
            fresh_user(&pool, bob.id).await.statuses_count,
            0,
            "so the author's count is not stepped down twice"
        );
        assert!(
            soft_delete(&pool, new_id()).await.unwrap().is_none(),
            "an unknown id is nothing to delete"
        );
    }

    #[tokio::test]
    async fn get_by_uri_still_finds_a_tombstone() {
        let db = crate::test_db::test_db().await.expect("test database slot");
        let pool = db.pool().clone();
        let alice = user(&pool, "alice").await;
        let mallory = remote_user(&pool, "mallory").await;
        let theirs = insert(
            &pool,
            BASE,
            &mirrored(&mallory, "https://remote.example/posts/7"),
        )
        .await
        .unwrap();
        let mine = post(&pool, &alice, "mine").await;

        assert_eq!(
            get_by_uri(&pool, &theirs.uri).await.unwrap(),
            Some(theirs.clone()),
            "federation finds a mirrored post by its remote id"
        );
        assert!(
            soft_delete(&pool, theirs.id).await.unwrap().is_some(),
            "a Delete turns it into a tombstone"
        );
        let found = get_by_uri(&pool, &theirs.uri)
            .await
            .unwrap()
            .expect("the uri stays reserved");
        assert_eq!(found.id, theirs.id, "the tombstone still answers by uri");
        assert!(found.deleted_at.is_some(), "and it is still marked deleted");
        assert_eq!(
            get_by_uri(&pool, &mine.uri).await.unwrap(),
            Some(mine.clone()),
            "a local status is found by its own object id too"
        );
        assert!(
            get_by_uri(&pool, "https://remote.example/posts/none")
                .await
                .unwrap()
                .is_none(),
            "an unseen uri is nothing"
        );

        let err = insert(&pool, BASE, &mirrored(&mallory, &theirs.uri))
            .await
            .expect_err("a deleted remote post cannot be imported again");
        assert_eq!(
            sqlstate_of(err),
            "23505",
            "the unique uri is what rejects the re-import"
        );
    }

    #[tokio::test]
    async fn deleting_a_reblog_steps_the_original_back() {
        let db = crate::test_db::test_db().await.expect("test database slot");
        let pool = db.pool().clone();
        let alice = user(&pool, "alice").await;
        let bob = user(&pool, "bob").await;
        let original = post(&pool, &alice, "worth boosting").await;

        let reblog = insert(
            &pool,
            BASE,
            &NewStatus {
                reblog_of_id: Some(original.id),
                ..NewStatus::local(&bob, "")
            },
        )
        .await
        .unwrap();
        assert_eq!(reblog.content, "", "a reblog carries no text of its own");
        assert_eq!(
            counter(&pool, original.id, Counter::Reblogs).await,
            1,
            "insert counts it on the original"
        );

        let tombstone = soft_delete(&pool, reblog.id)
            .await
            .unwrap()
            .expect("the reblog was live");
        assert_eq!(
            tombstone.reblog_of_id,
            Some(original.id),
            "the tombstone still names what it boosted"
        );
        assert_eq!(
            counter(&pool, original.id, Counter::Reblogs).await,
            0,
            "and the original no longer counts it"
        );
        assert_eq!(
            fresh_user(&pool, bob.id).await.statuses_count,
            0,
            "the booster stops counting it as their own"
        );
    }

    #[tokio::test]
    async fn deleting_a_status_takes_every_reblog_of_it() {
        let db = crate::test_db::test_db().await.expect("test database slot");
        let pool = db.pool().clone();
        let alice = user(&pool, "alice").await;
        let bob = user(&pool, "bob").await;
        let carol = user(&pool, "carol").await;
        let original = post(&pool, &alice, "delete me").await;
        let other = post(&pool, &carol, "someone else's post").await;
        let mine = boost(&pool, &bob, original.id).await;
        let theirs = boost(&pool, &carol, original.id).await;
        // A post of `other` that survives: only reblogs of the deleted
        // status are taken down, never the booster's unrelated rows.
        let untouched = boost(&pool, &bob, other.id).await;
        let attachment = attach(&pool, &alice, original.id).await;

        let tombstone = soft_delete_with_reblogs(&pool, original.id)
            .await
            .unwrap()
            .expect("the original was live");

        assert_eq!(tombstone.id, original.id, "the answer is the status");
        assert!(tombstone.deleted_at.is_some(), "and it is a tombstone");
        assert_eq!(tombstone.content, "", "its text is cleared");
        for reblog in [&mine, &theirs] {
            assert!(
                get_by_id(&pool, reblog.id).await.unwrap().is_none(),
                "the reblog of the deleted status is gone from the API"
            );
            let stored = get_by_id_including_deleted(&pool, reblog.id)
                .await
                .unwrap()
                .expect("but its row is still there");
            assert!(stored.deleted_at.is_some(), "as a tombstone too");
            assert_eq!(stored.content, "", "with its text cleared");
        }
        assert_eq!(
            counter(&pool, original.id, Counter::Reblogs).await,
            0,
            "the deleted status counts no boosts any more"
        );
        assert_eq!(
            fresh_user(&pool, bob.id).await.statuses_count,
            1,
            "bob keeps his other boost and loses the one that went down"
        );
        assert_eq!(
            fresh_user(&pool, carol.id).await.statuses_count,
            1,
            "and carol keeps her own post and loses hers"
        );
        assert_eq!(
            get_by_id(&pool, untouched.id).await.unwrap(),
            Some(untouched.clone()),
            "a reblog of another status is nobody's business here"
        );
        assert!(
            media_attachment::get_by_id(&pool, attachment.id)
                .await
                .unwrap()
                .is_some(),
            "the media row stays in the table for the cleanup job to find"
        );
    }

    #[tokio::test]
    async fn deleting_twice_with_reblogs_moves_nothing_twice() {
        let db = crate::test_db::test_db().await.expect("test database slot");
        let pool = db.pool().clone();
        let alice = user(&pool, "alice").await;
        let bob = user(&pool, "bob").await;
        let original = post(&pool, &alice, "already gone").await;
        let mine = boost(&pool, &bob, original.id).await;

        assert!(
            soft_delete_with_reblogs(&pool, original.id)
                .await
                .unwrap()
                .is_some(),
            "the first delete finds a live row"
        );
        assert!(
            soft_delete_with_reblogs(&pool, original.id)
                .await
                .unwrap()
                .is_none(),
            "the second one finds only the tombstone"
        );
        assert_eq!(
            fresh_user(&pool, alice.id).await.statuses_count,
            0,
            "so the author is not stepped down twice"
        );
        assert_eq!(
            fresh_user(&pool, bob.id).await.statuses_count,
            0,
            "and neither is the booster"
        );
        assert!(
            soft_delete_with_reblogs(&pool, new_id())
                .await
                .unwrap()
                .is_none(),
            "an unknown id is nothing to delete"
        );
        assert!(
            get_by_id_including_deleted(&pool, mine.id)
                .await
                .unwrap()
                .is_some(),
            "and the reblog row is still there, tombstoned once"
        );
    }

    #[tokio::test]
    async fn every_visibility_round_trips() {
        let db = crate::test_db::test_db().await.expect("test database slot");
        let pool = db.pool().clone();
        let alice = user(&pool, "alice").await;
        for visibility in Visibility::ALL {
            let status = insert(
                &pool,
                BASE,
                &NewStatus {
                    visibility,
                    ..NewStatus::local(&alice, "text")
                },
            )
            .await
            .unwrap();

            assert_eq!(
                stored(&pool, status.id).await.visibility,
                visibility,
                "the {visibility} enum value comes back"
            );
            let text: (String,) = sqlx::query_as("SELECT visibility FROM statuses WHERE id = $1")
                .bind(status.id)
                .fetch_one(&pool)
                .await
                .unwrap();
            assert_eq!(text.0, visibility.as_str(), "stored as its lowercase name");
            assert_eq!(
                text.0.parse::<Visibility>().ok(),
                Some(visibility),
                "the text parses back to the same variant"
            );
            assert_eq!(
                visibility.to_string(),
                visibility.as_str(),
                "Display is as_str"
            );
            assert!(
                get_by_id(&pool, status.id).await.unwrap().is_some(),
                "no visibility hides a live status from get_by_id"
            );
        }
    }

    #[tokio::test]
    async fn the_column_rejects_a_visibility_it_does_not_know() {
        let db = crate::test_db::test_db().await.expect("test database slot");
        let pool = db.pool().clone();
        let alice = user(&pool, "alice").await;
        let id = new_id();
        let err = sqlx::query(
            "INSERT INTO statuses (id, uri, user_id, conversation_id, local, visibility)
             VALUES ($1, $2, $3, $1, true, 'audience')",
        )
        .bind(id)
        .bind(format!("{BASE}/objects/bogus"))
        .bind(alice.id)
        .execute(&pool)
        .await
        .expect_err("an unknown visibility is not storable");
        assert_eq!(
            sqlstate_of(AppError::from(err)),
            "23514",
            "the failure is the visibility CHECK constraint"
        );
    }

    #[test]
    fn visibility_text_maps_both_ways() {
        assert_eq!(Visibility::Public.as_str(), "public");
        assert_eq!(Visibility::Local.as_str(), "local");
        for text in ["public", "unlisted", "private", "direct", "local"] {
            let parsed: Visibility = text.parse().expect("the five known spellings parse");
            assert_eq!(
                parsed.as_str(),
                text,
                "text -> enum -> text is the identity"
            );
        }
        for text in ["", "Public", "AUDIENCE", "list", "local "] {
            assert!(
                text.parse::<Visibility>().is_err(),
                "{text} is not a visibility"
            );
        }
    }

    #[tokio::test]
    async fn counters_never_go_below_zero() {
        let db = crate::test_db::test_db().await.expect("test database slot");
        let pool = db.pool().clone();
        let alice = user(&pool, "alice").await;
        let status = post(&pool, &alice, "count me").await;

        bump_counter(&pool, status.id, Counter::Favourites, 3)
            .await
            .expect("a favourite is countable");
        assert_eq!(
            counter(&pool, status.id, Counter::Favourites).await,
            3,
            "three favourites"
        );
        bump_counter(&pool, status.id, Counter::Favourites, -5)
            .await
            .expect("undoing more than there is stays legal");
        assert_eq!(
            counter(&pool, status.id, Counter::Favourites).await,
            0,
            "the counter stops at zero instead of going under"
        );

        bump_counter(&pool, status.id, Counter::Quotes, -1)
            .await
            .expect("a negative step on a fresh counter is ignored");
        assert_eq!(
            counter(&pool, status.id, Counter::Quotes).await,
            0,
            "quotes stay at zero"
        );
        bump_counter(&pool, status.id, Counter::Replies, 2)
            .await
            .expect("replies move through the same door");
        assert_eq!(
            counter(&pool, status.id, Counter::Replies).await,
            2,
            "and do"
        );

        bump_counter(&pool, new_id(), Counter::Favourites, 1)
            .await
            .expect("an unknown id bumps nothing");
        assert!(
            stored(&pool, status.id).await.updated_at >= status.updated_at,
            "the bumped row refreshed its timestamp"
        );
    }

    #[tokio::test]
    async fn get_many_reads_live_rows_in_id_order() {
        let db = crate::test_db::test_db().await.expect("test database slot");
        let pool = db.pool().clone();
        let alice = user(&pool, "alice").await;
        let one = post(&pool, &alice, "one").await;
        let two = post(&pool, &alice, "two").await;
        let mut wanted = vec![one.id, two.id];
        wanted.sort_unstable();

        assert_eq!(
            ids(&get_many(&pool, &[two.id, new_id(), one.id]).await.unwrap()),
            wanted,
            "the two live rows, oldest first, an unknown id skipped"
        );

        assert!(
            soft_delete(&pool, two.id).await.unwrap().is_some(),
            "one of them is deleted"
        );
        assert_eq!(
            ids(&get_many(&pool, &[one.id, two.id]).await.unwrap()),
            vec![one.id],
            "and it leaves the list"
        );
        assert!(
            get_many(&pool, &[]).await.unwrap().is_empty(),
            "no ids, no rows"
        );
    }

    /// Inserts `text` as `author`'s reblog of `original`.
    async fn reblog(pool: &PgPool, author: &User, original: &Status, text: &str) -> Status {
        insert(
            pool,
            BASE,
            &NewStatus {
                reblog_of_id: Some(original.id),
                ..NewStatus::local(author, text)
            },
        )
        .await
        .expect("a reblog of a stored status inserts")
    }

    /// Inserts `text` as `author`'s status at the given visibility.
    async fn visible(pool: &PgPool, author: &User, text: &str, visibility: Visibility) -> Status {
        insert(
            pool,
            BASE,
            &NewStatus {
                visibility,
                ..NewStatus::local(author, text)
            },
        )
        .await
        .expect("a fixture status at any visibility inserts")
    }

    /// The ids `newest_public_posts` answers, newest first, only the live
    /// `public` roots of the asked-for author: the other visibilities, the
    /// replies, the reblogs and the tombstones stay out, another author's
    /// posts stay out, `max_id` keys the next page on the id (decision 4),
    /// and `limit` cuts the page.
    #[tokio::test]
    async fn newest_public_posts_reads_the_author_s_public_roots() {
        let db = crate::test_db::test_db().await.expect("test database slot");
        let pool = db.pool().clone();
        let alice = user(&pool, "alice").await;
        let bob = user(&pool, "bob").await;
        let one = post(&pool, &alice, "one").await;
        let _unlisted = visible(&pool, &alice, "unlisted", Visibility::Unlisted).await;
        let _direct = visible(&pool, &alice, "direct", Visibility::Direct).await;
        let _private = visible(&pool, &alice, "private", Visibility::Private).await;
        let _localonly = visible(&pool, &alice, "local", Visibility::Local).await;
        let _reply = reply(&pool, &alice, &one, "in answer").await;
        let two = post(&pool, &alice, "two").await;
        let _reblog = reblog(&pool, &alice, &two, "").await;
        let three = post(&pool, &alice, "three").await;
        let bob_post = post(&pool, &bob, "bob speaks").await;
        assert!(
            soft_delete(&pool, two.id).await.unwrap().is_some(),
            "one public root is deleted"
        );

        assert_eq!(
            ids(&newest_public_posts(&pool, alice.id, None, 20)
                .await
                .unwrap()),
            vec![three.id, one.id],
            "newest first; the unlisted, direct, private, local, reply, \
             reblog and deleted rows are out, and bob's posts were never in"
        );
        assert_eq!(
            ids(&newest_public_posts(&pool, alice.id, Some(three.id), 20)
                .await
                .unwrap()),
            vec![one.id],
            "the cursor page holds only the strictly older posts"
        );
        assert_eq!(
            ids(&newest_public_posts(&pool, alice.id, None, 1).await.unwrap()),
            vec![three.id],
            "the limit cuts the page"
        );
        let carol = user(&pool, "carol").await;
        assert!(
            newest_public_posts(&pool, carol.id, None, 20)
                .await
                .unwrap()
                .is_empty(),
            "an author with nothing to show answers the empty page"
        );
        assert_eq!(
            ids(&newest_public_posts(&pool, bob.id, None, 20).await.unwrap()),
            vec![bob_post.id],
            "the other author keeps their own single public root"
        );
    }

    /// The walk up answers the whole chain above the requested status,
    /// root first, the status itself never; one hop and no hops cut the
    /// walk to what the cap admits; and a tombstone mid-chain stays in the
    /// raw answer because the walk passes *through* it — narrowing to the
    /// visible chain is the caller's step.
    #[tokio::test]
    async fn ancestor_chain_walks_up_oldest_first() {
        let db = crate::test_db::test_db().await.expect("test database slot");
        let pool = db.pool().clone();
        let alice = user(&pool, "alice").await;
        let root = post(&pool, &alice, "root").await;
        let middle = reply(&pool, &alice, &root, "middle").await;
        let leaf = reply(&pool, &alice, &middle, "leaf").await;
        assert_eq!(
            ids(&ancestor_chain(&pool, leaf.id, 100).await.unwrap()),
            vec![root.id, middle.id],
            "root first, then the direct parent, and never the requested \
             status itself"
        );
        assert!(
            ancestor_chain(&pool, root.id, 100)
                .await
                .unwrap()
                .is_empty(),
            "a root has nothing above it"
        );
        assert_eq!(
            ids(&ancestor_chain(&pool, leaf.id, 1).await.unwrap()),
            vec![middle.id],
            "one hop keeps only the direct parent"
        );
        assert!(
            ancestor_chain(&pool, leaf.id, 0).await.unwrap().is_empty(),
            "no hops walk nowhere"
        );
        let gone = soft_delete(&pool, middle.id)
            .await
            .unwrap()
            .expect("the middle reply was live");
        assert_eq!(
            ids(&ancestor_chain(&pool, leaf.id, 100).await.unwrap()),
            vec![root.id, gone.id],
            "the deleted link stays in the raw chain so the walk reaches \
             past it to the root"
        );
    }

    /// The walk down answers every branch in flat chronological (id-
    /// ascending) order, the requested status never; the depth cap stops
    /// the walk after that many hops and the row cap keeps the oldest rows
    /// of what it walked; a leaf has no tree, and a tombstone inside the
    /// tree stays in the raw answer for the caller to drop.
    #[tokio::test]
    async fn descendant_tree_walks_down_every_branch_in_id_order() {
        let db = crate::test_db::test_db().await.expect("test database slot");
        let pool = db.pool().clone();
        let alice = user(&pool, "alice").await;
        let root = post(&pool, &alice, "root").await;
        let left = reply(&pool, &alice, &root, "left").await;
        let right = reply(&pool, &alice, &root, "right").await;
        let deep = reply(&pool, &alice, &left, "deep").await;
        assert_eq!(
            ids(&descendant_tree(&pool, root.id, 100, 1000).await.unwrap()),
            vec![left.id, right.id, deep.id],
            "flat chronological order across branches: the second hop \
             follows its place in time, not its place in the tree"
        );
        assert!(
            descendant_tree(&pool, deep.id, 100, 1000)
                .await
                .unwrap()
                .is_empty(),
            "a leaf has nobody answering it"
        );
        assert_eq!(
            ids(&descendant_tree(&pool, root.id, 1, 1000).await.unwrap()),
            vec![left.id, right.id],
            "one hop keeps the direct replies and stops before the second"
        );
        assert!(
            descendant_tree(&pool, root.id, 0, 1000)
                .await
                .unwrap()
                .is_empty(),
            "no hops walk nowhere"
        );
        assert_eq!(
            ids(&descendant_tree(&pool, root.id, 100, 2).await.unwrap()),
            vec![left.id, right.id],
            "the row cap keeps the oldest rows of the walk"
        );
        let gone = soft_delete(&pool, right.id)
            .await
            .unwrap()
            .expect("the right reply was live");
        assert_eq!(
            ids(&descendant_tree(&pool, root.id, 100, 1000).await.unwrap()),
            vec![left.id, gone.id, deep.id],
            "the deleted reply is still a row of the raw tree"
        );
    }

    #[tokio::test]
    async fn a_mirrored_status_without_a_uri_is_rejected() {
        let db = crate::test_db::test_db().await.expect("test database slot");
        let pool = db.pool().clone();
        let mallory = remote_user(&pool, "mallory").await;
        let err = insert(
            &pool,
            BASE,
            &NewStatus {
                user_id: mallory.id,
                ..NewStatus::default()
            },
        )
        .await
        .expect_err("a mirrored post must name its object id");
        assert_eq!(
            sqlstate_of(err),
            "23502",
            "the failure is the NOT NULL uri column"
        );
        assert_eq!(
            fresh_user(&pool, mallory.id).await.statuses_count,
            0,
            "and the transaction counts nothing it did not write"
        );
    }

    #[tokio::test]
    async fn an_unknown_parent_or_author_is_rejected() {
        let db = crate::test_db::test_db().await.expect("test database slot");
        let pool = db.pool().clone();
        let alice = user(&pool, "alice").await;
        let err = insert(
            &pool,
            BASE,
            &NewStatus {
                in_reply_to_id: Some(new_id()),
                ..NewStatus::local(&alice, "nowhere")
            },
        )
        .await
        .expect_err("replying to an unsaved status is not possible");
        assert_eq!(
            sqlstate_of(err),
            "23503",
            "the failure is the in_reply_to_id foreign key"
        );

        let ghost_err = insert(
            &pool,
            BASE,
            &NewStatus {
                user_id: new_id(),
                ..NewStatus::local(&alice, "ghost")
            },
        )
        .await
        .expect_err("nobody posted this");
        assert_eq!(
            sqlstate_of(ghost_err),
            "23503",
            "the failure is the user_id foreign key"
        );
        assert_eq!(
            fresh_user(&pool, alice.id).await.statuses_count,
            0,
            "neither attempt left a count behind"
        );
    }

    #[tokio::test]
    async fn deleting_an_author_takes_their_statuses_with_it() {
        let db = crate::test_db::test_db().await.expect("test database slot");
        let pool = db.pool().clone();
        let alice = user(&pool, "alice").await;
        let root = post(&pool, &alice, "root").await;
        let answer = reply(&pool, &alice, &root, "answer").await;
        assert_eq!(
            answer.in_reply_to_id,
            Some(root.id),
            "the thread holds a root and an answer"
        );

        let dropped = sqlx::query("DELETE FROM users WHERE id = $1")
            .bind(alice.id)
            .execute(&pool)
            .await
            .unwrap();
        assert_eq!(dropped.rows_affected(), 1, "the fixture author existed");
        assert!(
            get_by_id_including_deleted(&pool, root.id)
                .await
                .unwrap()
                .is_none(),
            "cascade on user_id removed the author's statuses"
        );
    }

    /// `author`'s reblog of the status `original` through the reblog write,
    /// asserting this call created the row — the state the idempotence tests
    /// start from.
    async fn reblog_it(pool: &PgPool, author: &User, original: &Status) -> Status {
        let (reblog, created) = create_reblog(pool, BASE, author, original, Visibility::Public)
            .await
            .expect("a reblog of a stored status writes");
        assert!(created, "the fixture pair had not reblogged yet");
        reblog
    }

    /// `author` reblogs `original` through the write, for tests that only
    /// need the row to exist.
    async fn reblog_as(pool: &PgPool, author: &User, original: &Status) {
        let reblog = reblog_it(pool, author, original).await;
        assert_eq!(
            reblog.reblog_of_id,
            Some(original.id),
            "the fixture row names its original"
        );
    }

    /// The stored `reblogs_count` of status `id`, named the way the reblog
    /// tests read it.
    async fn reblog_count(pool: &PgPool, id: Uuid) -> i32 {
        counter(pool, id, Counter::Reblogs).await
    }

    #[tokio::test]
    async fn create_reblog_writes_the_activity_row_and_steps_both_counters() {
        let db = crate::test_db::test_db().await.expect("test database slot");
        let pool = db.pool().clone();
        let alice = user(&pool, "alice").await;
        let bob = user(&pool, "bob").await;
        let original = post(&pool, &alice, "worth boosting").await;
        let thread_member = reply(&pool, &alice, &original, "an answer").await;

        let (reblog, created) = create_reblog(&pool, BASE, &bob, &original, Visibility::Unlisted)
            .await
            .unwrap();

        assert!(created, "the first call wrote the row");
        assert_eq!(
            reblog.uri,
            format!("{BASE}/activities/{}", reblog.id),
            "a reblog row is the Announce: its uri is the activity id, not an object id"
        );
        assert_eq!(
            reblog.url,
            Some(format!("{BASE}/notice/{}", reblog.id)),
            "and the human url derives from the same id as any local row's"
        );
        assert_eq!(reblog.content, "", "a reblog carries no text of its own");
        assert_eq!(reblog.raw_text, "", "not even as source");
        assert_eq!(
            reblog.visibility,
            Visibility::Unlisted,
            "the requested visibility is the stored one"
        );
        assert!(reblog.local, "a boost made here is local");
        assert_eq!(
            reblog.reblog_of_id,
            Some(original.id),
            "the row names what it boosts"
        );
        assert_eq!(
            reblog.conversation_id, original.conversation_id,
            "a boost joins the conversation of what it boosts"
        );
        assert_eq!(
            reblog.conversation_id, thread_member.conversation_id,
            "which is the same thread the original's answers share"
        );

        assert_eq!(
            reblog_count(&pool, original.id).await,
            1,
            "the original counts one boost"
        );
        assert_eq!(
            fresh_user(&pool, bob.id).await.statuses_count,
            1,
            "and so does the reblogger"
        );
    }

    #[tokio::test]
    async fn reblogging_twice_is_one_row_and_one_count() {
        let db = crate::test_db::test_db().await.expect("test database slot");
        let pool = db.pool().clone();
        let alice = user(&pool, "alice").await;
        let bob = user(&pool, "bob").await;
        let original = post(&pool, &alice, "boost me once").await;
        let first = reblog_it(&pool, &bob, &original).await;

        let (again, created) = create_reblog(&pool, BASE, &bob, &original, Visibility::Public)
            .await
            .expect("repeating a reblog is not an error");

        assert!(!created, "the repeat wrote nothing");
        assert_eq!(
            again.id, first.id,
            "and answers the row that is already there"
        );
        assert_eq!(
            again.visibility, first.visibility,
            "the restated row keeps the visibility of the write that made it"
        );
        assert_eq!(
            reblog_count(&pool, original.id).await,
            1,
            "one boost, one count"
        );
        assert_eq!(
            fresh_user(&pool, bob.id).await.statuses_count,
            1,
            "and the reblogger counts it once"
        );
    }

    #[tokio::test]
    async fn a_withdrawn_reblog_leaves_the_pair_free_again() {
        let db = crate::test_db::test_db().await.expect("test database slot");
        let pool = db.pool().clone();
        let alice = user(&pool, "alice").await;
        let bob = user(&pool, "bob").await;
        let original = post(&pool, &alice, "boost, unboost, boost").await;
        let first = reblog_it(&pool, &bob, &original).await;

        let tombstone = remove_reblog(&pool, bob.id, original.id)
            .await
            .unwrap()
            .expect("the live reblog was there to withdraw");
        assert_eq!(
            tombstone.id, first.id,
            "the withdrawal answers the row that went"
        );
        assert!(tombstone.deleted_at.is_some(), "and it is a tombstone");
        assert_eq!(
            tombstone.reblog_of_id,
            Some(original.id),
            "the tombstone still names what it boosted"
        );

        let (second, created) = create_reblog(&pool, BASE, &bob, &original, Visibility::Public)
            .await
            .expect("reblogging after a withdrawal is allowed");
        assert!(
            created,
            "the partial index is free again once the row is gone"
        );
        assert_ne!(second.id, first.id, "and the new boost is a new row");
        assert_eq!(
            reblog_count(&pool, original.id).await,
            1,
            "the original counts the survivor"
        );
        assert_eq!(
            fresh_user(&pool, bob.id).await.statuses_count,
            1,
            "and bob's own count came back up through it"
        );
    }

    #[tokio::test]
    async fn remove_reblog_steps_both_counters_back_exactly_once() {
        let db = crate::test_db::test_db().await.expect("test database slot");
        let pool = db.pool().clone();
        let alice = user(&pool, "alice").await;
        let bob = user(&pool, "bob").await;
        let original = post(&pool, &alice, "unboost me").await;
        reblog_as(&pool, &bob, &original).await;

        assert!(
            remove_reblog(&pool, bob.id, original.id)
                .await
                .unwrap()
                .is_some(),
            "the first withdrawal finds the row"
        );
        assert_eq!(
            reblog_count(&pool, original.id).await,
            0,
            "the original stops counting it"
        );
        assert_eq!(
            fresh_user(&pool, bob.id).await.statuses_count,
            0,
            "and so does the reblogger"
        );

        assert!(
            remove_reblog(&pool, bob.id, original.id)
                .await
                .unwrap()
                .is_none(),
            "the second finds nothing to withdraw"
        );
        assert!(
            remove_reblog(&pool, alice.id, original.id)
                .await
                .unwrap()
                .is_none(),
            "and the author never reblogged their own post"
        );
        assert!(
            remove_reblog(&pool, bob.id, new_id())
                .await
                .unwrap()
                .is_none(),
            "an unknown original is nothing to withdraw"
        );
        assert_eq!(
            reblog_count(&pool, original.id).await,
            0,
            "no step moved twice"
        );
        assert_eq!(
            fresh_user(&pool, bob.id).await.statuses_count,
            0,
            "and no author-count step ran twice either"
        );
    }

    #[tokio::test]
    async fn a_deleted_original_takes_the_reblog_flag_with_it() {
        let db = crate::test_db::test_db().await.expect("test database slot");
        let pool = db.pool().clone();
        let alice = user(&pool, "alice").await;
        let bob = user(&pool, "bob").await;
        let original = post(&pool, &alice, "gone boosted").await;
        reblog_as(&pool, &bob, &original).await;

        assert!(
            soft_delete_with_reblogs(&pool, original.id)
                .await
                .unwrap()
                .is_some(),
            "the delete finds the live original"
        );
        assert_eq!(
            reblogged_status_ids(&pool, bob.id, &[original.id])
                .await
                .unwrap(),
            HashSet::new(),
            "the tombstoned boost flags nothing any more"
        );
        assert_eq!(
            reblog_count(&pool, original.id).await,
            0,
            "and the count went down with it"
        );
    }

    #[tokio::test]
    async fn reblogged_status_ids_answers_the_live_pairs() {
        let db = crate::test_db::test_db().await.expect("test database slot");
        let pool = db.pool().clone();
        let alice = user(&pool, "alice").await;
        let bob = user(&pool, "bob").await;
        let carol = user(&pool, "carol").await;
        let first = post(&pool, &alice, "first").await;
        let second = post(&pool, &alice, "second").await;
        let third = post(&pool, &alice, "third").await;
        reblog_as(&pool, &bob, &first).await;
        reblog_as(&pool, &bob, &third).await;
        reblog_as(&pool, &carol, &second).await;

        let asked = [first.id, second.id, third.id, new_id()];
        let bob_ids = reblogged_status_ids(&pool, bob.id, &asked).await.unwrap();
        assert!(
            bob_ids.contains(&first.id) && bob_ids.contains(&third.id),
            "bob's two boosts"
        );
        assert!(
            !bob_ids.contains(&second.id),
            "and carol's is nobody else's flag"
        );
        assert_eq!(
            reblogged_status_ids(&pool, carol.id, &asked).await.unwrap(),
            reblogged_status_ids(&pool, carol.id, &[second.id, second.id])
                .await
                .unwrap(),
            "a duplicated ask answers the one flagged original"
        );
        assert!(
            reblogged_status_ids(&pool, alice.id, &asked)
                .await
                .unwrap()
                .is_empty(),
            "the author boosted none of their own posts"
        );
        assert_eq!(
            reblogged_status_ids(&pool, bob.id, &[]).await.unwrap(),
            HashSet::new(),
            "an empty ask is an empty set"
        );

        assert!(
            remove_reblog(&pool, bob.id, first.id)
                .await
                .unwrap()
                .is_some(),
            "bob withdraws one boost"
        );
        assert_eq!(
            reblogged_status_ids(&pool, bob.id, &[first.id, third.id])
                .await
                .unwrap(),
            HashSet::from([third.id]),
            "and the flagged set shrinks with it"
        );
    }

    /// The page params `query` names, the way the extractor makes them.
    fn page(query: &str) -> PageParams {
        PageParams::from_query(Some(query), crate::pagination::MAX_LIMIT)
            .expect("the fixture page parses")
    }

    #[tokio::test]
    async fn the_reblogger_page_walks_the_reblog_row_ids() {
        let db = crate::test_db::test_db().await.expect("test database slot");
        let pool = db.pool().clone();
        let alice = user(&pool, "alice").await;
        let original = post(&pool, &alice, "boosted by many").await;
        let other = post(&pool, &alice, "boosted once").await;
        let mut rebloggers = Vec::new();
        for n in 0..3 {
            let booster = user(&pool, &format!("booster{n}")).await;
            reblog_as(&pool, &booster, &original).await;
            rebloggers.push(booster);
        }
        let lonely = user(&pool, "lonely").await;
        reblog_as(&pool, &lonely, &other).await;
        let all: Vec<Uuid> = sqlx::query_as::<_, (Uuid,)>(
            "SELECT id FROM statuses WHERE reblog_of_id = $1 ORDER BY id DESC",
        )
        .bind(original.id)
        .fetch_all(&pool)
        .await
        .unwrap()
        .into_iter()
        .map(|(id,)| id)
        .collect();
        assert_eq!(all.len(), 3, "three live boosts of the one original");
        let entry_ids = |entries: &[ReblogPageEntry]| -> Vec<Uuid> {
            entries.iter().map(|entry| entry.reblog_id).collect()
        };

        let first_page = reblogger_page(&pool, original.id, page("limit=2"))
            .await
            .unwrap();
        assert_eq!(
            entry_ids(&first_page),
            vec![all[0], all[1]],
            "the two newest boosts, keyed on the reblog row ids"
        );
        assert_eq!(
            first_page[0].user.id, rebloggers[2].id,
            "and each entry carries the account behind its boost"
        );

        let older = reblogger_page(&pool, original.id, page(&format!("max_id={}", all[1])))
            .await
            .unwrap();
        assert_eq!(
            entry_ids(&older),
            vec![all[2]],
            "max_id continues where the page ended, and `other`'s boost is on no page"
        );

        assert!(
            reblogger_page(&pool, original.id, page(&format!("max_id={}", all[2])))
                .await
                .unwrap()
                .is_empty(),
            "past the oldest boost the page is empty"
        );

        let newer = reblogger_page(&pool, original.id, page(&format!("min_id={}", all[2])))
            .await
            .unwrap();
        assert_eq!(
            entry_ids(&newer),
            vec![all[0], all[1]],
            "min_id answers the newer page, newest-first after the flip"
        );

        assert!(
            remove_reblog(&pool, rebloggers[1].id, original.id)
                .await
                .unwrap()
                .is_some(),
            "the middle booster withdraws"
        );
        let after = reblogger_page(&pool, original.id, page("")).await.unwrap();
        assert_eq!(
            entry_ids(&after),
            vec![all[0], all[2]],
            "a withdrawn boost leaves the page"
        );
    }
}
