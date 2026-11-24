//! The `user_domain_blocks` table: the domains one account blocks.
//!
//! Reference: Pleroma's `Pleroma.User.block_domain/2` and
//! `unblock_domain/2` (`lib/pleroma/user.ex`), which keep the blocked
//! domains as a `domain_blocks` text array on the users row. Here each
//! block is its own row: the same facts — one `(user, domain)` pair per
//! block, idempotent on that key — but keyed by a `UUIDv7` (decision 4),
//! which is what lets `GET /api/v1/domain_blocks` page with the usual
//! id cursors and lets [`crate::models::relationship`] ask the whole
//! `domain_blocking` question in one join.
//!
//! Domains are stored in one canonical spelling: [`normalize_domain`]
//! lowercases, strips a leading `@`, a leading `scheme://` and a trailing
//! `/`, and requires the remainder to contain a dot (so a bare
//! `localhost` is not a blockable domain). The endpoint runs the input
//! through it before calling [`block_domain`] / [`unblock_domain`], so
//! what those write is already canonical and one domain has exactly one
//! spelling per blocker.
//!
//! Deleting a user takes their blocks with it through the foreign key's
//! cascade.

use chrono::{DateTime, Utc};
use sqlx::{PgPool, Postgres, QueryBuilder};
use uuid::Uuid;

use crate::{error::AppResult, pagination::PageParams, util::id::new_id};

/// [`block_domain`]: the id this module mints (decision 4) and the pair
/// the caller names, with `inserted_at` left to the column default.
/// `ON CONFLICT (user_id, domain) DO NOTHING` is the idempotence the
/// endpoint promises: blocking a domain that is already blocked writes
/// nothing, exactly as Pleroma's `block_domain` returns the unchanged
/// user when its array already `Enum.member?`s the domain.
const BLOCK_SQL: &str = "INSERT INTO user_domain_blocks (id, user_id, domain)
     VALUES ($1, $2, $3)
ON CONFLICT (user_id, domain) DO NOTHING";

/// [`unblock_domain`]: one blocker's one domain; an unblocked or never
/// blocked domain deletes nothing, so unblocking is idempotent too.
const UNBLOCK_SQL: &str = "DELETE FROM user_domain_blocks WHERE user_id = $1 AND domain = $2";

/// The leading text of [`domains_page`]: the statement stops where its
/// one placeholder belongs, because that is where the function pushes
/// its bind and the builder is what numbers it `$1`; the keyset
/// [`PageParams::apply_keyset`] then appends runs on the block row's own
/// `id`.
const PAGE_SQL: &str = "SELECT id, user_id, domain, inserted_at FROM user_domain_blocks \
                        WHERE user_id = ";

/// [`blocked_domains`]: every domain one account blocks, oldest block
/// first on the id whose text sorts in creation order (decision 4). The
/// order is a stability promise, not semantics: callers filter, they
/// do not page.
const ALL_SQL: &str = "SELECT domain FROM user_domain_blocks WHERE user_id = $1 ORDER BY id";

/// [`is_domain_blocked`]: the one fact, asked as an existence check so a
/// blocked domain never costs more than a `true`.
const EXISTS_SQL: &str = "SELECT EXISTS (SELECT 1 FROM user_domain_blocks
     WHERE user_id = $1 AND domain = $2)";

/// One row of `user_domain_blocks`: one domain one account blocks.
#[derive(Clone, Debug, PartialEq, Eq, sqlx::FromRow)]
pub struct DomainBlock {
    /// `UUIDv7` primary key, minted by [`block_domain`] (decision 4); the
    /// id the endpoint's `Link` header pages by.
    pub id: Uuid,
    /// The blocking account; every read and write of a block is scoped
    /// to it.
    pub user_id: Uuid,
    /// The blocked domain, in [`normalize_domain`]'s canonical spelling.
    pub domain: String,
    /// When the block was created.
    pub inserted_at: DateTime<Utc>,
}

/// The canonical spelling of a blocked domain, `None` when the input
/// names no domain.
///
/// The rules are the ticket's: trimmed and lowercased, a leading `@`
/// (Pleroma-style `@user@host` input) and a leading `scheme://` are
/// stripped, one trailing `/` is stripped, and what remains must contain
/// a dot — which rules out the empty string, bare hostnames like
/// `localhost`, and anything left of those strips. The result keeps any
/// inner path text verbatim: this function normalises the spellings the
/// endpoint documents, it does not parse URLs.
#[must_use]
pub fn normalize_domain(raw: &str) -> Option<String> {
    let lowered = raw.trim().to_lowercase();
    let without_mention = lowered.strip_prefix('@').unwrap_or(&lowered);
    let without_scheme = match without_mention.split_once("://") {
        Some((_scheme, rest)) => rest,
        None => without_mention,
    };
    let domain = without_scheme.strip_suffix('/').unwrap_or(without_scheme);
    (!domain.is_empty() && domain.contains('.')).then(|| domain.to_owned())
}

/// Records `domain` as blocked by `user_id`, idempotently.
///
/// The domain is stored verbatim: callers hold the input from clients and
/// run it through [`normalize_domain`] first, so what lands here is
/// canonical and a repeat block of the same domain (in any spelling that
/// normalises alike) writes nothing.
///
/// # Errors
///
/// Any database failure (connection, encoding) arrives as
/// [`AppError::Database`](crate::error::AppError).
pub async fn block_domain(pool: &PgPool, user_id: Uuid, domain: &str) -> AppResult<()> {
    sqlx::query(BLOCK_SQL)
        .bind(new_id())
        .bind(user_id)
        .bind(domain)
        .execute(pool)
        .await
        .map(drop)?;
    Ok(())
}

/// Removes `user_id`'s block on `domain`, idempotently.
///
/// A domain that was never blocked (or already unblocked) deletes
/// nothing and is not an error. The same canonical-spelling rule as
/// [`block_domain`] applies — [`unblock_domain`] matches the stored text
/// exactly.
///
/// # Errors
///
/// Any database failure (connection, encoding) arrives as
/// [`AppError::Database`](crate::error::AppError).
pub async fn unblock_domain(pool: &PgPool, user_id: Uuid, domain: &str) -> AppResult<()> {
    sqlx::query(UNBLOCK_SQL)
        .bind(user_id)
        .bind(domain)
        .execute(pool)
        .await
        .map(drop)?;
    Ok(())
}

/// `user_id`'s blocks, newest first and `page`'s `limit` at a time, each
/// row carrying its own id — the cursor the endpoint's `Link` header
/// walks by ([`PageParams::finish`] flips a `min_id` fetch back to
/// newest-first).
///
/// # Errors
///
/// Any database failure (connection, encoding) arrives as
/// [`AppError::Database`](crate::error::AppError).
pub async fn domains_page(
    pool: &PgPool,
    user_id: Uuid,
    page: PageParams,
) -> AppResult<Vec<DomainBlock>> {
    let mut builder = QueryBuilder::<Postgres>::new(PAGE_SQL);
    // The builder's push methods hand back `&mut Self` for chaining; binding
    // it keeps the statement from having an unused result (decision 14).
    let _bound: &mut _ = builder.push_bind(user_id);
    page.apply_keyset(&mut builder, "id");
    let rows = builder
        .build_query_as::<DomainBlock>()
        .fetch_all(pool)
        .await?;
    Ok(page.finish(rows))
}

/// Every domain `user_id` blocks, oldest block first.
///
/// The flat list the timeline and notification tickets filter with (their
/// decision 13 follow-up), where the page-by-page [`domains_page`] view
/// the endpoint gives clients would be the wrong shape.
///
/// # Errors
///
/// Any database failure (connection, encoding) arrives as
/// [`AppError::Database`](crate::error::AppError).
pub async fn blocked_domains(pool: &PgPool, user_id: Uuid) -> AppResult<Vec<String>> {
    let rows = sqlx::query_as::<_, (String,)>(ALL_SQL)
        .bind(user_id)
        .fetch_all(pool)
        .await?;
    Ok(rows.into_iter().map(|(domain,)| domain).collect())
}

/// Whether `user_id` has blocked `domain`.
///
/// Any spelling of the domain that [`normalize_domain`] canonicalises to
/// the same text counts — which is the whole point of storing one
/// spelling. An input that normalises to no domain at all is not blocked
/// (and never was storable).
///
/// # Errors
///
/// Any database failure (connection, encoding) arrives as
/// [`AppError::Database`](crate::error::AppError).
pub async fn is_domain_blocked(pool: &PgPool, user_id: Uuid, domain: &str) -> AppResult<bool> {
    let Some(domain) = normalize_domain(domain) else {
        return Ok(false);
    };
    let blocked = sqlx::query_scalar::<_, bool>(EXISTS_SQL)
        .bind(user_id)
        .bind(domain)
        .fetch_one(pool)
        .await?;
    Ok(blocked)
}

#[cfg(test)]
mod tests {
    use sqlx::PgPool;

    use super::*;
    use crate::{error::AppError, pagination::MAX_LIMIT, test_fixtures::local_user as user};

    /// The ids of the rows of the table, descending — the expected cursor
    /// order for the pagination assertions, read straight from storage so
    /// same-millisecond `UUIDv7`s cannot make it depend on insert timing.
    async fn stored_ids(pool: &PgPool) -> Vec<Uuid> {
        sqlx::query_scalar::<_, Uuid>("SELECT id FROM user_domain_blocks ORDER BY id DESC")
            .fetch_all(pool)
            .await
            .expect("the ids read")
    }

    /// `page`'s query text parsed, with the default cap.
    fn page(query: &str) -> PageParams {
        PageParams::from_query(Some(query), MAX_LIMIT).expect("the fixture page parses")
    }

    #[test]
    fn a_plain_domain_is_its_own_canonical_form() {
        assert_eq!(
            normalize_domain("example.com").as_deref(),
            Some("example.com")
        );
        assert_eq!(
            normalize_domain("sub.domain.example.com").as_deref(),
            Some("sub.domain.example.com"),
            "inner dots and labels pass through"
        );
    }

    #[test]
    fn case_and_surrounding_whitespace_are_folded_away() {
        assert_eq!(
            normalize_domain("  BigCorp.COM \t").as_deref(),
            Some("bigcorp.com"),
            "lowercased and trimmed"
        );
        assert_eq!(
            normalize_domain("ÉXAMPLE.COM").as_deref(),
            Some("éxample.com"),
            "the fold is Unicode-aware, not ASCII-only"
        );
    }

    #[test]
    fn a_leading_mention_marker_is_stripped() {
        assert_eq!(
            normalize_domain("@spam.example").as_deref(),
            Some("spam.example"),
            "the acct-style @"
        );
        assert_eq!(
            normalize_domain("@@spam.example").as_deref(),
            Some("@spam.example"),
            "only one marker goes; the rest has a dot but is not canonical"
        );
    }

    #[test]
    fn a_leading_scheme_is_stripped() {
        for input in [
            "https://spam.example",
            "http://spam.example",
            "HTTPS://SPAM.EXAMPLE",
            "ftp://spam.example",
        ] {
            assert_eq!(
                normalize_domain(input).as_deref(),
                Some("spam.example"),
                "`{input}` loses its scheme"
            );
        }
    }

    #[test]
    fn a_trailing_slash_is_stripped() {
        assert_eq!(
            normalize_domain("spam.example/").as_deref(),
            Some("spam.example"),
            "one trailing slash"
        );
        assert_eq!(
            normalize_domain("https://spam.example/").as_deref(),
            Some("spam.example"),
            "scheme and slash together"
        );
        assert_eq!(
            normalize_domain("spam.example/path").as_deref(),
            Some("spam.example/path"),
            "no slash, nothing to strip: the path text is kept verbatim"
        );
    }

    #[test]
    fn without_a_dot_there_is_no_domain() {
        for input in ["", "  ", "localhost", "spam", "@", "https://", "/"] {
            assert_eq!(
                normalize_domain(input),
                None,
                "`{input:?}` leaves no dotted domain"
            );
        }
    }

    #[test]
    fn empty_after_the_strips_is_not_a_domain() {
        assert_eq!(
            normalize_domain("@"),
            None,
            "the mention marker alone leaves nothing"
        );
        assert_eq!(
            normalize_domain("://"),
            None,
            "a scheme separator alone leaves nothing"
        );
        assert_eq!(
            normalize_domain("//"),
            None,
            "slashes alone leave no dotted domain"
        );
    }

    #[tokio::test]
    async fn a_block_is_written_and_read_back() {
        let db = crate::test_db::test_db().await.expect("test database slot");
        let pool = db.pool().clone();
        let alice = user(&pool, "alice").await;
        block_domain(&pool, alice.id, "spam.example")
            .await
            .expect("the first block stores");

        assert_eq!(
            blocked_domains(&pool, alice.id)
                .await
                .expect("the read works"),
            vec!["spam.example".to_owned()],
            "the domain comes back verbatim"
        );
        assert!(
            is_domain_blocked(&pool, alice.id, "spam.example")
                .await
                .expect("the check reads"),
            "and it is there to find"
        );
    }

    #[tokio::test]
    async fn blocking_twice_is_one_row() {
        let db = crate::test_db::test_db().await.expect("test database slot");
        let pool = db.pool().clone();
        let alice = user(&pool, "alice").await;
        block_domain(&pool, alice.id, "spam.example").await.unwrap();
        let second = stored_ids(&pool).await;
        block_domain(&pool, alice.id, "spam.example")
            .await
            .expect("the repeat block is not an error");

        assert_eq!(
            blocked_domains(&pool, alice.id)
                .await
                .expect("the read works"),
            vec!["spam.example".to_owned()],
            "still one entry"
        );
        assert_eq!(
            stored_ids(&pool).await,
            second,
            "and the first row kept its id: nothing was rewritten"
        );
    }

    #[tokio::test]
    async fn unblocking_removes_the_block_and_repeating_is_fine() {
        let db = crate::test_db::test_db().await.expect("test database slot");
        let pool = db.pool().clone();
        let alice = user(&pool, "alice").await;
        block_domain(&pool, alice.id, "spam.example").await.unwrap();

        unblock_domain(&pool, alice.id, "spam.example")
            .await
            .expect("the unblock deletes");
        assert!(
            !is_domain_blocked(&pool, alice.id, "spam.example")
                .await
                .expect("the check reads"),
            "the domain is no longer blocked"
        );
        assert!(
            blocked_domains(&pool, alice.id).await.unwrap().is_empty(),
            "the domain is no longer blocked"
        );

        unblock_domain(&pool, alice.id, "spam.example")
            .await
            .expect("unblocking again is not an error");
        assert_eq!(stored_ids(&pool).await.len(), 0, "and deletes nothing");
    }

    #[tokio::test]
    async fn blocks_are_each_account_s_own() {
        let db = crate::test_db::test_db().await.expect("test database slot");
        let pool = db.pool().clone();
        let alice = user(&pool, "alice").await;
        let bob = user(&pool, "bob").await;
        block_domain(&pool, alice.id, "spam.example").await.unwrap();

        assert!(
            blocked_domains(&pool, bob.id).await.unwrap().is_empty(),
            "bob sees nothing of alice's block"
        );
        assert!(
            !is_domain_blocked(&pool, bob.id, "spam.example")
                .await
                .unwrap(),
            "and the check is per blocker too"
        );
        unblock_domain(&pool, bob.id, "spam.example")
            .await
            .expect("bob's unblock runs");
        assert!(
            is_domain_blocked(&pool, alice.id, "spam.example")
                .await
                .unwrap(),
            "and left alice's block alone"
        );
    }

    #[tokio::test]
    async fn is_domain_blocked_follows_the_canonical_spelling() {
        let db = crate::test_db::test_db().await.expect("test database slot");
        let pool = db.pool().clone();
        let alice = user(&pool, "alice").await;
        block_domain(&pool, alice.id, "spam.example").await.unwrap();

        for spelling in ["SPAM.Example", "@spam.example", "https://spam.example/"] {
            assert!(
                is_domain_blocked(&pool, alice.id, spelling).await.unwrap(),
                "`{spelling}` normalises to the blocked domain"
            );
        }
        assert!(
            !is_domain_blocked(&pool, alice.id, "localhost")
                .await
                .unwrap(),
            "input that normalises to no domain is simply not blocked"
        );
    }

    #[tokio::test]
    async fn blocked_domains_keeps_oldest_first() {
        let db = crate::test_db::test_db().await.expect("test database slot");
        let pool = db.pool().clone();
        let alice = user(&pool, "alice").await;
        for domain in ["one.example", "two.example", "three.example"] {
            block_domain(&pool, alice.id, domain).await.unwrap();
        }
        assert_eq!(
            blocked_domains(&pool, alice.id).await.unwrap(),
            vec!["one.example", "two.example", "three.example"],
            "creation order, whatever the spelling"
        );
    }

    #[tokio::test]
    async fn a_page_newest_first_walks_by_row_id() {
        let db = crate::test_db::test_db().await.expect("test database slot");
        let pool = db.pool().clone();
        let alice = user(&pool, "alice").await;
        for n in 0..5 {
            block_domain(&pool, alice.id, &format!("block{n}.example"))
                .await
                .unwrap();
        }
        // Newest-first expectations read from storage: `UUIDv7`s made in
        // the same millisecond do not strictly order by creation time, so
        // the walk asserts against the ids and domains the rows really
        // have, in id order.
        let stored = sqlx::query_as::<_, (Uuid, String)>(
            "SELECT id, domain FROM user_domain_blocks ORDER BY id DESC",
        )
        .fetch_all(&pool)
        .await
        .expect("the stored rows read");
        assert_eq!(stored.len(), 5, "five fixture blocks");
        let ids: Vec<Uuid> = stored.iter().map(|(id, _)| *id).collect();
        let expected: Vec<String> = stored.iter().map(|(_, domain)| domain.clone()).collect();

        let first = domains_page(&pool, alice.id, page("limit=2"))
            .await
            .expect("the first page reads");
        assert_eq!(first.len(), 2, "two rows");
        assert_eq!(
            first.iter().map(|row| row.id).collect::<Vec<_>>(),
            vec![ids[0], ids[1]],
            "the two newest rows"
        );
        assert_eq!(
            first
                .iter()
                .map(|row| row.domain.as_str())
                .collect::<Vec<_>>(),
            vec![expected[0].as_str(), expected[1].as_str()],
            "and their domains, newest block first"
        );

        let older = domains_page(&pool, alice.id, page(&format!("limit=2&max_id={}", ids[1])))
            .await
            .expect("the continuation reads");
        assert_eq!(
            older.iter().map(|row| row.id).collect::<Vec<_>>(),
            vec![ids[2], ids[3]],
            "max_id continues where the page ended"
        );

        let newer = domains_page(&pool, alice.id, page(&format!("min_id={}", ids[3])))
            .await
            .expect("the forward page reads");
        assert_eq!(
            newer.iter().map(|row| row.id).collect::<Vec<_>>(),
            vec![ids[0], ids[1], ids[2]],
            "a min_id page is answered newest-first too"
        );

        let tail = domains_page(&pool, alice.id, page(&format!("max_id={}", ids[4])))
            .await
            .expect("walking off the end is no error");
        assert!(tail.is_empty(), "and answers nothing");

        let nobody = user(&pool, "carol").await;
        assert!(
            domains_page(&pool, nobody.id, page(""))
                .await
                .expect("an empty listing is no error")
                .is_empty(),
            "a blocker with no blocks pages nothing"
        );
    }

    #[tokio::test]
    async fn deleting_the_blocker_takes_the_blocks() {
        let db = crate::test_db::test_db().await.expect("test database slot");
        let pool = db.pool().clone();
        let alice = user(&pool, "alice").await;
        let bob = user(&pool, "bob").await;
        block_domain(&pool, alice.id, "spam.example").await.unwrap();
        block_domain(&pool, bob.id, "other.example").await.unwrap();

        let deleted = sqlx::query("DELETE FROM users WHERE id = $1")
            .bind(alice.id)
            .execute(&pool)
            .await
            .expect("the fixture account deletes");
        assert_eq!(deleted.rows_affected(), 1, "one account went");
        assert_eq!(stored_ids(&pool).await.len(), 1, "its block went too");
        assert_eq!(
            blocked_domains(&pool, bob.id).await.unwrap(),
            vec!["other.example"],
            "and bob's block stands"
        );
    }

    #[tokio::test]
    async fn a_block_for_no_stored_account_fails() {
        let db = crate::test_db::test_db().await.expect("test database slot");
        let pool = db.pool().clone();
        // The foreign key is the model's only guard: the endpoint hands
        // ids that name live users, and a fabricated one is a database
        // error, never a silently stored orphan.
        let nobody = Uuid::now_v7();
        let err = block_domain(&pool, nobody, "spam.example")
            .await
            .expect_err("the foreign key refuses");
        assert!(
            matches!(err, AppError::Database(_)),
            "a constraint failure is the database variant: {err}"
        );
        assert!(stored_ids(&pool).await.is_empty(), "nothing was stored");
    }
}
