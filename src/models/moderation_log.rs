//! The `moderation_log` table: one row per staff action, recorded as the
//! full sentence Pleroma's `Pleroma.ModerationLog`
//! (`lib/pleroma/moderation_log.ex`) keeps for its admin frontend.
//!
//! Pleroma stores the whole entry in one `data` jsonb map and re-renders
//! the message on every read (`get_log_entry_message` there); here the
//! sentence is built at record time and stored, and the parts the listing
//! filters on — actor, subject ids and nicknames, action, time — are plain
//! columns of the row. The `data` jsonb still keeps Pleroma's map shape
//! (`actor`, `action`, `message` plus the action-specific subject and
//! extras), because `GET /api/v1/pleroma/admin/moderation_log` answers each
//! entry's stored `data` verbatim, exactly like Pleroma's
//! `ModerationLogView`
//! (`lib/pleroma/web/admin_api/views/moderation_log_view.ex`) renders
//! `log_entry.data`.
//!
//! [`record`] is the one writer: [`ModAction`] names the action, carries
//! the affected users (into the `subject_ids` / `subject_nicknames`
//! columns) and the sentence, and its `data` map is what the action adds
//! beyond actor/action/message. The small per-action helpers built on it —
//! [`deactivate`], [`activate`], [`delete`], [`approve`], [`reject`],
//! [`account_action`], [`create_users`],
//! [`tag`],
//! [`untag`], [`grant`], [`revoke`], [`force_password_reset`],
//! [`updated_users`], [`confirm_email`], [`resend_confirmation_email`],
//! [`disable_mfa`], [`add_suggestion`], [`remove_suggestion`],
//! [`follow`], [`unfollow`],
//! [`report_update`], [`report_assigned`], [`report_unassigned`],
//! [`report_note`], [`report_note_delete`], [`status_update`],
//! [`status_delete`], [`chat_message_delete`], [`relay_follow`], [`relay_unfollow`],
//! [`delete_instance`], [`reload_emoji`],
//! [`invite_create`], [`invite_revoke`], [`config_change`],
//! [`webhook_create`], [`webhook_update`], [`webhook_delete`],
//! [`webhook_rotate_secret`], [`emoji_pack_create`],
//! [`emoji_pack_update`], [`emoji_pack_delete`], [`rule_create`],
//! [`rule_update`], [`rule_delete`], [`announcement_create`],
//! [`announcement_update`] and [`announcement_delete`] — compose
//! the sentence word for word as Pleroma's `get_log_entry_message` clauses
//! spell it, so later admin tickets call
//! `moderation_log::deactivate(pool, &admin, &users)` and never
//! hand-assemble a message. Actions still without a helper here join the
//! set with the tickets that log them. The webhook maintenance actions —
//! [`webhook_create`], [`webhook_update`], [`webhook_delete`] and
//! [`webhook_rotate_secret`] —
//! name their row by id the way the invite pair does; the emoji-pack
//! administration actions — [`emoji_pack_create`], [`emoji_pack_update`]
//! and [`emoji_pack_delete`] — name their subject by its directory name,
//! since a pack has no id, and the content maintenance actions — the
//! [`rule_create`] trio and the [`announcement_create`] trio — name their
//! row by id as the webhook and invite families do.
//!
//! [`list`] is the read behind the endpoint: the newest-first `page` /
//! `page_size` window over the [`LogListParams`] filters, beside the total
//! count of rows the same filters select.

use std::slice;

use chrono::{DateTime, Utc};
use serde_json::{Map, Value};
use sqlx::{PgPool, Postgres, QueryBuilder};
use uuid::Uuid;

use crate::{error::AppResult, models::user::User, util::id::new_id};

/// [`record`]: the minted id, the actor and its (denormalised) nickname,
/// the subject arrays, the action, the sentence and the data map; the
/// `inserted_at` timestamp is the column default's. `RETURNING *` brings
/// the stored row back.
const INSERT_SQL: &str = "INSERT INTO moderation_log (
     id, actor_id, actor_nickname, subject_ids, subject_nicknames,
     action, message, data
) VALUES ($1, $2, $3, $4, $5, $6, $7, $8) RETURNING *";

/// The start of [`list`]'s page statement; [`push_filters`] appends the
/// `WHERE` clause, then the fixed newest-first order (Pleroma's
/// `get_all_query`: `order_by: [desc: q.inserted_at]`, with the `UUIDv7` id
/// breaking a same-instant tie) and the `LIMIT`/`OFFSET` window.
const SELECT_SQL: &str = "SELECT * FROM moderation_log";

/// The start of the total statement behind [`list`]: the same filters,
/// counted without the window.
const COUNT_SQL: &str = "SELECT count(*) FROM moderation_log";

/// The `type` Pleroma's `user_to_map` stamps on every user entry of the
/// data map.
const TYPE_USER: &str = "user";

/// Puts `key: value` into a data map, dropping whatever stood there:
/// [`Map::insert`] with its `Option<Value>` result spent, which the
/// [`unused_results`](https://rust-lang.github.io/rfcs) lint demands and
/// the builders here do not need.
fn put(map: &mut Map<String, Value>, key: &str, value: Value) {
    let dropped: Option<Value> = map.insert(key.to_owned(), value);
    drop(dropped);
}

/// One `moderation_log` row: a staff action as it was recorded.
#[derive(Debug, sqlx::FromRow)]
pub struct ModerationLog {
    /// `UUIDv7` primary key, minted by [`record`] (decision 4).
    pub id: Uuid,
    /// The acting staff account; `None` once that account was deleted
    /// (the foreign key's `SET NULL`), while [`actor_nickname`](Self::actor_nickname) keeps
    /// naming it.
    pub actor_id: Option<Uuid>,
    /// The actor's nickname as recorded.
    pub actor_nickname: String,
    /// The affected accounts, in the order the action named them; empty
    /// when the subject is not a user.
    pub subject_ids: Vec<Uuid>,
    /// The subjects' nicknames as recorded, parallel to
    /// [`subject_ids`](Self::subject_ids).
    pub subject_nicknames: Vec<String>,
    /// The machine-readable action name (`deactivate`, `grant`, …).
    pub action: String,
    /// The full sentence, stored as built (see [`record`]).
    pub message: String,
    /// The entry's data in Pleroma's shape: `actor`, `action` and
    /// `message` beside the action-specific subject and extras.
    pub data: Value,
    /// When the action was recorded.
    pub inserted_at: DateTime<Utc>,
}

/// What to record: everything [`record`] needs beside the actor it names
/// as such (the shape of [`NewJob`](super::job::NewJob) for the moderation
/// log).
#[derive(Debug)]
pub struct ModAction<'a> {
    /// The machine-readable action name.
    pub action: String,
    /// The affected accounts, into the subject columns; empty for actions
    /// whose subject lives in [`data`](Self::data) instead (a report, a
    /// status, a relay target).
    pub subjects: Vec<&'a User>,
    /// The sentence to store, built by the per-action helpers.
    pub message: String,
    /// The action-specific extras; [`record`] adds the `actor`, `action`
    /// and `message` keys on top of it.
    pub data: Map<String, Value>,
}

/// The page [`list`] answers: one window of entries, newest first, and the
/// total number of rows the same filters select.
#[derive(Debug)]
pub struct Page {
    /// The entries of this page.
    pub items: Vec<ModerationLog>,
    /// How many entries match the filters overall, pages included.
    pub total: i64,
}

/// The filters and window of one [`list`] read, as
/// `GET /api/v1/pleroma/admin/moderation_log` parses them (Pleroma's
/// `ModerationLog.get_all` params).
#[derive(Debug)]
pub struct LogListParams {
    /// One-based page number; the caller defaults and validates it (1).
    pub page: i64,
    /// Rows per page; the caller defaults and caps it (50, max 200).
    pub page_size: i64,
    /// Only actions this account performed (`user_id`).
    pub user_id: Option<Uuid>,
    /// Only entries whose message contains this text, case-insensitively
    /// (Pleroma's `ILIKE '%…%'` on the message).
    pub search: Option<String>,
    /// The earliest `inserted_at` still on the page (`start_date`).
    pub start_date: Option<DateTime<Utc>>,
    /// The latest `inserted_at` still on the page (`end_date`).
    pub end_date: Option<DateTime<Utc>>,
}

/// Records one staff action by `actor` and returns the stored row.
///
/// The id is a fresh `UUIDv7` (decision 4) and `inserted_at` the column
/// default; the subject columns come out of `action.subjects` and the
/// stored jsonb is `action.data` with the `actor`, `action` and `message`
/// keys [`record`] owns inserted on top — a data map already carrying those
/// keys is corrected, not trusted, because the columns are the source.
///
/// # Errors
///
/// Any database failure (connection, foreign key, encoding) arrives as
/// [`AppError::Database`](crate::error::AppError). The `actor_id` foreign
/// key is the constraint failure reachable here: the subjects live in the
/// array columns, which carry no key.
pub async fn record(
    pool: &PgPool,
    actor: &User,
    action: ModAction<'_>,
) -> AppResult<ModerationLog> {
    let ModAction {
        action: name,
        subjects,
        message,
        mut data,
    } = action;
    put(&mut data, "actor", Value::Object(user_map(actor)));
    put(&mut data, "action", Value::String(name.clone()));
    put(&mut data, "message", Value::String(message.clone()));
    let subject_ids: Vec<Uuid> = subjects.iter().map(|subject| subject.id).collect();
    let subject_nicknames: Vec<String> = subjects
        .iter()
        .map(|subject| subject.nickname.clone())
        .collect();
    let row = sqlx::query_as::<_, ModerationLog>(INSERT_SQL)
        .bind(new_id())
        .bind(actor.id)
        .bind(&actor.nickname)
        .bind(subject_ids)
        .bind(subject_nicknames)
        .bind(&name)
        .bind(&message)
        .bind(Value::Object(data))
        .fetch_one(pool)
        .await?;
    Ok(row)
}

/// Lists the log newest-first through `params`' filters and page window,
/// beside the total the filters select.
///
/// # Errors
///
/// Any database failure arrives as [`AppError::Database`](crate::error::AppError).
pub async fn list(pool: &PgPool, params: &LogListParams) -> AppResult<Page> {
    let total = count_matching(pool, params).await?;
    let offset = (params.page - 1).saturating_mul(params.page_size);
    let mut builder = QueryBuilder::<Postgres>::new(SELECT_SQL);
    push_filters(&mut builder, params);
    let _window: &mut _ = builder
        .push(" ORDER BY inserted_at DESC, id DESC LIMIT ")
        .push_bind(params.page_size)
        .push(" OFFSET ")
        .push_bind(offset);
    let items = builder
        .build_query_as::<ModerationLog>()
        .fetch_all(pool)
        .await?;
    Ok(Page { items, total })
}

/// The number of rows `params`' filters select, whatever page they land on.
async fn count_matching(pool: &PgPool, params: &LogListParams) -> AppResult<i64> {
    let mut builder = QueryBuilder::<Postgres>::new(COUNT_SQL);
    push_filters(&mut builder, params);
    let total = builder.build_query_scalar::<i64>().fetch_one(pool).await?;
    Ok(total)
}

/// Appends the shared `WHERE` clause of [`list`] and [`count_matching`]:
/// the actor filter on its column, the search as `ILIKE '%…%'` over the
/// message (Pleroma's `maybe_filter_by_search`), and the inclusive time
/// window on `inserted_at` (`maybe_filter_by_date`). The always-true
/// predicate leads so every filter is one more `AND`.
fn push_filters(builder: &mut QueryBuilder<Postgres>, params: &LogListParams) {
    let _where: &mut _ = builder.push(" WHERE true");
    if let Some(user_id) = params.user_id {
        let _actor: &mut _ = builder.push(" AND actor_id = ").push_bind(user_id);
    }
    if let Some(search) = &params.search {
        let _text: &mut _ = builder
            .push(" AND message ILIKE ")
            .push_bind(format!("%{search}%"));
    }
    if let Some(start) = params.start_date {
        let _from: &mut _ = builder.push(" AND inserted_at >= ").push_bind(start);
    }
    if let Some(end) = params.end_date {
        let _to: &mut _ = builder.push(" AND inserted_at <= ").push_bind(end);
    }
}

/// One changed entry of a [`config_change`] record: the whitelisted pair
/// the admin API wrote, and whether the write deleted it rather than
/// upserting it.
#[derive(Clone, Copy, Debug)]
pub struct ConfigChange<'a> {
    /// The configuration group, colon-spelled (`:pleroma`).
    pub group: &'a str,
    /// The configuration key, colon-spelled (`:instance`, `:mrf`, …).
    pub key: &'a str,
    /// Whether the entry was deleted rather than written.
    pub delete: bool,
}

/// Records `action` over `subjects` with the sentence `"{actor} {verb}
/// users: …"`: the user-list actions share Pleroma's exact wording (the
/// `deactivate`/`activate`/`delete`/`approve`/`reject`/
/// `force_password_reset` clauses of `get_log_entry_message`) and its
/// `subject` data map.
async fn users_action(
    pool: &PgPool,
    actor: &User,
    subjects: &[User],
    action: &str,
    verb: &str,
) -> AppResult<ModerationLog> {
    let nicknames = nicknames_of(subjects);
    let message = users_sentence(&actor.nickname, verb, &nicknames);
    record(
        pool,
        actor,
        ModAction {
            action: action.to_owned(),
            subjects: subjects.iter().collect(),
            message,
            data: subject_data(subjects),
        },
    )
    .await
}

/// Records a `deactivate`: `@admin deactivated users: @alice, @bob`.
///
/// # Errors
///
/// Any database failure arrives as
/// [`AppError::Database`](crate::error::AppError); see [`record`].
pub async fn deactivate(
    pool: &PgPool,
    actor: &User,
    subjects: &[User],
) -> AppResult<ModerationLog> {
    users_action(pool, actor, subjects, "deactivate", "deactivated").await
}

/// Records an `activate`: `@admin activated users: @alice, @bob`.
///
/// # Errors
///
/// Any database failure arrives as
/// [`AppError::Database`](crate::error::AppError); see [`record`].
pub async fn activate(pool: &PgPool, actor: &User, subjects: &[User]) -> AppResult<ModerationLog> {
    users_action(pool, actor, subjects, "activate", "activated").await
}

/// Records a `delete`: `@admin deleted users: @alice, @bob`.
///
/// # Errors
///
/// Any database failure arrives as
/// [`AppError::Database`](crate::error::AppError); see [`record`].
pub async fn delete(pool: &PgPool, actor: &User, subjects: &[User]) -> AppResult<ModerationLog> {
    users_action(pool, actor, subjects, "delete", "deleted").await
}

/// Records an `approve`: `@admin approved users: @alice, @bob`.
///
/// # Errors
///
/// Any database failure arrives as
/// [`AppError::Database`](crate::error::AppError); see [`record`].
pub async fn approve(pool: &PgPool, actor: &User, subjects: &[User]) -> AppResult<ModerationLog> {
    users_action(pool, actor, subjects, "approve", "approved").await
}

/// Records a `reject`: `@admin rejected users: @alice, @bob`.
///
/// The entry the Mastodon admin API's
/// `POST /api/v1/admin/accounts/{id}/reject` leaves behind (WBE-213),
/// Pleroma's `reject` clause of `get_log_entry_message`. The rejected
/// account's row is gone when this records — rejection DELETES the account —
/// and the entry is the surviving trace of it: the subject columns carry the
/// nickname of a row nothing joins to any more.
///
/// # Errors
///
/// Any database failure arrives as
/// [`AppError::Database`](crate::error::AppError); see [`record`].
pub async fn reject(pool: &PgPool, actor: &User, subjects: &[User]) -> AppResult<ModerationLog> {
    users_action(pool, actor, subjects, "reject", "rejected").await
}

/// Records one account action the endpoint names by its submitted type and
/// no dedicated clause spells out: `@admin performed action {action}`.
///
/// That is Pleroma's `get_log_entry_message` fall-through for an action it
/// has no sentence of its own for, over the same `subject` user list its
/// `insert_log` writes.
///
/// [`crate::api::mastodon_admin`] records every
/// `POST /api/v1/admin/accounts/{id}/action` through here under the type the
/// request stated (`none`, `sensitive`, `silence`, `disable`, `suspend`);
/// the state changes those types make keep the dedicated entry of their own
/// ([`deactivate`], [`report_update`], …), which is why the two families
/// appear side by side in the log for one request.
///
/// # Errors
///
/// Any database failure arrives as
/// [`AppError::Database`](crate::error::AppError); see [`record`].
pub async fn account_action(
    pool: &PgPool,
    actor: &User,
    subject: &User,
    action: &str,
) -> AppResult<ModerationLog> {
    let message = format!("@{} performed action {action}", actor.nickname);
    record(
        pool,
        actor,
        ModAction {
            action: action.to_owned(),
            subjects: vec![subject],
            message,
            data: subject_data(slice::from_ref(subject)),
        },
    )
    .await
}

/// Records a `create`: `@admin created users: @alice, @bob` — the
/// `create` clause of Pleroma's `get_log_entry_message`, the entry the
/// admin `POST /users` leaves behind (WBE-196).
///
/// The one departure from the sentence family of [`deactivate`] and its
/// siblings is where the entry puts the array: Pleroma's `create` clause
/// matches the PLURAL `"subjects"` key (its `insert_log` takes the map's
/// `subjects:` field verbatim into `data`), so this records the same user
/// maps there and the moderation-log read hands that spelling back
/// unchanged.
///
/// # Errors
///
/// Any database failure arrives as
/// [`AppError::Database`](crate::error::AppError); see [`record`].
pub async fn create_users(
    pool: &PgPool,
    actor: &User,
    subjects: &[User],
) -> AppResult<ModerationLog> {
    let message = users_sentence(&actor.nickname, "created", &nicknames_of(subjects));
    record(
        pool,
        actor,
        ModAction {
            action: "create".to_owned(),
            subjects: subjects.iter().collect(),
            message,
            data: subjects_data(subjects),
        },
    )
    .await
}

/// Records a `force_password_reset`: `@admin forced password reset for
/// users: @alice, @bob`.
///
/// # Errors
///
/// Any database failure arrives as
/// [`AppError::Database`](crate::error::AppError); see [`record`].
pub async fn force_password_reset(
    pool: &PgPool,
    actor: &User,
    subjects: &[User],
) -> AppResult<ModerationLog> {
    users_action(
        pool,
        actor,
        subjects,
        "force_password_reset",
        "forced password reset for",
    )
    .await
}

/// Records an `updated_users`: `@admin updated users: @alice, @bob` — the
/// entry the admin credentials write leaves behind (Pleroma's
/// `AdminAPIController.update_user_credentials`).
///
/// # Errors
///
/// Any database failure arrives as
/// [`AppError::Database`](crate::error::AppError); see [`record`].
pub async fn updated_users(
    pool: &PgPool,
    actor: &User,
    subjects: &[User],
) -> AppResult<ModerationLog> {
    users_action(pool, actor, subjects, "updated_users", "updated").await
}

/// Records a `confirm_email`: `@admin confirmed email for users: @alice,
/// @bob` — Pleroma's `confirm_email` clause of `get_log_entry_message`.
///
/// # Errors
///
/// Any database failure arrives as
/// [`AppError::Database`](crate::error::AppError); see [`record`].
pub async fn confirm_email(
    pool: &PgPool,
    actor: &User,
    subjects: &[User],
) -> AppResult<ModerationLog> {
    users_action(
        pool,
        actor,
        subjects,
        "confirm_email",
        "confirmed email for",
    )
    .await
}

/// Records a `resend_confirmation_email`: `@admin re-sent confirmation
/// email for users: @alice, @bob` — Pleroma's clause of the same name.
///
/// # Errors
///
/// Any database failure arrives as
/// [`AppError::Database`](crate::error::AppError); see [`record`].
pub async fn resend_confirmation_email(
    pool: &PgPool,
    actor: &User,
    subjects: &[User],
) -> AppResult<ModerationLog> {
    users_action(
        pool,
        actor,
        subjects,
        "resend_confirmation_email",
        "re-sent confirmation email for",
    )
    .await
}

/// Records a `disable_mfa`: `@admin disabled MFA for users: @alice, @bob`.
///
/// Pleroma's `disable_mfa` endpoint logs nothing at all; the ticket asks
/// every admin write to leave an entry, so this sentence joins the
/// user-list family with the verb the action states, following the shape
/// of the five clauses above.
///
/// # Errors
///
/// Any database failure arrives as
/// [`AppError::Database`](crate::error::AppError); see [`record`].
pub async fn disable_mfa(
    pool: &PgPool,
    actor: &User,
    subjects: &[User],
) -> AppResult<ModerationLog> {
    users_action(pool, actor, subjects, "disable_mfa", "disabled MFA for").await
}

/// Records an `add_suggestion`: `@admin added suggested users: @alice,
/// @bob` — Pleroma's `add_suggestion` clause of `get_log_entry_message`,
/// the entry the admin `PATCH /users/suggest` leaves behind.
///
/// # Errors
///
/// Any database failure arrives as
/// [`AppError::Database`](crate::error::AppError); see [`record`].
pub async fn add_suggestion(
    pool: &PgPool,
    actor: &User,
    subjects: &[User],
) -> AppResult<ModerationLog> {
    users_action(pool, actor, subjects, "add_suggestion", "added suggested").await
}

/// Records a `remove_suggestion`: `@admin removed suggested users: @alice,
/// @bob` — [`add_suggestion`]'s twin, the entry the admin
/// `PATCH /users/unsuggest` leaves behind.
///
/// # Errors
///
/// Any database failure arrives as
/// [`AppError::Database`](crate::error::AppError); see [`record`].
pub async fn remove_suggestion(
    pool: &PgPool,
    actor: &User,
    subjects: &[User],
) -> AppResult<ModerationLog> {
    users_action(
        pool,
        actor,
        subjects,
        "remove_suggestion",
        "removed suggested",
    )
    .await
}

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
async fn follow_action(
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

/// Records a `report_update`: `@admin updated report ##{report_id}` plus
/// ` ( on user @target)` when the report names one, plus ` with 'state'
/// state` — Pleroma's report clause.
///
/// The subject map is its `report_to_map`: `{type: "report", id, state}`.
///
/// # Errors
///
/// Any database failure arrives as
/// [`AppError::Database`](crate::error::AppError); see [`record`].
pub async fn report_update(
    pool: &PgPool,
    actor: &User,
    report_id: Uuid,
    state: &str,
    subject_actor: Option<&User>,
) -> AppResult<ModerationLog> {
    let message = report_update_sentence(
        &actor.nickname,
        report_id,
        state,
        subject_actor.map(|target| target.nickname.as_str()),
    );
    record(
        pool,
        actor,
        ModAction {
            action: "report_update".to_owned(),
            subjects: Vec::new(),
            message,
            data: report_data(report_id, Some(state), subject_actor),
        },
    )
    .await
}

/// Records a `report_assigned`: `@admin assigned report ##{report_id}`
/// plus ` ( on user @target)` when the report names one, plus
/// ` to user {assignee}`.
///
/// That is Pleroma's `report_assigned` clause of `get_log_entry_message`,
/// whose controller writes it from
/// `POST /api/v1/admin/reports/{id}/assign_to_self` with the acting
/// moderator's own nickname as the assignee. The subject map is the one of
/// [`report_update`] without a state, plus the `assigned_account` text
/// Pleroma's `insert_log` carries beside it.
///
/// # Errors
///
/// Any database failure arrives as
/// [`AppError::Database`](crate::error::AppError); see [`record`].
pub async fn report_assigned(
    pool: &PgPool,
    actor: &User,
    report_id: Uuid,
    assignee: &str,
    subject_actor: Option<&User>,
) -> AppResult<ModerationLog> {
    let message = report_assigned_sentence(
        &actor.nickname,
        report_id,
        assignee,
        subject_actor.map(|target| target.nickname.as_str()),
    );
    let mut data = report_data(report_id, None, subject_actor);
    put(
        &mut data,
        "assigned_account",
        Value::String(assignee.to_owned()),
    );
    record(
        pool,
        actor,
        ModAction {
            action: "report_assigned".to_owned(),
            subjects: Vec::new(),
            message,
            data,
        },
    )
    .await
}

/// Records a `report_unassigned`: `@admin unassigned report ##{report_id}`
/// plus ` ( on user @target)` when the report names one, plus
/// ` from a user`.
///
/// That is Pleroma's `report_unassigned` clause, written by
/// `POST /api/v1/admin/reports/{id}/unassign`.
///
/// # Errors
///
/// Any database failure arrives as
/// [`AppError::Database`](crate::error::AppError); see [`record`].
pub async fn report_unassigned(
    pool: &PgPool,
    actor: &User,
    report_id: Uuid,
    subject_actor: Option<&User>,
) -> AppResult<ModerationLog> {
    let message = report_unassigned_sentence(
        &actor.nickname,
        report_id,
        subject_actor.map(|target| target.nickname.as_str()),
    );
    record(
        pool,
        actor,
        ModAction {
            action: "report_unassigned".to_owned(),
            subjects: Vec::new(),
            message,
            data: report_data(report_id, None, subject_actor),
        },
    )
    .await
}

/// Records a `report_note`: `@admin added note 'text' to report
/// ##{report_id}` plus ` on user @target` when the report names one.
///
/// The note's text rides in the entry's `text` key, as Pleroma's
/// `report_note` clause reads it.
///
/// # Errors
///
/// Any database failure arrives as
/// [`AppError::Database`](crate::error::AppError); see [`record`].
pub async fn report_note(
    pool: &PgPool,
    actor: &User,
    report_id: Uuid,
    text: &str,
    subject_actor: Option<&User>,
) -> AppResult<ModerationLog> {
    let message = report_note_sentence(
        &actor.nickname,
        report_id,
        text,
        subject_actor.map(|target| target.nickname.as_str()),
    );
    let mut data = report_data(report_id, None, subject_actor);
    put(&mut data, "text", Value::String(text.to_owned()));
    record(
        pool,
        actor,
        ModAction {
            action: "report_note".to_owned(),
            subjects: Vec::new(),
            message,
            data,
        },
    )
    .await
}

/// Records a `report_note_delete`: `@admin deleted note 'text' from report
/// ##{report_id}` plus ` on user @target` when the report names one —
/// Pleroma's `report_note_delete` clause of `get_log_entry_message`.
///
/// The entry the admin note removal leaves behind. The note is gone when
/// this records, which is exactly why its text is kept: the log line is
/// the surviving trace of the deleted note.
///
/// # Errors
///
/// Any database failure arrives as
/// [`AppError::Database`](crate::error::AppError); see [`record`].
pub async fn report_note_delete(
    pool: &PgPool,
    actor: &User,
    report_id: Uuid,
    text: &str,
    subject_actor: Option<&User>,
) -> AppResult<ModerationLog> {
    let message = report_note_delete_sentence(
        &actor.nickname,
        report_id,
        text,
        subject_actor.map(|target| target.nickname.as_str()),
    );
    let mut data = report_data(report_id, None, subject_actor);
    put(&mut data, "text", Value::String(text.to_owned()));
    record(
        pool,
        actor,
        ModAction {
            action: "report_note_delete".to_owned(),
            subjects: Vec::new(),
            message,
            data,
        },
    )
    .await
}

/// Records a `status_update` of `status_id`, naming only the parts that
/// changed: `@admin updated status ##{id}, set sensitive: 'true',
/// visibility: 'private'`.
///
/// `None` for either part means untouched, and naming neither still reads
/// Pleroma's `set visibility: ''` fall-through; the subject map is its
/// `status_to_map`: `{type: "status", id}`.
///
/// # Errors
///
/// Any database failure arrives as
/// [`AppError::Database`](crate::error::AppError); see [`record`].
pub async fn status_update(
    pool: &PgPool,
    actor: &User,
    status_id: Uuid,
    sensitive: Option<bool>,
    visibility: Option<&str>,
) -> AppResult<ModerationLog> {
    let message = status_update_sentence(&actor.nickname, status_id, sensitive, visibility);
    record(
        pool,
        actor,
        ModAction {
            action: "status_update".to_owned(),
            subjects: Vec::new(),
            message,
            data: status_data(status_id, sensitive, visibility),
        },
    )
    .await
}

/// Records a `status_delete`: `@admin deleted status ##{status_id}`. The
/// deleted status may have no row left to name, so it rides in
/// `subject_id` as text, exactly like Pleroma's clause reads it.
///
/// # Errors
///
/// Any database failure arrives as
/// [`AppError::Database`](crate::error::AppError); see [`record`].
pub async fn status_delete(
    pool: &PgPool,
    actor: &User,
    status_id: Uuid,
) -> AppResult<ModerationLog> {
    let message = format!("@{} deleted status ##{status_id}", actor.nickname);
    let mut data = Map::new();
    put(
        &mut data,
        "subject_id",
        Value::String(status_id.to_string()),
    );
    record(
        pool,
        actor,
        ModAction {
            action: "status_delete".to_owned(),
            subjects: Vec::new(),
            message,
            data,
        },
    )
    .await
}

/// Records a `chat_message_delete`: `@admin deleted chat message ##{message_id}`.
///
/// A withdrawn chat message leaves no live row to name, so its id rides in
/// `subject_id` as text — the same shape [`status_delete`] gives a deleted
/// post, with the chat-message wording Pleroma's `get_log_entry_message`
/// clause spells.
///
/// # Errors
///
/// Any database failure arrives as
/// [`AppError::Database`](crate::error::AppError); see [`record`].
pub async fn chat_message_delete(
    pool: &PgPool,
    actor: &User,
    message_id: Uuid,
) -> AppResult<ModerationLog> {
    let message = format!("@{} deleted chat message ##{message_id}", actor.nickname);
    let mut data = Map::new();
    put(
        &mut data,
        "subject_id",
        Value::String(message_id.to_string()),
    );
    record(
        pool,
        actor,
        ModAction {
            action: "chat_message_delete".to_owned(),
            subjects: Vec::new(),
            message,
            data,
        },
    )
    .await
}

/// Records a `relay_follow`: `@admin followed relay: target`.
///
/// # Errors
///
/// Any database failure arrives as
/// [`AppError::Database`](crate::error::AppError); see [`record`].
pub async fn relay_follow(pool: &PgPool, actor: &User, target: &str) -> AppResult<ModerationLog> {
    relay_action(pool, actor, target, "relay_follow", "followed").await
}

/// Records a `relay_unfollow`: `@admin unfollowed relay: target`.
///
/// # Errors
///
/// Any database failure arrives as
/// [`AppError::Database`](crate::error::AppError); see [`record`].
pub async fn relay_unfollow(pool: &PgPool, actor: &User, target: &str) -> AppResult<ModerationLog> {
    relay_action(pool, actor, target, "relay_unfollow", "unfollowed").await
}

/// The two relay clauses share their sentence and their `target` key;
/// only the action name and the verb differ.
async fn relay_action(
    pool: &PgPool,
    actor: &User,
    target: &str,
    action: &str,
    verb: &str,
) -> AppResult<ModerationLog> {
    let message = format!("@{} {verb} relay: {target}", actor.nickname);
    let mut data = Map::new();
    put(&mut data, "target", Value::String(target.to_owned()));
    record(
        pool,
        actor,
        ModAction {
            action: action.to_owned(),
            subjects: Vec::new(),
            message,
            data,
        },
    )
    .await
}

/// Records a `delete_instance`: `@admin deleted instance: host`. The
/// subject is a domain, not an account, so it rides in `data.target` —
/// the same spot a relay target occupies.
///
/// # Errors
///
/// Any database failure arrives as
/// [`AppError::Database`](crate::error::AppError); see [`record`].
pub async fn delete_instance(pool: &PgPool, actor: &User, host: &str) -> AppResult<ModerationLog> {
    let message = format!("@{} deleted instance: {host}", actor.nickname);
    let mut data = Map::new();
    put(&mut data, "target", Value::String(host.to_owned()));
    record(
        pool,
        actor,
        ModAction {
            action: "delete_instance".to_owned(),
            subjects: Vec::new(),
            message,
            data,
        },
    )
    .await
}

/// Records a `reload_emoji`: `@admin reloaded emoji`. There is no subject
/// — the action resyncs the whole on-disk pack set — so beside the
/// actor/action/message keys [`record`] owns, the data map stays empty.
///
/// # Errors
///
/// Any database failure arrives as
/// [`AppError::Database`](crate::error::AppError); see [`record`].
pub async fn reload_emoji(pool: &PgPool, actor: &User) -> AppResult<ModerationLog> {
    let message = format!("@{} reloaded emoji", actor.nickname);
    record(
        pool,
        actor,
        ModAction {
            action: "reload_emoji".to_owned(),
            subjects: Vec::new(),
            message,
            data: Map::new(),
        },
    )
    .await
}

/// Records a `config_change`: `@admin changed config: :pleroma/:instance,
/// :pleroma/:mrf`.
///
/// One row per `POST /api/v1/pleroma/admin/config` that changed
/// something, its `configs` data array naming every pair and whether it
/// was written or deleted. The subject columns stay empty: the change is
/// the instance's own state, not a user's, so the pairs live in `data`
/// the way a report's or a relay target's does.
///
/// # Errors
///
/// Any database failure arrives as
/// [`AppError::Database`](crate::error::AppError); see [`record`].
pub async fn config_change(
    pool: &PgPool,
    actor: &User,
    changes: &[ConfigChange<'_>],
) -> AppResult<ModerationLog> {
    let names: Vec<String> = changes
        .iter()
        .map(|change| format!("{}/{}", change.group, change.key))
        .collect();
    let message = format!("@{} changed config: {}", actor.nickname, names.join(", "));
    let mut data = Map::new();
    let configs = changes
        .iter()
        .map(config_change_map)
        .collect::<Vec<Value>>();
    put(&mut data, "configs", Value::Array(configs));
    record(
        pool,
        actor,
        ModAction {
            action: "config_change".to_owned(),
            subjects: Vec::new(),
            message,
            data,
        },
    )
    .await
}

/// One `configs` entry of the [`config_change`] data map.
fn config_change_map(change: &ConfigChange<'_>) -> Value {
    let mut entry = Map::new();
    put(&mut entry, "group", Value::String(change.group.to_owned()));
    put(&mut entry, "key", Value::String(change.key.to_owned()));
    put(&mut entry, "delete", Value::Bool(change.delete));
    Value::Object(entry)
}

/// Records an `invite_create`: `@admin created invite: {invite_id}`.
///
/// The admin API's `POST /users/invite_token` logs here. The invite is no
/// user, so — like a deleted status — its id rides in `subject_id` as
/// text and the subject columns stay empty.
///
/// # Errors
///
/// Any database failure arrives as
/// [`AppError::Database`](crate::error::AppError); see [`record`].
pub async fn invite_create(
    pool: &PgPool,
    actor: &User,
    invite_id: Uuid,
) -> AppResult<ModerationLog> {
    invite_action(pool, actor, invite_id, "invite_create", "created").await
}

/// Records an `invite_revoke`: `@admin revoked invite: {invite_id}`.
///
/// The admin API's `POST /users/revoke_invite` logs here. The invite row
/// is gone when this records, which is exactly why the id is kept: the log
/// line is the surviving trace of the revoked invite.
///
/// # Errors
///
/// Any database failure arrives as
/// [`AppError::Database`](crate::error::AppError); see [`record`].
pub async fn invite_revoke(
    pool: &PgPool,
    actor: &User,
    invite_id: Uuid,
) -> AppResult<ModerationLog> {
    invite_action(pool, actor, invite_id, "invite_revoke", "revoked").await
}

/// The two invite clauses share their sentence shape and their
/// `subject_id` key; only the action name and the verb differ.
async fn invite_action(
    pool: &PgPool,
    actor: &User,
    invite_id: Uuid,
    action: &str,
    verb: &str,
) -> AppResult<ModerationLog> {
    let message = format!("@{} {verb} invite: {invite_id}", actor.nickname);
    let mut data = Map::new();
    put(
        &mut data,
        "subject_id",
        Value::String(invite_id.to_string()),
    );
    record(
        pool,
        actor,
        ModAction {
            action: action.to_owned(),
            subjects: Vec::new(),
            message,
            data,
        },
    )
    .await
}

/// Records a `webhook_create`: `@admin created webhook: {webhook_id}`.
///
/// The admin API's `POST /webhooks` logs here. A webhook is no user, so —
/// like an invite — its id rides in `subject_id` as text and the subject
/// columns stay empty.
///
/// # Errors
///
/// Any database failure arrives as
/// [`AppError::Database`](crate::error::AppError); see [`record`].
pub async fn webhook_create(
    pool: &PgPool,
    actor: &User,
    webhook_id: Uuid,
) -> AppResult<ModerationLog> {
    webhook_action(pool, actor, webhook_id, "webhook_create", "created").await
}

/// Records a `webhook_update`: `@admin updated webhook: {webhook_id}`.
///
/// The admin API's `PATCH /webhooks/{id}` logs here.
///
/// # Errors
///
/// Any database failure arrives as
/// [`AppError::Database`](crate::error::AppError); see [`record`].
pub async fn webhook_update(
    pool: &PgPool,
    actor: &User,
    webhook_id: Uuid,
) -> AppResult<ModerationLog> {
    webhook_action(pool, actor, webhook_id, "webhook_update", "updated").await
}

/// Records a `webhook_delete`: `@admin deleted webhook: {webhook_id}`.
///
/// The admin API's `DELETE /webhooks/{id}` logs here. The row is gone when
/// this records, which is exactly why the id is kept: the log line is the
/// surviving trace of the deleted webhook.
///
/// # Errors
///
/// Any database failure arrives as
/// [`AppError::Database`](crate::error::AppError); see [`record`].
pub async fn webhook_delete(
    pool: &PgPool,
    actor: &User,
    webhook_id: Uuid,
) -> AppResult<ModerationLog> {
    webhook_action(pool, actor, webhook_id, "webhook_delete", "deleted").await
}

/// Records a `webhook_rotate_secret`:
/// `@admin rotated secret for webhook: {webhook_id}`.
///
/// The admin API's `POST /webhooks/{id}/rotate_secret` logs here. The new
/// secret itself never reaches the log: the line names the row, not its
/// secret material.
///
/// # Errors
///
/// Any database failure arrives as
/// [`AppError::Database`](crate::error::AppError); see [`record`].
pub async fn webhook_rotate_secret(
    pool: &PgPool,
    actor: &User,
    webhook_id: Uuid,
) -> AppResult<ModerationLog> {
    webhook_action(
        pool,
        actor,
        webhook_id,
        "webhook_rotate_secret",
        "rotated secret for",
    )
    .await
}

/// The four webhook clauses share their sentence shape and their
/// `subject_id` key; only the action name and the verb differ.
async fn webhook_action(
    pool: &PgPool,
    actor: &User,
    webhook_id: Uuid,
    action: &str,
    verb: &str,
) -> AppResult<ModerationLog> {
    let message = format!("@{} {verb} webhook: {webhook_id}", actor.nickname);
    let mut data = Map::new();
    put(
        &mut data,
        "subject_id",
        Value::String(webhook_id.to_string()),
    );
    record(
        pool,
        actor,
        ModAction {
            action: action.to_owned(),
            subjects: Vec::new(),
            message,
            data,
        },
    )
    .await
}

/// Records an `emoji_pack_create`: `@admin created emoji pack: {name}`.
///
/// The pack admin API's `POST /api/v1/pleroma/emoji/pack` logs here. A
/// pack is no user and has no id — its directory name is its identity — so
/// the name rides in `subject_id` as text and the subject columns stay
/// empty, as with the invite and webhook actions.
///
/// # Errors
///
/// Any database failure arrives as
/// [`AppError::Database`](crate::error::AppError); see [`record`].
pub async fn emoji_pack_create(
    pool: &PgPool,
    actor: &User,
    pack: &str,
) -> AppResult<ModerationLog> {
    emoji_pack_action(pool, actor, pack, "emoji_pack_create", "created").await
}

/// Records an `emoji_pack_update`: `@admin updated emoji pack: {name}`.
///
/// The pack admin API's `PATCH /api/v1/pleroma/emoji/pack` logs here.
///
/// # Errors
///
/// Any database failure arrives as
/// [`AppError::Database`](crate::error::AppError); see [`record`].
pub async fn emoji_pack_update(
    pool: &PgPool,
    actor: &User,
    pack: &str,
) -> AppResult<ModerationLog> {
    emoji_pack_action(pool, actor, pack, "emoji_pack_update", "updated").await
}

/// Records an `emoji_pack_delete`: `@admin deleted emoji pack: {name}`.
///
/// The pack admin API's `DELETE /api/v1/pleroma/emoji/pack` logs here. The
/// pack is gone when this records, which is exactly why the name is kept:
/// the log line is the surviving trace of the deleted pack.
///
/// # Errors
///
/// Any database failure arrives as
/// [`AppError::Database`](crate::error::AppError); see [`record`].
pub async fn emoji_pack_delete(
    pool: &PgPool,
    actor: &User,
    pack: &str,
) -> AppResult<ModerationLog> {
    emoji_pack_action(pool, actor, pack, "emoji_pack_delete", "deleted").await
}

/// The three emoji-pack clauses share their sentence shape and their
/// `subject_id` key (holding the pack's name); only the action name and
/// the verb differ.
async fn emoji_pack_action(
    pool: &PgPool,
    actor: &User,
    pack: &str,
    action: &str,
    verb: &str,
) -> AppResult<ModerationLog> {
    let message = format!("@{} {verb} emoji pack: {pack}", actor.nickname);
    let mut data = Map::new();
    put(&mut data, "subject_id", Value::String(pack.to_owned()));
    record(
        pool,
        actor,
        ModAction {
            action: action.to_owned(),
            subjects: Vec::new(),
            message,
            data,
        },
    )
    .await
}

/// Records a `rule_create`: `@admin created rule: {rule_id}`.
///
/// The admin content API's `POST /rules` logs here. A rule is no user, so
/// — like an invite, a webhook or a pack — its id rides in `subject_id` as
/// text and the subject columns stay empty.
///
/// # Errors
///
/// Any database failure arrives as
/// [`AppError::Database`](crate::error::AppError); see [`record`].
pub async fn rule_create(pool: &PgPool, actor: &User, rule_id: Uuid) -> AppResult<ModerationLog> {
    rule_action(pool, actor, rule_id, "rule_create", "created").await
}

/// Records a `rule_update`: `@admin updated rule: {rule_id}`.
///
/// The admin content API's `PATCH /rules/{id}` logs here.
///
/// # Errors
///
/// Any database failure arrives as
/// [`AppError::Database`](crate::error::AppError); see [`record`].
pub async fn rule_update(pool: &PgPool, actor: &User, rule_id: Uuid) -> AppResult<ModerationLog> {
    rule_action(pool, actor, rule_id, "rule_update", "updated").await
}

/// Records a `rule_delete`: `@admin deleted rule: {rule_id}`.
///
/// The admin content API's `DELETE /rules/{id}` logs here. The row is gone
/// when this records, which is exactly why the id is kept: the line is the
/// surviving trace of the deleted rule.
///
/// # Errors
///
/// Any database failure arrives as
/// [`AppError::Database`](crate::error::AppError); see [`record`].
pub async fn rule_delete(pool: &PgPool, actor: &User, rule_id: Uuid) -> AppResult<ModerationLog> {
    rule_action(pool, actor, rule_id, "rule_delete", "deleted").await
}

/// The three rule clauses share their sentence shape and their
/// `subject_id` key; only the action name and the verb differ.
async fn rule_action(
    pool: &PgPool,
    actor: &User,
    rule_id: Uuid,
    action: &str,
    verb: &str,
) -> AppResult<ModerationLog> {
    let message = format!("@{} {verb} rule: {rule_id}", actor.nickname);
    let mut data = Map::new();
    put(&mut data, "subject_id", Value::String(rule_id.to_string()));
    record(
        pool,
        actor,
        ModAction {
            action: action.to_owned(),
            subjects: Vec::new(),
            message,
            data,
        },
    )
    .await
}

/// Records an `announcement_create`:
/// `@admin created announcement: {announcement_id}`.
///
/// The admin content API's `POST /announcements` logs here, beside the
/// stream event the route publishes.
///
/// # Errors
///
/// Any database failure arrives as
/// [`AppError::Database`](crate::error::AppError); see [`record`].
pub async fn announcement_create(
    pool: &PgPool,
    actor: &User,
    announcement_id: Uuid,
) -> AppResult<ModerationLog> {
    announcement_action(
        pool,
        actor,
        announcement_id,
        "announcement_create",
        "created",
    )
    .await
}

/// Records an `announcement_update`:
/// `@admin updated announcement: {announcement_id}`.
///
/// The admin content API's `PATCH /announcements/{id}` logs here.
///
/// # Errors
///
/// Any database failure arrives as
/// [`AppError::Database`](crate::error::AppError); see [`record`].
pub async fn announcement_update(
    pool: &PgPool,
    actor: &User,
    announcement_id: Uuid,
) -> AppResult<ModerationLog> {
    announcement_action(
        pool,
        actor,
        announcement_id,
        "announcement_update",
        "updated",
    )
    .await
}

/// Records an `announcement_delete`:
/// `@admin deleted announcement: {announcement_id}`.
///
/// The admin content API's `DELETE /announcements/{id}` logs here, the line
/// being what survives the notice and its dismissal rows.
///
/// # Errors
///
/// Any database failure arrives as
/// [`AppError::Database`](crate::error::AppError); see [`record`].
pub async fn announcement_delete(
    pool: &PgPool,
    actor: &User,
    announcement_id: Uuid,
) -> AppResult<ModerationLog> {
    announcement_action(
        pool,
        actor,
        announcement_id,
        "announcement_delete",
        "deleted",
    )
    .await
}

/// The three announcement clauses share their sentence shape and their
/// `subject_id` key; only the action name and the verb differ.
async fn announcement_action(
    pool: &PgPool,
    actor: &User,
    announcement_id: Uuid,
    action: &str,
    verb: &str,
) -> AppResult<ModerationLog> {
    let message = format!("@{} {verb} announcement: {announcement_id}", actor.nickname);
    let mut data = Map::new();
    put(
        &mut data,
        "subject_id",
        Value::String(announcement_id.to_string()),
    );
    record(
        pool,
        actor,
        ModAction {
            action: action.to_owned(),
            subjects: Vec::new(),
            message,
            data,
        },
    )
    .await
}

/// The `@alice, @bob` list Pleroma's `users_to_nicknames_string` and
/// `nicknames_to_string` build.
fn mention_list(nicknames: &[String]) -> String {
    nicknames
        .iter()
        .map(|nickname| format!("@{nickname}"))
        .collect::<Vec<String>>()
        .join(", ")
}

/// The nicknames of `users`, in order.
fn nicknames_of(users: &[User]) -> Vec<String> {
    users.iter().map(|user| user.nickname.clone()).collect()
}

/// `"{actor} {verb} users: {mentions}"` — the five user-list sentences.
fn users_sentence(actor: &str, verb: &str, nicknames: &[String]) -> String {
    format!("@{actor} {verb} users: {}", mention_list(nicknames))
}

/// `"{actor} added tags: {tags} to users: {mentions}"`.
fn tag_sentence(actor: &str, tags: &[String], nicknames: &[String]) -> String {
    format!(
        "@{actor} added tags: {} to users: {}",
        tags.join(", "),
        mention_list(nicknames)
    )
}

/// `"{actor} removed tags: {tags} from users: {mentions}"`.
fn untag_sentence(actor: &str, tags: &[String], nicknames: &[String]) -> String {
    format!(
        "@{actor} removed tags: {} from users: {}",
        tags.join(", "),
        mention_list(nicknames)
    )
}

/// `"{actor} made {mentions} {permission}"`.
fn grant_sentence(actor: &str, nicknames: &[String], permission: &str) -> String {
    format!("@{actor} made {} {permission}", mention_list(nicknames))
}

/// `"{actor} revoked {permission} role from {mentions}"`.
fn revoke_sentence(actor: &str, nicknames: &[String], permission: &str) -> String {
    format!(
        "@{actor} revoked {permission} role from {}",
        mention_list(nicknames)
    )
}

/// `"{actor} updated report ##{report_id}"`, the ` ( on user @{subject})`
/// Pleroma's `subject_actor_nickname/3` splices in when the report names
/// its target, and the ` with '{state}' state` close.
fn report_update_sentence(
    actor: &str,
    report_id: Uuid,
    state: &str,
    subject_nickname: Option<&str>,
) -> String {
    format!(
        "@{actor} updated report ##{report_id}{} with '{state}' state",
        subject_nickname.map_or_else(String::new, |target| format!(" ( on user @{target})"))
    )
}

/// `"{actor} assigned report ##{report_id}"`, the ` ( on user @{subject})`
/// Pleroma's `subject_actor_nickname/3` splices in when the report names
/// its target, and the ` to user {assignee}` close — Pleroma's
/// `report_assigned` clause, whose assignee is a bare nickname, not a
/// mention.
fn report_assigned_sentence(
    actor: &str,
    report_id: Uuid,
    assignee: &str,
    subject_nickname: Option<&str>,
) -> String {
    format!(
        "@{actor} assigned report ##{report_id}{} to user {assignee}",
        subject_nickname.map_or_else(String::new, |target| format!(" ( on user @{target})"))
    )
}

/// `"{actor} unassigned report ##{report_id}"` with the same spliced
/// ` ( on user @{subject})` and the ` from a user` close — Pleroma's
/// `report_unassigned` clause.
fn report_unassigned_sentence(
    actor: &str,
    report_id: Uuid,
    subject_nickname: Option<&str>,
) -> String {
    format!(
        "@{actor} unassigned report ##{report_id}{} from a user",
        subject_nickname.map_or_else(String::new, |target| format!(" ( on user @{target})"))
    )
}

/// `"{actor} added note '{text}' to report ##{report_id}"` with Pleroma's
/// ` on user @{subject}` tail (`subject_actor_nickname/2`, no paren).
fn report_note_sentence(
    actor: &str,
    report_id: Uuid,
    text: &str,
    subject_nickname: Option<&str>,
) -> String {
    format!(
        "@{actor} added note '{text}' to report ##{report_id}{}",
        subject_nickname.map_or_else(String::new, |target| format!(" on user @{target}"))
    )
}

/// `"{actor} deleted note '{text}' from report ##{report_id}"` with the
/// same ` on user @{subject}` tail [`report_note_sentence`] gives —
/// Pleroma's `report_note_delete` clause reads the pair with
/// `subject_actor_nickname/2` too.
fn report_note_delete_sentence(
    actor: &str,
    report_id: Uuid,
    text: &str,
    subject_nickname: Option<&str>,
) -> String {
    format!(
        "@{actor} deleted note '{text}' from report ##{report_id}{}",
        subject_nickname.map_or_else(String::new, |target| format!(" on user @{target}"))
    )
}

/// `"{actor} updated status ##{status_id}"` with the changed parts after
/// a comma, in Pleroma's clause order: sensitive, then visibility; when
/// neither is named, Pleroma's sensitive-`nil` clause still renders the
/// visibility of `''`.
fn status_update_sentence(
    actor: &str,
    status_id: Uuid,
    sensitive: Option<bool>,
    visibility: Option<&str>,
) -> String {
    let base = format!("@{actor} updated status ##{status_id}");
    match (sensitive, visibility) {
        (Some(sensitive), Some(visibility)) => {
            format!("{base}, set sensitive: '{sensitive}', visibility: '{visibility}'")
        }
        (Some(sensitive), None) => format!("{base}, set sensitive: '{sensitive}'"),
        (None, Some(visibility)) => format!("{base}, set visibility: '{visibility}'"),
        (None, None) => format!("{base}, set visibility: ''"),
    }
}

/// Pleroma's `user_to_map`: `{id, nickname, type: "user"}`, the id as its
/// hyphenated text (decision 4).
fn user_map(user: &User) -> Map<String, Value> {
    let mut map = Map::new();
    put(&mut map, "id", Value::String(user.id.to_string()));
    put(&mut map, "nickname", Value::String(user.nickname.clone()));
    put(&mut map, "type", Value::String(TYPE_USER.to_owned()));
    map
}

/// The `subject` entry for the user-list actions: an array of
/// [`user_map`]s, Pleroma's `user_to_map` over the list.
fn subject_data(users: &[User]) -> Map<String, Value> {
    subjects_under("subject", users)
}

/// The same array under Pleroma's plural `subjects` key, which only the
/// `create` clause of `get_log_entry_message` matches on.
fn subjects_data(users: &[User]) -> Map<String, Value> {
    subjects_under("subjects", users)
}

/// A user array under `key`: the one shape the singular and plural
/// subject entries share.
fn subjects_under(key: &str, users: &[User]) -> Map<String, Value> {
    let mut data = Map::new();
    let subjects = users
        .iter()
        .map(|user| Value::Object(user_map(user)))
        .collect::<Vec<Value>>();
    put(&mut data, key, Value::Array(subjects));
    data
}

/// The same with the role the grant/revoke sentence names.
fn permission_data(users: &[User], permission: &str) -> Map<String, Value> {
    let mut data = subject_data(users);
    put(
        &mut data,
        "permission",
        Value::String(permission.to_owned()),
    );
    data
}

/// The `nicknames` and `tags` keys Pleroma's tag/untag clauses read.
fn tags_data(tags: &[String], nicknames: &[String]) -> Map<String, Value> {
    let mut data = Map::new();
    put(
        &mut data,
        "nicknames",
        Value::Array(
            nicknames
                .iter()
                .cloned()
                .map(Value::String)
                .collect::<Vec<Value>>(),
        ),
    );
    put(
        &mut data,
        "tags",
        Value::Array(tags.iter().cloned().map(Value::String).collect()),
    );
    data
}

/// The `subject` map of a report action (Pleroma's `report_to_map`,
/// state-included for [`report_update`]) plus the optional
/// `subject_actor` user map the sentence reads its ` on user @…` tail
/// from.
fn report_data(
    report_id: Uuid,
    state: Option<&str>,
    subject_actor: Option<&User>,
) -> Map<String, Value> {
    let mut subject = Map::new();
    put(&mut subject, "type", Value::String("report".to_owned()));
    put(&mut subject, "id", Value::String(report_id.to_string()));
    if let Some(state) = state {
        put(&mut subject, "state", Value::String(state.to_owned()));
    }
    let mut data = Map::new();
    put(&mut data, "subject", Value::Object(subject));
    if let Some(actor) = subject_actor {
        put(&mut data, "subject_actor", Value::Object(user_map(actor)));
    }
    data
}

/// The `subject` map of a status action plus both changed parts: Pleroma's
/// `status_update` data always carries `sensitive` and `visibility`, the
/// untouched one as `null`.
fn status_data(
    status_id: Uuid,
    sensitive: Option<bool>,
    visibility: Option<&str>,
) -> Map<String, Value> {
    let mut subject = Map::new();
    put(&mut subject, "type", Value::String("status".to_owned()));
    put(&mut subject, "id", Value::String(status_id.to_string()));
    let mut data = Map::new();
    put(&mut data, "subject", Value::Object(subject));
    put(
        &mut data,
        "sensitive",
        sensitive.map_or(Value::Null, Value::Bool),
    );
    put(
        &mut data,
        "visibility",
        visibility.map_or(Value::Null, |visibility| {
            Value::String(visibility.to_owned())
        }),
    );
    data
}

#[cfg(test)]
mod tests {
    use chrono::Duration;
    use std::iter;

    use super::*;
    use crate::{
        config::TEST_BASE_URL,
        models::user::{NewUser, insert, local_ap_id},
    };

    /// The nickname list every sentence test reads.
    fn names(texts: &[&str]) -> Vec<String> {
        texts.iter().map(|text| (*text).to_owned()).collect()
    }

    /// A one-user subject slice, for the helpers a test calls on a lone
    /// account.
    fn one(user: &User) -> &[User] {
        slice::from_ref(user)
    }

    /// The fixed report id the report and status sentence tests read.
    fn report_id() -> Uuid {
        Uuid::parse_str("00000000-0000-0000-0000-000000000042").expect("test uuid")
    }

    #[test]
    fn user_list_sentences_read_like_pleroma() {
        let subjects = names(&["alice", "bob"]);
        assert_eq!(
            users_sentence("admin", "deactivated", &subjects),
            "@admin deactivated users: @alice, @bob",
            "the deactivate clause"
        );
        assert_eq!(
            users_sentence("admin", "activated", &subjects),
            "@admin activated users: @alice, @bob",
            "the activate clause"
        );
        assert_eq!(
            users_sentence("admin", "deleted", &subjects),
            "@admin deleted users: @alice, @bob",
            "the delete clause"
        );
        assert_eq!(
            users_sentence("admin", "approved", &subjects),
            "@admin approved users: @alice, @bob",
            "the approve clause"
        );
        assert_eq!(
            users_sentence("admin", "rejected", &subjects),
            "@admin rejected users: @alice, @bob",
            "the reject clause"
        );
        assert_eq!(
            users_sentence("admin", "created", &subjects),
            "@admin created users: @alice, @bob",
            "the create clause"
        );
        assert_eq!(
            users_sentence("admin", "forced password reset for", &subjects),
            "@admin forced password reset for users: @alice, @bob",
            "the force_password_reset clause"
        );
        assert_eq!(
            users_sentence("admin", "updated", &subjects),
            "@admin updated users: @alice, @bob",
            "the updated_users clause"
        );
        assert_eq!(
            users_sentence("admin", "confirmed email for", &subjects),
            "@admin confirmed email for users: @alice, @bob",
            "the confirm_email clause"
        );
        assert_eq!(
            users_sentence("admin", "re-sent confirmation email for", &subjects),
            "@admin re-sent confirmation email for users: @alice, @bob",
            "the resend_confirmation_email clause"
        );
        assert_eq!(
            users_sentence("admin", "disabled MFA for", &subjects),
            "@admin disabled MFA for users: @alice, @bob",
            "the disable_mfa clause the ticket adds"
        );
        assert_eq!(
            users_sentence("admin", "added suggested", &subjects),
            "@admin added suggested users: @alice, @bob",
            "the add_suggestion clause"
        );
        assert_eq!(
            users_sentence("admin", "removed suggested", &subjects),
            "@admin removed suggested users: @alice, @bob",
            "the remove_suggestion clause"
        );
        assert_eq!(
            users_sentence("admin", "deactivated", &names(&["solo"])),
            "@admin deactivated users: @solo",
            "a lone subject carries no comma"
        );
    }

    #[test]
    fn tag_and_role_sentences_read_like_pleroma() {
        let subjects = names(&["alice", "bob"]);
        let tags = names(&["staff", "vip"]);
        assert_eq!(
            tag_sentence("admin", &tags, &subjects),
            "@admin added tags: staff, vip to users: @alice, @bob",
            "the tag clause"
        );
        assert_eq!(
            untag_sentence("admin", &tags, &subjects),
            "@admin removed tags: staff, vip from users: @alice, @bob",
            "the untag clause"
        );
        assert_eq!(
            grant_sentence("admin", &subjects, "moderator"),
            "@admin made @alice, @bob moderator",
            "the grant clause"
        );
        assert_eq!(
            revoke_sentence("admin", &subjects, "moderator"),
            "@admin revoked moderator role from @alice, @bob",
            "the revoke clause"
        );
    }

    #[test]
    fn report_and_status_sentences_read_like_pleroma() {
        let id = report_id();
        assert_eq!(
            report_update_sentence("admin", id, "closed", None),
            format!("@admin updated report ##{id} with 'closed' state"),
            "the report_update clause without a subject actor"
        );
        assert_eq!(
            report_update_sentence("admin", id, "resolved", Some("target")),
            format!("@admin updated report ##{id} ( on user @target) with 'resolved' state"),
            "the report_update clause with the parenthesised target"
        );
        assert_eq!(
            report_assigned_sentence("admin", id, "moddy", None),
            format!("@admin assigned report ##{id} to user moddy"),
            "the report_assigned clause without a subject actor"
        );
        assert_eq!(
            report_assigned_sentence("admin", id, "moddy", Some("target")),
            format!("@admin assigned report ##{id} ( on user @target) to user moddy"),
            "the report_assigned clause with the parenthesised target"
        );
        assert_eq!(
            report_unassigned_sentence("admin", id, None),
            format!("@admin unassigned report ##{id} from a user"),
            "the report_unassigned clause without a subject actor"
        );
        assert_eq!(
            report_unassigned_sentence("admin", id, Some("target")),
            format!("@admin unassigned report ##{id} ( on user @target) from a user"),
            "the report_unassigned clause with the parenthesised target"
        );
        assert_eq!(
            report_note_sentence("admin", id, "noted", None),
            format!("@admin added note 'noted' to report ##{id}"),
            "the report_note clause without a subject actor"
        );
        assert_eq!(
            report_note_sentence("admin", id, "noted", Some("target")),
            format!("@admin added note 'noted' to report ##{id} on user @target"),
            "the report_note clause with the bare tail"
        );
        assert_eq!(
            report_note_delete_sentence("admin", id, "noted", None),
            format!("@admin deleted note 'noted' from report ##{id}"),
            "the report_note_delete clause without a subject actor"
        );
        assert_eq!(
            report_note_delete_sentence("admin", id, "noted", Some("target")),
            format!("@admin deleted note 'noted' from report ##{id} on user @target"),
            "the report_note_delete clause with the bare tail"
        );
        assert_eq!(
            status_update_sentence("admin", id, Some(true), Some("private")),
            format!("@admin updated status ##{id}, set sensitive: 'true', visibility: 'private'"),
            "the status_update clause naming both parts"
        );
        assert_eq!(
            status_update_sentence("admin", id, Some(false), None),
            format!("@admin updated status ##{id}, set sensitive: 'false'"),
            "the sensitive-only arm"
        );
        assert_eq!(
            status_update_sentence("admin", id, None, Some("public")),
            format!("@admin updated status ##{id}, set visibility: 'public'"),
            "the visibility-only arm"
        );
        assert_eq!(
            status_update_sentence("admin", id, None, None),
            format!("@admin updated status ##{id}, set visibility: ''"),
            "Pleroma's nil-sensitive clause renders the empty visibility"
        );
    }

    /// An inserted local user named `nickname`.
    async fn stored(pool: &PgPool, nickname: &str) -> User {
        insert(
            pool,
            &NewUser::local(nickname, &local_ap_id(TEST_BASE_URL, nickname)),
        )
        .await
        .expect("the user fixture inserts")
    }

    #[tokio::test]
    async fn record_stores_actor_subjects_and_merged_data() {
        let db = crate::test_db::test_db().await.expect("test database slot");
        let pool = db.pool().clone();
        let actor = stored(&pool, "mod").await;
        let alice = stored(&pool, "alice").await;
        let row = record(
            &pool,
            &actor,
            ModAction {
                action: "test_action".to_owned(),
                subjects: vec![&alice],
                message: "a sentence".to_owned(),
                data: subject_data(one(&alice)),
            },
        )
        .await
        .expect("the entry records");
        assert_eq!(row.action, "test_action", "the action column");
        assert_eq!(row.message, "a sentence", "the message column");
        assert_eq!(row.actor_id, Some(actor.id), "the actor column");
        assert_eq!(
            row.actor_nickname, actor.nickname,
            "and its recorded nickname"
        );
        assert_eq!(row.subject_ids, vec![alice.id], "the subject ids column");
        assert_eq!(
            row.subject_nicknames,
            vec!["alice".to_owned()],
            "and the subject nicknames column"
        );
        assert!(
            row.inserted_at > Utc::now() - Duration::minutes(1),
            "stamped by the column default"
        );
        assert_eq!(
            row.data
                .get("actor")
                .and_then(Value::as_object)
                .and_then(|entry| entry.get("nickname"))
                .and_then(Value::as_str),
            Some("mod"),
            "the data map carries Pleroma's actor map"
        );
        assert_eq!(
            row.data.get("message").and_then(Value::as_str),
            Some("a sentence"),
            "and the keys record injects"
        );
        assert_eq!(
            row.data
                .get("subject")
                .and_then(Value::as_array)
                .map(Vec::len),
            Some(1),
            "beside the caller's own extras"
        );
    }

    /// The user-list helpers store their action, sentence and subject
    /// columns.
    #[tokio::test]
    async fn user_list_helpers_record_their_sentences() {
        let db = crate::test_db::test_db().await.expect("test database slot");
        let pool = db.pool().clone();
        let actor = stored(&pool, "mod").await;
        let alice = stored(&pool, "alice").await;
        let bob = stored(&pool, "bob").await;
        let (alice_id, bob_id) = (alice.id, bob.id);
        let subjects = [alice, bob];
        let cases = [
            deactivate(&pool, &actor, &subjects).await.expect("records"),
            activate(&pool, &actor, &subjects).await.expect("records"),
            delete(&pool, &actor, &subjects).await.expect("records"),
            approve(&pool, &actor, &subjects).await.expect("records"),
            reject(&pool, &actor, &subjects).await.expect("records"),
            create_users(&pool, &actor, &subjects)
                .await
                .expect("records"),
            force_password_reset(&pool, &actor, &subjects)
                .await
                .expect("records"),
            updated_users(&pool, &actor, &subjects)
                .await
                .expect("records"),
            confirm_email(&pool, &actor, &subjects)
                .await
                .expect("records"),
            resend_confirmation_email(&pool, &actor, &subjects)
                .await
                .expect("records"),
            disable_mfa(&pool, &actor, &subjects)
                .await
                .expect("records"),
            add_suggestion(&pool, &actor, &subjects)
                .await
                .expect("records"),
            remove_suggestion(&pool, &actor, &subjects)
                .await
                .expect("records"),
        ];
        let expectations = [
            ("deactivate", "subject"),
            ("activate", "subject"),
            ("delete", "subject"),
            ("approve", "subject"),
            ("reject", "subject"),
            ("create", "subjects"),
            ("force_password_reset", "subject"),
            ("updated_users", "subject"),
            ("confirm_email", "subject"),
            ("resend_confirmation_email", "subject"),
            ("disable_mfa", "subject"),
            ("add_suggestion", "subject"),
            ("remove_suggestion", "subject"),
        ];
        for (row, (action, subject_key)) in iter::zip(cases, expectations) {
            assert_eq!(row.action, action, "the action column");
            assert_eq!(row.subject_ids, vec![alice_id, bob_id], "{action} subjects");
            assert_eq!(
                row.subject_nicknames,
                names(&["alice", "bob"]),
                "{action} nicknames"
            );
            assert_eq!(
                row.data
                    .get(subject_key)
                    .and_then(Value::as_array)
                    .map(Vec::len),
                Some(2),
                "{action} carries its user array under {subject_key}"
            );
            assert!(
                row.message.starts_with("@mod ") && row.message.ends_with(" users: @alice, @bob"),
                "{action} reads like Pleroma: {}",
                row.message
            );
        }
    }

    /// The account-action helper stores Pleroma's fall-through sentence —
    /// `@mod performed action {type}` — with the one account in the subject
    /// columns and the same user array under `subject` the user-list
    /// actions write.
    #[tokio::test]
    async fn account_action_entries_name_their_type() {
        let db = crate::test_db::test_db().await.expect("test database slot");
        let pool = db.pool().clone();
        let actor = stored(&pool, "mod").await;
        let alice = stored(&pool, "alice").await;
        let alice_id = alice.id;
        for kind in ["none", "sensitive", "silence", "disable", "suspend"] {
            let row = account_action(&pool, &actor, &alice, kind)
                .await
                .expect("records");
            assert_eq!(row.action, kind, "the entry is named by the type");
            assert_eq!(row.subject_ids, vec![alice_id], "{kind} names its subject");
            assert_eq!(
                row.data["subject"].as_array().map_or(0, Vec::len),
                1,
                "{kind} carries the one user map"
            );
            assert_eq!(
                row.message,
                format!("@mod performed action {kind}"),
                "Pleroma's fall-through sentence"
            );
        }
    }

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

    /// `report_update` and `report_note` store their sentences, subject
    /// maps, and optional target user; `report_assigned` adds its
    /// `assigned_account` key.
    #[tokio::test]
    async fn report_helpers_record_their_entries() {
        let db = crate::test_db::test_db().await.expect("test database slot");
        let pool = db.pool().clone();
        let actor = stored(&pool, "mod").await;
        let target = stored(&pool, "target").await;
        let report = new_id();
        let updated = report_update(&pool, &actor, report, "closed", Some(&target))
            .await
            .expect("report_update records");
        assert_eq!(
            updated.message,
            format!("@mod updated report ##{report} ( on user @target) with 'closed' state"),
            "the report_update entry"
        );
        assert_eq!(
            updated
                .data
                .get("subject")
                .and_then(Value::as_object)
                .and_then(|inner| inner.get("state"))
                .and_then(Value::as_str),
            Some("closed"),
            "the report subject map carries its state"
        );
        assert_eq!(
            updated
                .data
                .get("subject_actor")
                .and_then(Value::as_object)
                .and_then(|who| who.get("nickname"))
                .and_then(Value::as_str),
            Some("target"),
            "and the target user map rides beside it"
        );
        let assigned = report_assigned(&pool, &actor, report, "mod", Some(&target))
            .await
            .expect("report_assigned records");
        assert_eq!(
            assigned.message,
            format!("@mod assigned report ##{report} ( on user @target) to user mod"),
            "the report_assigned entry"
        );
        assert_eq!(
            assigned
                .data
                .get("assigned_account")
                .and_then(Value::as_str),
            Some("mod"),
            "the new owner rides in the data map, Pleroma's extra key"
        );
        assert_eq!(
            assigned.action, "report_assigned",
            "and the action column names the clause"
        );
        let noted = report_note(&pool, &actor, report, "looked", None)
            .await
            .expect("report_note records");
        assert_eq!(
            noted.message,
            format!("@mod added note 'looked' to report ##{report}"),
            "the report_note entry"
        );
        assert!(
            noted.data.get("subject_actor").is_none(),
            "no target means no subject_actor key"
        );
        assert_eq!(
            noted.data.get("text").and_then(Value::as_str),
            Some("looked"),
            "the note rides in the data map"
        );
    }

    /// `report_unassigned` stores its sentence with the named target and
    /// without one, and the action column carries the clause name.
    #[tokio::test]
    async fn report_unassigned_records_its_entry() {
        let db = crate::test_db::test_db().await.expect("test database slot");
        let pool = db.pool().clone();
        let actor = stored(&pool, "mod").await;
        let target = stored(&pool, "target").await;
        let report = new_id();
        let unassigned = report_unassigned(&pool, &actor, report, Some(&target))
            .await
            .expect("report_unassigned records");
        assert_eq!(
            unassigned.action, "report_unassigned",
            "the action names the clause"
        );
        assert_eq!(
            unassigned.message,
            format!("@mod unassigned report ##{report} ( on user @target) from a user"),
            "the target rides in the sentence"
        );
        let bare = report_unassigned(&pool, &actor, report, None)
            .await
            .expect("the unnamed target records too");
        assert_eq!(
            bare.message,
            format!("@mod unassigned report ##{report} from a user"),
            "no target is the bare sentence"
        );
        assert!(
            bare.data.get("subject_actor").is_none(),
            "no target means no subject_actor key"
        );
    }

    /// `status_update` and `status_delete` store their sentences and
    /// subject maps, and the relay pair stores its target.
    #[tokio::test]
    async fn status_and_relay_helpers_record_their_entries() {
        let db = crate::test_db::test_db().await.expect("test database slot");
        let pool = db.pool().clone();
        let actor = stored(&pool, "mod").await;
        let post = new_id();
        let changed = status_update(&pool, &actor, post, Some(true), Some("private"))
            .await
            .expect("status_update records");
        assert_eq!(
            changed.message,
            format!("@mod updated status ##{post}, set sensitive: 'true', visibility: 'private'"),
            "the status_update entry"
        );
        assert_eq!(
            changed
                .data
                .get("subject")
                .and_then(Value::as_object)
                .and_then(|inner| inner.get("type"))
                .and_then(Value::as_str),
            Some("status"),
            "the status subject map says what it is"
        );
        let deleted = status_delete(&pool, &actor, post)
            .await
            .expect("status_delete records");
        assert_eq!(
            deleted.message,
            format!("@mod deleted status ##{post}"),
            "the status_delete entry"
        );
        assert_eq!(
            deleted.data.get("subject_id").and_then(Value::as_str),
            Some(post.to_string().as_str()),
            "the deleted status rides in subject_id"
        );
        let followed = relay_follow(&pool, &actor, "https://relay.example")
            .await
            .expect("relay_follow records");
        assert_eq!(
            followed.message, "@mod followed relay: https://relay.example",
            "the relay_follow entry"
        );
        let unfollowed = relay_unfollow(&pool, &actor, "https://relay.example")
            .await
            .expect("relay_unfollow records");
        assert_eq!(
            unfollowed.message, "@mod unfollowed relay: https://relay.example",
            "the relay_unfollow entry"
        );
        assert_eq!(
            unfollowed.data.get("target").and_then(Value::as_str),
            Some("https://relay.example"),
            "the relay target rides in the data map"
        );
    }

    /// `chat_message_delete` stores Pleroma's chat-message sentence with the
    /// withdrawn message's id in `subject_id` — the shape the admin chat
    /// endpoint logs on every delete.
    #[tokio::test]
    async fn the_chat_message_delete_entry_states_its_subject() {
        let db = crate::test_db::test_db().await.expect("test database slot");
        let pool = db.pool().clone();
        let actor = stored(&pool, "mod").await;
        let message = new_id();

        let deleted = chat_message_delete(&pool, &actor, message)
            .await
            .expect("chat_message_delete records");

        assert_eq!(deleted.action, "chat_message_delete", "the action column");
        assert_eq!(
            deleted.message,
            format!("@mod deleted chat message ##{message}"),
            "Pleroma's chat_message_delete sentence"
        );
        assert_eq!(
            deleted.data.get("subject_id").and_then(Value::as_str),
            Some(message.to_string().as_str()),
            "the withdrawn message rides in subject_id"
        );
        assert!(
            deleted.subject_ids.is_empty(),
            "a chat message is no user subject: the columns stay empty"
        );
    }

    /// The federation admin pair: `delete_instance` states the domain in
    /// the sentence and carries it as `target`; `reload_emoji` names its
    /// action and nothing else — the whole pack set is its subject.
    #[tokio::test]
    async fn the_federation_entries_state_their_subjects() {
        let db = crate::test_db::test_db().await.expect("test database slot");
        let pool = db.pool().clone();
        let actor = stored(&pool, "mod").await;

        let deleted = delete_instance(&pool, &actor, "peer.example")
            .await
            .expect("delete_instance records");
        assert_eq!(
            deleted.message, "@mod deleted instance: peer.example",
            "the delete_instance sentence"
        );
        assert_eq!(
            deleted.data.get("action").and_then(Value::as_str),
            Some("delete_instance"),
            "the action names the entry"
        );
        assert_eq!(
            deleted.data.get("target").and_then(Value::as_str),
            Some("peer.example"),
            "the domain rides in the data map"
        );
        assert!(
            deleted.subject_ids.is_empty(),
            "a domain is no account subject"
        );

        let reloaded = reload_emoji(&pool, &actor)
            .await
            .expect("reload_emoji records");
        assert_eq!(
            reloaded.message, "@mod reloaded emoji",
            "the reload_emoji sentence"
        );
        assert_eq!(
            reloaded.data.get("action").and_then(Value::as_str),
            Some("reload_emoji"),
            "and its action"
        );
        assert!(
            reloaded.data.get("target").is_none(),
            "there is no subject to name: {}",
            reloaded.data
        );
    }

    /// The `config_change` entry: the sentence names every changed pair
    /// in order, the `configs` data array spells each as
    /// `{group, key, delete}`, and the subject columns stay empty — the
    /// instance itself is what changed.
    #[tokio::test]
    async fn config_change_lists_the_changed_pairs() {
        let db = crate::test_db::test_db().await.expect("test database slot");
        let pool = db.pool().clone();
        let actor = stored(&pool, "mod").await;
        let entry = config_change(
            &pool,
            &actor,
            &[
                ConfigChange {
                    group: ":pleroma",
                    key: ":instance",
                    delete: false,
                },
                ConfigChange {
                    group: ":pleroma",
                    key: ":mrf",
                    delete: true,
                },
            ],
        )
        .await
        .expect("config_change records");
        assert_eq!(
            entry.message, "@mod changed config: :pleroma/:instance, :pleroma/:mrf",
            "the sentence names the pairs, in request order"
        );
        assert_eq!(entry.action, "config_change", "the action column");
        assert!(
            entry.subject_ids.is_empty() && entry.subject_nicknames.is_empty(),
            "no user is the subject of a config change"
        );
        let configs = entry
            .data
            .get("configs")
            .and_then(Value::as_array)
            .expect("the data names its configs");
        assert_eq!(
            configs,
            &vec![
                serde_json::json!({"group": ":pleroma", "key": ":instance", "delete": false}),
                serde_json::json!({"group": ":pleroma", "key": ":mrf", "delete": true}),
            ],
            "each pair with its write-or-delete flag"
        );
    }

    /// The `invite_create` and `invite_revoke` entries: their sentences
    /// name the invite id, the id rides in `subject_id` as text (the
    /// invite is no user, and a revoked one has no row left), and the
    /// subject columns stay empty.
    #[tokio::test]
    async fn invite_helpers_record_their_entries() {
        let db = crate::test_db::test_db().await.expect("test database slot");
        let pool = db.pool().clone();
        let actor = stored(&pool, "mod").await;
        let invite = new_id();
        let created = invite_create(&pool, &actor, invite)
            .await
            .expect("invite_create records");
        assert_eq!(created.action, "invite_create", "the action column");
        assert_eq!(
            created.message,
            format!("@mod created invite: {invite}"),
            "the sentence names the invite id"
        );
        assert_eq!(
            created.data.get("subject_id").and_then(Value::as_str),
            Some(invite.to_string().as_str()),
            "the invite rides in subject_id"
        );
        assert!(
            created.subject_ids.is_empty() && created.subject_nicknames.is_empty(),
            "an invite is not a user subject"
        );
        let revoked = invite_revoke(&pool, &actor, invite)
            .await
            .expect("invite_revoke records");
        assert_eq!(revoked.action, "invite_revoke", "the action column");
        assert_eq!(
            revoked.message,
            format!("@mod revoked invite: {invite}"),
            "the revoke sentence, same shape"
        );
        assert_eq!(
            revoked.data.get("subject_id").and_then(Value::as_str),
            Some(invite.to_string().as_str()),
            "and the revoked invite keeps its id in the data map"
        );
    }

    /// Moves entry `id`'s `inserted_at` `days` into the past, so a test can
    /// order the log without sleeping on the clock.
    async fn age(pool: &PgPool, id: Uuid, days: i32) {
        let _moved = sqlx::query(
            "UPDATE moderation_log SET inserted_at = now() - make_interval(days => $2) WHERE id = $1",
        )
        .bind(id)
        .bind(days)
        .execute(pool)
        .await
        .expect("the backdate runs");
    }

    /// The params of a full first page, unfiltered.
    fn open_page() -> LogListParams {
        LogListParams {
            page: 1,
            page_size: 200,
            user_id: None,
            search: None,
            start_date: None,
            end_date: None,
        }
    }

    /// The model listing: newest-first order, the page window, the total,
    /// and every filter — actor, message search and time window.
    #[tokio::test]
    async fn list_pages_filters_and_counts_newest_first() {
        let db = crate::test_db::test_db().await.expect("test database slot");
        let pool = db.pool().clone();
        let modone = stored(&pool, "modone").await;
        let modtwo = stored(&pool, "modtwo").await;
        let alice = stored(&pool, "alice").await;
        let old = deactivate(&pool, &modone, one(&alice))
            .await
            .expect("records");
        let mid = activate(&pool, &modtwo, one(&alice))
            .await
            .expect("records");
        let fresh = delete(&pool, &modone, one(&alice)).await.expect("records");
        age(&pool, old.id, 10).await;
        age(&pool, mid.id, 5).await;

        let all = list(&pool, &open_page()).await.expect("lists");
        assert_eq!(all.total, 3, "everything counts");
        assert_eq!(
            all.items
                .iter()
                .map(|entry| entry.id)
                .collect::<Vec<Uuid>>(),
            vec![fresh.id, mid.id, old.id],
            "newest first"
        );

        let by_actor = LogListParams {
            user_id: Some(modtwo.id),
            ..open_page()
        };
        let one = list(&pool, &by_actor).await.expect("filters");
        assert_eq!(one.total, 1, "one actor's page");
        assert_eq!(one.items[0].id, mid.id, "and it is that actor's entry");

        let by_search = LogListParams {
            search: Some("DEACTIV".to_owned()),
            ..open_page()
        };
        let hit = list(&pool, &by_search).await.expect("searches");
        assert_eq!(hit.total, 1, "the search is a case-insensitive substring");
        assert_eq!(hit.items[0].id, old.id, "and finds the deactivated entry");

        let window = LogListParams {
            start_date: Some(Utc::now() - Duration::days(7)),
            end_date: Some(Utc::now() - Duration::days(3)),
            ..open_page()
        };
        let in_window = list(&pool, &window).await.expect("windows");
        assert_eq!(
            in_window.total, 1,
            "only the five-day-old entry stands in the window"
        );
        assert_eq!(in_window.items[0].id, mid.id, "and it is named");

        let paged = LogListParams {
            page: 2,
            page_size: 2,
            ..open_page()
        };
        let second = list(&pool, &paged).await.expect("pages");
        assert_eq!(second.total, 3, "the total ignores the window");
        assert_eq!(
            second
                .items
                .iter()
                .map(|entry| entry.id)
                .collect::<Vec<Uuid>>(),
            vec![old.id],
            "the second page holds the oldest entry"
        );
    }

    #[test]
    fn data_maps_keep_pleromas_shapes() {
        let subject = subject_data(&[]);
        assert_eq!(
            subject.get("subject"),
            Some(&Value::Array(Vec::new())),
            "the user-list actions carry an array of user maps"
        );
        let status = status_data(report_id(), None, Some("public"));
        assert_eq!(
            status.get("sensitive"),
            Some(&Value::Null),
            "the untouched part rides as null, as Pleroma merges it"
        );
        assert_eq!(
            status
                .get("subject")
                .and_then(Value::as_object)
                .and_then(|inner| inner.get("type"))
                .and_then(Value::as_str),
            Some("status"),
            "the subject map says what it is"
        );
    }
}
