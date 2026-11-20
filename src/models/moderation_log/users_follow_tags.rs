//! The follow pair and the tag and permission-group entries: the
//! relationship writes, the tag pair and the grant and revoke pair.

use serde_json::{Map, Value};
use sqlx::PgPool;

use super::data::{permission_data, tags_data, user_map};
use super::sentences::{
    grant_sentence, nicknames_of, revoke_sentence, tag_sentence, untag_sentence,
};
use super::{ModAction, ModerationLog, put, record};
use crate::{error::AppResult, models::user::User};

/// Records a `follow`: `@admin made @alice follow @bob`, the admin's
/// forced follow (Pleroma's `UserController.follow`, whose log entry names
/// both accounts as user maps and no subject list).
///
/// # Errors
///
/// Any database failure arrives as
/// [`AppError::Database`](crate::error::AppError); see [`record`].
pub async fn follow(
    pool: &PgPool,
    actor: &User,
    follower: &User,
    target: &User,
) -> AppResult<ModerationLog> {
    follow_action(pool, actor, follower, target, "follow").await
}

/// Records an `unfollow`: `@admin made @alice unfollow @bob`, the pair of
/// [`follow`].
///
/// # Errors
///
/// Any database failure arrives as
/// [`AppError::Database`](crate::error::AppError); see [`record`].
pub async fn unfollow(
    pool: &PgPool,
    actor: &User,
    follower: &User,
    target: &User,
) -> AppResult<ModerationLog> {
    follow_action(pool, actor, follower, target, "unfollow").await
}

/// The one writer of the two follow sentences: Pleroma's `follow` /
/// `unfollow` `insert_log` clause puts the pair into `data` under
/// `follower` and `followed` (each a [`user_map`]) and spells the message
/// `"@actor made @follower {action} @followed"`; the subject columns carry
/// both accounts, which is what the pair is about.
pub(super) async fn follow_action(
    pool: &PgPool,
    actor: &User,
    follower: &User,
    target: &User,
    action: &str,
) -> AppResult<ModerationLog> {
    let mut data = Map::new();
    put(&mut data, "follower", Value::Object(user_map(follower)));
    put(&mut data, "followed", Value::Object(user_map(target)));
    let message = format!(
        "@{} made @{} {action} @{}",
        actor.nickname, follower.nickname, target.nickname
    );
    record(
        pool,
        actor,
        ModAction {
            action: action.to_owned(),
            subjects: vec![follower, target],
            message,
            data,
        },
    )
    .await
}

/// Records a `tag`: `@admin added tags: staff, vip to users: @alice,
/// @bob`. Pleroma's tag entry names its subjects by `nicknames` and
/// `tags`, not by a subject map.
///
/// # Errors
///
/// Any database failure arrives as
/// [`AppError::Database`](crate::error::AppError); see [`record`].
pub async fn tag(
    pool: &PgPool,
    actor: &User,
    subjects: &[User],
    tags: &[String],
) -> AppResult<ModerationLog> {
    let nicknames = nicknames_of(subjects);
    let message = tag_sentence(&actor.nickname, tags, &nicknames);
    record(
        pool,
        actor,
        ModAction {
            action: "tag".to_owned(),
            subjects: subjects.iter().collect(),
            message,
            data: tags_data(tags, &nicknames),
        },
    )
    .await
}

/// Records an `untag`: `@admin removed tags: staff, vip from users:
/// @alice, @bob`.
///
/// # Errors
///
/// Any database failure arrives as
/// [`AppError::Database`](crate::error::AppError); see [`record`].
pub async fn untag(
    pool: &PgPool,
    actor: &User,
    subjects: &[User],
    tags: &[String],
) -> AppResult<ModerationLog> {
    let nicknames = nicknames_of(subjects);
    let message = untag_sentence(&actor.nickname, tags, &nicknames);
    record(
        pool,
        actor,
        ModAction {
            action: "untag".to_owned(),
            subjects: subjects.iter().collect(),
            message,
            data: tags_data(tags, &nicknames),
        },
    )
    .await
}

/// Records a `grant`: `@admin made @alice, @bob moderator` — Pleroma's
/// grant clause, with the role in its `permission` key.
///
/// # Errors
///
/// Any database failure arrives as
/// [`AppError::Database`](crate::error::AppError); see [`record`].
pub async fn grant(
    pool: &PgPool,
    actor: &User,
    subjects: &[User],
    permission: &str,
) -> AppResult<ModerationLog> {
    let nicknames = nicknames_of(subjects);
    let message = grant_sentence(&actor.nickname, &nicknames, permission);
    record(
        pool,
        actor,
        ModAction {
            action: "grant".to_owned(),
            subjects: subjects.iter().collect(),
            message,
            data: permission_data(subjects, permission),
        },
    )
    .await
}

/// Records a `revoke`: `@admin revoked moderator role from @alice, @bob`.
///
/// # Errors
///
/// Any database failure arrives as
/// [`AppError::Database`](crate::error::AppError); see [`record`].
pub async fn revoke(
    pool: &PgPool,
    actor: &User,
    subjects: &[User],
    permission: &str,
) -> AppResult<ModerationLog> {
    let nicknames = nicknames_of(subjects);
    let message = revoke_sentence(&actor.nickname, &nicknames, permission);
    record(
        pool,
        actor,
        ModAction {
            action: "revoke".to_owned(),
            subjects: subjects.iter().collect(),
            message,
            data: permission_data(subjects, permission),
        },
    )
    .await
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::models::moderation_log::test_support::{names, one, stored};

    /// The follow pair stores Pleroma's `made … follow` sentence, the two
    /// user maps in `data` and both accounts in the subject columns.
    #[tokio::test]
    async fn follow_helpers_record_their_entries() {
        let db = crate::test_db::test_db().await.expect("test database slot");
        let pool = db.pool().clone();
        let actor = stored(&pool, "mod").await;
        let alice = stored(&pool, "alice").await;
        let bob = stored(&pool, "bob").await;
        let followed = follow(&pool, &actor, &alice, &bob)
            .await
            .expect("follow records");
        assert_eq!(followed.action, "follow", "the action column");
        assert_eq!(
            followed.message, "@mod made @alice follow @bob",
            "Pleroma's follow sentence"
        );
        assert_eq!(
            followed.subject_ids,
            vec![alice.id, bob.id],
            "both accounts are the subjects"
        );
        assert_eq!(
            followed
                .data
                .get("follower")
                .and_then(|map| map.get("nickname"))
                .and_then(Value::as_str),
            Some("alice"),
            "the follower map rides in the data"
        );
        assert_eq!(
            followed
                .data
                .get("followed")
                .and_then(|map| map.get("type"))
                .and_then(Value::as_str),
            Some("user"),
            "and so does the followed one, in user_to_map shape"
        );
        let withdrawn = unfollow(&pool, &actor, &alice, &bob)
            .await
            .expect("unfollow records");
        assert_eq!(withdrawn.action, "unfollow", "the pair's other action");
        assert_eq!(
            withdrawn.message, "@mod made @alice unfollow @bob",
            "Pleroma's unfollow sentence"
        );
    }

    /// `tag`, `untag`, `grant` and `revoke` store their sentences and the
    /// Pleroma data keys (`tags`/`nicknames`, `subject`/`permission`).
    #[tokio::test]
    async fn tag_and_role_helpers_record_their_entries() {
        let db = crate::test_db::test_db().await.expect("test database slot");
        let pool = db.pool().clone();
        let actor = stored(&pool, "mod").await;
        let alice = stored(&pool, "alice").await;
        let tags = names(&["staff", "vip"]);
        let tagged = tag(&pool, &actor, one(&alice), &tags)
            .await
            .expect("tag records");
        assert_eq!(
            tagged.message, "@mod added tags: staff, vip to users: @alice",
            "the tag sentence"
        );
        assert_eq!(
            tagged
                .data
                .get("tags")
                .and_then(Value::as_array)
                .map(Vec::len),
            Some(2),
            "the tags ride in the data map"
        );
        assert_eq!(
            tagged.data.get("nicknames"),
            Some(&Value::Array(vec![Value::String("alice".to_owned())])),
            "and the raw nicknames Pleroma's clause reads"
        );
        let untagged = untag(&pool, &actor, one(&alice), &tags)
            .await
            .expect("untag records");
        assert_eq!(
            untagged.message, "@mod removed tags: staff, vip from users: @alice",
            "the untag sentence"
        );
        let granted = grant(&pool, &actor, one(&alice), "moderator")
            .await
            .expect("grant records");
        assert_eq!(
            granted.message, "@mod made @alice moderator",
            "the grant sentence"
        );
        assert_eq!(
            granted.data.get("permission").and_then(Value::as_str),
            Some("moderator"),
            "the role rides in the data map"
        );
        let revoked = revoke(&pool, &actor, one(&alice), "moderator")
            .await
            .expect("revoke records");
        assert_eq!(
            revoked.message, "@mod revoked moderator role from @alice",
            "the revoke sentence"
        );
    }
}
