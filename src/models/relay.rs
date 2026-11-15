//! The `relays` table: the relay actors THIS instance follows.
//!
//! One row per followed relay, keyed by the relay actor's `ActivityPub` id
//! — the `object` of the `Follow` [`crate::services::relay`] sends, and
//! the `actor` of the `Accept` that turns [`RelayState::Pending`] into
//! [`RelayState::Accepted`] ([`mark_accepted`]) and of the `Announce`es the
//! [Announce handler](crate::federation::handlers::interactions) folds
//! without a reblog row. The reference is Pleroma's
//! `Pleroma.Web.ActivityPub.Relay`
//! (`lib/pleroma/web/activity_pub/relay.ex`), whose subscriptions ride on
//! the service actor's `Following` relationships; here the relationship is
//! a `follows` row and this table carries the state and the inbox the
//! [publisher](crate::federation::publisher) fans public activities to
//! ([`crate::models::instance::relay_inboxes`], answering
//! [`accepted_inboxes`]).
//!
//! Nothing here talks the network or builds documents: the handshake is
//! [`crate::services::relay`]'s; this module is the table.

use chrono::{DateTime, Utc};
use sqlx::PgPool;

use crate::error::AppResult;

/// The state of a relay subscription (the `relays.state` column).
#[derive(Clone, Copy, Debug, PartialEq, Eq, sqlx::Type)]
#[sqlx(type_name = "text", rename_all = "lowercase")]
pub enum RelayState {
    /// The `Follow` has been sent; the relay has not answered with its
    /// `Accept` yet, so its inboxes are not fan-out targets.
    Pending,
    /// The relay accepted the follow: its inbox joins the publisher's
    /// target set for public activities, and its `Announce`es fold.
    Accepted,
}

/// One row of `relays`: a relay actor this instance follows.
#[derive(Clone, Debug, PartialEq, Eq, sqlx::FromRow)]
pub struct Relay {
    /// The relay actor's `ActivityPub` id — the primary key, and the
    /// `object` the `Follow` named.
    pub actor_ap_id: String,
    /// The relay actor's inbox URL: where the `Follow` went and what the
    /// publisher delivers public activities to.
    pub inbox: String,
    /// Whether the relay has answered our `Follow` with its `Accept`.
    pub state: RelayState,
    /// When the follow was sent.
    pub inserted_at: DateTime<Utc>,
}

/// [`upsert`]: re-following a known relay refreshes its inbox — a relay
/// that moved its endpoint must keep hearing us — and leaves the stored
/// `state` alone, so a restated follow of an accepted relay does not
/// re-pend it.
const UPSERT_SQL: &str = "INSERT INTO relays (actor_ap_id, inbox) VALUES ($1, $2) \
                          ON CONFLICT (actor_ap_id) DO UPDATE SET inbox = EXCLUDED.inbox \
                          RETURNING actor_ap_id, inbox, state, inserted_at";

/// [`mark_accepted`]: the grant alone — a `Reject`, an unknown actor or a
/// row that stands accepted already matches nothing here.
const MARK_ACCEPTED_SQL: &str = "UPDATE relays SET state = 'accepted' \
                                 WHERE actor_ap_id = $1 AND state <> 'accepted'";

/// [`get`]: the one subscription named by its actor id.
const GET_SQL: &str = "SELECT actor_ap_id, inbox, state, inserted_at FROM relays \
                       WHERE actor_ap_id = $1";

/// [`is_accepted`]: the one fact the `Announce` handler asks — is this
/// actor a relay whose `Accept` we hold? — as an existence check.
const IS_ACCEPTED_SQL: &str = "SELECT EXISTS (SELECT 1 FROM relays \
                               WHERE actor_ap_id = $1 AND state = 'accepted')";

/// [`accepted_inboxes`]: the fan-out addresses of the accepted
/// subscriptions, oldest follow first. A pending relay has not promised
/// us its distribution, so its inbox joins the set only after
/// [`mark_accepted`].
const ACCEPTED_INBOXES_SQL: &str = "SELECT inbox FROM relays \
                                    WHERE state = 'accepted' ORDER BY inserted_at, actor_ap_id";

/// [`list`]: every subscription, oldest first — the admin ledger, pending
/// and accepted alike.
const LIST_SQL: &str = "SELECT actor_ap_id, inbox, state, inserted_at FROM relays \
                        ORDER BY inserted_at, actor_ap_id";

/// [`delete`]: the withdrawal of the follow; whether a row stood there is
/// the answer.
const DELETE_SQL: &str = "DELETE FROM relays WHERE actor_ap_id = $1";

/// Records the follow of the relay `actor_ap_id` with inbox `inbox`,
/// answering with the stored row.
///
/// The write is an upsert on the actor id: a re-follow refreshes a moved
/// inbox and keeps the standing state, so following a relay that already
/// accepted us does not re-pend it.
///
/// # Errors
///
/// Any database failure (connection, constraint, decoding) arrives as
/// [`AppError::Database`](crate::error::AppError).
pub async fn upsert(pool: &PgPool, actor_ap_id: &str, inbox: &str) -> AppResult<Relay> {
    let row = sqlx::query_as::<_, Relay>(UPSERT_SQL)
        .bind(actor_ap_id)
        .bind(inbox)
        .fetch_one(pool)
        .await?;
    Ok(row)
}

/// Turns the subscription to `actor_ap_id` accepted — the arm the inbound
/// `Accept` handler runs after it grants the follow, for the case where
/// the followee is a relay we follow.
///
/// `true` is the pending row this grant moved; `false` means there was
/// nothing to move — the actor is not one of our relays (the ordinary
/// case) or its row stood accepted already.
///
/// # Errors
///
/// Any database failure (connection) arrives as
/// [`AppError::Database`](crate::error::AppError).
pub async fn mark_accepted(pool: &PgPool, actor_ap_id: &str) -> AppResult<bool> {
    let moved = sqlx::query(MARK_ACCEPTED_SQL)
        .bind(actor_ap_id)
        .execute(pool)
        .await?;
    Ok(moved.rows_affected() > 0)
}

/// The subscription to `actor_ap_id`, `None` when this instance does not
/// follow that relay.
///
/// # Errors
///
/// Any database failure (connection, decoding) arrives as
/// [`AppError::Database`](crate::error::AppError).
pub async fn get(pool: &PgPool, actor_ap_id: &str) -> AppResult<Option<Relay>> {
    let row = sqlx::query_as::<_, Relay>(GET_SQL)
        .bind(actor_ap_id)
        .fetch_optional(pool)
        .await?;
    Ok(row)
}

/// Whether `actor_ap_id` is a relay this instance follows and the relay
/// has accepted: the gate the `Announce` handler reads to fold a relay's
/// re-broadcast as an ingest rather than a boost.
///
/// # Errors
///
/// Any database failure (connection) arrives as
/// [`AppError::Database`](crate::error::AppError).
pub async fn is_accepted(pool: &PgPool, actor_ap_id: &str) -> AppResult<bool> {
    let accepted = sqlx::query_scalar::<_, bool>(IS_ACCEPTED_SQL)
        .bind(actor_ap_id)
        .fetch_one(pool)
        .await?;
    Ok(accepted)
}

/// The inbox URLs of every accepted relay, oldest follow first: the body
/// behind [`crate::models::instance::relay_inboxes`], the set the
/// publisher adds for public and self-broadcast activities.
///
/// # Errors
///
/// Any database failure (connection, decoding) arrives as
/// [`AppError::Database`](crate::error::AppError).
pub async fn accepted_inboxes(pool: &PgPool) -> AppResult<Vec<String>> {
    let inboxes = sqlx::query_scalar::<_, String>(ACCEPTED_INBOXES_SQL)
        .fetch_all(pool)
        .await?;
    Ok(inboxes)
}

/// Every relay this instance follows, pending and accepted, oldest
/// first — the ledger the admin API lists.
///
/// # Errors
///
/// Any database failure (connection, decoding) arrives as
/// [`AppError::Database`](crate::error::AppError).
pub async fn list(pool: &PgPool) -> AppResult<Vec<Relay>> {
    let rows = sqlx::query_as::<_, Relay>(LIST_SQL).fetch_all(pool).await?;
    Ok(rows)
}

/// Removes the subscription to `actor_ap_id`, reporting whether a row
/// stood there: [`crate::services::relay::unfollow_relay`] calls it after
/// the `Undo` has been queued.
///
/// # Errors
///
/// Any database failure (connection) arrives as
/// [`AppError::Database`](crate::error::AppError).
pub async fn delete(pool: &PgPool, actor_ap_id: &str) -> AppResult<bool> {
    let gone = sqlx::query(DELETE_SQL)
        .bind(actor_ap_id)
        .execute(pool)
        .await?;
    Ok(gone.rows_affected() > 0)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The relay every single-relay test here is about.
    const RELAY: &str = "https://relay.example/relay";
    /// Its inbox.
    const INBOX: &str = "https://relay.example/inbox";

    /// A row-count read, for the delete arms.
    async fn relay_count(pool: &PgPool) -> i64 {
        sqlx::query_scalar::<_, i64>("SELECT count(*) FROM relays")
            .fetch_one(pool)
            .await
            .expect("the count reads")
    }

    /// A fresh follow stores a `pending` row at the actor id and inbox it
    /// names.
    #[tokio::test]
    async fn upsert_stores_a_pending_subscription() {
        let db = crate::test_db::test_db().await.expect("test database slot");
        let pool = db.pool().clone();
        let row = upsert(&pool, RELAY, INBOX).await.unwrap();

        assert_eq!(row.actor_ap_id, RELAY, "keyed by the actor id");
        assert_eq!(row.inbox, INBOX, "with the inbox it will deliver to");
        assert_eq!(row.state, RelayState::Pending, "pending until accepted");

        let found = get(&pool, RELAY).await.unwrap().expect("the row stands");
        assert_eq!(found, row, "and reads back unchanged");
        assert!(
            get(&pool, "https://other.example/relay")
                .await
                .unwrap()
                .is_none(),
            "an actor we never followed is no relay"
        );
    }

    /// Re-following refreshes a moved inbox and never re-pends an
    /// accepted subscription: one row per actor, whatever the repeat.
    #[tokio::test]
    async fn upsert_refreshes_the_inbox_and_keeps_the_state() {
        let db = crate::test_db::test_db().await.expect("test database slot");
        let pool = db.pool().clone();
        let _pending = upsert(&pool, RELAY, INBOX).await.unwrap();
        let _moved = mark_accepted(&pool, RELAY).await.unwrap();

        let restated = upsert(&pool, RELAY, "https://relay.example/new-inbox")
            .await
            .unwrap();
        assert_eq!(
            restated.inbox, "https://relay.example/new-inbox",
            "the newer inbox stands"
        );
        assert_eq!(
            restated.state,
            RelayState::Accepted,
            "and the grant is not undone by a restated follow"
        );
        assert_eq!(relay_count(&pool).await, 1, "one row, two upserts");
    }

    /// [`mark_accepted`] moves a pending row, is quiet about an accepted
    /// one, and refuses to invent a row for an unknown actor.
    #[tokio::test]
    async fn mark_accepted_moves_only_a_pending_row() {
        let db = crate::test_db::test_db().await.expect("test database slot");
        let pool = db.pool().clone();
        assert!(
            !mark_accepted(&pool, RELAY).await.unwrap(),
            "a stranger cannot be granted"
        );
        assert_eq!(relay_count(&pool).await, 0, "and no row was made");

        let _pending = upsert(&pool, RELAY, INBOX).await.unwrap();
        assert!(
            mark_accepted(&pool, RELAY).await.unwrap(),
            "the pending row moves"
        );
        assert!(
            !mark_accepted(&pool, RELAY).await.unwrap(),
            "the second grant has nothing left to move"
        );
        assert!(is_accepted(&pool, RELAY).await.unwrap(), "and it stands");
        assert!(
            !is_accepted(&pool, "https://other.example/relay")
                .await
                .unwrap(),
            "an unknown actor is no accepted relay"
        );
    }

    /// [`accepted_inboxes`] answers only the granted subscriptions, oldest
    /// follow first — the set the publisher fans public activities to.
    #[tokio::test]
    async fn accepted_inboxes_answers_the_granted_subscriptions() {
        let db = crate::test_db::test_db().await.expect("test database slot");
        let pool = db.pool().clone();
        assert!(
            accepted_inboxes(&pool).await.unwrap().is_empty(),
            "with no relays the answer is empty"
        );

        let _first = upsert(
            &pool,
            "https://one.example/relay",
            "https://one.example/inbox",
        )
        .await
        .unwrap();
        let _second = upsert(&pool, RELAY, INBOX).await.unwrap();
        let _moved = mark_accepted(&pool, RELAY).await.unwrap();

        let inboxes = accepted_inboxes(&pool).await.unwrap();
        assert_eq!(
            inboxes,
            vec![INBOX.to_owned()],
            "the accepted relay only, the pending one stays out"
        );

        // Order follows the subscriptions; a re-follow keeps the row's
        // original age.
        let _third = upsert(
            &pool,
            "https://two.example/relay",
            "https://two.example/inbox",
        )
        .await
        .unwrap();
        let _one_granted = mark_accepted(&pool, "https://one.example/relay")
            .await
            .unwrap();
        let _two_granted = mark_accepted(&pool, "https://two.example/relay")
            .await
            .unwrap();
        let all = accepted_inboxes(&pool).await.unwrap();
        assert_eq!(
            all,
            vec![
                "https://one.example/inbox".to_owned(),
                INBOX.to_owned(),
                "https://two.example/inbox".to_owned(),
            ],
            "oldest follow first"
        );
    }

    /// [`list`] shows the whole ledger — pending and accepted — and
    /// [`delete`] takes a row out, reporting whether one stood there.
    #[tokio::test]
    async fn list_covers_both_states_and_delete_removes_the_row() {
        let db = crate::test_db::test_db().await.expect("test database slot");
        let pool = db.pool().clone();
        let _relay = upsert(&pool, RELAY, INBOX).await.unwrap();
        let _one = upsert(
            &pool,
            "https://one.example/relay",
            "https://one.example/inbox",
        )
        .await
        .unwrap();
        let _granted = mark_accepted(&pool, RELAY).await.unwrap();
        // Age the first subscription so the listing's order is settled
        // rather than a race against the microsecond clock.
        let _aged = sqlx::query(
            "UPDATE relays SET inserted_at = now() - interval '1 minute' WHERE actor_ap_id = $1",
        )
        .bind(RELAY)
        .execute(&pool)
        .await
        .unwrap();

        let listed = list(&pool).await.unwrap();
        assert_eq!(listed.len(), 2, "both subscriptions are listed");
        assert_eq!(
            listed[0].actor_ap_id, RELAY,
            "oldest subscription first — the accepted one"
        );
        assert_eq!(
            listed[0].state,
            RelayState::Accepted,
            "the ledger keeps the granted row"
        );
        assert_eq!(listed[1].state, RelayState::Pending, "and the waiting one");

        assert!(delete(&pool, RELAY).await.unwrap(), "the row was there");
        assert!(
            !delete(&pool, RELAY).await.unwrap(),
            "and a repeat removes nothing"
        );
        assert_eq!(
            list(&pool).await.unwrap().len(),
            1,
            "one ledger entry stands"
        );
        assert!(
            !is_accepted(&pool, RELAY).await.unwrap(),
            "an unfollowed relay is nobody's target again"
        );
    }
}
