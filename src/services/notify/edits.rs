//! The edit telling of [`crate::services::notify`]: who hears about a
//! post's edit. The shared creation plumbing this telling calls lives in
//! the parent module.

use tracing::warn;

use crate::{
    error::AppResult,
    models::{
        notification::TYPE_UPDATE,
        status::{self, Status as StatusRow},
        user,
    },
    state::AppState,
};

use super::tell;

/// The edit route's notification, landed from `after_status_edited` in
/// [`crate::api::statuses`].
///
/// An edit tells every **local** account still boosting the post an
/// `update` naming it, with the author as the actor (Pleroma's `Update`
/// arm of `get_potential_receiver_ap_ids`: the object's `Announce` actors,
/// `where u.local`, minus the actor — `lib/pleroma/notification.ex`). A
/// withdrawn boost names nobody ([`status::local_reblogger_ids`] reads live
/// rows only), a remote booster is federation's to tell, and the author
/// boosting their own post is no one's news.
pub async fn on_status_edited(state: &AppState, row: &StatusRow) {
    if let Err(err) = edit_telling(state, row).await {
        warn!(
            status = %row.id,
            %err,
            "could not tell a post's boosters about its edit"
        );
    }
}

/// [`on_status_edited`]'s work, one [`tell`] per local booster.
async fn edit_telling(state: &AppState, row: &StatusRow) -> AppResult<()> {
    for booster_id in status::local_reblogger_ids(&state.db, row.id).await? {
        if let Some(booster) = user::get_by_id(&state.db, booster_id).await? {
            tell(state, &booster, row.user_id, TYPE_UPDATE, Some(row.id)).await?;
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{
        models::status::Visibility,
        services::notify::test_support::{BASE, app_state, facts, local, post, remote, rows},
    };

    #[tokio::test]
    async fn an_edit_tells_the_boosters_only() {
        let db = crate::test_db::test_db().await.expect("test database slot");
        let pool = db.pool();
        let state = app_state(pool);
        let alice = local(pool, "ed-alice").await;
        let bob = local(pool, "ed-bob").await;
        let carol = local(pool, "ed-carol").await;
        let mallory = remote(pool, "ed-mallory", "remote.example").await;

        let posted = post(pool, &alice, "worth boosting").await;
        let _bobs_boost = status::create_reblog(pool, BASE, &bob, &posted, Visibility::Public)
            .await
            .expect("bob's boost writes");
        let _carols_boost = status::create_reblog(pool, BASE, &carol, &posted, Visibility::Public)
            .await
            .expect("carol's boost starts");
        // Carol withdraws hers: a tombstoned boost names nobody.
        let _tombstone = status::remove_reblog(pool, carol.id, posted.id)
            .await
            .expect("the withdrawal runs");
        // A remote account's boost is federation's to tell.
        let _mirrored_boost =
            status::create_reblog(pool, BASE, &mallory, &posted, Visibility::Public)
                .await
                .expect("the mirrored boost writes");
        // And the author boosting her own post is her own news.
        let _own_boost = status::create_reblog(pool, BASE, &alice, &posted, Visibility::Public)
            .await
            .expect("the self boost writes");

        on_status_edited(&state, &posted).await;
        assert_eq!(
            facts(pool, bob.id).await,
            vec![(TYPE_UPDATE.to_owned(), alice.id, Some(posted.id))],
            "the one live local booster is told, naming the edited original"
        );
        for quiet in [&alice, &carol] {
            assert!(
                rows(pool, quiet.id).await.is_empty(),
                "{} (author or withdrawn booster) hears nothing",
                quiet.nickname
            );
        }
        assert!(
            rows(pool, mallory.id).await.is_empty(),
            "the remote booster is not told from here"
        );

        // A second edit — and a booster who took the boost back since —
        // reaches exactly the audience that stands.
        let _gone = status::remove_reblog(pool, bob.id, posted.id)
            .await
            .expect("bob withdraws after the first edit");
        on_status_edited(&state, &posted).await;
        assert_eq!(
            facts(pool, bob.id).await,
            vec![(TYPE_UPDATE.to_owned(), alice.id, Some(posted.id))],
            "the second edit finds no booster and grows no row"
        );
    }
}
