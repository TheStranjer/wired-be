//! The archive itself: [`build`] writes an account's four JSON entries —
//! the actor, the outbox of `Create`s, the likes and the bookmarks —
//! straight into a [`tempfile::NamedTempFile`]'s zip, streaming each
//! collection page by page through [`CollectionEntry`] so no item list is
//! ever a held value, and hands back the owned [`BackupArchive`]. The
//! entry names and the [`PAGE_SIZE`] keyset step are this child's;
//! [`super`] sketches the archive's shape, and [`run`](super::run) and
//! [`purge`](super::purge) are the queue jobs that build and sweep it.

use std::{fmt, fs::File, io::Write as _};

use serde::Serialize;
use tempfile::NamedTempFile;
use tracing::debug;
use uuid::Uuid;
use zip::{CompressionMethod, ZipWriter, write::SimpleFileOptions};

use crate::{
    config::Config,
    error::{AppError, AppResult},
    federation::{
        actor::{actor_json, profile_emojis, stored_public_key},
        note::{create_activity_json, load_note_context, note_json},
        vocab::AS_VOCABULARY,
    },
    models::{
        bookmark,
        custom_emoji::CustomEmoji,
        favourite,
        status::{self, Status},
        user::User,
    },
    state::AppState,
};

/// Rows one keyset page of any of the three walks carries: small enough
/// that a page of rendered activities is a bounded working set, large
/// enough that a normal account's archive costs one query per collection.
const PAGE_SIZE: usize = 200;

/// The archive's first entry: the account's own actor document.
pub(super) const ACTOR_ENTRY: &str = "actor.json";
/// The second entry: every live status as its `Create`.
pub(super) const OUTBOX_ENTRY: &str = "outbox.json";
/// The third entry: the favourited statuses' object ids.
pub(super) const LIKES_ENTRY: &str = "likes.json";
/// The fourth entry: the bookmarked statuses' object ids.
pub(super) const BOOKMARKS_ENTRY: &str = "bookmarks.json";

/// The finished archive: the name the export is stored and served under,
/// the byte count the uploader is told, and the temp file holding it.
///
/// The fields are all private — which also keeps the repo's
/// `partial_pub_fields` lint whole — and read through accessors; the temp
/// path in particular stays hidden because the value cannot outlive its
/// archive (drop deletes the file) and a caller has no path to chase after
/// one that is gone.
#[derive(Debug)]
pub struct BackupArchive {
    /// The archive's name — [`crate::models::backup::file_name`]'s shape,
    /// handed to [`build`] by the caller so the row and the file agree.
    file_name: String,
    /// The zip's size in bytes, measured on the written file.
    size: u64,
    /// The temp file the zip lives in: read only to log its release by
    /// [`Drop`], and deleted when this value drops.
    pub(super) temp: NamedTempFile,
}

impl Drop for BackupArchive {
    /// Ends the archive: `tempfile` deletes the owned file here, so the
    /// temp bytes can never outlive the value that describes them. The log
    /// names what went.
    fn drop(&mut self) {
        debug!(path = %self.temp.path().display(), "released backup archive");
    }
}

impl BackupArchive {
    /// The name this archive is stored and served under: the one
    /// [`build`] was handed.
    #[must_use]
    pub fn file_name(&self) -> &str {
        &self.file_name
    }

    /// The archive's size in bytes, equal to the length of the file the
    /// value owns — the number the `backups` row's `file_size` records.
    #[must_use]
    pub const fn size(&self) -> u64 {
        self.size
    }
}

/// Builds `user`'s export archive into a temp file and returns its name,
/// size and owner.
///
/// `file_name` is the name [`crate::models::backup::file_name`] minted for
/// the request this archive answers; it is echoed on the returned value so
/// the caller's row write and the upload key can agree on one string.
///
/// # Errors
///
/// [`AppError::Internal`] for a temp file that cannot be created or
/// written and for any zip or JSON encoding failure; any database failure
/// in the paged reads arrives as
/// [`AppError::Database`](crate::error::AppError).
pub async fn build(state: &AppState, user: &User, file_name: &str) -> AppResult<BackupArchive> {
    let temp = NamedTempFile::new().map_err(|err| archive_error("create temp file", &err))?;
    let mut zip = ZipWriter::new(
        temp.as_file()
            .try_clone()
            .map_err(|err| archive_error("open temp file", &err))?,
    );
    let emojis = profile_emojis(&state.db, user).await?;
    let public_key = stored_public_key(&state.db, user).await?;
    write_actor(
        &mut zip,
        user,
        public_key.as_deref(),
        &state.config,
        &emojis,
    )?;
    let outbox_items = write_outbox(&mut zip, state, user).await?;
    let like_items = write_uris(&mut zip, state, user.id, LIKES_ENTRY, UriTable::Likes).await?;
    let bookmark_items = write_uris(
        &mut zip,
        state,
        user.id,
        BOOKMARKS_ENTRY,
        UriTable::Bookmarks,
    )
    .await?;
    let file: File = zip
        .finish()
        .map_err(|err| archive_error("finish archive", &err))?;
    let size = file
        .metadata()
        .map_err(|err| archive_error("measure archive", &err))?
        .len();
    debug!(
        user = %user.nickname,
        outbox = outbox_items,
        likes = like_items,
        bookmarks = bookmark_items,
        size,
        "built backup archive"
    );
    Ok(BackupArchive {
        file_name: file_name.to_owned(),
        size,
        temp,
    })
}

/// Wraps one archive failure, naming the operation that hit it;
/// [`AppError::Internal`] never sends its detail to a client.
fn archive_error(operation: &str, err: &dyn fmt::Display) -> AppError {
    AppError::Internal(format!("backup archive {operation} failed: {err}"))
}

/// [`PAGE_SIZE`] as the `i64` limit the paged readers take. A page of 200
/// rows fits any platform's `i64`; the saturating arm only keeps the
/// conversion itself total.
fn page_limit() -> i64 {
    i64::try_from(PAGE_SIZE).unwrap_or(i64::MAX)
}

/// The one file-option template every entry is written with: deflate, like
/// the emoji-pack archive of [`crate::services::emoji_packs`].
fn zip_options() -> SimpleFileOptions {
    SimpleFileOptions::default().compression_method(CompressionMethod::Deflated)
}

/// One JSON value as the archive stores it: compact `serde_json` bytes.
fn encode<T: ?Sized + Serialize>(value: &T) -> AppResult<Vec<u8>> {
    serde_json::to_vec(value).map_err(|err| archive_error("encode entry", &err))
}

/// Opens one deflated entry named `name`.
fn start_entry(zip: &mut ZipWriter<File>, name: &str) -> AppResult<()> {
    zip.start_file(name, zip_options())
        .map_err(|err| archive_error("open entry", &err))?;
    Ok(())
}

/// Writes `actor.json`: this instance's actor document for `user`,
/// verbatim — no relative-path key merge, the archive carries the same
/// absolute document the fediverse already sees. `public_key` is the PEM
/// stored for `user` in `activitypub_actors`.
fn write_actor(
    zip: &mut ZipWriter<File>,
    user: &User,
    public_key: Option<&str>,
    config: &Config,
    emojis: &[CustomEmoji],
) -> AppResult<()> {
    let bytes = encode(&actor_json(user, public_key, config, emojis))?;
    start_entry(zip, ACTOR_ENTRY)?;
    zip.write_all(&bytes)
        .map_err(|err| archive_error("write actor", &err))?;
    Ok(())
}

/// Writes `outbox.json`: every live status the account authored as its
/// `Create` activity, oldest id first, streamed page by page into one open
/// entry.
async fn write_outbox(
    zip: &mut ZipWriter<File>,
    state: &AppState,
    user: &User,
) -> AppResult<usize> {
    let mut entry = CollectionEntry::start(zip, OUTBOX_ENTRY)?;
    let mut after = None;
    loop {
        let page = status::backup_page(&state.db, user.id, after, page_limit()).await?;
        let last_page = page.len() < PAGE_SIZE;
        for row in &page {
            if let Some(bytes) = create_bytes(state, row).await? {
                entry.item(&bytes)?;
            }
        }
        after = page.last().map(|row| row.id);
        if last_page {
            break;
        }
    }
    entry.finish()
}

/// The one `Create` activity `row` rides in `outbox.json`, serialised;
/// `None` when the row vanished or stopped rendering between the page read
/// and its write — the same drop-instead-of-fail rule
/// [`crate::federation::collections`] states for the served outbox.
async fn create_bytes(state: &AppState, row: &Status) -> AppResult<Option<Vec<u8>>> {
    let ctx = match load_note_context(&state.db, &state.config, row.id).await {
        Ok(ctx) => ctx,
        Err(AppError::NotFound) => return Ok(None),
        Err(err) => return Err(err),
    };
    // `None` is the `local` visibility, which `backup_page` never reads; a
    // row that changed under the walk still must not break the build.
    let Some(note) = note_json(&ctx) else {
        return Ok(None);
    };
    encode(&create_activity_json(&note, &ctx.status, &state.config)).map(Some)
}

/// Which of the two uri collections a walk is filling: the pages have the
/// same `(row id, status uri)` shape and only the table read differs.
#[derive(Clone, Copy)]
enum UriTable {
    /// The account's favourites, walked on the favourite id.
    Likes,
    /// The account's bookmarks, walked on the bookmark id.
    Bookmarks,
}

/// One page of `user_id`'s rows in `table`, as `(row id, status uri)`.
async fn read_uris(
    state: &AppState,
    user_id: Uuid,
    table: UriTable,
    after: Option<Uuid>,
) -> AppResult<Vec<(Uuid, String)>> {
    match table {
        UriTable::Likes => favourite::backup_page(&state.db, user_id, after, page_limit()).await,
        UriTable::Bookmarks => bookmark::backup_page(&state.db, user_id, after, page_limit()).await,
    }
}

/// Writes one of `likes.json` / `bookmarks.json`: the marked statuses'
/// `uri`s as JSON strings, in row-id order, streamed page by page into one
/// open entry.
async fn write_uris(
    zip: &mut ZipWriter<File>,
    state: &AppState,
    user_id: Uuid,
    name: &str,
    table: UriTable,
) -> AppResult<usize> {
    let mut entry = CollectionEntry::start(zip, name)?;
    let mut after = None;
    loop {
        let page = read_uris(state, user_id, table, after).await?;
        let last_page = page.len() < PAGE_SIZE;
        for (_id, uri) in &page {
            entry.item(&encode(uri)?)?;
        }
        after = page.last().map(|(id, _uri)| *id);
        if last_page {
            break;
        }
    }
    entry.finish()
}

/// One streamed `OrderedCollection` entry: opened in the archive, items
/// appended one serialised value at a time, closed with the `totalItems`
/// tail. The three collection entries share this shape; only their items
/// (activities versus uri strings) and their page reads differ.
struct CollectionEntry<'w> {
    /// The open archive whose current entry the items stream into.
    zip: &'w mut ZipWriter<File>,
    /// Items written so far: the comma before every one after the first,
    /// and the `totalItems` the tail states.
    items: usize,
}

impl<'w> CollectionEntry<'w> {
    /// Opens `name` as a deflated entry and writes the header: the
    /// `@context`, the `id` (the entry's own name), the
    /// `OrderedCollection` type and the opening of `orderedItems`.
    fn start(zip: &'w mut ZipWriter<File>, name: &str) -> AppResult<Self> {
        start_entry(zip, name)?;
        write!(
            zip,
            r#"{{"@context": "{AS_VOCABULARY}", "id": "{name}", "type": "OrderedCollection", "orderedItems": ["#
        )
        .map_err(|err| archive_error("write entry header", &err))?;
        Ok(Self { zip, items: 0 })
    }

    /// Appends one serialised item to the array — a comma first for every
    /// item after the first, which is also the page-seam rule.
    fn item(&mut self, bytes: &[u8]) -> AppResult<()> {
        if self.items > 0 {
            self.zip
                .write_all(b",")
                .map_err(|err| archive_error("separate items", &err))?;
        }
        self.zip
            .write_all(bytes)
            .map_err(|err| archive_error("write item", &err))?;
        self.items += 1;
        Ok(())
    }

    /// Closes the array, writes the `],"totalItems": {n}}` tail and ends
    /// the entry; the item count the archive holds.
    fn finish(self) -> AppResult<usize> {
        write!(self.zip, r#"],"totalItems": {}}}"#, self.items)
            .map_err(|err| archive_error("write entry tail", &err))?;
        Ok(self.items)
    }
}

#[cfg(test)]
mod tests;
