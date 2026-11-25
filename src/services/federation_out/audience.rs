//! The inboxes a local status' `Update` or `Delete` owes beyond its own
//! addressing: the remote accounts that voted on its poll and the remote
//! accounts that boosted it, none of whom need follow the author.
//!
//! Mastodon adds both to the recipients (`DistributePollUpdateWorker`,
//! `StatusUpdateDistributionWorker`), so a server that only met the post
//! through a boost, a search or a thread still hears the new tally, the
//! edit or the delete. The publisher collapses what this returns by inbox
//! URL, so voters and followers on one host cost one delivery whenever the
//! host publishes a shared inbox.

use std::collections::HashSet;

use serde_json::Value;
use tracing::{debug, warn};
use uuid::Uuid;

use crate::{
    error::AppResult,
    federation::{delivery::best_inbox, publisher},
    models::{
        block, domain_block,
        status::{remote_boosters, remote_voters},
        user::User,
    },
    state::AppState,
};

/// Which activity the extra recipients are computed for.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) enum Kind {
    /// An `Update`: the live boosts and the voters.
    Update,
    /// A `Delete`: the boosts the delete tombstoned count too.
    Delete,
}

/// The inboxes of the remote voters and boosters of `status_id`, as the
/// author `author` may reach them.
///
/// Voters and boosters the author blocks, and those on a domain the author
/// blocks, are left out. A read that fails is logged and answers what could
/// not be read as nobody: the hook's own write already stands.
pub(super) async fn extra_inboxes(
    state: &AppState,
    author: &User,
    status_id: Uuid,
    kind: Kind,
) -> Vec<String> {
    match try_extra_inboxes(state, author, status_id, kind).await {
        Ok(inboxes) => inboxes,
        Err(err) => {
            warn!(%err, status = %status_id, "could not read the voters and boosters to tell");
            Vec::new()
        }
    }
}

/// [`extra_inboxes`] with the database failures returned.
async fn try_extra_inboxes(
    state: &AppState,
    author: &User,
    status_id: Uuid,
    kind: Kind,
) -> AppResult<Vec<String>> {
    let mut accounts = remote_voters(&state.db, status_id).await?;
    accounts.extend(remote_boosters(&state.db, status_id, kind == Kind::Delete).await?);
    let blocked: HashSet<Uuid> = block::blocked_ids(&state.db, author.id)
        .await?
        .into_iter()
        .collect();
    let mut seen: HashSet<String> = HashSet::new();
    let mut inboxes = Vec::new();
    for account in accounts {
        if may_tell(state, author, &blocked, &account).await?
            && let Some(inbox) = best_inbox(&account)
            && seen.insert(inbox.clone())
        {
            inboxes.push(inbox);
        }
    }
    Ok(inboxes)
}

/// Whether `author` may tell `account`: not blocked by them, and not on a
/// domain they block. (`blocked` is the ids `author` blocks.)
async fn may_tell(
    state: &AppState,
    author: &User,
    blocked: &HashSet<Uuid>,
    account: &User,
) -> AppResult<bool> {
    if blocked.contains(&account.id) {
        debug!(account = %account.ap_id, "a blocked account is not told");
        return Ok(false);
    }
    let Some(domain) = account.domain.as_deref() else {
        return Ok(true);
    };
    let domain_blocked = domain_block::is_domain_blocked(&state.db, author.id, domain).await?;
    if domain_blocked {
        debug!(account = %account.ap_id, "an account on a blocked domain is not told");
    }
    Ok(!domain_blocked)
}

/// Queues `document` as `author`, adding the remote voters and boosters of
/// `status_id` to the recipients its own addressing names.
pub(super) async fn queue_with_audience(
    state: &AppState,
    author: &User,
    status_id: Uuid,
    document: &Value,
    kind: Kind,
) {
    let extra = extra_inboxes(state, author, status_id, kind).await;
    match publisher::publish_in_background(state, author.id, document, &extra, None).await {
        Ok(()) => debug!(actor = %author.ap_id, extra = extra.len(), "queued status federation"),
        Err(err) => warn!(%err, actor = %author.ap_id, "could not queue status federation"),
    }
}

#[cfg(test)]
mod tests {
    use sqlx::PgPool;

    use super::*;
    use crate::{
        config::TEST_BASE_URL,
        models::{
            poll::{self, PollEdit, PollParams},
            status::{self, Status, Visibility},
            user::{Flag, NewUser, insert},
        },
        services::federation_out::{
            after_status_deleted, after_status_edited,
            polls::handle_publish_update,
            test_support::{
                accepted_follow, app_state, delivered_inboxes, first_attempt, local, local_post,
                remote, run_publishes,
            },
        },
    };

    /// A remote mirror `nickname@peer.example` with a shared inbox: the
    /// second host the fan-out tests need beside `remote`'s.
    async fn peer(pool: &PgPool, nickname: &str) -> User {
        insert(
            pool,
            &NewUser {
                nickname: format!("{nickname}@peer.example"),
                ap_id: format!("https://peer.example/users/{nickname}"),
                local: Flag::new(false),
                domain: Some("peer.example".to_owned()),
                inbox: Some(format!("https://peer.example/users/{nickname}/inbox")),
                shared_inbox: Some("https://peer.example/inbox".to_owned()),
                public_key: Some("PUB".to_owned()),
                ..NewUser::default()
            },
        )
        .await
        .expect("the peer fixture inserts")
    }

    /// A public post of `author` carrying a two-option poll, and the
    /// options' ids.
    async fn poll_post(pool: &PgPool, author: &User) -> (Status, Vec<Uuid>) {
        let posted = local_post(pool, author, "tea or coffee?", Visibility::Public).await;
        let mut conn = pool.acquire().await.expect("a slot connection");
        let stand = poll::create(
            &mut conn,
            posted.id,
            &PollParams {
                options: vec!["tea".to_owned(), "coffee".to_owned()],
                ..PollParams::default()
            },
        )
        .await
        .expect("the poll creates");
        let ids = poll::options(pool, stand.id)
            .await
            .expect("the options read")
            .into_iter()
            .map(|option| option.id)
            .collect();
        (posted, ids)
    }

    /// Writes `voter`'s mark on `option` of the poll of `posted`.
    async fn vote(pool: &PgPool, posted: &Status, voter: &User, option: Uuid) {
        let stand = poll::get_for_status(pool, posted.id)
            .await
            .expect("the poll reads")
            .expect("the poll stands");
        let counted = poll::cast_vote(pool, stand.id, voter.id, &[option])
            .await
            .expect("the ballot writes");
        assert!(counted, "the fixture ballot counts");
    }

    /// Runs the poll's deferred `Update` handler for `posted`.
    async fn run_tally_update(state: &AppState, posted: &Status) {
        let outcome = handle_publish_update(
            state.clone(),
            first_attempt(),
            serde_json::json!({ "status_id": posted.id }),
        )
        .await;
        assert_eq!(outcome, Ok(()), "the update job runs");
    }

    /// The edit route's write of new options on the poll of `posted`.
    async fn reset_poll(pool: &PgPool, posted: &Status) {
        let mut conn = pool.acquire().await.expect("a slot connection");
        let standing = poll::standing(&mut conn, posted.id)
            .await
            .expect("the poll reads");
        let edit = PollEdit {
            options: vec!["x".to_owned(), "y".to_owned()],
            multiple: false,
            hide_totals: false,
            expires_at: None,
        };
        poll::apply_edit(&mut conn, posted.id, &edit, standing.as_ref())
            .await
            .expect("the edit applies");
    }

    /// A remote voter who does not follow the author gets the tally
    /// `Update`; followers and voters on one host cost one delivery.
    #[tokio::test]
    async fn a_voter_who_does_not_follow_gets_the_tally_update() {
        let db = crate::test_db::test_db().await.expect("test database slot");
        let pool = db.pool().clone();
        let alice = local(&pool, "alice").await;
        let carol = remote(&pool, "carol").await;
        let bob = remote(&pool, "bob").await;
        let dave = peer(&pool, "dave").await;
        accepted_follow(&pool, &carol, &alice).await;
        let (posted, options) = poll_post(&pool, &alice).await;
        vote(&pool, &posted, &bob, options[0]).await;
        vote(&pool, &posted, &dave, options[1]).await;
        let state = app_state(&pool);

        run_tally_update(&state, &posted).await;

        assert_eq!(
            delivered_inboxes(&pool).await,
            vec!["https://peer.example/inbox", "https://remote.example/inbox"],
            "the voter's host is reached, and the follower and voter on one host share a delivery"
        );
    }

    /// The `Update` of an edit that resets the poll still reaches the
    /// voters whose ballots the edit wiped, and forgets them afterwards.
    #[tokio::test]
    async fn an_edit_that_resets_the_poll_still_reaches_its_voters() {
        let db = crate::test_db::test_db().await.expect("test database slot");
        let pool = db.pool().clone();
        let alice = local(&pool, "alice").await;
        let dave = peer(&pool, "dave").await;
        let (posted, options) = poll_post(&pool, &alice).await;
        vote(&pool, &posted, &dave, options[0]).await;
        reset_poll(&pool, &posted).await;
        let state = app_state(&pool);

        after_status_edited(&state, &posted).await;
        run_publishes(&state, &pool).await;

        assert_eq!(
            delivered_inboxes(&pool).await,
            vec!["https://peer.example/inbox"],
            "the voter's server hears of the edit though the vote is gone"
        );
        let remembered: i64 = sqlx::query_scalar("SELECT count(*) FROM poll_departed_voters")
            .fetch_one(&pool)
            .await
            .expect("the count reads");
        assert_eq!(remembered, 0, "the voters are forgotten once told");
    }

    /// A remote booster who does not follow gets an edit and a delete,
    /// the delete even after the delete tombstoned the boost.
    #[tokio::test]
    async fn a_booster_who_does_not_follow_gets_the_edit_and_the_delete() {
        let db = crate::test_db::test_db().await.expect("test database slot");
        let pool = db.pool().clone();
        let alice = local(&pool, "alice").await;
        let dave = peer(&pool, "dave").await;
        let posted = local_post(&pool, &alice, "boost me", Visibility::Public).await;
        let (_boost, wrote) =
            status::create_reblog(&pool, TEST_BASE_URL, &dave, &posted, Visibility::Public)
                .await
                .expect("the reblog writes");
        assert!(wrote, "the boost is written");
        let state = app_state(&pool);

        after_status_edited(&state, &posted).await;
        run_publishes(&state, &pool).await;
        assert_eq!(
            delivered_inboxes(&pool).await,
            vec!["https://peer.example/inbox"],
            "the edit reaches the booster's server"
        );

        let (tombstone, _boosts) = status::soft_delete_with_reblogs(&pool, posted.id)
            .await
            .expect("the delete writes")
            .expect("the post was live");
        after_status_deleted(&state, &tombstone).await;
        run_publishes(&state, &pool).await;
        assert_eq!(
            delivered_inboxes(&pool).await.len(),
            2,
            "and so does the delete, a second delivery"
        );
    }

    /// Voters the author blocks, and voters on a domain the author blocks,
    /// are not told.
    #[tokio::test]
    async fn blocked_voters_and_blocked_domains_get_nothing() {
        let db = crate::test_db::test_db().await.expect("test database slot");
        let pool = db.pool().clone();
        let alice = local(&pool, "alice").await;
        let bob = remote(&pool, "bob").await;
        let dave = peer(&pool, "dave").await;
        let (posted, options) = poll_post(&pool, &alice).await;
        vote(&pool, &posted, &bob, options[0]).await;
        vote(&pool, &posted, &dave, options[1]).await;
        let blocked = block::block(&pool, TEST_BASE_URL, &alice, &bob)
            .await
            .expect("the block writes");
        assert_eq!(blocked.blocked_id, bob.id, "the block names the voter");
        domain_block::block_domain(&pool, alice.id, "peer.example")
            .await
            .expect("the domain block writes");
        let state = app_state(&pool);

        run_tally_update(&state, &posted).await;

        assert!(
            delivered_inboxes(&pool).await.is_empty(),
            "neither the blocked voter's nor the blocked domain's server hears anything"
        );
    }
}
