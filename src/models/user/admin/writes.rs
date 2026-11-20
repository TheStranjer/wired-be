//! The staff read and the one-account writes of staff state and profile
//! tags: [`staff_with_email`], [`Role`], [`set_role`] and [`set_tags`].

use sqlx::PgPool;
use uuid::Uuid;

use super::super::{Flag, User};
use crate::error::AppResult;

/// [`staff_with_email`]: the staff predicate of
/// [`crate::api::nodeinfo`]'s `staffAccounts` list — an admin or a
/// moderator this instance owns and that is active — narrowed to the rows
/// holding an address and ordered by id so the letters go out in creation
/// order.
const STAFF_WITH_EMAIL_SQL: &str = "SELECT * FROM users \
     WHERE (is_admin OR is_moderator) AND local AND is_active \
       AND email IS NOT NULL \
     ORDER BY id";

/// Every staff account the instance can write to: the active local admins
/// and moderators that gave an address.
///
/// The recipients of the report letter
/// ([`crate::services::webhooks`]'s `report.created` mailing) — Pleroma's
/// `all_users_with_privilege(:reports_manage_reports)` filtered to the ones
/// with an `email`.
///
/// # Errors
///
/// Any database failure arrives as [`AppError::Database`](crate::error::AppError).
pub async fn staff_with_email(pool: &PgPool) -> AppResult<Vec<User>> {
    let rows = sqlx::query_as::<_, User>(STAFF_WITH_EMAIL_SQL)
        .fetch_all(pool)
        .await?;
    Ok(rows)
}

/// One of the two staff groups Pleroma's admin API grants and revokes: the
/// `permission_group` its routes name, told apart by the column each one
/// flips.
///
/// The reference is Pleroma's `permission_group in ["moderator", "admin"]`
/// guard of `AdminAPIController.right_add` / `right_delete`
/// (`lib/pleroma/web/admin_api/controllers/admin_api_controller.ex`), whose
/// `:"is_#{permission_group}"` atom is this table's `is_…` column. A text
/// off the two is no group at all, and the endpoints answer it their 404.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Role {
    /// `admin`: the [`is_admin`](User::is_admin) column.
    Admin,
    /// `moderator`: the [`is_moderator`](User::is_moderator) column.
    Moderator,
}

impl Role {
    /// The group one path segment names, matched exactly — `Admin`,
    /// `Moderator` and nothing else (Pleroma's guard lists the two in its
    /// own lowercase spelling, and so do the admin frontends that send
    /// them).
    #[must_use]
    pub fn parse(text: &str) -> Option<Self> {
        match text {
            "admin" => Some(Self::Admin),
            "moderator" => Some(Self::Moderator),
            _ => None,
        }
    }

    /// The group's own name: the segment it is routed by and the
    /// `permission` value Pleroma's grant and revoke log entries carry.
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Admin => "admin",
            Self::Moderator => "moderator",
        }
    }

    /// The `users` column of the group, which is also the key the grant and
    /// revoke answers report it under — Pleroma's `%{:"is_#{group}" => …}`
    /// map, spelled as the wire spells it.
    #[must_use]
    pub const fn column(self) -> &'static str {
        match self {
            Self::Admin => "is_admin",
            Self::Moderator => "is_moderator",
        }
    }

    /// Whether `user`'s row carries the group.
    #[must_use]
    pub const fn holds(self, user: &User) -> bool {
        match self {
            Self::Admin => user.is_admin.get(),
            Self::Moderator => user.is_moderator.get(),
        }
    }
}

/// The `UPDATE` behind [`set_role`], one statement per group: the column is
/// named by the SQL text rather than assembled from the request, and only
/// that one column moves — granting `admin` never touches `is_moderator`,
/// which is Pleroma's `admin_api_update` cast of the single key.
const SET_ADMIN_SQL: &str = "UPDATE users SET is_admin = $2, updated_at = now() WHERE id = $1";
/// [`SET_ADMIN_SQL`]'s twin for the moderator group.
const SET_MODERATOR_SQL: &str =
    "UPDATE users SET is_moderator = $2, updated_at = now() WHERE id = $1";

/// Writes `granted` into user `id`'s column for `role`.
///
/// The other staff column — and every other column — stays alone: the
/// grant and the revoke of `POST` and `DELETE …/permission_group/{group}`
/// and of their nickname-list twins.
///
/// # Errors
///
/// Any database failure arrives as [`AppError::Database`](crate::error::AppError).
pub async fn set_role(pool: &PgPool, id: Uuid, role: Role, granted: bool) -> AppResult<()> {
    let sql = match role {
        Role::Admin => SET_ADMIN_SQL,
        Role::Moderator => SET_MODERATOR_SQL,
    };
    sqlx::query(sql)
        .bind(id)
        .bind(Flag::new(granted))
        .execute(pool)
        .await
        .map(drop)?;
    Ok(())
}

/// The `UPDATE` behind [`set_tags`]: replaces the whole tag array and
/// refreshes `updated_at`.
const SET_TAGS_SQL: &str = "UPDATE users SET tags = $2, updated_at = now() WHERE id = $1";

/// Stores `tags` as user `id`'s [`tags`](User::tags) array, replacing what
/// was there.
///
/// The admin tag pair keeps Pleroma's merge and subtraction rules in the
/// API layer (see `merged_tags` and `kept_tags` in
/// [`crate::api::admin::users`]) and hands the whole new array in here —
/// the same split [`set_also_known_as`](super::super::set_also_known_as)
/// documents. Unknown ids match no row and change nothing.
///
/// # Errors
///
/// Any database failure arrives as [`AppError::Database`](crate::error::AppError).
pub async fn set_tags(pool: &PgPool, id: Uuid, tags: &[String]) -> AppResult<()> {
    sqlx::query(SET_TAGS_SQL)
        .bind(id)
        .bind(tags.to_vec())
        .execute(pool)
        .await
        .map(drop)?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    use crate::models::user::admin::get_by_nickname_or_id;
    use crate::models::user::admin::test_support::{fixture, seeded};
    use crate::models::user::{NewUser, get_by_id, insert};

    #[tokio::test]
    async fn staff_with_email_answers_the_mailable_staff_only() {
        let db = crate::test_db::test_db().await.expect("test database slot");
        let pool = db.pool().clone();
        seeded(&pool, "rooty", |user| NewUser {
            is_admin: Flag::new(true),
            email: Some("rooty@example.org".to_owned()),
            ..user
        })
        .await;
        seeded(&pool, "moddy", |user| NewUser {
            is_moderator: Flag::new(true),
            email: Some("moddy@example.org".to_owned()),
            ..user
        })
        .await;
        seeded(&pool, "silentmod", |user| NewUser {
            is_moderator: Flag::new(true),
            ..user
        })
        .await;
        seeded(&pool, "offduty", |user| NewUser {
            is_admin: Flag::new(true),
            is_active: Flag::new(false),
            email: Some("offduty@example.org".to_owned()),
            ..user
        })
        .await;
        seeded(&pool, "member", |user| NewUser {
            email: Some("member@example.org".to_owned()),
            ..user
        })
        .await;
        seeded(&pool, "remotecrew", |user| NewUser {
            local: Flag::new(false),
            domain: Some("remote.example".to_owned()),
            is_admin: Flag::new(true),
            email: Some("remotecrew@remote.example".to_owned()),
            ..user
        })
        .await;

        let staff = staff_with_email(&pool).await.expect("the read runs");
        let nicknames: Vec<&str> = staff
            .iter()
            .map(|account| account.nickname.as_str())
            .collect();
        assert_eq!(
            nicknames,
            vec!["rooty", "moddy"],
            "only an active local admin or moderator with an address is mailable, in creation order"
        );
        assert!(
            staff.iter().all(|account| account.email.is_some()),
            "every answer holds an address to write to"
        );
    }

    /// The group parser: the two names Pleroma's guard clause lists, and
    /// nothing else — `Admin` and an empty segment are no group.
    #[test]
    fn only_the_two_group_names_are_a_group() {
        assert_eq!(Role::parse("admin"), Some(Role::Admin));
        assert_eq!(Role::parse("moderator"), Some(Role::Moderator));
        for text in ["Admin", "mod", "", "is_admin", " admin"] {
            assert_eq!(Role::parse(text), None, "no such group name: {text}");
        }
    }

    /// The group's own two spellings: the name the log's `permission` key
    /// and the route segment use, and the column the flag lives in.
    #[test]
    fn a_group_names_itself_and_its_column() {
        assert_eq!(Role::Admin.as_str(), "admin", "the log's permission");
        assert_eq!(
            Role::Moderator.as_str(),
            "moderator",
            "and the other group's"
        );
        assert_eq!(Role::Admin.column(), "is_admin", "the admin column");
        assert_eq!(
            Role::Moderator.column(),
            "is_moderator",
            "the moderator column"
        );
    }

    /// The flag reader, against the shared fixture's rows: `admin` weighs
    /// `root`, `moderator` weighs `moddy`, and neither finds a flag on the
    /// plain account.
    #[tokio::test]
    async fn a_group_reads_its_own_flag_off_the_row() {
        let (_db, pool) = fixture().await;
        let root = get_by_nickname_or_id(&pool, "root")
            .await
            .expect("the admin row reads")
            .expect("the fixture seeded it");
        let moddy = get_by_nickname_or_id(&pool, "moddy")
            .await
            .expect("the moderator row reads")
            .expect("the fixture seeded it");
        let plain = get_by_nickname_or_id(&pool, "plain")
            .await
            .expect("the plain row reads")
            .expect("the fixture seeded it");
        assert!(Role::Admin.holds(&root), "root is an admin");
        assert!(!Role::Moderator.holds(&root), "and no moderator");
        assert!(Role::Moderator.holds(&moddy), "moddy is a moderator");
        assert!(!Role::Admin.holds(&moddy), "and no admin");
        assert!(
            !Role::Admin.holds(&plain) && !Role::Moderator.holds(&plain),
            "the plain account holds neither"
        );
    }

    /// The two writers that replace one column: the tag array lands as it
    /// was handed — an empty list clears it — and the group write moves only
    /// its own flag, leaving the other one and the tags alone.
    #[tokio::test]
    async fn the_tag_and_group_writes_move_their_own_column_only() {
        let db = crate::test_db::test_db().await.expect("test database slot");
        let pool = db.pool().clone();
        let alice = insert(
            &pool,
            &NewUser {
                tags: vec!["bots".to_owned()],
                is_moderator: Flag::new(true),
                ..NewUser::local("alice", "https://test.example/users/alice")
            },
        )
        .await
        .expect("the fixture inserts");

        set_tags(&pool, alice.id, &["staff".to_owned(), "ops".to_owned()])
            .await
            .expect("the tag write runs");
        let stored = get_by_id(&pool, alice.id)
            .await
            .expect("the row reads back")
            .expect("it was there");
        assert_eq!(
            stored.tags,
            vec!["staff", "ops"],
            "the array is replaced, not merged"
        );
        assert!(stored.is_moderator.get(), "and the flags are untouched");

        set_role(&pool, alice.id, Role::Admin, true)
            .await
            .expect("the grant runs");
        let granted = get_by_id(&pool, alice.id)
            .await
            .expect("the row reads back")
            .expect("it was there");
        assert!(granted.is_admin.get(), "the admin flag moved");
        assert!(
            granted.is_moderator.get(),
            "the moderator flag kept its value"
        );
        assert_eq!(granted.tags, vec!["staff", "ops"], "and the tags stayed");

        set_role(&pool, alice.id, Role::Moderator, false)
            .await
            .expect("the revoke runs");
        let revoked = get_by_id(&pool, alice.id)
            .await
            .expect("the row reads back")
            .expect("it was there");
        assert!(!revoked.is_moderator.get(), "the moderator flag moved");
        assert!(revoked.is_admin.get(), "and the admin one stayed");

        set_tags(&pool, Uuid::now_v7(), &[])
            .await
            .expect("a miss writes nothing, and is no error");
        set_role(&pool, Uuid::now_v7(), Role::Admin, false)
            .await
            .expect("the group write is content with a miss");
    }
}
