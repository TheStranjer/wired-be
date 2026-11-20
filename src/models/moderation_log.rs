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

use chrono::{DateTime, Utc};
use serde_json::{Map, Value};
use sqlx::{PgPool, Postgres, QueryBuilder};
use uuid::Uuid;

use crate::{error::AppResult, models::user::User, util::id::new_id};
use data::user_map;

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

/// The invite, webhook and emoji-pack entries.
mod admin_objects;
/// The configuration-change entry.
mod config;
/// The content and federation entries.
mod content;
/// The rule and announcement entries.
mod content_objects;
/// The `data` map fragments.
mod data;
/// The report entries.
mod reports;
/// The stored sentences.
mod sentences;
/// Fixtures more than one child's tests build on.
#[cfg(test)]
mod test_support;
/// The user-list actions.
mod users;
/// The follow, tag and permission-group entries.
mod users_follow_tags;

pub use admin_objects::{
    emoji_pack_create, emoji_pack_delete, emoji_pack_update, invite_create, invite_revoke,
    webhook_create, webhook_delete, webhook_rotate_secret, webhook_update,
};
pub use config::{ConfigChange, config_change};
pub use content::{
    chat_message_delete, create_backup, delete_instance, relay_follow, relay_unfollow,
    reload_emoji, status_delete, status_update,
};
pub use content_objects::{
    announcement_create, announcement_delete, announcement_update, rule_create, rule_delete,
    rule_update,
};
pub use reports::{
    report_assigned, report_note, report_note_delete, report_unassigned, report_update,
};
pub use users::{
    account_action, activate, add_suggestion, approve, confirm_email, create_users, deactivate,
    delete, disable_mfa, force_password_reset, reject, remove_suggestion,
    resend_confirmation_email, updated_users,
};
pub use users_follow_tags::{follow, grant, revoke, tag, unfollow, untag};

#[cfg(test)]
mod tests {
    use chrono::Duration;

    use super::*;
    use crate::models::moderation_log::test_support::{one, stored};

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
}
