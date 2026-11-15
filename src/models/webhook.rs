//! The `webhooks` table: the HTTP endpoints event deliveries are `POST`ed
//! to, maintained through the admin API (see
//! [`crate::api::admin::webhooks`]).
//!
//! Reference: Pleroma's `Pleroma.Webhook` (`lib/pleroma/webhook.ex`).
//! A webhook is a `url` subscribed to some of the [`KNOWN_EVENTS`]
//! (`report.created`, `report.updated`, `account.created`), gated by
//! `enabled`, and carrying a signing `secret`: this table's secret is 32
//! random bytes as lowercase hex ([`random_hex`], where Pleroma mints 20;
//! the ticket fixes the width). `internal` marks webhooks the instance
//! itself installed: the admin API reads them but refuses to change them,
//! and never takes the flag from a request body (Pleroma's
//! `webhook_params` drops the key likewise).
//!
//! [`create`] mints the id (decision 4) and the secret; [`update`],
//! [`set_enabled`] and [`rotate_secret`] raise `updated_at` and change
//! only what their parameters carry — [`WebhookChanges`]'s `None` fields
//! stay as stored, the PATCH's present-only rule. [`list`] answers a
//! `LIMIT`/`OFFSET` window in creation order on the id (decision 4),
//! [`enabled_for_event`] is the fan-out read the delivery service queues
//! through, and [`get`] and [`delete`] work one row by id. `secret` is
//! secret material:
//! the manual [`Debug`](Webhook) impl below redacts it.

use std::fmt;

use chrono::{DateTime, Utc};
use sqlx::PgPool;
use uuid::Uuid;

use crate::{
    error::AppResult,
    util::{id::new_id, token::random_hex},
};

/// [`create`] / [`rotate_secret`]: bytes behind a signing secret, hex
/// encoded to 64 lowercase characters (the ticket's "random 32 bytes hex").
const SECRET_BYTES: usize = 32;

/// [`create`]: the id and secret this module mints plus the three columns
/// a request carries; `internal` and the two stamps ride on the column
/// defaults. `RETURNING *` brings the stored row back.
const INSERT_SQL: &str = "INSERT INTO webhooks (id, url, events, secret, enabled) \
     VALUES ($1, $2, $3, $4, $5) RETURNING *";

/// [`get`]: the one row behind its primary key.
const SELECT_SQL: &str = "SELECT * FROM webhooks WHERE id = $1";

/// [`list`]: the page window, ordered by the id whose text sorts in
/// creation order (decision 4) — the order the admin listing answers.
const LIST_SQL: &str = "SELECT * FROM webhooks ORDER BY id LIMIT $1 OFFSET $2";

/// [`enabled_for_event`]: the delivery fan-out's read — Pleroma's
/// `Webhook.get_by_type`, `where(^type in w.events)` and
/// `where(w.enabled == true)` in one statement.
const ENABLED_FOR_EVENT_SQL: &str =
    "SELECT * FROM webhooks WHERE enabled AND $1 = ANY(events) ORDER BY id";

/// [`update`]: the present-only PATCH — every `COALESCE` keeps the stored
/// value when its parameter is `NULL`.
const UPDATE_SQL: &str = "UPDATE webhooks \
     SET url = COALESCE($2, url), \
         events = COALESCE($3, events), \
         enabled = COALESCE($4, enabled), \
         updated_at = now() \
     WHERE id = $1 RETURNING *";

/// [`delete`]: one webhook by id.
const DELETE_SQL: &str = "DELETE FROM webhooks WHERE id = $1";

/// [`set_enabled`]: the enable/disable routes' single-column write.
const SET_ENABLED_SQL: &str =
    "UPDATE webhooks SET enabled = $2, updated_at = now() WHERE id = $1 RETURNING *";

/// [`rotate_secret`]: a freshly minted secret over the stored one.
const ROTATE_SQL: &str =
    "UPDATE webhooks SET secret = $2, updated_at = now() WHERE id = $1 RETURNING *";

/// The event types a webhook may subscribe to.
///
/// [`is_known_event`] is the membership test the admin API validates bodies
/// against; the three are what [`crate::services::webhooks`] emits —
/// `report.created` when a report is filed, `report.updated` when moderation
/// moves one, and `account.created` when a local account is registered.
pub const KNOWN_EVENTS: [&str; 3] = ["report.created", "report.updated", "account.created"];

/// Whether `event` is one of [`KNOWN_EVENTS`].
#[must_use]
pub fn is_known_event(event: &str) -> bool {
    KNOWN_EVENTS.contains(&event)
}

/// One row of `webhooks`: an event-delivery endpoint.
#[derive(Clone, PartialEq, Eq, sqlx::FromRow)]
pub struct Webhook {
    /// `UUIDv7` primary key, minted by [`create`] (decision 4).
    pub id: Uuid,
    /// Where events are delivered: an `http(s)` URL, validated by the API.
    pub url: String,
    /// The subscribed event types ([`KNOWN_EVENTS`]).
    pub events: Vec<String>,
    /// The signing secret: 32 random bytes as lowercase hex, minted by
    /// [`create`] and turned only by [`rotate_secret`]; redacted from
    /// `Debug`.
    pub secret: String,
    /// Whether deliveries happen at all.
    pub enabled: bool,
    /// Whether the instance itself installed this webhook; the admin API
    /// refuses to change those.
    pub internal: bool,
    /// When the webhook was created.
    pub inserted_at: DateTime<Utc>,
    /// When it last changed.
    pub updated_at: DateTime<Utc>,
}

/// [`secret`](Self::secret) is secret material and renders as
/// `<redacted>`; everything else shows.
impl fmt::Debug for Webhook {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("Webhook")
            .field("id", &self.id)
            .field("url", &self.url)
            .field("events", &self.events)
            .field("secret", &"<redacted>")
            .field("enabled", &self.enabled)
            .field("internal", &self.internal)
            .field("inserted_at", &self.inserted_at)
            .field("updated_at", &self.updated_at)
            .finish()
    }
}

/// The fields of a PATCH: present-only — each [`None`] leaves its column
/// as stored (Pleroma's `update_changeset` casts what the body carries).
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct WebhookChanges {
    /// The new delivery URL; `None` keeps the stored one.
    pub url: Option<String>,
    /// The new subscription list; `None` keeps the stored one.
    pub events: Option<Vec<String>>,
    /// The new gate; `None` keeps the stored one.
    pub enabled: Option<bool>,
}

/// Inserts a webhook for `url` subscribed to `events`, minting the id
/// (decision 4) and a fresh [`random_hex`] secret; `internal` stays at its
/// column default — the admin API never takes the flag from a body.
///
/// # Errors
///
/// Any database failure (connection, encoding) arrives as
/// [`AppError::Database`](crate::error::AppError).
pub async fn create(
    pool: &PgPool,
    url: &str,
    events: &[String],
    enabled: bool,
) -> AppResult<Webhook> {
    let row = sqlx::query_as::<_, Webhook>(INSERT_SQL)
        .bind(new_id())
        .bind(url)
        .bind(events)
        .bind(random_hex(SECRET_BYTES))
        .bind(enabled)
        .fetch_one(pool)
        .await?;
    Ok(row)
}

/// Fetches the webhook whose id is `id`, `None` when none carries it.
///
/// # Errors
///
/// Any database failure arrives as [`AppError::Database`](crate::error::AppError).
pub async fn get(pool: &PgPool, id: Uuid) -> AppResult<Option<Webhook>> {
    let row = sqlx::query_as::<_, Webhook>(SELECT_SQL)
        .bind(id)
        .fetch_optional(pool)
        .await?;
    Ok(row)
}

/// Every webhook to notify of `event`: the enabled rows whose `events`
/// array carries it.
///
/// Creation order (decision 4), the fan-out read of
/// [`crate::services::webhooks::emit`] — Pleroma's `Webhook.get_by_type`.
/// A disabled webhook is never among them, whatever it subscribes to.
///
/// # Errors
///
/// Any database failure arrives as [`AppError::Database`](crate::error::AppError).
pub async fn enabled_for_event(pool: &PgPool, event: &str) -> AppResult<Vec<Webhook>> {
    let rows = sqlx::query_as::<_, Webhook>(ENABLED_FOR_EVENT_SQL)
        .bind(event)
        .fetch_all(pool)
        .await?;
    Ok(rows)
}

/// One page of webhooks: `limit` rows starting at `offset`, in creation
/// order on the id (decision 4) — the order the admin listing answers,
/// Pleroma's unordered `Repo.all` pinned by the ticket.
///
/// # Errors
///
/// Any database failure arrives as [`AppError::Database`](crate::error::AppError).
pub async fn list(pool: &PgPool, limit: i64, offset: i64) -> AppResult<Vec<Webhook>> {
    let rows = sqlx::query_as::<_, Webhook>(LIST_SQL)
        .bind(limit)
        .bind(offset)
        .fetch_all(pool)
        .await?;
    Ok(rows)
}

/// Applies `changes` to the webhook `id`, answering the updated row or
/// `None` when no webhook carries the id. Fields the changes leave `None`
/// keep their stored values.
///
/// # Errors
///
/// Any database failure arrives as [`AppError::Database`](crate::error::AppError).
pub async fn update(
    pool: &PgPool,
    id: Uuid,
    changes: &WebhookChanges,
) -> AppResult<Option<Webhook>> {
    let row = sqlx::query_as::<_, Webhook>(UPDATE_SQL)
        .bind(id)
        .bind(changes.url.as_deref())
        .bind(changes.events.as_deref())
        .bind(changes.enabled)
        .fetch_optional(pool)
        .await?;
    Ok(row)
}

/// Deletes the webhook `id`, reporting whether there was one to remove.
///
/// # Errors
///
/// Any database failure arrives as [`AppError::Database`](crate::error::AppError).
pub async fn delete(pool: &PgPool, id: Uuid) -> AppResult<bool> {
    let outcome = sqlx::query(DELETE_SQL).bind(id).execute(pool).await?;
    Ok(outcome.rows_affected() > 0)
}

/// Sets the `enabled` gate of the webhook `id`, answering the updated row
/// or `None` when no webhook carries the id.
///
/// # Errors
///
/// Any database failure arrives as [`AppError::Database`](crate::error::AppError).
pub async fn set_enabled(pool: &PgPool, id: Uuid, enabled: bool) -> AppResult<Option<Webhook>> {
    let row = sqlx::query_as::<_, Webhook>(SET_ENABLED_SQL)
        .bind(id)
        .bind(enabled)
        .fetch_optional(pool)
        .await?;
    Ok(row)
}

/// Replaces the secret of the webhook `id` with a fresh [`random_hex`]
/// one, answering the updated row or `None` when no webhook carries the
/// id.
///
/// # Errors
///
/// Any database failure arrives as [`AppError::Database`](crate::error::AppError).
pub async fn rotate_secret(pool: &PgPool, id: Uuid) -> AppResult<Option<Webhook>> {
    let row = sqlx::query_as::<_, Webhook>(ROTATE_SQL)
        .bind(id)
        .bind(random_hex(SECRET_BYTES))
        .fetch_optional(pool)
        .await?;
    Ok(row)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The stored row behind `id`, re-read to prove what a write changed.
    async fn stored(pool: &PgPool, id: Uuid) -> Webhook {
        let row = get(pool, id).await.expect("the row reads back");
        row.expect("the webhook exists")
    }

    #[test]
    fn the_known_events_are_the_three_the_ticket_names() {
        for event in KNOWN_EVENTS {
            assert!(is_known_event(event), "{event} is known");
        }
        for event in ["report.deleted", "account.created ", "created", ""] {
            assert!(!is_known_event(event), "{event:?} is not a known event");
        }
    }

    #[tokio::test]
    async fn create_mints_id_secret_and_defaults() {
        let db = crate::test_db::test_db().await.expect("test database slot");
        let pool = db.pool().clone();
        let events = vec!["report.created".to_owned()];
        let created = create(&pool, "https://example.org/hook", &events, true)
            .await
            .expect("the insert runs");
        assert_ne!(created.id, Uuid::nil(), "the id is minted");
        assert_eq!(
            created.url, "https://example.org/hook",
            "the url round-trips"
        );
        assert_eq!(created.events, events, "the events round-trip");
        assert_eq!(created.secret.len(), 64, "32 bytes are 64 hex characters");
        assert!(
            created
                .secret
                .chars()
                .all(|c| c.is_ascii_digit() || ('a'..='f').contains(&c)),
            "the secret is lowercase hex"
        );
        assert!(created.enabled, "the submitted gate stands");
        assert!(!created.internal, "a created webhook is external");

        let second = create(&pool, "https://example.org/other", &[], false)
            .await
            .expect("the second insert runs");
        assert_ne!(created.secret, second.secret, "two mints do not collide");
        assert!(
            second.events.is_empty(),
            "no subscriptions round-trips as []"
        );
        assert!(!second.enabled, "the gate follows the body");
        assert_eq!(
            stored(&pool, created.id).await,
            created,
            "the row stands as returned"
        );
    }

    #[tokio::test]
    async fn get_answers_the_row_or_none() {
        let db = crate::test_db::test_db().await.expect("test database slot");
        let pool = db.pool().clone();
        let created = create(&pool, "https://example.org/h", &[], true)
            .await
            .expect("the insert runs");
        let found = get(&pool, created.id)
            .await
            .expect("the lookup runs")
            .expect("the webhook is found");
        assert_eq!(found, created, "the exact row");
        let miss = get(&pool, Uuid::now_v7()).await.expect("the lookup runs");
        assert!(miss.is_none(), "an unknown id finds nothing");
    }

    #[tokio::test]
    async fn update_applies_only_the_present_fields() {
        let db = crate::test_db::test_db().await.expect("test database slot");
        let pool = db.pool().clone();
        let events = vec!["report.created".to_owned(), "account.created".to_owned()];
        let created = create(&pool, "https://example.org/hook", &events, true)
            .await
            .expect("the insert runs");

        let changed = update(
            &pool,
            created.id,
            &WebhookChanges {
                url: Some("https://example.org/moved".to_owned()),
                events: Some(vec!["report.updated".to_owned()]),
                enabled: None,
            },
        )
        .await
        .expect("the update runs")
        .expect("the webhook exists");
        assert_eq!(changed.url, "https://example.org/moved", "the url moved");
        assert_eq!(
            changed.events,
            vec!["report.updated".to_owned()],
            "and the events"
        );
        assert!(changed.enabled, "an absent gate stays");
        assert_eq!(
            changed.secret, created.secret,
            "an update never touches the secret"
        );
        assert!(!changed.internal, "and neither the flag");

        let flipped = update(
            &pool,
            created.id,
            &WebhookChanges {
                enabled: Some(false),
                ..WebhookChanges::default()
            },
        )
        .await
        .expect("the second update runs")
        .expect("the webhook exists");
        assert_eq!(flipped.url, changed.url, "absent url stays");
        assert_eq!(flipped.events, changed.events, "absent events stay");
        assert!(!flipped.enabled, "the present gate flips");
        assert_eq!(
            stored(&pool, created.id).await,
            flipped,
            "the row stands as returned"
        );

        let miss = update(&pool, Uuid::now_v7(), &WebhookChanges::default())
            .await
            .expect("the update runs");
        assert!(miss.is_none(), "an unknown id updates nothing");
    }

    #[tokio::test]
    async fn set_enabled_flips_the_gate_both_ways() {
        let db = crate::test_db::test_db().await.expect("test database slot");
        let pool = db.pool().clone();
        let created = create(&pool, "https://example.org/hook", &[], true)
            .await
            .expect("the insert runs");
        let off = set_enabled(&pool, created.id, false)
            .await
            .expect("the disable runs")
            .expect("the webhook exists");
        assert!(!off.enabled, "disabled");
        assert_eq!(off.url, created.url, "the rest of the row stands");
        let on = set_enabled(&pool, created.id, true)
            .await
            .expect("the enable runs")
            .expect("the webhook exists");
        assert!(on.enabled, "and enabled again");
        let miss = set_enabled(&pool, Uuid::now_v7(), true)
            .await
            .expect("the enable runs");
        assert!(miss.is_none(), "an unknown id gates nothing");
    }

    #[tokio::test]
    async fn rotate_secret_mints_a_fresh_one_and_keeps_the_rest() {
        let db = crate::test_db::test_db().await.expect("test database slot");
        let pool = db.pool().clone();
        let events = vec!["report.created".to_owned()];
        let created = create(&pool, "https://example.org/hook", &events, true)
            .await
            .expect("the insert runs");
        let rotated = rotate_secret(&pool, created.id)
            .await
            .expect("the rotation runs")
            .expect("the webhook exists");
        assert_ne!(rotated.secret, created.secret, "a new secret");
        assert_eq!(rotated.secret.len(), 64, "the same hex shape");
        assert_eq!(rotated.url, created.url, "everything else stands");
        assert_eq!(rotated.events, created.events, "url, events");
        assert_eq!(rotated.enabled, created.enabled, "gate");
        assert_eq!(rotated.id, created.id, "and id");
        assert_eq!(
            stored(&pool, created.id).await,
            rotated,
            "the row carries the new secret"
        );
        let miss = rotate_secret(&pool, Uuid::now_v7())
            .await
            .expect("the rotation runs");
        assert!(miss.is_none(), "an unknown id rotates nothing");
    }

    #[tokio::test]
    async fn delete_removes_the_row_once() {
        let db = crate::test_db::test_db().await.expect("test database slot");
        let pool = db.pool().clone();
        let doomed = create(&pool, "https://example.org/gone", &[], true)
            .await
            .expect("the doomed webhook inserts");
        assert!(
            delete(&pool, doomed.id).await.expect("the delete runs"),
            "the row went"
        );
        assert!(
            !delete(&pool, doomed.id)
                .await
                .expect("the second delete runs"),
            "deleting twice removes nothing the second time"
        );
        assert!(
            get(&pool, doomed.id)
                .await
                .expect("the lookup runs")
                .is_none(),
            "it is gone"
        );
    }

    #[tokio::test]
    async fn list_pages_in_creation_order() {
        let db = crate::test_db::test_db().await.expect("test database slot");
        let pool = db.pool().clone();
        // Controlled ids so the expected order is fixed, not raced: the
        // v7 text sorts as the numbers here do.
        for (index, id) in [1_u128, 2, 3].into_iter().enumerate() {
            let url = format!("https://example.org/h{index}");
            let _seeded = sqlx::query(
                "INSERT INTO webhooks (id, url, events, secret, enabled, internal) \
                 VALUES ($1, $2, '{}', 'seed-secret', true, false)",
            )
            .bind(Uuid::from_u128(id))
            .bind(&url)
            .execute(&pool)
            .await
            .expect("the seed webhook inserts");
        }
        let first_page = list(&pool, 2, 0).await.expect("the listing runs");
        let ids: Vec<Uuid> = first_page.iter().map(|hook| hook.id).collect();
        assert_eq!(
            ids,
            vec![Uuid::from_u128(1), Uuid::from_u128(2)],
            "the first window"
        );
        let second_page = list(&pool, 2, 2).await.expect("the second window runs");
        assert_eq!(
            second_page
                .iter()
                .map(|hook| hook.id)
                .collect::<Vec<Uuid>>(),
            vec![Uuid::from_u128(3)],
            "the tail of the table"
        );
        let past = list(&pool, 2, 9).await.expect("the empty window runs");
        assert!(past.is_empty(), "a window past the table is empty");
    }

    #[tokio::test]
    async fn enabled_for_event_selects_the_subscribed_enabled_rows() {
        let db = crate::test_db::test_db().await.expect("test database slot");
        let pool = db.pool().clone();
        let subscribed = create(
            &pool,
            "https://example.org/on",
            &["report.created".to_owned()],
            true,
        )
        .await
        .expect("the subscribed webhook inserts");
        let _other_event = create(
            &pool,
            "https://example.org/other-event",
            &["account.created".to_owned()],
            true,
        )
        .await
        .expect("the second webhook inserts");
        let _disabled = create(
            &pool,
            "https://example.org/off",
            &["report.created".to_owned()],
            false,
        )
        .await
        .expect("the disabled webhook inserts");
        let _unsubscribed = create(&pool, "https://example.org/no-events", &[], true)
            .await
            .expect("the unsubscribed webhook inserts");
        let multi = create(
            &pool,
            "https://example.org/both",
            &["report.created".to_owned(), "report.updated".to_owned()],
            true,
        )
        .await
        .expect("the multi-event webhook inserts");

        let hits = enabled_for_event(&pool, "report.created")
            .await
            .expect("the fan-out read runs");
        let ids: Vec<Uuid> = hits.iter().map(|hook| hook.id).collect();
        assert!(
            ids.contains(&subscribed.id) && ids.contains(&multi.id),
            "both subscribed enabled rows answer, the multi-event one included: {ids:?}"
        );
        assert_eq!(ids.len(), 2, "the disabled and unsubscribed rows stay out");
        assert!(
            enabled_for_event(&pool, "report.deleted")
                .await
                .expect("the unknown event read runs")
                .is_empty(),
            "an event nobody subscribes to fans out to nothing"
        );

        let off = set_enabled(&pool, subscribed.id, false)
            .await
            .expect("the disable runs");
        assert!(off.is_some(), "the row exists to disable");
        assert!(
            !enabled_for_event(&pool, "report.created")
                .await
                .expect("the read after disabling runs")
                .iter()
                .any(|hook| hook.id == subscribed.id),
            "disabling takes the row out of the fan-out"
        );
    }

    #[test]
    fn webhook_debug_redacts_the_secret() {
        let webhook = Webhook {
            id: Uuid::now_v7(),
            url: "https://example.org".to_owned(),
            events: vec![],
            secret: "deadbeef".to_owned(),
            enabled: true,
            internal: false,
            inserted_at: Utc::now(),
            updated_at: Utc::now(),
        };
        let rendered = format!("{webhook:?}");
        assert!(
            !rendered.contains("deadbeef"),
            "the secret must not leak into Debug output"
        );
        assert!(
            rendered.contains(r#"secret: "<redacted>""#),
            "the marker names the field"
        );
    }
}
