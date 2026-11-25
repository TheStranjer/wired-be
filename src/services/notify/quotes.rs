//! The quote tellings of [`crate::services::notify`]: who hears that their
//! post was quoted, and that a post quoting theirs was edited. The shared
//! creation plumbing these call lives in the parent module.

use tracing::warn;
use uuid::Uuid;

use crate::{
    error::AppResult,
    models::{
        notification::{self, TYPE_QUOTE, TYPE_QUOTED_UPDATE, Telling},
        quote_request,
        status::{self, Status as StatusRow},
        status_visibility,
        user::{self, User},
    },
    state::AppState,
};

use super::tell;

/// The notification of a fresh quote, landed from `after_status_created`
/// in [`crate::services::create_status`] and, for a quote linked after
/// the fact, from [`on_quote_linked`].
///
/// The author of the quoted post, when local, is told `quote` naming the
/// **quoting** status with its author as the actor (Mastodon 4.5's `quote`
/// notification). It is told once per quoting status, never of a
/// self-quote, never of a quote its author refused or revoked, never of a
/// boost row or a tombstone, and never of a quote the recipient cannot see.
/// Blocks and mutes are the shared creation arm's business, as for a
/// mention.
pub async fn on_quote(state: &AppState, row: &StatusRow) {
    if let Err(err) = quote_telling(state, row, TYPE_QUOTE).await {
        warn!(
            status = %row.id,
            %err,
            "could not tell a quoted post's author about the quote"
        );
    }
}

/// The notification of an edited quote, landed from the edit hook
/// `after_status_edited` in [`crate::api::statuses`].
///
/// The author of the quoted post, when local, is told `quoted_update`
/// naming the edited quoting status (Mastodon 4.5's `quoted_update`), under
/// the same rules as [`on_quote`] — except that every edit tells again.
pub async fn on_quoted_update(state: &AppState, row: &StatusRow) {
    if let Err(err) = quote_telling(state, row, TYPE_QUOTED_UPDATE).await {
        warn!(
            status = %row.id,
            %err,
            "could not tell a quoted post's author about the edit"
        );
    }
}

/// The notification of a quote linked after it was stored: the quoted post
/// arrived later, or its backfill or grant linked it.
///
/// The quoting status is read again, for the link that was just written is
/// not in the row the caller holds; a quote that is gone or still unlinked
/// tells nobody.
pub async fn on_quote_linked(state: &AppState, quoting_id: Uuid) {
    match status::get_by_id(&state.db, quoting_id).await {
        Ok(Some(row)) => on_quote(state, &row).await,
        Ok(None) => {}
        Err(err) => warn!(
            status = %quoting_id,
            %err,
            "could not read a linked quote to tell its target's author"
        ),
    }
}

/// The work of [`on_quote`] and [`on_quoted_update`]: find the quoted
/// post's local author, apply the quote's own rules, and tell them
/// `event_type` of the quoting status.
async fn quote_telling(state: &AppState, row: &StatusRow, event_type: &str) -> AppResult<()> {
    let Some(quoted_id) = row.quote_of_id else {
        return Ok(());
    };
    if row.reblog_of_id.is_some() || row.deleted_at.is_some() {
        return Ok(());
    }
    let Some(quoted) = status::get_by_id(&state.db, quoted_id).await? else {
        return Ok(());
    };
    if quoted.user_id == row.user_id {
        return Ok(());
    }
    let Some(recipient) = user::get_by_id(&state.db, quoted.user_id).await? else {
        return Ok(());
    };
    if !recipient.local.get() || !may_tell(state, row, &recipient).await? {
        return Ok(());
    }
    if event_type == TYPE_QUOTE && already_told(state, row, &recipient).await? {
        return Ok(());
    }
    tell(state, &recipient, row.user_id, event_type, Some(row.id)).await
}

/// Whether the quote stands and `recipient` may see the quoting post: no
/// refused or revoked request, and the post's audience admits them.
async fn may_tell(state: &AppState, row: &StatusRow, recipient: &User) -> AppResult<bool> {
    let denied = quote_request::denied_for_statuses(&state.db, &[row.id]).await?;
    if !denied.is_empty() {
        return Ok(false);
    }
    status_visibility::can_view(&state.db, row, Some(recipient)).await
}

/// Whether `recipient` already holds a live `quote` notification of `row`.
async fn already_told(state: &AppState, row: &StatusRow, recipient: &User) -> AppResult<bool> {
    notification::already_told(
        &state.db,
        &Telling {
            user_id: recipient.id,
            from_account_id: row.user_id,
            notification_type: TYPE_QUOTE,
            status_id: Some(row.id),
            emoji: None,
        },
    )
    .await
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{
        models::{
            block,
            quote_request::{NewQuoteRequest, QuoteRequestState},
            status::{NewStatus, Visibility},
        },
        services::notify::test_support::{app_state, facts, local, post, remote, rows},
    };

    /// The base the fixtures' local statuses are stored under.
    const BASE: &str = crate::config::TEST_BASE_URL;

    /// `author`'s public post quoting `quoted`, stored linked.
    async fn quote_of(pool: &sqlx::PgPool, author: &User, quoted: &StatusRow) -> StatusRow {
        status::insert(
            pool,
            BASE,
            &NewStatus {
                quote_of_id: Some(quoted.id),
                ..NewStatus::local(author, "quoting")
            },
        )
        .await
        .expect("the quoting status inserts")
    }

    #[tokio::test]
    async fn a_quote_tells_the_quoted_author_once() {
        let db = crate::test_db::test_db().await.expect("test database slot");
        let pool = db.pool();
        let state = app_state(pool);
        let alice = local(pool, "qn-alice").await;
        let bob = local(pool, "qn-bob").await;
        let original = post(pool, &alice, "quote me").await;
        let quoting = quote_of(pool, &bob, &original).await;

        on_quote(&state, &quoting).await;
        on_quote(&state, &quoting).await;

        assert_eq!(
            facts(pool, alice.id).await,
            vec![(TYPE_QUOTE.to_owned(), bob.id, Some(quoting.id))],
            "one quote notification names the quoting status and its author"
        );
        assert!(
            rows(pool, bob.id).await.is_empty(),
            "the quoter hears nothing"
        );
    }

    #[tokio::test]
    async fn a_self_quote_and_a_plain_post_tell_nobody() {
        let db = crate::test_db::test_db().await.expect("test database slot");
        let pool = db.pool();
        let state = app_state(pool);
        let alice = local(pool, "qs-alice").await;
        let original = post(pool, &alice, "mine").await;
        let own = quote_of(pool, &alice, &original).await;
        on_quote(&state, &own).await;
        on_quote(&state, &original).await;
        assert!(
            rows(pool, alice.id).await.is_empty(),
            "a self-quote and a plain post tell nobody"
        );
    }

    #[tokio::test]
    async fn a_quote_from_a_blocked_account_makes_no_row() {
        let db = crate::test_db::test_db().await.expect("test database slot");
        let pool = db.pool();
        let state = app_state(pool);
        let alice = local(pool, "qb-alice").await;
        let mallory = local(pool, "qb-mallory").await;
        let _block = block::block(pool, BASE, &alice, &mallory)
            .await
            .expect("the block lands");
        let original = post(pool, &alice, "quote me").await;
        let quoting = quote_of(pool, &mallory, &original).await;

        on_quote(&state, &quoting).await;

        assert!(rows(pool, alice.id).await.is_empty(), "no notification row");
    }

    #[tokio::test]
    async fn a_remote_quoted_author_is_not_told_from_here() {
        let db = crate::test_db::test_db().await.expect("test database slot");
        let pool = db.pool();
        let state = app_state(pool);
        let wanda = remote(pool, "qr-wanda", "remote.example").await;
        let bob = local(pool, "qr-bob").await;
        let original = post(pool, &wanda, "remote post").await;
        let quoting = quote_of(pool, &bob, &original).await;

        on_quote(&state, &quoting).await;

        assert!(
            rows(pool, wanda.id).await.is_empty(),
            "federation tells them"
        );
    }

    #[tokio::test]
    async fn a_refused_quote_tells_nobody() {
        let db = crate::test_db::test_db().await.expect("test database slot");
        let pool = db.pool();
        let state = app_state(pool);
        let alice = local(pool, "qd-alice").await;
        let bob = local(pool, "qd-bob").await;
        let original = post(pool, &alice, "quote me").await;
        let quoting = quote_of(pool, &bob, &original).await;
        let request = quote_request::insert(
            pool,
            &NewQuoteRequest {
                quoting_status_id: quoting.id,
                quoted_status_id: original.id,
                quoted_user_id: alice.id,
                request_uri: "https://test.example/activities/qd-1",
            },
        )
        .await
        .expect("the request writes");
        quote_request::set_state(pool, request.id, QuoteRequestState::Rejected)
            .await
            .expect("the refusal writes");

        on_quote(&state, &quoting).await;

        assert!(
            rows(pool, alice.id).await.is_empty(),
            "a rejected quote is silent"
        );
    }

    #[tokio::test]
    async fn a_quote_the_author_cannot_see_tells_nobody() {
        let db = crate::test_db::test_db().await.expect("test database slot");
        let pool = db.pool();
        let state = app_state(pool);
        let alice = local(pool, "qv-alice").await;
        let bob = local(pool, "qv-bob").await;
        let original = post(pool, &alice, "quote me").await;
        let quoting = status::insert(
            pool,
            BASE,
            &NewStatus {
                quote_of_id: Some(original.id),
                visibility: Visibility::Direct,
                ..NewStatus::local(&bob, "for others only")
            },
        )
        .await
        .expect("the direct quote inserts");

        on_quote(&state, &quoting).await;

        assert!(
            rows(pool, alice.id).await.is_empty(),
            "no leak of a direct post"
        );
    }

    #[tokio::test]
    async fn a_quote_linked_later_tells_once() {
        let db = crate::test_db::test_db().await.expect("test database slot");
        let pool = db.pool();
        let state = app_state(pool);
        let alice = local(pool, "ql-alice").await;
        let bob = remote(pool, "ql-bob", "remote.example").await;
        let original = post(pool, &alice, "quote me").await;
        let quoting = status::insert(
            pool,
            BASE,
            &NewStatus {
                user_id: bob.id,
                quote_uri: Some(original.uri.clone()),
                uri: Some("https://remote.example/objects/ql-quote".to_owned()),
                ..NewStatus::default()
            },
        )
        .await
        .expect("the unlinked quote inserts");

        on_quote_linked(&state, quoting.id).await;
        assert!(
            rows(pool, alice.id).await.is_empty(),
            "unlinked: nothing yet"
        );

        let linked = status::link_backfilled_quote(pool, quoting.id, &original)
            .await
            .expect("the link writes");
        assert!(linked, "the link wrote");
        on_quote_linked(&state, quoting.id).await;
        on_quote_linked(&state, quoting.id).await;

        assert_eq!(
            facts(pool, alice.id).await,
            vec![(TYPE_QUOTE.to_owned(), bob.id, Some(quoting.id))],
            "the late link tells exactly once"
        );
    }

    #[tokio::test]
    async fn an_edited_quote_tells_quoted_update_each_time() {
        let db = crate::test_db::test_db().await.expect("test database slot");
        let pool = db.pool();
        let state = app_state(pool);
        let alice = local(pool, "qu-alice").await;
        let bob = local(pool, "qu-bob").await;
        let original = post(pool, &alice, "quote me").await;
        let quoting = quote_of(pool, &bob, &original).await;

        on_quoted_update(&state, &quoting).await;
        on_quoted_update(&state, &quoting).await;

        assert_eq!(
            facts(pool, alice.id).await,
            vec![
                (TYPE_QUOTED_UPDATE.to_owned(), bob.id, Some(quoting.id)),
                (TYPE_QUOTED_UPDATE.to_owned(), bob.id, Some(quoting.id)),
            ],
            "every edit is its own news, naming the quoting status"
        );
    }
}
