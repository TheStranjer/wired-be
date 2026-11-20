//! The direct admin reads of one account or a named batch: the show
//! endpoint's [`get_by_nickname_or_id`] and the list-taking endpoints'
//! [`resolve_nicknames`].

use sqlx::PgPool;

use super::super::User;
use crate::{error::AppResult, util::id::parse_id};

/// The account the show endpoint's path segment names, or `None` when no
/// row answers to it.
///
/// That is the row whose nickname is `raw` — citext, so its case does not
/// matter, and a remote `nick@domain` handle *is* a stored nickname
/// (decision 7) — or, when no nickname matches and `raw` is id text
/// ([`parse_id`]), the row behind that primary key.
///
/// Pleroma's `User.get_cached_by_nickname_or_id` is the same two steps in
/// the same order. A deactivated account and the instance actor are both
/// found here: this lookup names one account directly, while the exclusions
/// of [`list`](super::pleroma_list::list) are listing rules.
///
/// # Errors
///
/// Any database failure arrives as [`AppError::Database`](crate::error::AppError).
pub async fn get_by_nickname_or_id(pool: &PgPool, raw: &str) -> AppResult<Option<User>> {
    if let Some(found) = super::super::get_by_nickname(pool, raw).await? {
        return Ok(Some(found));
    }
    match parse_id(raw) {
        Some(id) => super::super::get_by_id(pool, id).await,
        None => Ok(None),
    }
}

/// The accounts one admin body's `nicknames` list names, in the order the
/// list spells them: [`get_by_nickname`](super::super::get_by_nickname) per
/// entry, and a name no account carries is quietly dropped.
///
/// That dropping is Pleroma's admin behavior, not an error: its
/// `force_password_reset`, `confirm_email` and `resend_confirmation_email`
/// map every submitted nickname through `get_cached_by_nickname` and act
/// on the list as it comes, an unknown name having no user to act on. The
/// answered list is what those endpoints write to and what their
/// moderation-log entries name as subjects.
///
/// # Errors
///
/// Any database failure arrives as [`AppError::Database`](crate::error::AppError).
pub async fn resolve_nicknames(pool: &PgPool, nicknames: &[String]) -> AppResult<Vec<User>> {
    let mut found = Vec::with_capacity(nicknames.len());
    for nickname in nicknames {
        if let Some(user) = super::super::get_by_nickname(pool, nickname).await? {
            found.push(user);
        }
    }
    Ok(found)
}

#[cfg(test)]
mod tests {
    use super::*;

    use crate::models::user::admin::test_support::{INSTANCE_ACTOR, fixture};

    /// The show lookup, step by step: a nickname in any case, a remote
    /// handle, the id when the nickname misses, the instance actor by name,
    /// and `None` for text that is neither a nickname nor an id.
    #[tokio::test]
    async fn the_show_lookup_tries_the_nickname_then_the_id() {
        let (_db, pool) = fixture().await;
        let by_name = get_by_nickname_or_id(&pool, "PLAIN")
            .await
            .expect("a nickname reads")
            .expect("citext folds the case");
        assert_eq!(by_name.nickname, "plain", "and finds her account");
        let mirror = get_by_nickname_or_id(&pool, "faraway@remote.example")
            .await
            .expect("a handle reads")
            .expect("a remote handle is a stored nickname");
        assert!(!mirror.local.get(), "and the row is the mirror");
        let by_id = get_by_nickname_or_id(&pool, &by_name.id.to_string())
            .await
            .expect("an id reads")
            .expect("the id step runs when the nickname misses");
        assert_eq!(by_id.id, by_name.id, "and it finds the same row");
        let actor = get_by_nickname_or_id(&pool, INSTANCE_ACTOR)
            .await
            .expect("a name reads")
            .expect("a listing hides the actor, a lookup finds it");
        assert_eq!(actor.nickname, INSTANCE_ACTOR, "by name");
        assert!(
            get_by_nickname_or_id(&pool, "ghost")
                .await
                .expect("a miss reads")
                .is_none(),
            "nobody answers to a name that was never stored"
        );
        assert!(
            get_by_nickname_or_id(&pool, "not-an-uuid")
                .await
                .expect("a miss reads")
                .is_none(),
            "and unroutable text is no id either"
        );
    }

    /// The batch lookup keeps the submitted order, folds case through the
    /// citext column, and drops the names nobody answers to — Pleroma's
    /// admin list actions act on what was found and ignore the rest.
    #[tokio::test]
    async fn resolve_nicknames_keeps_order_and_skips_the_misses() {
        let (_db, pool) = fixture().await;
        let submitted = ["root", "ghost", "PLAIN", "faraway@remote.example"]
            .iter()
            .map(|text| (*text).to_owned())
            .collect::<Vec<String>>();
        let found = resolve_nicknames(&pool, &submitted)
            .await
            .expect("the batch read runs");
        assert_eq!(
            found
                .iter()
                .map(|user| user.nickname.as_str())
                .collect::<Vec<_>>(),
            vec!["root", "plain", "faraway@remote.example"],
            "three accounts in the submitted order, the unknown name gone"
        );
        let misses = ["nobody", "ghost"]
            .iter()
            .map(|text| (*text).to_owned())
            .collect::<Vec<String>>();
        assert!(
            resolve_nicknames(&pool, &misses)
                .await
                .expect("a list of misses reads")
                .is_empty(),
            "nothing found is the empty list, not an error"
        );
        assert!(
            resolve_nicknames(&pool, &[])
                .await
                .expect("the empty list reads")
                .is_empty(),
            "and an empty list is empty"
        );
    }
}
