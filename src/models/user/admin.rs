//! The admin API's reads and writes over `users`.
//!
//! The reads are the filtered, counted page behind
//! `GET /api/v1/pleroma/admin/users` and the nickname-or-id lookup behind
//! `GET /api/v1/pleroma/admin/users/{nickname}` (WBE-195).
//!
//! [`staff_with_email`] is a third read: the active local admins and
//! moderators that hold an address, which is who the report letter goes to
//! (WBE-212). [`resolve_nicknames`] is the fourth: the batch nickname
//! lookup the list-taking admin account endpoints act on (WBE-198).
//!
//! The three writers belong to the maintenance the admin API does to one
//! account rather than to the profile the account edits itself, so they
//! sit here beside the lookups that name the row: [`set_tags`] replaces the
//! profile-tag array (WBE-197), [`set_role`] moves one of the two staff
//! flags [`Role`] names, and [`set_suggested`] the suggestion-pool flag the
//! `GET /api/v{1,2}/suggestions` listings read back. Like
//! [`set_also_known_as`](super::set_also_known_as) and
//! [`set_settings_store`](super::set_settings_store) above them, each
//! writes exactly the whole value the endpoint computed — the merge,
//! dedup and skip rules of Pleroma's `User.tag`, `User.untag` and
//! `User.set_suggestion` live in `src/api/admin/users.rs`, where the
//! requests are.
//!
//! The reference is `Pleroma.Web.AdminAPI.Search.user`
//! (`lib/pleroma/web/admin_api/search.ex`) over `Pleroma.User.Query.build`
//! (`lib/pleroma/user/query.ex`): one `WHERE` clause assembled from the
//! request's criteria, counted without the page window and then windowed by
//! `LIMIT`/`OFFSET` (`User.Query.paginate`). Pleroma orders that listing by
//! `desc: id`; the ticket pins the nickname order instead, which the unique
//! citext column makes stable without a tie-break.
//!
//! Four of Pleroma's criteria are re-worded for this schema:
//!
//! - `internal: false` (its `nickname NOT LIKE 'internal.%'`) is this
//!   repo's [`NOT_INSTANCE_ACTOR`] — the one account a listing hides. The
//!   direct lookup does not apply it: naming an account is a lookup, not a
//!   listing (see the note on [`NOT_INSTANCE_ACTOR`]).
//! - `is_approved` and `is_confirmed` are not columns here;
//!   [`approval_pending`] and [`email_confirmed`] are their opposites, so
//!   `active`, `deactivated`, `need_approval` and `unconfirmed` read those
//!   two columns inverted.
//! - `query` is Pleroma's nickname `ILIKE` arm widened to the three text
//!   identities an admin searches by: nickname, display name and email.
//! - `invisible` (`info->>'invisible'`) has no column here, so it is not a
//!   criterion: nothing hides a row beyond the instance actor.
//!
//! The citext columns are cast to `text` wherever they meet `ILIKE` (the
//! same cast [`user_search`](crate::models::user_search) applies) and to
//! `citext` wherever they meet `=`, because a bare text parameter would
//! fold to the case-sensitive `text = text` operator — the reason
//! [`get_by_email`] casts too.
//!
//! [`approval_pending`]: super::User::approval_pending
//! [`email_confirmed`]: super::User::email_confirmed
//! [`NOT_INSTANCE_ACTOR`]: super::NOT_INSTANCE_ACTOR
//! [`get_by_email`]: super::get_by_email

use sqlx::{PgPool, Postgres, QueryBuilder};
use uuid::Uuid;

use crate::{error::AppResult, models::user::NOT_INSTANCE_ACTOR, util::id::parse_id};

use super::{Flag, User};

/// The head of [`list`]'s page statement; [`push_where`] appends the shared
/// `WHERE` clause and [`push_page`] the order and the window.
const LIST_SQL: &str = "SELECT * FROM users";

/// The head of [`count_matching`]: the same rows, counted without the
/// window (Pleroma's `Repo.aggregate(query, :count, :id)`).
const COUNT_SQL: &str = "SELECT count(*) FROM users";

/// The boolean criteria of Pleroma's admin user listing: the eight names of
/// its `@filters` list, each set when the request's comma list carried it.
///
/// Every one of them narrows the same set — Pleroma's reduce over the
/// filters map adds one `WHERE` arm per name, so `local,deactivated` is a
/// deactivated local account and `local,external` answers nothing.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct ListFilters {
    /// `local`: accounts this instance owns.
    pub local: Flag,
    /// `external`: remote mirrors.
    pub external: Flag,
    /// `active`: Pleroma's full green light — active, approved, confirmed.
    pub active: Flag,
    /// `deactivated`: [`is_active`](super::User::is_active) is false.
    pub deactivated: Flag,
    /// `need_approval`: [`approval_pending`](super::User::approval_pending)
    /// is set, so staff still have to approve the registration.
    pub need_approval: Flag,
    /// `unconfirmed`: [`email_confirmed`](super::User::email_confirmed) is
    /// false.
    pub unconfirmed: Flag,
    /// `is_admin`: the account holds the admin role.
    pub is_admin: Flag,
    /// `is_moderator`: the account holds the moderator role.
    pub is_moderator: Flag,
}

impl ListFilters {
    /// The filters of one request, from the names its comma-separated
    /// `filters` parameter carries: [`local`](Self::local) and its seven
    /// siblings, matched exactly — Pleroma's `maybe_parse_filters` splits
    /// on `,` and keeps only the names on its list, so an unknown, an empty
    /// or a spaced spelling flips nothing. A missing parameter is the all
    /// unset default.
    #[must_use]
    pub fn parse(text: Option<&str>) -> Self {
        let mut found = Self::default();
        for name in text.unwrap_or_default().split(',') {
            found.set(name);
        }
        found
    }

    /// Flips the one filter `name` names, leaving the rest as they were.
    fn set(&mut self, name: &str) {
        let selected = Flag::new(true);
        match name {
            "local" => self.local = selected,
            "external" => self.external = selected,
            "active" => self.active = selected,
            "deactivated" => self.deactivated = selected,
            "need_approval" => self.need_approval = selected,
            "unconfirmed" => self.unconfirmed = selected,
            "is_admin" => self.is_admin = selected,
            "is_moderator" => self.is_moderator = selected,
            _ => {}
        }
    }
}

/// The criteria of one [`list`] call: Pleroma's `search_params` as this
/// schema can answer them, plus the page window.
///
/// Each optional text criterion is `None` when the request carried neither
/// the key nor a value worth filtering on, and each list is empty by
/// default; an empty criterion narrows nothing, the way
/// `User.Query.compose_query` guards `not_empty_string` and ignores an
/// empty list.
#[derive(Clone, Debug, Default)]
pub struct ListParams {
    /// Case-insensitive substring of nickname, display name or email.
    pub query: Option<String>,
    /// Case-insensitive substring of the display name.
    pub name: Option<String>,
    /// Exact email (the citext column folds its case, as Pleroma's equality
    /// criterion on its own citext column does).
    pub email: Option<String>,
    /// Profile tags to overlap with: a row matches when it carries any one
    /// of them (Pleroma's `tags && ^tags`).
    pub tags: Vec<String>,
    /// Actor types to keep — `Person`, `Service`, ... — matched exactly.
    pub actor_types: Vec<String>,
    /// The boolean criteria of the `filters` parameter.
    pub filters: ListFilters,
    /// 1-based page number; [`list`] treats anything below the first page as
    /// the first page.
    pub page: i64,
    /// Rows in the page. The endpoint owns the default and the cap
    /// (Pleroma's `@users_page_size` of 50 under the ticket's 200 ceiling);
    /// [`list`] never hands a negative bound to Postgres.
    pub page_size: i64,
}

/// One [`list`] answer: the page's rows and the total the same criteria
/// select across every page.
#[derive(Debug)]
pub struct ListPage {
    /// The page, in [`list`]'s nickname order.
    pub users: Vec<User>,
    /// How many rows the criteria match, pages included — the `count` of
    /// the endpoint's answer.
    pub count: i64,
}

/// The admin listing: every account `params` selects, the instance actor
/// excepted, one page at a time beside the whole total.
///
/// # Errors
///
/// Any database failure arrives as [`AppError::Database`](crate::error::AppError).
pub async fn list(pool: &PgPool, params: &ListParams) -> AppResult<ListPage> {
    let count = count_matching(pool, params).await?;
    let mut builder = QueryBuilder::<Postgres>::new(LIST_SQL);
    push_where(&mut builder, params);
    push_page(&mut builder, params);
    let users = builder.build_query_as::<User>().fetch_all(pool).await?;
    Ok(ListPage { users, count })
}

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
/// of [`list`] are listing rules.
///
/// # Errors
///
/// Any database failure arrives as [`AppError::Database`](crate::error::AppError).
pub async fn get_by_nickname_or_id(pool: &PgPool, raw: &str) -> AppResult<Option<User>> {
    if let Some(found) = super::get_by_nickname(pool, raw).await? {
        return Ok(Some(found));
    }
    match parse_id(raw) {
        Some(id) => super::get_by_id(pool, id).await,
        None => Ok(None),
    }
}

/// The accounts one admin body's `nicknames` list names, in the order the
/// list spells them: [`get_by_nickname`](super::get_by_nickname) per
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
        if let Some(user) = super::get_by_nickname(pool, nickname).await? {
            found.push(user);
        }
    }
    Ok(found)
}

/// The number of rows `params`' criteria select, whatever page they land
/// on.
async fn count_matching(pool: &PgPool, params: &ListParams) -> AppResult<i64> {
    let mut builder = QueryBuilder::<Postgres>::new(COUNT_SQL);
    push_where(&mut builder, params);
    let total = builder.build_query_scalar::<i64>().fetch_one(pool).await?;
    Ok(total)
}

/// Appends the `WHERE` clause [`list`] and [`count_matching`] share: an
/// always-true predicate leads (the way [`crate::models::moderation_log`]
/// arranges it) so every criterion is one more `AND`, then the instance-actor
/// exclusion, the boolean filters and the text criteria.
fn push_where(builder: &mut QueryBuilder<Postgres>, params: &ListParams) {
    let _where: &mut _ = builder.push(" WHERE true").push(NOT_INSTANCE_ACTOR);
    push_filters(builder, params.filters);
    push_criteria(builder, params);
}

/// Appends the `WHERE` arms of [`ListFilters`], grouped by what they weigh:
/// origin, account state, roles.
fn push_filters(builder: &mut QueryBuilder<Postgres>, filters: ListFilters) {
    push_location(builder, filters);
    push_state(builder, filters);
    push_roles(builder, filters);
}

/// The `local` / `external` arms: Pleroma's `location_query` over the
/// `local` column of decision 7.
fn push_location(builder: &mut QueryBuilder<Postgres>, filters: ListFilters) {
    if filters.local.get() {
        let _local: &mut _ = builder.push(" AND local");
    }
    if filters.external.get() {
        let _external: &mut _ = builder.push(" AND NOT local");
    }
}

/// The arms that weigh [`is_active`](super::User::is_active),
/// [`approval_pending`](super::User::approval_pending) and
/// [`email_confirmed`](super::User::email_confirmed): Pleroma's `active`
/// wants all three green — the mirror's as much as a local account's —
/// `deactivated` a false `is_active`, `need_approval` a pending approval and
/// `unconfirmed` an address nobody confirmed.
fn push_state(builder: &mut QueryBuilder<Postgres>, filters: ListFilters) {
    if filters.active.get() {
        let _active: &mut _ =
            builder.push(" AND is_active AND NOT approval_pending AND email_confirmed");
    }
    if filters.deactivated.get() {
        let _deactivated: &mut _ = builder.push(" AND NOT is_active");
    }
    if filters.need_approval.get() {
        let _pending: &mut _ = builder.push(" AND approval_pending");
    }
    if filters.unconfirmed.get() {
        let _unconfirmed: &mut _ = builder.push(" AND NOT email_confirmed");
    }
}

/// The role arms: Pleroma's `is_admin` / `is_moderator` criteria, each an
/// equality on `true`.
fn push_roles(builder: &mut QueryBuilder<Postgres>, filters: ListFilters) {
    if filters.is_admin.get() {
        let _admin: &mut _ = builder.push(" AND is_admin");
    }
    if filters.is_moderator.get() {
        let _moderator: &mut _ = builder.push(" AND is_moderator");
    }
}

/// Appends the text and list criteria: [`query`](ListParams::query) over the
/// three identity columns, [`name`](ListParams::name),
/// [`email`](ListParams::email), the [`tags`](ListParams::tags) overlap and
/// the [`actor_types`](ListParams::actor_types) membership.
fn push_criteria(builder: &mut QueryBuilder<Postgres>, params: &ListParams) {
    if let Some(needle) = &params.query {
        let pattern = contains_pattern(needle);
        let _three: &mut _ = builder
            .push(" AND (nickname::text ILIKE ")
            .push_bind(pattern.clone())
            .push(" OR name ILIKE ")
            .push_bind(pattern.clone())
            .push(" OR email::text ILIKE ")
            .push_bind(pattern)
            .push(")");
    }
    if let Some(needle) = &params.name {
        let _name: &mut _ = builder
            .push(" AND name ILIKE ")
            .push_bind(contains_pattern(needle));
    }
    if let Some(address) = &params.email {
        let _email: &mut _ = builder
            .push(" AND email = ")
            .push_bind(address.clone())
            .push("::citext");
    }
    if !params.tags.is_empty() {
        let _tags: &mut _ = builder.push(" AND tags && ").push_bind(params.tags.clone());
    }
    if !params.actor_types.is_empty() {
        let _types: &mut _ = builder
            .push(" AND actor_type = ANY(")
            .push_bind(params.actor_types.clone())
            .push(")");
    }
}

/// The `ILIKE` pattern of a substring criterion: `%` around `needle`, whose
/// own metacharacters (`%`, `_` and the escape character) are
/// backslash-escaped so a search matches what was typed (Pleroma's
/// `ilike(u.nickname, "%#{query}%")` over `escape_ilike`).
fn contains_pattern(needle: &str) -> String {
    let mut pattern = String::with_capacity(needle.len() + 2);
    pattern.push('%');
    for symbol in needle.chars() {
        if matches!(symbol, '%' | '_' | '\\') {
            pattern.push('\\');
        }
        pattern.push(symbol);
    }
    pattern.push('%');
    pattern
}

/// Appends the order and the window: the nickname order the ticket pins
/// (citext, so case-insensitive ascending) and Pleroma's
/// `User.Query.paginate` window, whose bounds come from [`page_window`] so
/// neither can reach Postgres negative.
fn push_page(builder: &mut QueryBuilder<Postgres>, params: &ListParams) {
    let (size, offset) = page_window(params);
    let _window: &mut _ = builder
        .push(" ORDER BY nickname LIMIT ")
        .push_bind(size)
        .push(" OFFSET ")
        .push_bind(offset);
}

/// The `LIMIT` and `OFFSET` of one page: `page_size` rows, `page_size` of
/// them skipped for each page before the requested one. A page below the
/// first starts at the first row; a negative size lists nothing rather than
/// failing the statement.
fn page_window(params: &ListParams) -> (i64, i64) {
    let size = params.page_size.max(0);
    (size, (params.page - 1).max(0).saturating_mul(size))
}

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
/// the same split [`set_also_known_as`](super::set_also_known_as)
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

/// The `UPDATE` behind [`set_suggested`]: writes the pool flag, refreshes
/// `updated_at` and returns the stored row.
const SET_SUGGESTED_SQL: &str = "UPDATE users SET is_suggested = $2, updated_at = now() \
                                 WHERE id = $1 RETURNING *";

/// Stores `suggested` as user `id`'s [`is_suggested`](User::is_suggested)
/// flag and returns the updated row.
///
/// The write of the admin `PATCH /users/suggest` and `PATCH
/// /users/unsuggest` pair, whose answer is the affected accounts as the
/// admin entity reads them, so the fresh row comes back for the endpoint to
/// render.
///
/// `None` when `id` names no row: the account went away between the
/// endpoint's lookup and this write, and it is simply not among the
/// answered users.
///
/// # Errors
///
/// Any database failure arrives as [`AppError::Database`](crate::error::AppError).
pub async fn set_suggested(pool: &PgPool, id: Uuid, suggested: bool) -> AppResult<Option<User>> {
    let updated = sqlx::query_as::<_, User>(SET_SUGGESTED_SQL)
        .bind(id)
        .bind(Flag::new(suggested))
        .fetch_optional(pool)
        .await?;
    Ok(updated)
}

#[cfg(test)]
mod tests {
    use sqlx::PgPool;

    use super::*;
    use crate::models::user::{NewUser, insert};

    /// The account the listing must never answer with, and the direct
    /// lookup may.
    const INSTANCE_ACTOR: &str = "internal.fetch";

    /// Inserts a local account named `nickname` with `customize` applied on
    /// top of [`NewUser::local`]; the closure is how a fixture turns that
    /// into a remote mirror or an account in some other state.
    async fn seeded<F>(pool: &PgPool, nickname: &str, customize: F)
    where
        F: FnOnce(NewUser) -> NewUser,
    {
        let new = customize(NewUser::local(
            nickname,
            &format!("https://test.example/users/{nickname}"),
        ));
        let _row: User = insert(pool, &new).await.expect("seed inserts");
    }

    /// The nine accounts the tests below read: the instance actor, six
    /// locals each in a state of its own, a tagged `Service` and a remote
    /// mirror. One slot per test, which the fixture empties between tests.
    async fn fixture() -> (crate::test_db::TestDb, PgPool) {
        let db = crate::test_db::test_db().await.expect("test database slot");
        let pool = db.pool().clone();
        seeded(&pool, INSTANCE_ACTOR, |user| user).await;
        seeded(&pool, "plain", |user| NewUser {
            name: "Plain Person".to_owned(),
            email: Some("plain@example.org".to_owned()),
            ..user
        })
        .await;
        seeded(&pool, "root", |user| NewUser {
            is_admin: Flag::new(true),
            email: Some("root@example.org".to_owned()),
            ..user
        })
        .await;
        seeded(&pool, "moddy", |user| NewUser {
            is_moderator: Flag::new(true),
            ..user
        })
        .await;
        seeded(&pool, "suspended", |user| NewUser {
            is_active: Flag::new(false),
            ..user
        })
        .await;
        seeded(&pool, "newbie", |user| NewUser {
            approval_pending: Flag::new(true),
            registration_reason: Some("please let me in".to_owned()),
            ..user
        })
        .await;
        seeded(&pool, "unminded", |user| NewUser {
            email_confirmed: Flag::new(false),
            ..user
        })
        .await;
        seeded(&pool, "beeper", |user| NewUser {
            actor_type: "Service".to_owned(),
            tags: vec!["bots".to_owned()],
            ..user
        })
        .await;
        seeded(&pool, "faraway@remote.example", |user| NewUser {
            local: Flag::new(false),
            ap_id: "https://remote.example/users/faraway".to_owned(),
            domain: Some("remote.example".to_owned()),
            name: "Far Away".to_owned(),
            ..user
        })
        .await;
        (db, pool)
    }

    /// The nicknames one page answers with, in the order it answers them.
    fn listed(page: &ListPage) -> Vec<&str> {
        page.users
            .iter()
            .map(|found| found.nickname.as_str())
            .collect()
    }

    /// Everything, on one page of `size` rows.
    fn everything(size: i64) -> ListParams {
        ListParams {
            page: 1,
            page_size: size,
            ..ListParams::default()
        }
    }

    /// The listing with one `filters` parameter and no other criterion.
    fn filtered(names: &str) -> ListParams {
        ListParams {
            filters: ListFilters::parse(Some(names)),
            ..everything(50)
        }
    }

    /// The criterion-free page lists every account but the instance actor,
    /// in nickname order, with the same eight as its total.
    #[tokio::test]
    async fn the_unfiltered_page_orders_by_nickname_and_hides_the_actor() {
        let (_db, pool) = fixture().await;
        let page = list(&pool, &everything(50))
            .await
            .expect("the listing reads");
        assert_eq!(
            listed(&page),
            vec![
                "beeper",
                "faraway@remote.example",
                "moddy",
                "newbie",
                "plain",
                "root",
                "suspended",
                "unminded",
            ],
            "eight accounts, `internal.fetch` excluded"
        );
        assert_eq!(page.count, 8, "the total is the same eight");
    }

    /// Each of the eight filters selects exactly its own accounts, weighed
    /// against rows that differ from every other row in exactly one state.
    #[tokio::test]
    async fn every_filter_selects_its_own_rows() {
        let (_db, pool) = fixture().await;
        let cases: &[(&str, &[&str])] = &[
            (
                "local",
                &[
                    "beeper",
                    "moddy",
                    "newbie",
                    "plain",
                    "root",
                    "suspended",
                    "unminded",
                ],
            ),
            ("external", &["faraway@remote.example"]),
            (
                "active",
                &["beeper", "faraway@remote.example", "moddy", "plain", "root"],
            ),
            ("deactivated", &["suspended"]),
            ("need_approval", &["newbie"]),
            ("unconfirmed", &["unminded"]),
            ("is_admin", &["root"]),
            ("is_moderator", &["moddy"]),
        ];
        for (name, expected) in cases {
            let page = list(&pool, &filtered(name)).await.expect("a filter reads");
            assert_eq!(listed(&page), expected.to_vec(), "the {name} filter");
            assert_eq!(
                page.count,
                i64::try_from(expected.len()).expect("a small count"),
                "the {name} total"
            );
        }
    }

    /// Two filters narrow the set together, and a pair no row satisfies
    /// answers nothing with a zero total: `count` runs the same `WHERE`
    /// clause as the page.
    #[tokio::test]
    async fn filters_combine_with_and() {
        let (_db, pool) = fixture().await;
        let both = list(&pool, &filtered("external,active"))
            .await
            .expect("a filter pair reads");
        assert_eq!(
            listed(&both),
            vec!["faraway@remote.example"],
            "the mirror is active too"
        );
        assert_eq!(both.count, 1, "and counts as one");
        let impossible = list(&pool, &filtered("local,external"))
            .await
            .expect("a filter pair reads");
        assert!(
            impossible.users.is_empty(),
            "no account is both local and remote"
        );
        assert_eq!(impossible.count, 0, "and the total says so");
    }

    /// `query` is a case-insensitive substring of nickname, display name or
    /// email — the three columns the ticket names — while `name` is the
    /// display name only and `email` an exact address. The registration
    /// reason is none of those three, so it is not searched.
    #[tokio::test]
    async fn the_text_criteria_search_the_columns_the_ticket_names() {
        let (_db, pool) = fixture().await;
        let cases: &[(&str, &str)] = &[
            ("far", "faraway@remote.example"),
            ("PLAIN", "plain"),
            ("person", "plain"),
            ("ROOT@EXAMPLE.ORG", "root"),
            ("BEEPER", "beeper"),
        ];
        for (needle, expected) in cases {
            let params = ListParams {
                query: Some((*needle).to_owned()),
                ..everything(50)
            };
            let page = list(&pool, &params).await.expect("a query reads");
            assert_eq!(listed(&page), vec![*expected], "the {needle} substring");
            assert_eq!(page.count, 1, "the {needle} total");
        }
        let reason = ListParams {
            query: Some("please let me in".to_owned()),
            ..everything(50)
        };
        assert_eq!(
            list(&pool, &reason).await.expect("a query reads").count,
            0,
            "the registration reason is no search column"
        );
        let name = ListParams {
            name: Some("far".to_owned()),
            ..everything(50)
        };
        assert_eq!(
            listed(&list(&pool, &name).await.expect("a name reads")),
            vec!["faraway@remote.example"],
            "the display name holds the only `far`"
        );
        let address = ListParams {
            email: Some("ROOT@example.org".to_owned()),
            ..everything(50)
        };
        assert_eq!(
            listed(&list(&pool, &address).await.expect("an email reads")),
            vec!["root"],
            "citext folds the address's case"
        );
        let partial = ListParams {
            email: Some("root@example".to_owned()),
            ..everything(50)
        };
        assert!(
            list(&pool, &partial)
                .await
                .expect("an email reads")
                .users
                .is_empty(),
            "but `email` is no substring match"
        );
    }

    /// A `query` made of `ILIKE` metacharacters matches them literally, and
    /// a needle nothing carries answers the empty page with a zero total.
    #[tokio::test]
    async fn the_query_escapes_wildcards_and_can_miss() {
        let (_db, pool) = fixture().await;
        let wildcards = ListParams {
            query: Some("%a%".to_owned()),
            ..everything(50)
        };
        assert!(
            list(&pool, &wildcards)
                .await
                .expect("a query reads")
                .users
                .is_empty(),
            "a percent in the needle is a literal percent"
        );
        let miss = ListParams {
            query: Some("nobody".to_owned()),
            ..everything(50)
        };
        assert_eq!(
            list(&pool, &miss).await.expect("a query reads").count,
            0,
            "an unmatched needle counts nothing"
        );
    }

    /// `tags` overlaps the profile tags and `actor_types` keeps the actor
    /// types it names; a list that names nothing selects nothing, and an
    /// empty list is no criterion at all.
    #[tokio::test]
    async fn tags_and_actor_types_select_by_membership() {
        let (_db, pool) = fixture().await;
        let by_tag = ListParams {
            tags: vec!["bots".to_owned(), "staff".to_owned()],
            ..everything(50)
        };
        assert_eq!(
            listed(&list(&pool, &by_tag).await.expect("a tag reads")),
            vec!["beeper"],
            "one of two tags is enough"
        );
        let by_type = ListParams {
            actor_types: vec!["Service".to_owned(), "Application".to_owned()],
            ..everything(50)
        };
        assert_eq!(
            listed(&list(&pool, &by_type).await.expect("a type reads")),
            vec!["beeper"],
            "one of two types is enough"
        );
        let unknown = ListParams {
            actor_types: vec!["Group".to_owned()],
            tags: vec!["nobody".to_owned()],
            ..everything(50)
        };
        assert_eq!(
            list(&pool, &unknown)
                .await
                .expect("a criterion reads")
                .count,
            0,
            "both lists are criteria, and both miss"
        );
    }

    /// The window moves while the total stays: page 2 answers the next
    /// `page_size` nicknames, a page past the last row answers nothing, and
    /// `count` is the whole set on every page.
    #[tokio::test]
    async fn the_page_window_moves_while_the_total_stays() {
        let (_db, pool) = fixture().await;
        let first = list(&pool, &everything(3))
            .await
            .expect("the first page reads");
        assert_eq!(
            listed(&first),
            vec!["beeper", "faraway@remote.example", "moddy"],
            "the first three nicknames"
        );
        assert_eq!(first.count, 8, "the total counts every page");
        let second = ListParams {
            page: 2,
            ..everything(3)
        };
        let next = list(&pool, &second).await.expect("a page reads");
        assert_eq!(
            listed(&next),
            vec!["newbie", "plain", "root"],
            "the next three"
        );
        assert_eq!(next.count, 8, "and the total is untouched by the window");
        let last = ListParams {
            page: 3,
            ..everything(3)
        };
        assert_eq!(
            listed(&list(&pool, &last).await.expect("a page reads")),
            vec!["suspended", "unminded"],
            "the third page is the short one"
        );
        let past = ListParams {
            page: 9,
            ..everything(3)
        };
        assert!(
            list(&pool, &past)
                .await
                .expect("a page past the end reads")
                .users
                .is_empty(),
            "and a page past the last row answers nothing"
        );
    }

    /// An unusable window cannot reach Postgres as a syntax error: a page
    /// below the first is the first page and a negative size lists nothing,
    /// while the total still counts.
    #[tokio::test]
    async fn an_unusable_window_is_clamped_not_rejected() {
        let (_db, pool) = fixture().await;
        let params = ListParams {
            page: 0,
            page_size: -5,
            ..ListParams::default()
        };
        let page = list(&pool, &params).await.expect("a clamped window reads");
        assert!(page.users.is_empty(), "a negative size lists nothing");
        assert_eq!(page.count, 8, "and the count is untouched");
        assert_eq!(page_window(&params), (0, 0), "both bounds fold to zero");
        assert_eq!(
            page_window(&ListParams {
                page: -3,
                page_size: 10,
                ..ListParams::default()
            }),
            (10, 0),
            "a page before the first is the first page"
        );
        assert_eq!(
            page_window(&ListParams {
                page: 4,
                page_size: 25,
                ..ListParams::default()
            }),
            (25, 75),
            "an ordinary page is Pleroma's offset"
        );
    }

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

    /// The filter parser: a known name flips its own flag; absent, empty,
    /// unknown, spaced and wrongly-cased spellings flip nothing; and every
    /// name on Pleroma's `@filters` list is a flag of [`ListFilters`].
    #[test]
    fn the_filter_parser_keeps_only_known_names() {
        assert_eq!(
            ListFilters::parse(None),
            ListFilters::default(),
            "an absent parameter selects nothing"
        );
        assert_eq!(
            ListFilters::parse(Some("")),
            ListFilters::default(),
            "so does an empty one"
        );
        let one = ListFilters::parse(Some("is_moderator"));
        assert!(
            one.is_moderator.get() && !one.is_admin.get(),
            "one name flips one flag"
        );
        let junk = ListFilters::parse(Some("local,ghost, LOCAL,,need_approval "));
        assert_eq!(
            junk,
            ListFilters {
                local: Flag::new(true),
                ..ListFilters::default()
            },
            "an unknown, a spaced, an empty and a wrongly-cased name add nothing"
        );
        let all = ListFilters::parse(Some(
            "local,external,active,deactivated,need_approval,unconfirmed,is_admin,is_moderator",
        ));
        for flag in [
            all.local,
            all.external,
            all.active,
            all.deactivated,
            all.need_approval,
            all.unconfirmed,
            all.is_admin,
            all.is_moderator,
        ] {
            assert!(flag.get(), "every name on the list flips its flag");
        }
    }

    /// The substring pattern: `%` wrapped around the needle, whose own
    /// wildcards and escape character are backslash-prefixed.
    #[test]
    fn the_substring_pattern_wraps_and_escapes() {
        assert_eq!(contains_pattern("alice"), "%alice%", "an ordinary needle");
        assert_eq!(
            contains_pattern("50%_off"),
            "%50\\%\\_off%",
            "metacharacters stay literal"
        );
        assert_eq!(contains_pattern(""), "%%", "an empty needle matches all");
    }

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
        let stored = super::super::get_by_id(&pool, alice.id)
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
        let granted = super::super::get_by_id(&pool, alice.id)
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
        let revoked = super::super::get_by_id(&pool, alice.id)
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

    /// The pool flag lands and the fresh row comes back with it — the pair
    /// the suggest endpoint answers its entities from — and an unknown id
    /// answers `None` rather than inventing a user.
    #[tokio::test]
    async fn set_suggested_writes_the_flag_and_returns_the_row() {
        let db = crate::test_db::test_db().await.expect("test database slot");
        let pool = db.pool().clone();
        let alice = insert(
            &pool,
            &NewUser::local("alice", "https://test.example/users/alice"),
        )
        .await
        .expect("the fixture inserts");

        let picked = set_suggested(&pool, alice.id, true)
            .await
            .expect("the suggest runs")
            .expect("the row comes back");
        assert!(picked.is_suggested.get(), "the account is in the pool");
        assert_eq!(picked.nickname, "alice", "the whole row came back");

        let dropped = set_suggested(&pool, alice.id, false)
            .await
            .expect("the unsuggest runs")
            .expect("the row comes back");
        assert!(!dropped.is_suggested.get(), "and it is out of the pool");
        assert!(
            set_suggested(&pool, Uuid::now_v7(), true)
                .await
                .expect("a miss writes nothing")
                .is_none(),
            "an unknown id matches no row"
        );
    }
}
