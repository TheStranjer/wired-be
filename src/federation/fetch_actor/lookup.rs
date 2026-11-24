//! The get-or-fetch mirrors: lookups that consult the stored row first.
//!
//! [`get_or_fetch_by_ap_id`] (child module `by_ap_id`) resolves an
//! `ActivityPub` id and [`get_or_fetch_by_acct`] (child module
//! `by_acct`) an `acct` handle; [`refresh_user`] forces a refetch past
//! the window, and `fresh_since` and `is_local_ap_id` are the rules
//! they share.

mod by_acct;
mod by_ap_id;

use chrono::{DateTime, Utc};

use crate::{
    config::Config,
    models::user::{User, get_by_id},
    state::AppState,
    util::url::{is_local_url, parse_http_url},
};

use super::{
    FetchError, REFRESH_AFTER, error::server_error, fetch::fetch_and_store, handle::Naming,
};

pub use by_acct::get_or_fetch_by_acct;
pub use by_ap_id::{get_or_fetch_by_ap_id, get_or_fetch_by_fingered_ap_id};

pub(in crate::federation::fetch_actor) use by_acct::dev_webfinger_base;

/// Refetches `user`'s actor document and updates the mirror right away,
/// ignoring the 24-hour freshness window (Pleroma's
/// `UserRefreshWorker` performs the same forced refresh).
///
/// A local user is this instance's own actor: there is nothing to fetch,
/// so its row is simply re-read and returned as stored.
///
/// # Errors
///
/// Whatever [`fetch_actor_json`](super::fetch_actor_json) or the mirror write reports (no stale
/// fallback: this is the forced refetch), [`FetchError::NotFound`] when
/// the local row vanished.
pub async fn refresh_user(state: &AppState, user: &User) -> Result<User, FetchError> {
    if user.local.get() {
        return get_by_id(&state.db, user.id)
            .await
            .map_err(|err| server_error(&err))?
            .ok_or(FetchError::NotFound);
    }
    fetch_and_store(state, &user.ap_id, Naming::Confirm).await
}

/// Whether `stamp` falls inside the [`REFRESH_AFTER`] window; a mirror
/// that was never fetched is not fresh. A stamp in the future (peer or
/// clock skew) counts as fresh rather than triggering a fetch loop.
fn fresh_since(stamp: Option<DateTime<Utc>>) -> bool {
    stamp.is_some_and(|fetched| Utc::now().signed_duration_since(fetched) < REFRESH_AFTER)
}

/// Whether `ap_id` lives on this instance's own origin: ids here are
/// served from this database, never fetched from the network (a lookup
/// that misses is plain [`FetchError::NotFound`]). The comparison is on
/// the parsed URL authority, so a look-alike prefix host cannot pass.
/// Shared with the object resolver (see [`crate::federation::fetch_object`]),
/// which answers an unknown local object id without touching the network
/// too.
pub(crate) fn is_local_ap_id(config: &Config, ap_id: &str) -> bool {
    parse_http_url(ap_id).is_some_and(|url| is_local_url(config, &url))
}

#[cfg(test)]
mod tests {
    use std::sync::{Arc, atomic::Ordering};

    use axum::{Router, routing::get};
    use chrono::Duration;

    use super::*;
    use crate::{
        config::TEST_BASE_URL,
        federation::fetch_actor::test_support::*,
        models::user::{NewUser, insert, local_ap_id},
    };

    #[tokio::test]
    async fn refresh_user_refetches_despite_a_fresh_mirror() {
        let db = crate::test_db::test_db().await.expect("test database slot");
        let pool = db.pool().clone();
        let listener = bind().await;
        let base = origin(&listener);
        let ap_id = format!("{base}/users/alice");
        let peer = peer_at(&base);
        let hits = Arc::clone(&peer.hits);
        let server = spawn(
            listener,
            Router::new()
                .route("/users/alice", get(versioned_actor))
                .with_state(peer),
        );
        let state = test_state(pool);

        let first = get_or_fetch_by_ap_id(&state, &ap_id).await.unwrap();
        assert_eq!(hits.load(Ordering::SeqCst), 1, "the initial fetch");

        let refreshed = refresh_user(&state, &first).await.unwrap();

        assert_eq!(refreshed.id, first.id, "the same row is updated");
        assert_eq!(refreshed.name, "Second", "the forced refetch landed");
        assert_eq!(hits.load(Ordering::SeqCst), 2, "despite the fresh stamp");
        server.abort();
    }

    #[tokio::test]
    async fn refreshing_a_local_user_rereads_the_row() {
        let db = crate::test_db::test_db().await.expect("test database slot");
        let pool = db.pool().clone();
        let alice = insert(
            &pool,
            &NewUser::local("alice", &local_ap_id(TEST_BASE_URL, "alice")),
        )
        .await
        .unwrap();
        let state = test_state(pool);

        let refreshed = refresh_user(&state, &alice).await.unwrap();
        assert_eq!(refreshed.id, alice.id, "a local actor is never fetched");

        sqlx::query("DELETE FROM users WHERE id = $1")
            .bind(alice.id)
            .execute(&state.db)
            .await
            .map(drop)
            .unwrap();
        let error = refresh_user(&state, &alice).await.unwrap_err();
        assert!(
            matches!(error, FetchError::NotFound),
            "a vanished local row reports NotFound: {error:?}"
        );
    }

    #[test]
    fn freshness_is_the_24_hour_window_on_the_fetch_stamp() {
        let at = |hours_ago: i64| Utc::now() - Duration::hours(hours_ago);
        assert!(!fresh_since(None), "never fetched is not fresh");
        assert!(fresh_since(Some(at(23))), "inside the window");
        assert!(!fresh_since(Some(at(25))), "past the window");
        assert!(
            fresh_since(Some(at(-1))),
            "a stamp from the future (clock skew) counts as fresh"
        );
    }

    #[test]
    fn only_this_origin_counts_as_local() {
        let config = Config::for_tests();
        assert!(
            is_local_ap_id(&config, "https://test.example/users/alice"),
            "this instance's actor path"
        );
        assert!(
            is_local_ap_id(&config, "https://TEST.EXAMPLE/activities/x"),
            "origins fold case"
        );
        assert!(
            !is_local_ap_id(&config, "https://test.example.evil/users/alice"),
            "a look-alike prefix host is somebody else"
        );
        assert!(
            !is_local_ap_id(&config, "http://other.example/users/alice"),
            "another origin is another instance"
        );
        assert!(!is_local_ap_id(&config, "not a url"), "junk is junk");
    }
}
