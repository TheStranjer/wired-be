//! The `custom_emojis` table: shortcodes this instance serves as images.
//!
//! One row per emoji, local or mirrored from a remote instance: Pleroma
//! keeps custom emojis as pack files on disk plus remote emoji maps
//! (`lib/pleroma/emoji.ex`, `lib/pleroma/emoji/pack.ex`); the row shape
//! follows `docs/ARCHITECTURE.md` decision 3. A `NULL`
//! [`domain`](CustomEmoji::domain) marks a local emoji; remote ones carry
//! their origin instance so the same shortcode can coexist per domain
//! (the unique index folds `domain` through `coalesce(domain, '')`).
//!
//! [`upsert`] mints the primary key (`UUIDv7`, decision 4) and writes
//! everything else from a [`NewCustomEmoji`], whose
//! [`Default`](NewCustomEmoji::default) mirrors the column defaults; the
//! row identity it conflicts on is `(shortcode, coalesce(domain, ''))`,
//! so re-upserting an emoji updates it in place and keeps its id.

use chrono::{DateTime, Utc};
use sqlx::PgPool;
use uuid::Uuid;

use crate::{
    error::AppResult,
    util::{id::new_id, like},
};

/// One row of `custom_emojis`: a single shortcode-to-image mapping.
#[derive(Clone, Debug, sqlx::FromRow)]
pub struct CustomEmoji {
    /// `UUIDv7` primary key, minted by [`upsert`] (decision 4).
    pub id: Uuid,
    /// Shortcode without surrounding colons, e.g. `railroad_track`.
    pub shortcode: String,
    /// Origin instance of a remote emoji; `None` for a local one.
    pub domain: Option<String>,
    /// URL of the emoji image (animated included).
    pub image_url: String,
    /// URL of the still variant; `None` means clients use
    /// [`image_url`](Self::image_url) for both.
    pub static_url: Option<String>,
    /// Pack name the emoji belongs to; `None` for an ungrouped one.
    pub category: Option<String>,
    /// Picker tags, e.g. `["transport", "trains"]`; empty by default.
    pub tags: Vec<String>,
    /// Whether clients show the emoji in their pickers.
    pub visible_in_picker: bool,
    /// Moderation kill switch: disabled emojis are served to nobody.
    pub disabled: bool,
    /// When the row was inserted.
    pub inserted_at: DateTime<Utc>,
    /// When the row last changed; refreshed by every [`upsert`].
    pub updated_at: DateTime<Utc>,
}

/// What to upsert: every `custom_emojis` column except the `id` [`upsert`]
/// mints itself and the timestamps the database stamps.
///
/// [`Default::default`] mirrors the column defaults, so a caller sets
/// only what it knows; the usual minimum is `shortcode` and `image_url`.
#[derive(Clone, Debug)]
pub struct NewCustomEmoji {
    /// Shortcode; required, empty in the default.
    pub shortcode: String,
    /// Origin instance; `None` for a local emoji.
    pub domain: Option<String>,
    /// Image URL; required, empty in the default.
    pub image_url: String,
    /// Still-variant URL.
    pub static_url: Option<String>,
    /// Pack name.
    pub category: Option<String>,
    /// Picker tags; empty in the default, like the column.
    pub tags: Vec<String>,
    /// Picker visibility; `true` by default, like the column.
    pub visible_in_picker: bool,
    /// Moderation flag; `false` by default, like the column.
    pub disabled: bool,
}

/// The column defaults: empty required fields, everything optional
/// absent, an empty tag array, visible in pickers and enabled.
impl Default for NewCustomEmoji {
    fn default() -> Self {
        Self {
            shortcode: String::new(),
            domain: None,
            image_url: String::new(),
            static_url: None,
            category: None,
            tags: Vec::new(),
            visible_in_picker: true,
            disabled: false,
        }
    }
}

impl NewCustomEmoji {
    /// A visible, enabled local emoji named `shortcode` whose image is at
    /// `image_url`, with no still URL, no pack and no tags: the shape the
    /// tests and pack importers start from.
    #[must_use]
    pub fn local(shortcode: &str, image_url: &str) -> Self {
        Self {
            shortcode: shortcode.to_owned(),
            image_url: image_url.to_owned(),
            ..Self::default()
        }
    }
}

/// The `INSERT` behind [`upsert`]: the row identity `(shortcode,
/// coalesce(domain, ''))` matches the unique index, so a repeat write
/// updates every content column in place — keeping the minted `id` and
/// the insertion timestamp — instead of failing.
const UPSERT_SQL: &str = "INSERT INTO custom_emojis (
     id, shortcode, domain, image_url, static_url, category, tags,
     visible_in_picker, disabled
) VALUES (
     $1, $2, $3, $4, $5, $6, $7, $8, $9
) ON CONFLICT (shortcode, (coalesce(domain, ''))) DO UPDATE SET
     image_url = EXCLUDED.image_url,
     static_url = EXCLUDED.static_url,
     category = EXCLUDED.category,
     tags = EXCLUDED.tags,
     visible_in_picker = EXCLUDED.visible_in_picker,
     disabled = EXCLUDED.disabled,
     updated_at = now()
RETURNING *";

/// Inserts `emoji` or updates the existing row with the same `(shortcode,
/// domain)` in place, and returns the stored row.
///
/// Idempotent: a second upsert of an unchanged emoji changes nothing but
/// `updated_at`, and the row keeps the `UUIDv7` id the first upsert
/// minted (decision 4). `domain` is part of the identity, so a local and
/// a remote emoji of the same shortcode are two rows.
///
/// # Errors
///
/// Any database failure (connection, encoding) arrives as
/// [`AppError::Database`](crate::error::AppError).
pub async fn upsert(pool: &PgPool, emoji: &NewCustomEmoji) -> AppResult<CustomEmoji> {
    let row = sqlx::query_as::<_, CustomEmoji>(UPSERT_SQL)
        .bind(new_id())
        .bind(emoji.shortcode.as_str())
        .bind(emoji.domain.as_deref())
        .bind(emoji.image_url.as_str())
        .bind(emoji.static_url.as_deref())
        .bind(emoji.category.as_deref())
        .bind(&emoji.tags)
        .bind(emoji.visible_in_picker)
        .bind(emoji.disabled)
        .fetch_one(pool)
        .await?;
    Ok(row)
}

/// The `INSERT` behind [`upsert_remote`]: a new row takes the column
/// defaults, an existing one gets only its image and `updated_at` changed,
/// so the operator's `disabled`, `visible_in_picker`, `category` and `tags`
/// survive every post that uses the emoji.
const UPSERT_REMOTE_SQL: &str = "INSERT INTO custom_emojis (id, shortcode, domain, image_url)
VALUES ($1, $2, $3, $4)
ON CONFLICT (shortcode, (coalesce(domain, ''))) DO UPDATE SET
     image_url = EXCLUDED.image_url,
     updated_at = now()";

/// Records an emoji a remote post announced.
///
/// A new `(shortcode, domain)` is inserted with the column defaults; a
/// stored one has only `image_url` and `updated_at` updated, leaving the
/// moderation flags, pack and tags alone (Mastodon likewise only refreshes
/// `image_remote_url`).
///
/// # Errors
///
/// Any database failure (connection, encoding) arrives as
/// [`AppError::Database`](crate::error::AppError).
pub async fn upsert_remote(
    pool: &PgPool,
    shortcode: &str,
    domain: Option<&str>,
    image_url: &str,
) -> AppResult<()> {
    let _written = sqlx::query(UPSERT_REMOTE_SQL)
        .bind(new_id())
        .bind(shortcode)
        .bind(domain)
        .bind(image_url)
        .execute(pool)
        .await?;
    Ok(())
}

/// Fetches every local (`domain IS NULL`), enabled emoji, ordered by
/// category then shortcode — the list `GET /api/v1/custom_emojis` serves.
///
/// Disabled emojis are hidden the way Pleroma hides them from
/// `Pleroma.Emoji.get_all/0`; `visible_in_picker` is *not* filtered here,
/// it travels in the entity for the client to honour. Rows with no
/// category sort last (`NULLS LAST` is `ORDER BY`'s default), keeping the
/// grouped packs together up front.
///
/// # Errors
///
/// Any database failure (connection, encoding) arrives as
/// [`AppError::Database`](crate::error::AppError).
pub async fn list_local(pool: &PgPool) -> AppResult<Vec<CustomEmoji>> {
    let rows = sqlx::query_as::<_, CustomEmoji>(
        "SELECT * FROM custom_emojis
          WHERE domain IS NULL AND NOT disabled
          ORDER BY category, shortcode",
    )
    .fetch_all(pool)
    .await?;
    Ok(rows)
}

/// Fetches the emojis of `domain` (`None` = local) whose shortcodes are
/// in `shortcodes`, ordered by shortcode. Shortcodes with no row are
/// skipped; an empty `shortcodes` yields an empty `Vec`.
///
/// Unlike [`list_local`] this is a lookup for post-rendering and pack
/// sync, so it returns disabled emojis too — callers that hide them
/// filter the result.
///
/// # Errors
///
/// Any database failure (connection, encoding) arrives as
/// [`AppError::Database`](crate::error::AppError).
pub async fn get_by_shortcodes(
    pool: &PgPool,
    shortcodes: &[String],
    domain: Option<&str>,
) -> AppResult<Vec<CustomEmoji>> {
    let shortcodes = shortcodes.to_vec();
    let rows = sqlx::query_as::<_, CustomEmoji>(
        "SELECT * FROM custom_emojis
          WHERE shortcode = ANY($1) AND domain IS NOT DISTINCT FROM $2
          ORDER BY shortcode",
    )
    .bind(&shortcodes)
    .bind(domain)
    .fetch_all(pool)
    .await?;
    Ok(rows)
}

/// Deletes the emoji `shortcode` of `domain` (`None` = local), reporting
/// whether a row went. Other domains carrying the same shortcode are
/// untouched; this is how a pack removal retires one emoji.
///
/// # Errors
///
/// Any database failure (connection, encoding) arrives as
/// [`AppError::Database`](crate::error::AppError).
pub async fn delete(pool: &PgPool, shortcode: &str, domain: Option<&str>) -> AppResult<bool> {
    let deleted = sqlx::query(
        "DELETE FROM custom_emojis
          WHERE shortcode = $1 AND domain IS NOT DISTINCT FROM $2",
    )
    .bind(shortcode)
    .bind(domain)
    .execute(pool)
    .await?;
    Ok(deleted.rows_affected() > 0)
}

/// Deletes the local (`domain IS NULL`) emojis whose `image_url` starts
/// with `url_prefix` and whose shortcode is not in `keep`, returning how
/// many rows went.
///
/// This is the sweep [`sync_custom_emojis`] runs after re-upserting every
/// pack file: local emojis served from under `{base_url}/emoji/` that
/// no pack names any more leave the table, while remote rows, rows of other
/// hosts and the still-present pack emojis stay.
///
/// [`sync_custom_emojis`]: crate::services::emoji_packs::sync_custom_emojis
///
/// # Errors
///
/// Any database failure (connection, encoding) arrives as
/// [`AppError::Database`](crate::error::AppError).
pub async fn delete_local_under(
    pool: &PgPool,
    url_prefix: &str,
    keep: &[String],
) -> AppResult<u64> {
    let pattern = like::prefix(url_prefix);
    let deleted = sqlx::query(
        "DELETE FROM custom_emojis
          WHERE domain IS NULL
            AND image_url LIKE $1
            AND NOT (shortcode = ANY($2))",
    )
    .bind(&pattern)
    .bind(keep)
    .execute(pool)
    .await?;
    Ok(deleted.rows_affected())
}

#[cfg(test)]
mod tests {
    use sqlx::PgPool;

    use super::*;

    /// The instance origin the fixture URLs point at.
    const BASE: &str = "https://test.example";

    /// A local emoji named `shortcode` in pack `category`, stored under
    /// [`BASE`].
    fn local_emoji(shortcode: &str, category: Option<&str>) -> NewCustomEmoji {
        NewCustomEmoji {
            category: category.map(str::to_owned),
            ..NewCustomEmoji::local(shortcode, &format!("{BASE}/emoji/custom/{shortcode}.png"))
        }
    }

    /// Reads the local row named `shortcode` back, panicking if absent.
    async fn fetch(pool: &PgPool, shortcode: &str) -> CustomEmoji {
        get_by_shortcodes(pool, &[shortcode.to_owned()], None)
            .await
            .unwrap()
            .into_iter()
            .next()
            .unwrap_or_else(|| panic!("local emoji {shortcode} exists"))
    }

    /// Upserts `emoji`, discarding the stored row.
    async fn seed(pool: &PgPool, emoji: &NewCustomEmoji) {
        let _stored: CustomEmoji = upsert(pool, emoji).await.unwrap();
    }

    /// Flags the local emoji named `shortcode` as disabled, asserting the
    /// fixture row exists.
    async fn set_disabled(pool: &PgPool, shortcode: &str) {
        let summary = sqlx::query("UPDATE custom_emojis SET disabled = true WHERE shortcode = $1")
            .bind(shortcode)
            .execute(pool)
            .await
            .unwrap();
        assert_eq!(summary.rows_affected(), 1, "the {shortcode} row exists");
    }

    /// The shortcodes of `rows`, in row order.
    fn shortcodes(rows: &[CustomEmoji]) -> Vec<&str> {
        rows.iter().map(|row| row.shortcode.as_str()).collect()
    }

    #[tokio::test]
    async fn upsert_round_trips_every_column() {
        let db = crate::test_db::test_db().await.expect("test database slot");
        let pool = db.pool().clone();
        let stored = upsert(
            &pool,
            &NewCustomEmoji {
                domain: Some("remote.example".to_owned()),
                static_url: Some(format!("{BASE}/emoji/custom/cat-still.png")),
                category: Some("cats".to_owned()),
                tags: vec!["cat".to_owned(), "animal".to_owned()],
                visible_in_picker: false,
                disabled: true,
                ..NewCustomEmoji::local("cat", "https://remote.example/emoji/cat.gif")
            },
        )
        .await
        .unwrap();
        assert!(stored.id.get_version_num() == 7, "id is a UUIDv7");
        assert_eq!(stored.shortcode, "cat", "shortcode round trips");
        assert_eq!(
            stored.domain.as_deref(),
            Some("remote.example"),
            "domain round trips"
        );
        assert_eq!(
            stored.image_url, "https://remote.example/emoji/cat.gif",
            "image url round trips"
        );
        assert_eq!(
            stored.static_url.as_deref(),
            Some("https://test.example/emoji/custom/cat-still.png"),
            "static url round trips"
        );
        assert_eq!(stored.category.as_deref(), Some("cats"), "pack round trips");
        assert_eq!(stored.tags, ["cat", "animal"], "tags round trips");
        assert!(!stored.visible_in_picker, "picker visibility round trips");
        assert!(stored.disabled, "the disabled flag round trips");
        assert_eq!(
            stored.inserted_at, stored.updated_at,
            "a fresh row is stamped once"
        );
    }

    #[tokio::test]
    async fn upsert_defaults_match_the_column_defaults() {
        let db = crate::test_db::test_db().await.expect("test database slot");
        let pool = db.pool().clone();
        let stored = upsert(
            &pool,
            &NewCustomEmoji::local("blob", &format!("{BASE}/e/blob.png")),
        )
        .await
        .unwrap();
        assert!(stored.domain.is_none(), "a default emoji is local");
        assert!(stored.static_url.is_none(), "no still url by default");
        assert!(stored.category.is_none(), "no pack by default");
        assert!(stored.tags.is_empty(), "the tag array defaults to empty");
        assert!(stored.visible_in_picker, "the picker default is visible");
        assert!(!stored.disabled, "fresh emojis are enabled");
    }

    #[tokio::test]
    async fn upsert_is_idempotent_and_updates_in_place() {
        let db = crate::test_db::test_db().await.expect("test database slot");
        let pool = db.pool().clone();
        let first = upsert(&pool, &local_emoji("cat", Some("cats")))
            .await
            .unwrap();
        let again = upsert(&pool, &local_emoji("cat", Some("cats")))
            .await
            .unwrap();
        assert_eq!(again.id, first.id, "a repeat upsert keeps the row id");
        assert_eq!(
            again.inserted_at, first.inserted_at,
            "and the insertion stamp"
        );
        assert_eq!(again.category, first.category, "unchanged fields stay");
        assert_eq!(
            fetch(&pool, "cat").await.id,
            first.id,
            "there is still exactly one row for the shortcode"
        );

        let moved = upsert(
            &pool,
            &NewCustomEmoji {
                category: Some("animals".to_owned()),
                tags: vec!["pet".to_owned()],
                disabled: true,
                ..local_emoji("cat", Some("cats"))
            },
        )
        .await
        .unwrap();
        assert_eq!(moved.id, first.id, "an update rewrites the same row");
        assert_eq!(
            moved.category.as_deref(),
            Some("animals"),
            "the pack is rewritten"
        );
        assert_eq!(moved.tags, ["pet"], "the tags are rewritten");
        assert!(moved.disabled, "the flag is rewritten");
        assert!(
            moved.updated_at >= first.updated_at,
            "the update stamp moves forward"
        );
    }

    #[tokio::test]
    async fn the_same_shortcode_coexists_across_domains() {
        let db = crate::test_db::test_db().await.expect("test database slot");
        let pool = db.pool().clone();
        let mine = upsert(&pool, &local_emoji("cat", None)).await.unwrap();
        let theirs = upsert(
            &pool,
            &NewCustomEmoji {
                domain: Some("remote.example".to_owned()),
                ..local_emoji("cat", None)
            },
        )
        .await
        .unwrap();
        assert_ne!(mine.id, theirs.id, "local and remote cats are two rows");
        assert!(
            mine.domain.is_none() && theirs.domain.is_some(),
            "each keeps its own domain"
        );

        // Upserting one domain again must not touch the other.
        let again = upsert(
            &pool,
            &NewCustomEmoji {
                image_url: format!("{BASE}/emoji/custom/cat-v2.png"),
                ..local_emoji("cat", None)
            },
        )
        .await
        .unwrap();
        assert_eq!(again.id, mine.id, "the local row is updated in place");
        let local = fetch(&pool, "cat").await;
        assert_eq!(local.image_url, again.image_url, "the local rewrite landed");
        let remote = get_by_shortcodes(&pool, &["cat".to_owned()], Some("remote.example"))
            .await
            .unwrap();
        assert_eq!(shortcodes(&remote), ["cat"], "the remote cat still exists");
        assert_eq!(
            remote.first().map(|row| row.image_url.as_str()),
            Some(theirs.image_url.as_str()),
            "and was not rewritten by the local upsert"
        );
    }

    #[tokio::test]
    async fn upsert_remote_inserts_defaults_then_updates_only_the_image() {
        let db = crate::test_db::test_db().await.expect("test database slot");
        let pool = db.pool().clone();
        let domain = Some("remote.example");
        upsert_remote(&pool, "wave", domain, "https://remote.example/1.png")
            .await
            .unwrap();
        let first = get_by_shortcodes(&pool, &["wave".to_owned()], domain)
            .await
            .unwrap();
        let [first] = first.as_slice() else {
            panic!("one row expected, got {}", first.len());
        };
        assert!(!first.disabled && first.visible_in_picker, "the defaults");

        let flagged = NewCustomEmoji {
            domain: domain.map(str::to_owned),
            image_url: first.image_url.clone(),
            shortcode: "wave".to_owned(),
            category: Some("pack".to_owned()),
            tags: vec!["hi".to_owned()],
            visible_in_picker: false,
            disabled: true,
            ..NewCustomEmoji::default()
        };
        seed(&pool, &flagged).await;
        upsert_remote(&pool, "wave", domain, "https://remote.example/2.png")
            .await
            .unwrap();

        let after = get_by_shortcodes(&pool, &["wave".to_owned()], domain)
            .await
            .unwrap();
        let [after] = after.as_slice() else {
            panic!("one row expected, got {}", after.len());
        };
        assert_eq!(after.id, first.id, "the row is updated in place");
        assert_eq!(after.image_url, "https://remote.example/2.png", "new image");
        assert!(after.disabled, "still disabled");
        assert!(!after.visible_in_picker, "still hidden from the picker");
        assert_eq!(after.category.as_deref(), Some("pack"), "pack kept");
        assert_eq!(after.tags, ["hi"], "tags kept");
    }

    #[tokio::test]
    async fn list_local_serves_enabled_local_emojis_ordered() {
        let db = crate::test_db::test_db().await.expect("test database slot");
        let pool = db.pool().clone();
        seed(&pool, &local_emoji("uncategorised", None)).await;
        seed(&pool, &local_emoji("zebra", Some("animals"))).await;
        seed(&pool, &local_emoji("aardvark", Some("animals"))).await;
        seed(&pool, &local_emoji("hidden", Some("secret"))).await;
        set_disabled(&pool, "hidden").await;
        seed(
            &pool,
            &NewCustomEmoji {
                domain: Some("remote.example".to_owned()),
                ..local_emoji("remote", None)
            },
        )
        .await;

        let listed = list_local(&pool).await.unwrap();
        assert_eq!(
            shortcodes(&listed),
            ["aardvark", "zebra", "uncategorised"],
            "category order, then shortcode, no pack last; remote and disabled are out"
        );
        assert!(
            listed
                .iter()
                .all(|row| row.domain.is_none() && !row.disabled),
            "every listed row is a local, enabled emoji"
        );
    }

    #[tokio::test]
    async fn list_local_of_an_empty_table_is_empty() {
        let db = crate::test_db::test_db().await.expect("test database slot");
        let pool = db.pool().clone();
        assert!(
            list_local(&pool).await.unwrap().is_empty(),
            "no rows, no emojis"
        );
    }

    #[tokio::test]
    async fn get_by_shortcodes_selects_domain_and_skips_misses() {
        let db = crate::test_db::test_db().await.expect("test database slot");
        let pool = db.pool().clone();
        seed(&pool, &local_emoji("cat", None)).await;
        seed(&pool, &local_emoji("dog", Some("pets"))).await;
        seed(
            &pool,
            &NewCustomEmoji {
                domain: Some("remote.example".to_owned()),
                ..local_emoji("cat", None)
            },
        )
        .await;
        set_disabled(&pool, "dog").await;

        let locals = get_by_shortcodes(
            &pool,
            &["cat".to_owned(), "dog".to_owned(), "ghost".to_owned()],
            None,
        )
        .await
        .unwrap();
        assert_eq!(
            shortcodes(&locals),
            ["cat", "dog"],
            "unknown shortcodes are skipped, disabled local rows are still found"
        );

        let remote = get_by_shortcodes(&pool, &["cat".to_owned()], Some("remote.example"))
            .await
            .unwrap();
        assert_eq!(
            shortcodes(&remote),
            ["cat"],
            "the remote domain has its cat"
        );
        assert!(
            remote.iter().all(|row| row.domain.is_some()),
            "the local cat is not in it"
        );

        assert!(
            get_by_shortcodes(&pool, &[], None)
                .await
                .unwrap()
                .is_empty(),
            "no shortcodes, no rows"
        );
        assert!(
            get_by_shortcodes(&pool, &["cat".to_owned()], Some("nowhere.example"))
                .await
                .unwrap()
                .is_empty(),
            "an unknown domain finds nothing"
        );
    }

    #[tokio::test]
    async fn delete_removes_one_domain_row_once() {
        let db = crate::test_db::test_db().await.expect("test database slot");
        let pool = db.pool().clone();
        seed(&pool, &local_emoji("cat", None)).await;
        seed(
            &pool,
            &NewCustomEmoji {
                domain: Some("remote.example".to_owned()),
                ..local_emoji("cat", None)
            },
        )
        .await;

        assert!(
            delete(&pool, "cat", Some("remote.example")).await.unwrap(),
            "the remote row goes first"
        );
        assert!(
            delete(&pool, "cat", None).await.unwrap(),
            "then the local one"
        );
        assert!(
            !(delete(&pool, "cat", None).await.unwrap()),
            "deleting again finds nothing"
        );
        assert!(
            !(delete(&pool, "ghost", None).await.unwrap()),
            "an unknown shortcode deletes nothing"
        );
        assert!(
            get_by_shortcodes(&pool, &["cat".to_owned()], None)
                .await
                .unwrap()
                .is_empty(),
            "no cat rows are left"
        );
    }

    #[tokio::test]
    async fn delete_local_under_sweeps_only_prefix_local_rows() {
        let db = crate::test_db::test_db().await.expect("test database slot");
        let pool = db.pool().clone();
        let prefix = format!("{BASE}/emoji/");
        seed(
            &pool,
            &NewCustomEmoji::local("kept", &format!("{prefix}pack/kept.png")),
        )
        .await;
        seed(
            &pool,
            &NewCustomEmoji::local("stale", &format!("{prefix}pack/stale.png")),
        )
        .await;
        seed(
            &pool,
            &NewCustomEmoji::local("elsewhere", "https://cdn.example/emoji/pack/x.png"),
        )
        .await;
        seed(
            &pool,
            &NewCustomEmoji {
                domain: Some("remote.example".to_owned()),
                ..NewCustomEmoji::local("remote-in", &format!("{prefix}pack/remote.png"))
            },
        )
        .await;

        let keep = vec!["kept".to_owned()];
        let removed = delete_local_under(&pool, &prefix, &keep).await.unwrap();
        assert_eq!(removed, 1, "only the stale local row inside goes");
        for shortcode in ["kept", "elsewhere", "remote-in"] {
            let domain = (shortcode == "remote-in").then_some("remote.example");
            assert_eq!(
                get_by_shortcodes(&pool, &[shortcode.to_owned()], domain)
                    .await
                    .unwrap()
                    .len(),
                1,
                "{shortcode} survives the sweep"
            );
        }
        assert!(
            get_by_shortcodes(&pool, &["stale".to_owned()], None)
                .await
                .unwrap()
                .is_empty(),
            "the swept row is gone"
        );
    }

    #[tokio::test]
    async fn delete_local_under_matches_prefix_bytes_literally() {
        let db = crate::test_db::test_db().await.expect("test database slot");
        let pool = db.pool().clone();
        // The `_` in the prefix must match an underscore, not any character:
        // the underscored host goes, the look-alike without one stays.
        seed(
            &pool,
            &NewCustomEmoji::local("under", "https://ex_ample.test/emoji/pack/x.png"),
        )
        .await;
        seed(
            &pool,
            &NewCustomEmoji::local("plain", "https://exxample.test/emoji/pack/x.png"),
        )
        .await;

        let removed = delete_local_under(&pool, "https://ex_ample.test/emoji/", &[])
            .await
            .unwrap();
        assert_eq!(removed, 1, "the underscore is not a single-char wildcard");
        assert!(
            get_by_shortcodes(&pool, &["under".to_owned()], None)
                .await
                .unwrap()
                .is_empty(),
            "the row inside the escaped prefix is gone"
        );
        assert_eq!(
            get_by_shortcodes(&pool, &["plain".to_owned()], None)
                .await
                .unwrap()
                .len(),
            1,
            "the look-alike host survives"
        );
    }
}
