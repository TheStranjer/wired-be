//! The subscription telling of [`crate::services::notify`]: who hears
//! about a fresh post from the followers who subscribed to it. The shared
//! creation plumbing this telling calls lives in the parent module.

use tracing::warn;

use crate::{
    error::AppResult,
    models::{
        follow,
        notification::TYPE_STATUS,
        status::{Status as StatusRow, Visibility},
    },
    state::AppState,
};

use super::tell;

/// The create service's subscription notifications, landed from
/// `after_status_created` in [`crate::services::create_status`] beside
/// [`on_mention`](crate::services::notify::on_mention).
///
/// Every local user **subscribed** to the author — an accepted follow with
/// its `notify` toggle on ([`follow::subscribers`], Pleroma's
/// `User.subscriber_users` behind `get_notified_subscribers`) — is told
/// `status` naming the fresh post. The audience rules, per the ticket:
/// never a reblog row (its telling is the boost's
/// [`on_reblog`](crate::services::notify::on_reblog), and a boosted post
/// was already the original author's news), never a `direct` post (an
/// address is not a subscription's subject), and every other visibility as
/// its own audience allows — a `local` post reaches the local subscribers
/// this read returns, and remote subscribers, which it never does, are
/// federation's to tell.
pub async fn on_subscribed_status(state: &AppState, row: &StatusRow) {
    if let Err(err) = subscription_telling(state, row).await {
        warn!(
            status = %row.id,
            %err,
            "could not tell an author's subscribers about their status"
        );
    }
}

/// [`on_subscribed_status`]'s work: the two never-arms answered before any
/// read, then one [`tell`] per subscriber (whose own mention telling, from
/// [`on_mention`](crate::services::notify::on_mention), is a separate fact
/// and stands beside this one).
async fn subscription_telling(state: &AppState, row: &StatusRow) -> AppResult<()> {
    if row.reblog_of_id.is_some() || row.visibility == Visibility::Direct {
        return Ok(());
    }
    for subscriber in follow::subscribers(&state.db, row.user_id).await? {
        tell(state, &subscriber, row.user_id, TYPE_STATUS, Some(row.id)).await?;
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use sqlx::PgPool;

    use super::*;
    use crate::{
        models::{
            follow::FollowState,
            status::{self, NewStatus},
            user::User,
        },
        services::notify::test_support::{
            BASE, app_state, facts, followed, local, locked, post, remote, rows,
        },
    };

    /// A post of `author` at `visibility`.
    async fn post_vis(
        pool: &PgPool,
        author: &User,
        text: &str,
        visibility: Visibility,
    ) -> StatusRow {
        status::insert(
            pool,
            BASE,
            &NewStatus {
                visibility,
                ..NewStatus::local(author, text)
            },
        )
        .await
        .expect("the visibility status fixture inserts")
    }

    /// `subscriber` holds an accepted, notified follow of `author`.
    async fn subscribed(pool: &PgPool, subscriber: &User, author: &User) {
        let _open = followed(pool, subscriber, author).await;
        assert!(
            follow::set_notify(pool, subscriber.id, author.id, true)
                .await
                .expect("the toggle runs")
                .is_some(),
            "the accepted pair takes the flag"
        );
    }

    #[tokio::test]
    async fn subscribers_are_told_of_every_visible_visibility() {
        let db = crate::test_db::test_db().await.expect("test database slot");
        let pool = db.pool();
        let state = app_state(pool);
        let alice = local(pool, "su-alice").await;
        let bob = local(pool, "su-bob").await;
        let carol = local(pool, "su-carol").await;
        subscribed(pool, &bob, &alice).await;
        // Carol follows without the notify toggle: a plain follower.
        let _quiet = followed(pool, &carol, &alice).await;

        for (label, visibility) in [
            ("public", Visibility::Public),
            ("unlisted", Visibility::Unlisted),
            ("private", Visibility::Private),
            ("local", Visibility::Local),
        ] {
            let row = post_vis(pool, &alice, &format!("{label} news"), visibility).await;
            on_subscribed_status(&state, &row).await;
        }
        let told: Vec<String> = facts(pool, bob.id)
            .await
            .into_iter()
            .map(|found| found.0)
            .collect();
        assert_eq!(
            told,
            vec![TYPE_STATUS.to_owned(); 4],
            "the subscriber is told of every visibility but direct, newest first"
        );
        assert!(
            rows(pool, carol.id).await.is_empty(),
            "the unsubscribed follower is told nothing"
        );

        // A `direct` post is never a subscriber's news.
        let dm = post_vis(pool, &alice, "for addressees only", Visibility::Direct).await;
        on_subscribed_status(&state, &dm).await;
        assert_eq!(
            facts(pool, bob.id).await.len(),
            4,
            "the direct post grew no fifth telling"
        );
    }

    #[tokio::test]
    async fn a_reblog_row_and_a_pending_request_tell_no_subscribers() {
        let db = crate::test_db::test_db().await.expect("test database slot");
        let pool = db.pool();
        let state = app_state(pool);
        let alice = local(pool, "sr2-alice").await;
        let bob = local(pool, "sr2-bob").await;
        subscribed(pool, &bob, &alice).await;

        // Alice boosts a mirrored remote post: the reblog row is not a
        // subscriber's `status` news (the boost's own arm tells the
        // original's author instead).
        let mallory = remote(pool, "sr2-mallory", "remote.example").await;
        let theirs = post(pool, &mallory, "their post").await;
        let (boost, _) = status::create_reblog(pool, BASE, &alice, &theirs, Visibility::Public)
            .await
            .expect("the boost writes");
        on_subscribed_status(&state, &boost).await;
        assert!(
            rows(pool, bob.id).await.is_empty(),
            "a reblog row notifies no subscriptions"
        );

        // A pending request with its toggle on has subscribed nobody yet.
        let lara = locked(pool, "sr2-lara").await;
        let fred = local(pool, "sr2-fred").await;
        let request = followed(pool, &fred, &lara).await;
        assert_eq!(request.state, FollowState::Pending, "the fixture waits");
        let _toggled = follow::set_toggles(
            pool,
            fred.id,
            lara.id,
            follow::FollowToggles {
                show_reblogs: None,
                notify: Some(true),
            },
        )
        .await
        .expect("the toggles run");
        let news = post(pool, &lara, "locked news").await;
        on_subscribed_status(&state, &news).await;
        assert!(
            rows(pool, fred.id).await.is_empty(),
            "the waiting request carries no subscription yet"
        );
    }
}
