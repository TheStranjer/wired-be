//! The account export archive: the deflate zip a backup request describes,
//! built straight from the database into a temp file.
//!
//! Reference: Pleroma's `Pleroma.Export.export/1`
//! (`lib/pleroma/export/import.ex`), which writes an account's actor,
//! outbox, likes and bookmarks as JSON files and zips them into
//! `archive-<nickname>-<stamp>.zip` under a temp directory. Here [`build`]
//! is that pure archive step only: it writes the zip, returns the name and
//! the byte count of the result, and touches nothing else — no `backups`
//! row, no uploader, no `jobs` registration. The follow-up ticket for the
//! `backups.run` handler wires those around this call.
//!
//! The archive holds exactly four entries, in this order:
//!
//! - `actor.json` — [`crate::federation::actor::actor_json`] for the
//!   account, verbatim;
//! - `outbox.json` — one `OrderedCollection` whose items are the `Create`
//!   activities of every live status the account authored across all four
//!   federated visibilities (`local` posts have no document to export);
//!   reblogs are ordinary status rows (decision 6) and ride along as
//!   `Create`, exactly as their rows stand — no `Announce` is synthesised;
//! - `likes.json` and `bookmarks.json` — the same collection shape whose
//!   items are the `statuses.uri` strings the account favourited and
//!   bookmarked, in row-id order.
//!
//! Memory is bounded by construction: each collection is one open zip
//! entry that a keyset walk streams into — a header fragment, then one
//! serialised item at a time (`PAGE_SIZE` rows per query at most), then
//! the `],"totalItems": {n}}` tail — so no page of items is ever a held
//! `serde_json::Value` and the whole archive is never a held `Vec<u8>`.
//!
//! The zip lives in a [`tempfile::NamedTempFile`] the returned
//! [`BackupArchive`] owns: the path stays private, the value's drop deletes
//! the file, and [`BackupArchive::size`] is the length measured on the
//! written file before its handle closes.

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
        actor::actor_json,
        note::{create_activity_json, load_note_context, note_json},
    },
    models::{
        bookmark, favourite,
        status::{self, Status},
        user::User,
    },
    state::AppState,
};

/// Rows one keyset page of any of the three walks carries: small enough
/// that a page of rendered activities is a bounded working set, large
/// enough that a normal account's archive costs one query per collection.
const PAGE_SIZE: usize = 200;

/// The `ActivityStreams` context every collection entry declares.
const AS_CONTEXT: &str = "https://www.w3.org/ns/activitystreams";

/// The archive's first entry: the account's own actor document.
const ACTOR_ENTRY: &str = "actor.json";
/// The second entry: every live status as its `Create`.
const OUTBOX_ENTRY: &str = "outbox.json";
/// The third entry: the favourited statuses' object ids.
const LIKES_ENTRY: &str = "likes.json";
/// The fourth entry: the bookmarked statuses' object ids.
const BOOKMARKS_ENTRY: &str = "bookmarks.json";

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
    temp: NamedTempFile,
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
    write_actor(&mut zip, user, &state.config)?;
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
/// absolute document the fediverse already sees.
fn write_actor(zip: &mut ZipWriter<File>, user: &User, config: &Config) -> AppResult<()> {
    let bytes = encode(&actor_json(user, config))?;
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
            r#"{{"@context": "{AS_CONTEXT}", "id": "{name}", "type": "OrderedCollection", "orderedItems": ["#
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
mod tests {
    use std::{collections::HashSet, io::Read as _, sync::Arc};

    use chrono::Utc;
    use serde_json::Value;
    use sqlx::PgPool;
    use zip::ZipArchive;

    use super::*;
    use crate::{
        config::TEST_BASE_URL,
        http_client::HttpClient,
        models::{
            backup::file_name,
            status::{NewStatus, Visibility, insert, soft_delete},
            user::{NewUser, insert as insert_user, local_ap_id},
        },
        uploads::UploaderBackend,
    };

    /// The instance origin the fixtures post under.
    const BASE: &str = TEST_BASE_URL;
    /// How many statuses the page-seam test seeds: just over one page.
    const OVER_PAGE: usize = PAGE_SIZE + 5;

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

    /// A local user named `nickname`.
    async fn user(pool: &PgPool, nickname: &str) -> User {
        insert_user(
            pool,
            &NewUser::local(nickname, &local_ap_id(BASE, nickname)),
        )
        .await
        .expect("the fixture user inserts")
    }

    /// A post of `text` by `author`, visible to `visibility`.
    async fn post(pool: &PgPool, author: &User, visibility: Visibility, text: &str) -> Status {
        insert(
            pool,
            BASE,
            &NewStatus {
                visibility,
                ..NewStatus::local(author, text)
            },
        )
        .await
        .expect("the fixture post inserts")
    }

    /// Favourites `status_id` as `who`, asserting the row is new.
    async fn like(pool: &PgPool, who: &User, status_id: Uuid) {
        assert!(
            favourite::favourite(pool, BASE, who.id, status_id)
                .await
                .expect("the fixture favourite stores")
                .is_some(),
            "the fixture pair had not favourited yet"
        );
    }

    /// Bookmarks `status_id` as `who`.
    async fn mark(pool: &PgPool, who: &User, status_id: Uuid) {
        let _row: bookmark::Bookmark = bookmark::add(pool, who.id, status_id, None)
            .await
            .expect("the fixture bookmark stores");
    }

    /// Builds `owner`'s archive under the name
    /// [`crate::models::backup::file_name`] mints for them right now.
    async fn build_for(state: &AppState, owner: &User) -> BackupArchive {
        let name = file_name(&owner.nickname, Utc::now());
        let archive = build(state, owner, &name)
            .await
            .expect("the archive builds");
        assert_eq!(
            archive.file_name(),
            name,
            "the value echoes the name it was handed"
        );
        archive
    }

    /// The raw text of entry `name`, read back through `ZipArchive`.
    fn entry_text(archive: &BackupArchive, name: &str) -> String {
        let file = File::open(archive.temp.path()).expect("the temp file exists");
        let mut zip = ZipArchive::new(file).expect("the archive reads back");
        let mut entry = zip.by_name(name).expect("the entry is there");
        let mut text = String::new();
        let _read: usize = entry.read_to_string(&mut text).expect("the entry decodes");
        text
    }

    /// The parsed JSON of entry `name`.
    fn entry_json(archive: &BackupArchive, name: &str) -> Value {
        serde_json::from_str(&entry_text(archive, name))
            .unwrap_or_else(|err| panic!("{name} is not valid JSON: {err}"))
    }

    /// The entry names, in archive order.
    fn entry_names(archive: &BackupArchive) -> Vec<String> {
        let file = File::open(archive.temp.path()).expect("the temp file exists");
        let zip = ZipArchive::new(file).expect("the archive reads back");
        zip.file_names()
            .map(|name| name.expect("entry names read").into_owned())
            .collect()
    }

    /// The `orderedItems` of one parsed collection entry, after asserting
    /// the shared header and that `totalItems` counts its items.
    fn collection(archive: &BackupArchive, name: &str) -> Vec<Value> {
        let doc = entry_json(archive, name);
        assert_eq!(
            doc.get("@context").and_then(Value::as_str),
            Some(AS_CONTEXT),
            "{name} declares the plain ActivityStreams context"
        );
        assert_eq!(
            doc.get("id").and_then(Value::as_str),
            Some(name),
            "{name} is its own name's id"
        );
        assert_eq!(
            doc.get("type").and_then(Value::as_str),
            Some("OrderedCollection"),
            "{name} is an OrderedCollection"
        );
        let items = doc
            .get("orderedItems")
            .and_then(Value::as_array)
            .expect("the entry carries an orderedItems array");
        assert_eq!(
            doc.get("totalItems").and_then(Value::as_u64),
            Some(u64::try_from(items.len()).expect("an archive fits its item count into a u64")),
            "{name}'s totalItems matches its item count"
        );
        items.clone()
    }

    /// The `object.id` of every item, asserting each is a `Create`
    /// carrying an embedded object.
    fn create_object_ids(items: &[Value]) -> Vec<String> {
        items
            .iter()
            .map(|item| {
                assert_eq!(
                    item.get("type").and_then(Value::as_str),
                    Some("Create"),
                    "every outbox item is a Create"
                );
                item.get("object")
                    .and_then(|object| object.get("id"))
                    .and_then(Value::as_str)
                    .expect("the Create embeds an object with an id")
                    .to_owned()
            })
            .collect()
    }

    /// The uri strings of a parsed likes/bookmarks entry.
    fn uri_items(items: Vec<Value>) -> Vec<String> {
        items
            .into_iter()
            .map(|item| {
                item.as_str()
                    .expect("a like/bookmark item is a uri string")
                    .to_owned()
            })
            .collect()
    }

    #[tokio::test]
    async fn the_archive_holds_the_four_entries_and_owns_its_temp_file() {
        let db = crate::test_db::test_db().await.expect("test database slot");
        let pool = db.pool().clone();
        let state = app_state(&pool);
        let alice = user(&pool, "alice").await;
        let _first: Status = post(&pool, &alice, Visibility::Public, "hello").await;

        let archive = build_for(&state, &alice).await;
        assert_eq!(
            entry_names(&archive),
            [ACTOR_ENTRY, OUTBOX_ENTRY, LIKES_ENTRY, BOOKMARKS_ENTRY],
            "the four entries stand in this order"
        );
        for name in [ACTOR_ENTRY, OUTBOX_ENTRY, LIKES_ENTRY, BOOKMARKS_ENTRY] {
            assert!(
                entry_json(&archive, name).is_object(),
                "{name} parses as a JSON object"
            );
        }
        assert_eq!(
            entry_json(&archive, ACTOR_ENTRY),
            actor_json(&alice, &state.config),
            "actor.json is the federation actor verbatim"
        );

        // The size says what the file holds while the value owns it...
        let path = archive.temp.path().to_path_buf();
        assert_eq!(
            archive.size(),
            std::fs::metadata(&path)
                .expect("the temp file is there")
                .len(),
            "size equals the archive file's length"
        );
        // ...and the temp file cannot outlive the value.
        drop(archive);
        assert!(!path.exists(), "dropping the archive deleted its file");
    }

    #[tokio::test]
    async fn outbox_lists_every_federated_status_once_as_a_create() {
        let db = crate::test_db::test_db().await.expect("test database slot");
        let pool = db.pool().clone();
        let state = app_state(&pool);
        let alice = user(&pool, "alice").await;
        let bob = user(&pool, "bob").await;
        let public = post(&pool, &alice, Visibility::Public, "public").await;
        let unlisted = post(&pool, &alice, Visibility::Unlisted, "unlisted").await;
        let private = post(&pool, &alice, Visibility::Private, "followers").await;
        let direct = post(&pool, &alice, Visibility::Direct, "psst").await;
        let local = post(&pool, &alice, Visibility::Local, "instance only").await;
        let gone = post(&pool, &alice, Visibility::Public, "deleted").await;
        assert!(
            soft_delete(&pool, gone.id)
                .await
                .expect("the delete runs")
                .is_some(),
            "the fixture tombstone lands"
        );
        // Another account's post must never ride along.
        let _theirs: Status = post(&pool, &bob, Visibility::Public, "not yours").await;

        let archive = build_for(&state, &alice).await;
        let items = collection(&archive, OUTBOX_ENTRY);
        let got: HashSet<String> = create_object_ids(&items).into_iter().collect();
        let want: HashSet<String> = [
            public.uri.as_str(),
            unlisted.uri.as_str(),
            private.uri.as_str(),
            direct.uri.as_str(),
        ]
        .into_iter()
        .map(str::to_owned)
        .collect();
        assert_eq!(
            got, want,
            "all four visibilities, private and direct included"
        );
        assert_eq!(items.len(), want.len(), "each status exactly once");
        assert!(
            !got.contains(&local.uri),
            "a local post has no document to export"
        );
        assert!(!got.contains(&gone.uri), "a soft-deleted status stays out");
    }

    #[tokio::test]
    async fn likes_and_bookmarks_list_the_marked_uris_in_row_order() {
        let db = crate::test_db::test_db().await.expect("test database slot");
        let pool = db.pool().clone();
        let state = app_state(&pool);
        let alice = user(&pool, "alice").await;
        let one = post(&pool, &alice, Visibility::Public, "one").await;
        let two = post(&pool, &alice, Visibility::Public, "two").await;
        let three = post(&pool, &alice, Visibility::Public, "three").await;
        like(&pool, &alice, two.id).await;
        like(&pool, &alice, one.id).await;
        mark(&pool, &alice, three.id).await;

        let archive = build_for(&state, &alice).await;
        assert_eq!(
            uri_items(collection(&archive, LIKES_ENTRY)),
            [two.uri.as_str(), one.uri.as_str()],
            "likes are exactly the favourites' uris, in favourite id order"
        );
        assert_eq!(
            uri_items(collection(&archive, BOOKMARKS_ENTRY)),
            [three.uri.as_str()],
            "bookmarks are exactly the bookmarks' uris, in bookmark id order"
        );
    }

    #[tokio::test]
    async fn a_whole_page_beyond_the_seam_still_reaches_one_entry() {
        let db = crate::test_db::test_db().await.expect("test database slot");
        let pool = db.pool().clone();
        let state = app_state(&pool);
        let alice = user(&pool, "paged").await;
        let mut uris = Vec::with_capacity(OVER_PAGE);
        let mut ids = Vec::with_capacity(OVER_PAGE);
        for n in 0..OVER_PAGE {
            let row = post(&pool, &alice, Visibility::Public, &format!("post {n}")).await;
            uris.push(row.uri);
            ids.push(row.id);
        }
        for id in &ids {
            like(&pool, &alice, *id).await;
            mark(&pool, &alice, *id).await;
        }

        let archive = build_for(&state, &alice).await;
        let items = collection(&archive, OUTBOX_ENTRY);
        assert_eq!(items.len(), OVER_PAGE, "every page reached the entry");
        let got: HashSet<String> = create_object_ids(&items).into_iter().collect();
        assert_eq!(
            got,
            uris.into_iter().collect::<HashSet<String>>(),
            "no status lost and none doubled across the page seam"
        );
        let likes: HashSet<String> = uri_items(collection(&archive, LIKES_ENTRY))
            .into_iter()
            .collect();
        assert_eq!(likes, got, "the likes crossed their seam too");
        let marks: HashSet<String> = uri_items(collection(&archive, BOOKMARKS_ENTRY))
            .into_iter()
            .collect();
        assert_eq!(marks, got, "and the bookmarks");
    }

    #[tokio::test]
    async fn an_empty_account_yields_three_empty_collections() {
        let db = crate::test_db::test_db().await.expect("test database slot");
        let pool = db.pool().clone();
        let state = app_state(&pool);
        let nobody = user(&pool, "nobody").await;

        let archive = build_for(&state, &nobody).await;
        for name in [OUTBOX_ENTRY, LIKES_ENTRY, BOOKMARKS_ENTRY] {
            assert!(
                entry_text(&archive, name).ends_with(r#""orderedItems": [],"totalItems": 0}"#),
                "{name} states the empty collection verbatim"
            );
            assert!(
                collection(&archive, name).is_empty(),
                "{name} parses as an empty collection"
            );
        }
    }
}
