//! Turning `@nick` and `@nick@domain.tld` references in post text into
//! accounts.
//!
//! The reference is Pleroma's formatter (`lib/pleroma/formatter.ex`), which
//! expands mentions while formatting a post, looking each one up and — for
//! accounts it does not know yet — fetching them over the network. This
//! module is the lookup half: [`resolve_local_mentions`] runs
//! [`extract_mentions`] over the text and returns the references it can
//! resolve from the [`users`] table alone — local accounts (written bare or
//! qualified with this instance's own domain) and remote mirrors already
//! stored here. Unknown accounts are skipped; the federation ticket that
//! fetches remote actors hooks into the documented
//! `resolve_remote_mention` stub in this module, so the resolution order is
//! already in place and only its remote arm needs filling.
//!
//! What callers do with the pairs is theirs: [`crate::models::status`]
//! persists the links (see [`NewStatus::mentioned_user_ids`]), and the
//! Status entity renders them (see [`crate::entities::mention`]).
//!
//! [`users`]: crate::models::user
//! [`NewStatus::mentioned_user_ids`]: crate::models::status::NewStatus::mentioned_user_ids

use sqlx::PgPool;

use crate::{
    config::Config,
    error::AppResult,
    models::user::{self, User},
    util::entities::{MentionRef, extract_mentions},
};

/// Resolves every mention [`extract_mentions`] finds in `text` against
/// this instance's own data.
///
/// Answers the pairs it knows in order of first appearance: a
/// [`MentionRef`] (with the case it was written in) beside the [`User`] it
/// names.
///
/// A reference resolves when [`user::get_by_acct`] finds a row — a local
/// account as bare `nick` or as `nick@`this instance's domain, and a remote
/// account stored under its full `nick@domain` (decision 7). Anything else
/// is skipped: unknown locals have nowhere to be found, and unknown remotes
/// go begging at `resolve_remote_mention` below, which fetches nothing
/// until the federation ticket implements it. A mention repeated in the text
/// appears once, as [`extract_mentions`] deduplicates it.
///
/// # Errors
///
/// Any database failure behind the lookups (connection, encoding) arrives
/// as [`AppError::Database`](crate::error::AppError).
pub async fn resolve_local_mentions(
    pool: &PgPool,
    config: &Config,
    text: &str,
) -> AppResult<Vec<(MentionRef, User)>> {
    let mut resolved = Vec::new();
    for mention in extract_mentions(text) {
        if let Some(found) = resolve_mention(pool, config, &mention).await? {
            resolved.push((mention, found));
        }
    }
    Ok(resolved)
}

/// The one-mention step of [`resolve_local_mentions`]: the stored rows
/// first, and only a mention naming a foreign domain reaches the remote
/// hook.
///
/// A bare `@nick` or an `@nick@our-domain` names a local account, and a
/// local account that is not stored simply is not there.
async fn resolve_mention(
    pool: &PgPool,
    config: &Config,
    mention: &MentionRef,
) -> AppResult<Option<User>> {
    if let Some(found) = user::get_by_acct(pool, config, &mention.acct()).await? {
        return Ok(Some(found));
    }
    if names_own_domain(config, mention) {
        return Ok(None);
    }
    resolve_remote_mention(pool, config, mention).await
}

/// Whether a mention can name a local account at all: a bare `@nick`, or a
/// `@nick@domain` whose domain is this instance's own
/// ([`Config::domain`], compared case-insensitively).
fn names_own_domain(config: &Config, mention: &MentionRef) -> bool {
    mention
        .domain
        .as_deref()
        .is_none_or(|domain| domain.eq_ignore_ascii_case(config.domain()))
}

/// The federation hook of [`resolve_mention`]: find the account a mention
/// of a foreign domain names even though no row stores it yet — Pleroma's
/// formatter resolves such mentions through `User.remote_fetch`
/// (`lib/pleroma/formatter.ex`).
///
/// Deliberately empty until the federation ticket that processes remote
/// actors lands: it answers `None` (resolve nothing, touch nothing), so
/// unknown remote mentions are skipped. Wiring the real lookup — the
/// WebFinger/actor fetch of [`crate::federation::fetch_actor`] behind this
/// call — is that ticket's job, and [`resolve_local_mentions`] starts
/// returning its findings here without further change.
///
/// The `std::future::ready` await is what keeps this the `async fn` that
/// fetch will be: an empty body without one could not be awaited.
async fn resolve_remote_mention(
    _pool: &PgPool,
    _config: &Config,
    _mention: &MentionRef,
) -> AppResult<Option<User>> {
    std::future::ready(Ok(None)).await
}

#[cfg(test)]
mod tests {
    use sqlx::PgPool;
    use uuid::Uuid;

    use super::*;
    use crate::{
        config::TEST_BASE_URL,
        models::user::{NewUser, insert},
    };

    /// The instance the fixtures resolve against.
    const BASE: &str = TEST_BASE_URL;

    /// Inserts a local user named `nickname`.
    async fn local_user(pool: &PgPool, nickname: &str) -> User {
        insert(
            pool,
            &NewUser::local(nickname, &format!("{BASE}/users/{nickname}")),
        )
        .await
        .expect("fixture local user inserts")
    }

    /// Inserts a stored remote mirror, `nickname@remote.example`.
    async fn remote_user(pool: &PgPool, nickname: &str) -> User {
        insert(
            pool,
            &NewUser {
                nickname: format!("{nickname}@remote.example"),
                ap_id: format!("https://remote.example/users/{nickname}"),
                domain: Some("remote.example".to_owned()),
                ..NewUser::default()
            },
        )
        .await
        .expect("fixture remote user inserts")
    }

    /// The resolved pairs as `(written nickname, written domain, user id)`,
    /// so assertions name both sides of each pair.
    async fn resolved(pool: &PgPool, text: &str) -> Vec<(String, Option<String>, Uuid)> {
        resolve_local_mentions(pool, &Config::for_tests(), text)
            .await
            .expect("resolution reads")
            .into_iter()
            .map(|(mention, user)| (mention.nickname, mention.domain, user.id))
            .collect()
    }

    #[tokio::test]
    async fn a_bare_nickname_resolves_its_local_account() {
        let db = crate::test_db::test_db().await.expect("test database slot");
        let pool = db.pool().clone();
        let alice = local_user(&pool, "alice").await;
        assert_eq!(
            resolved(&pool, "hello @alice, how are you?").await,
            vec![("alice".to_owned(), None, alice.id)],
            "the reference keeps its written shape, the user is the row"
        );
    }

    #[tokio::test]
    async fn a_locally_written_mention_matches_case_insensitively() {
        let db = crate::test_db::test_db().await.expect("test database slot");
        let pool = db.pool().clone();
        let alice = local_user(&pool, "alice").await;
        assert_eq!(
            resolved(&pool, "@ALICE!").await,
            vec![("ALICE".to_owned(), None, alice.id)],
            "the citext column finds alice from any spelling"
        );
    }

    #[tokio::test]
    async fn an_own_domain_qualifier_resolves_the_local_account() {
        let db = crate::test_db::test_db().await.expect("test database slot");
        let pool = db.pool().clone();
        let alice = local_user(&pool, "alice").await;
        assert_eq!(
            resolved(&pool, "@alice@test.example @ALICE@Test.Example").await,
            vec![(
                "alice".to_owned(),
                Some("test.example".to_owned()),
                alice.id
            )],
            "nick@our-domain is the same account as nick in any case, and \
             extract_mentions' case-insensitive dedupe leaves one reference"
        );
    }

    #[tokio::test]
    async fn a_stored_remote_account_resolves_by_its_full_acct() {
        let db = crate::test_db::test_db().await.expect("test database slot");
        let pool = db.pool().clone();
        let bob = remote_user(&pool, "bob").await;
        assert_eq!(
            resolved(&pool, "@bob@remote.example").await,
            vec![("bob".to_owned(), Some("remote.example".to_owned()), bob.id)],
            "the mirror already in users is found without any fetching"
        );
    }

    #[tokio::test]
    async fn unknown_mentions_are_skipped() {
        let db = crate::test_db::test_db().await.expect("test database slot");
        let pool = db.pool().clone();
        let alice = local_user(&pool, "alice").await;
        assert!(
            resolved(&pool, "@nobody says hi").await.is_empty(),
            "an unknown local is no pair"
        );
        assert!(
            resolved(&pool, "@whoever@nowhere.example").await.is_empty(),
            "an unknown remote is skipped too, not fetched"
        );
        assert_eq!(
            resolved(&pool, "@nobody @alice @whoever@nowhere.example").await,
            vec![("alice".to_owned(), None, alice.id)],
            "the known mention still comes back between the unknown ones"
        );
    }

    #[tokio::test]
    async fn resolution_keeps_text_order_and_dedupe() {
        let db = crate::test_db::test_db().await.expect("test database slot");
        let pool = db.pool().clone();
        let alice = local_user(&pool, "alice").await;
        let bob = local_user(&pool, "bob").await;
        assert_eq!(
            resolved(&pool, "@bob @alice and @bob again, plus @alice").await,
            vec![
                ("bob".to_owned(), None, bob.id),
                ("alice".to_owned(), None, alice.id),
            ],
            "first appearance order, each acct once"
        );
    }

    #[tokio::test]
    async fn a_bare_name_never_resolves_to_a_remote_mirror() {
        let db = crate::test_db::test_db().await.expect("test database slot");
        let pool = db.pool().clone();
        let _carol = remote_user(&pool, "carol").await;
        assert!(
            resolved(&pool, "@carol @carol@test.example")
                .await
                .is_empty(),
            "carol exists only on remote.example, so neither the bare \
             mention nor one qualified with OUR domain names her"
        );
    }

    #[tokio::test]
    async fn plain_text_resolves_nothing() {
        let db = crate::test_db::test_db().await.expect("test database slot");
        let pool = db.pool().clone();
        assert!(
            resolved(
                &pool,
                "no mentions in an email a@b.c or a url \
                            https://x.example/@alice"
            )
            .await
            .is_empty(),
            "extract_mentions' rules carry over: nothing to look up"
        );
        assert!(resolved(&pool, "").await.is_empty(), "empty text");
    }

    #[tokio::test]
    async fn the_remote_hook_resolves_nothing_until_federation_lands() {
        let db = crate::test_db::test_db().await.expect("test database slot");
        let pool = db.pool().clone();
        let mention = MentionRef {
            nickname: "whoever".to_owned(),
            domain: Some("nowhere.example".to_owned()),
        };
        assert!(
            resolve_remote_mention(&pool, &Config::for_tests(), &mention)
                .await
                .expect("the stub answers without touching anything")
                .is_none(),
            "the documented stub finds nobody yet"
        );
    }
}
