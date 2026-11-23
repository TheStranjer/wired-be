//! Filling the other direction: [`import_packs`] writes the missing
//! `pack.json` manifests from the loose image files a manual install left
//! behind, and [`sync_custom_emojis`] reconciles the packs with the
//! `custom_emojis` table.

use std::{collections::BTreeMap, ffi::OsStr, path::Path};

use sqlx::PgPool;
use tracing::debug;

use crate::{
    error::{AppError, AppResult},
    models::custom_emoji::{self, NewCustomEmoji},
};

use super::{
    PACK_IMAGE_EXTENSIONS, Pack, PackMeta, emoji_image_url, file_name_valid, list_packs, load_pack,
    names::io_error, names::pack_name_valid, save_pack, shortcode_valid,
};

/// Pleroma's `import_from_filesystem`: write the missing pack manifests.
///
/// Every pack directory under the packs root `dir` that lacks a valid
/// `pack.json` — a loose pile of images a manual install left behind, or
/// a directory whose manifest cannot be parsed — gets one written from
/// the image files it holds, and its name is reported.
///
/// Directories with a valid manifest are left exactly as they are; so
/// are hidden or otherwise unusable names (their manifests could not be
/// [saved](save_pack) safely anyway) and plain files under the root. A
/// missing root imports nothing. The files themselves are never moved
/// or renamed — only `pack.json` is written.
///
/// Returns the imported pack names, sorted.
///
/// # Errors
///
/// [`AppError::Internal`] for a filesystem failure; the walk only ever
/// touches names it can store.
pub async fn import_packs(dir: &Path) -> AppResult<Vec<String>> {
    let Ok(mut entries) = tokio::fs::read_dir(dir).await else {
        return Ok(Vec::new());
    };
    let mut imported = Vec::new();
    while let Some(entry) = entries
        .next_entry()
        .await
        .map_err(|err| io_error("list emoji packs", dir, &err))?
    {
        let Some(name) = entry.file_name().to_str().map(str::to_owned) else {
            continue;
        };
        if !pack_name_valid(&name) || !entry.file_type().await.is_ok_and(|kind| kind.is_dir()) {
            continue;
        }
        match load_pack(dir, &name).await {
            Ok(_) => {}
            Err(AppError::NotFound | AppError::Internal(_)) => {
                let files = collect_pack_images(&dir.join(&name)).await?;
                let pack = Pack {
                    pack: PackMeta::default(),
                    files,
                };
                save_pack(dir, &name, &pack).await?;
                imported.push(name);
            }
            Err(err) => return Err(err),
        }
    }
    imported.sort_unstable();
    debug!(packs = imported.len(), "imported emoji pack manifests");
    Ok(imported)
}

/// The `files` map [`import_packs`] writes for a pack directory: every
/// accepted image under it, recursively, keyed by the stem of its file
/// name — Pleroma's `files_from_path` without the `emoji.txt` spelling
/// the file routes do not speak. The walk is a work stack, not
/// recursion; names that could not be stored, and stems that could not
/// be shortcodes, are skipped.
async fn collect_pack_images(root: &Path) -> AppResult<BTreeMap<String, String>> {
    let mut files = BTreeMap::new();
    let mut pending = vec![root.to_path_buf()];
    while let Some(current) = pending.pop() {
        let mut entries = tokio::fs::read_dir(&current)
            .await
            .map_err(|err| io_error("list emoji pack", &current, &err))?;
        while let Some(entry) = entries
            .next_entry()
            .await
            .map_err(|err| io_error("list emoji pack", &current, &err))?
        {
            let path = entry.path();
            if entry.file_type().await.is_ok_and(|kind| kind.is_dir()) {
                pending.push(path);
                continue;
            }
            let Some(relative) = path
                .strip_prefix(root)
                .ok()
                .and_then(|rel| rel.to_str())
                .map(str::to_owned)
            else {
                continue;
            };
            let Some(shortcode) = stem_shortcode(&relative) else {
                continue;
            };
            let _entry: Option<String> = files.insert(shortcode, relative);
        }
    }
    Ok(files)
}

/// The shortcode a loose pack file gets: its extension must be one of
/// [`PACK_IMAGE_EXTENSIONS`] and its file stem a valid shortcode, or
/// there is none.
fn stem_shortcode(file: &str) -> Option<String> {
    let path = Path::new(file);
    if !pack_image_extension(path) {
        return None;
    }
    let stem = path.file_stem().and_then(OsStr::to_str)?;
    shortcode_valid(stem).then(|| stem.to_owned())
}

/// Whether `path` ends in one of [`PACK_IMAGE_EXTENSIONS`], case
/// insensitively like Pleroma's `Path.extname` comparison.
fn pack_image_extension(path: &Path) -> bool {
    path.extension()
        .and_then(OsStr::to_str)
        .is_some_and(|ext| PACK_IMAGE_EXTENSIONS.contains(&ext.to_ascii_lowercase().as_str()))
}

/// Reconciles the `custom_emojis` table with the packs under `dir`.
///
/// Every pack file is upserted as a local emoji (`category` and sole tag =
/// the pack name, `image_url` = [`emoji_image_url`]), and local emojis
/// under the `{base_url}/emoji/` prefix that no pack names any more are
/// deleted. Remote emojis and rows outside the prefix are never touched.
/// Call this at startup and after every change to the packs directory.
///
/// Returns the number of pack files stored.
///
/// # Errors
///
/// [`AppError::Internal`] for a packs directory that cannot be listed, and
/// any database failure as [`AppError::Database`](crate::error::AppError).
pub async fn sync_custom_emojis(pool: &PgPool, base_url: &str, dir: &Path) -> AppResult<usize> {
    let packs = list_packs(dir).await?;
    let mut stored = Vec::new();
    for (name, pack) in &packs {
        for (shortcode, file) in &pack.files {
            if !shortcode_valid(shortcode) || !file_name_valid(file) {
                debug!(
                    pack = name.as_str(),
                    shortcode = shortcode.as_str(),
                    file = file.as_str(),
                    "skipped a pack entry with an invalid shortcode or file name"
                );
                continue;
            }
            let new = NewCustomEmoji {
                category: Some(name.clone()),
                tags: vec![name.clone()],
                ..NewCustomEmoji::local(shortcode, &emoji_image_url(base_url, name, file))
            };
            let _row: custom_emoji::CustomEmoji = custom_emoji::upsert(pool, &new).await?;
            stored.push(shortcode.clone());
        }
    }
    let prefix = format!("{base_url}/emoji/");
    let removed = custom_emoji::delete_local_under(pool, &prefix, &stored).await?;
    debug!(
        packs = packs.len(),
        emojis = stored.len(),
        removed,
        "synced emoji packs into custom_emojis"
    );
    Ok(stored.len())
}

#[cfg(test)]
mod tests {
    use crate::services::emoji_packs::PACK_FILE;
    use crate::services::emoji_packs::test_support::{
        BASE, GIF_BYTES, JPEG_BYTES, PNG_BYTES, pack_with, seed_pack, temp_dir, write_file,
    };

    use super::*;

    /// Seeds the fixture for [`import_packs`]: a valid pack, a loose
    /// image directory, a directory with a broken manifest, a hidden
    /// directory and a stray file under the root.
    async fn seed_import_fixture(dir: &Path) {
        save_pack(dir, "cats", &pack_with(true, &[("cat", "cat.png")]))
            .await
            .unwrap();
        write_file(&dir.join("cats").join("cat.png"), PNG_BYTES);
        write_file(&dir.join("dogs").join("dog.png"), PNG_BYTES);
        write_file(&dir.join("dogs").join("sub/puppy.gif"), GIF_BYTES);
        write_file(&dir.join("dogs").join("notes.txt"), b"prose".as_slice());
        write_file(
            &dir.join("dogs").join("emoji.txt"),
            b"dog, dog.png\n".as_slice(),
        );
        write_file(
            &dir.join("broken").join(PACK_FILE),
            b"{ not json".as_slice(),
        );
        write_file(&dir.join("broken").join("a.jpg"), JPEG_BYTES);
        write_file(&dir.join(".secret").join("x.png"), PNG_BYTES);
        write_file(&dir.join("stray.png"), PNG_BYTES);
    }

    #[tokio::test]
    async fn import_writes_manifests_for_loose_directories_only() {
        let dir = temp_dir();
        seed_import_fixture(dir.path()).await;
        let cats_before = std::fs::read_to_string(dir.path().join("cats").join(PACK_FILE)).unwrap();

        let names = import_packs(dir.path()).await.unwrap();
        assert_eq!(
            names,
            ["broken", "dogs"],
            "the valid pack, the hidden name and the stray file stand aside"
        );
        assert_eq!(
            std::fs::read_to_string(dir.path().join("cats").join(PACK_FILE)).unwrap(),
            cats_before,
            "the valid manifest was not rewritten"
        );
        assert_eq!(
            import_packs(dir.path()).await.unwrap(),
            Vec::<String>::new(),
            "a second import finds nothing left to write"
        );

        let dogs = load_pack(dir.path(), "dogs").await.unwrap();
        assert_eq!(
            dogs.files
                .iter()
                .map(|(code, file)| (code.as_str(), file.as_str()))
                .collect::<Vec<_>>(),
            [("dog", "dog.png"), ("puppy", "sub/puppy.gif")],
            "stems key the images, nested paths kept, texts ignored"
        );
        assert!(!dogs.pack.share_files, "imported packs start unshared");
        let broken = load_pack(dir.path(), "broken").await.unwrap();
        assert_eq!(
            broken.files.get("a").map(String::as_str),
            Some("a.jpg"),
            "the broken manifest was replaced from the images"
        );
        assert!(
            !dir.path().join(".secret").join(PACK_FILE).exists(),
            "the hidden directory keeps no manifest it could not be read by"
        );
    }

    #[tokio::test]
    async fn import_of_a_missing_or_empty_root_imports_nothing() {
        let dir = temp_dir();
        assert_eq!(
            import_packs(&dir.path().join("nowhere")).await.unwrap(),
            Vec::<String>::new(),
            "a missing root imports nothing"
        );
        assert_eq!(
            import_packs(dir.path()).await.unwrap(),
            Vec::<String>::new(),
            "an empty root imports nothing"
        );
    }

    /// The `(shortcode, id)` pairs of every local row, for id stability.
    async fn local_ids(pool: &PgPool) -> Vec<(String, uuid::Uuid)> {
        custom_emoji::list_local(pool)
            .await
            .unwrap()
            .into_iter()
            .map(|row| (row.shortcode, row.id))
            .collect()
    }

    /// One synced row as `(shortcode, image_url, first tag, category)`.
    type RowFacts<'a> = (&'a str, &'a str, &'a str, Option<&'a str>);

    #[tokio::test]
    async fn sync_stores_every_pack_file_as_a_local_emoji() {
        let db = crate::test_db::test_db().await.expect("test database slot");
        let pool = db.pool().clone();
        let dir = temp_dir();
        seed_pack(dir.path(), "cats", &[("cat", "cat.png")]).await;
        save_pack(
            dir.path(),
            "dogs",
            &pack_with(false, &[("dog", "sub/dog.png")]),
        )
        .await
        .unwrap();
        write_file(&dir.path().join("dogs").join("sub/dog.png"), PNG_BYTES);

        let stored = sync_custom_emojis(&pool, BASE, dir.path()).await.unwrap();
        assert_eq!(stored, 2, "both pack files counted");
        let rows = custom_emoji::list_local(&pool).await.unwrap();
        let got: Vec<RowFacts<'_>> = rows
            .iter()
            .map(|row| {
                (
                    row.shortcode.as_str(),
                    row.image_url.as_str(),
                    row.tags.first().map_or_default(String::as_str),
                    row.category.as_deref(),
                )
            })
            .collect();
        assert_eq!(
            got,
            [
                (
                    "cat",
                    "https://test.example/emoji/cats/cat.png",
                    "cats",
                    Some("cats")
                ),
                (
                    "dog",
                    "https://test.example/emoji/dogs/sub/dog.png",
                    "dogs",
                    Some("dogs")
                ),
            ],
            "category = pack name, tag = pack name, url = the served address"
        );
    }

    #[tokio::test]
    async fn sync_is_idempotent_and_keeps_row_ids() {
        let db = crate::test_db::test_db().await.expect("test database slot");
        let pool = db.pool().clone();
        let dir = temp_dir();
        seed_pack(dir.path(), "cats", &[("cat", "cat.png")]).await;
        let first = sync_custom_emojis(&pool, BASE, dir.path()).await.unwrap();
        let first_ids = local_ids(&pool).await;
        let second = sync_custom_emojis(&pool, BASE, dir.path()).await.unwrap();
        let second_ids = local_ids(&pool).await;
        assert_eq!(first, second, "the same count every run");
        assert_eq!(
            first_ids, second_ids,
            "rows are updated in place (decision 4)"
        );
    }

    #[tokio::test]
    async fn sync_forgets_pack_files_that_are_gone() {
        let db = crate::test_db::test_db().await.expect("test database slot");
        let pool = db.pool().clone();
        let dir = temp_dir();
        seed_pack(
            dir.path(),
            "cats",
            &[("cat", "cat.png"), ("gone", "gone.png")],
        )
        .await;
        let seeded: usize = sync_custom_emojis(&pool, BASE, dir.path()).await.unwrap();
        assert_eq!(seeded, 2, "both files synced before the pack shrinks");
        let kept_ids = local_ids(&pool).await;
        // An emoji that is not pack-served, and a remote one under the same
        // kind of url, must survive the sweep.
        let _upload: custom_emoji::CustomEmoji = custom_emoji::upsert(
            &pool,
            &NewCustomEmoji::local("upload", "https://cdn.example/custom/upload.png"),
        )
        .await
        .unwrap();
        let _remote: custom_emoji::CustomEmoji = custom_emoji::upsert(
            &pool,
            &NewCustomEmoji {
                domain: Some("remote.example".to_owned()),
                ..NewCustomEmoji::local("remote-cat", "https://test.example/emoji/remote/cat.png")
            },
        )
        .await
        .unwrap();

        save_pack(dir.path(), "cats", &pack_with(true, &[("cat", "cat.png")]))
            .await
            .unwrap();
        let stored = sync_custom_emojis(&pool, BASE, dir.path()).await.unwrap();
        assert_eq!(stored, 1, "only the remaining file counts");
        let rows = custom_emoji::list_local(&pool).await.unwrap();
        let codes: Vec<&str> = rows.iter().map(|row| row.shortcode.as_str()).collect();
        assert_eq!(
            codes,
            ["cat", "upload"],
            "the stale pack emoji is gone; non-prefix and remote rows stay"
        );
        let after_ids = local_ids(&pool).await;
        assert!(
            after_ids.iter().all(|(code, id)| kept_ids
                .iter()
                .find(|(kept_code, _)| kept_code == code)
                .is_none_or(|(_, kept_id)| kept_id == id)),
            "surviving pack emojis keep their ids"
        );
    }

    #[tokio::test]
    async fn sync_skips_entries_with_an_invalid_shortcode_or_file_name() {
        let db = crate::test_db::test_db().await.expect("test database slot");
        let pool = db.pool().clone();
        let dir = temp_dir();
        save_pack(
            dir.path(),
            "cats",
            &pack_with(
                true,
                &[
                    ("cat", "cat.png"),
                    ("bad code", "cat.png"),
                    ("<x>", "cat.png"),
                    ("sneaky", "../cat.png"),
                ],
            ),
        )
        .await
        .unwrap();
        write_file(&dir.path().join("cats").join("cat.png"), PNG_BYTES);

        let stored = sync_custom_emojis(&pool, BASE, dir.path()).await.unwrap();
        assert_eq!(stored, 1, "only the valid entry counts");
        let rows = custom_emoji::list_local(&pool).await.unwrap();
        let codes: Vec<&str> = rows.iter().map(|row| row.shortcode.as_str()).collect();
        assert_eq!(codes, ["cat"], "no invalid shortcode or path is stored");
    }

    #[tokio::test]
    async fn sync_of_an_empty_or_missing_root_stores_nothing() {
        let db = crate::test_db::test_db().await.expect("test database slot");
        let pool = db.pool().clone();
        let dir = temp_dir();
        assert_eq!(
            sync_custom_emojis(&pool, BASE, &dir.path().join("nowhere"))
                .await
                .unwrap(),
            0,
            "a missing root syncs to zero"
        );
        assert_eq!(
            sync_custom_emojis(&pool, BASE, dir.path()).await.unwrap(),
            0,
            "an empty root syncs to zero"
        );
        assert!(
            custom_emoji::list_local(&pool).await.unwrap().is_empty(),
            "and stores no rows"
        );
    }
}
