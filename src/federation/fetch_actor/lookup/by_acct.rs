//! The lookup by `acct` handle: [`get_or_fetch_by_acct`] through the
//! `WebFinger` resolver, [`get_or_fetch_by_acct_with_base`]'s origin
//! override, [`dev_webfinger_base`]'s development-mode base and the
//! [`acct_matches`] vouch that closes the round trip.

use crate::{
    config::Config,
    federation::{
        fetch_actor::{FetchError, error::server_error, handle::Naming},
        mrf,
        remote_actor::ActorError,
        webfinger_client::{Resolved, resolve_acct_with_base, strip_finger_prefixes},
    },
    models::user::{User, get_by_acct, set_nickname},
    state::AppState,
};

use super::{by_ap_id::serve_or_refresh, get_or_fetch_by_fingered_ap_id};

/// Looks an account up by `acct` handle, resolving it through `WebFinger`
/// and fetching the actor when this instance does not know it yet.
///
/// The stored nickname comes first — a local account, and any remote
/// mirror this instance already keeps under `nick@domain` (decision 7),
/// answers from the database, case-insensitively, with a handle
/// carrying this instance's own domain folded to the bare local nickname
/// (see [`get_by_acct`]). A local account and a mirror
/// fetched within 24 hours answer with no network at all; an older mirror
/// is refetched inline through the lookup by id's refresh policy (as
/// Mastodon's `ResolveAccountService` and Pleroma's `needs_update?` do),
/// and a failed refresh serves the stored row, `Gone` excepted. Otherwise the [`WebFinger`] resolver finds the
/// actor URL and [`get_or_fetch_by_fingered_ap_id`] mirrors it, without a
/// second `WebFinger` lookup of its own. The fetched actor
/// must then vouch for the handle: its `preferredUsername@host` has to
/// equal `acct` case-insensitively (the Mastodon round trip), or the
/// `WebFinger` subject has to name `acct` while the fetched actor is the
/// one the answer pointed at (a split-domain account, whose mirror is then
/// renamed to `acct`), or the lookup reports [`FetchError::AcctMismatch`].
///
/// [`WebFinger`]: crate::federation::webfinger_client
///
/// # Errors
///
/// [`FetchError::Webfinger`] when the handle cannot be resolved,
/// [`FetchError::AcctMismatch`] when the resolved actor denies it, and
/// otherwise whatever [`get_or_fetch_by_fingered_ap_id`] reports.
pub async fn get_or_fetch_by_acct(state: &AppState, acct: &str) -> Result<User, FetchError> {
    get_or_fetch_by_acct_with_base(state, acct, None).await
}

/// [`get_or_fetch_by_acct`] with the `WebFinger` origin replaced by
/// `webfinger_base` (`scheme://authority`) when given. `WebFinger` is
/// HTTPS-only by RFC 7033 and resolves against the handle's domain, so
/// only [`resolve_acct_with_base`]'s test override can point it at a
/// loopback mock; production passes [`None`] and this override keeps the
/// account tests able to drive the same full round trip. The one exception
/// the production path itself makes is [`dev_webfinger_base`]'s: a handle
/// whose domain names an explicit port is fingered over plain `http://`
/// while the instance runs in the development mode
/// [`FetchSettings::http_allow_private_hosts`](crate::config::FetchSettings::http_allow_private_hosts)
/// selects.
async fn get_or_fetch_by_acct_with_base(
    state: &AppState,
    acct: &str,
    webfinger_base: Option<&str>,
) -> Result<User, FetchError> {
    if let Some(stored) = get_by_acct(&state.db, &state.config, acct)
        .await
        .map_err(|err| server_error(&err))?
    {
        // The same freshness policy as the lookup by id: a local row and a
        // mirror inside the refresh window answer from the database with no
        // network; an older mirror is refetched inline (this is the path of
        // `resolve=true` searches, follows by handle and mention parsing,
        // and a stale mirror is rare and refreshed once per day), keeping
        // the stored row when the refresh fails. `Naming::Caller` keeps a
        // split-domain handle's stored nickname without a second
        // `WebFinger` query.
        let ap_id = stored.ap_id.clone();
        return serve_or_refresh(state, stored, &ap_id, Naming::Caller).await;
    }
    if let Some((_, domain)) = strip_finger_prefixes(acct).rsplit_once('@') {
        mrf::refuse_rejected(state, &format!("https://{domain}"))
            .await
            .map_err(FetchError::Invalid)?;
    }
    let dev_base = dev_webfinger_base(&state.config, acct);
    let base = webfinger_base.or(dev_base.as_deref());
    let resolved = resolve_acct_with_base(&state.http_client, acct, base).await?;
    let user = get_or_fetch_by_fingered_ap_id(state, &resolved.actor).await?;
    if acct_matches(&user.nickname, acct) {
        return Ok(user);
    }
    if !subject_vouches(&resolved, &user, acct) {
        return Err(FetchError::AcctMismatch);
    }
    let nickname = strip_finger_prefixes(acct);
    set_nickname(&state.db, user.id, nickname)
        .await
        .map_err(|err| server_error(&err))?
        .ok_or_else(|| FetchError::from(ActorError::NicknameConflict))
}

/// The split-domain vouch: the `WebFinger` answer's `subject` names the
/// requested handle (case-insensitively, `acct:` stripped) and the fetched
/// actor is the very one that answer pointed at, so the handle belongs to
/// that actor even though its id lives on another host. A subject naming a
/// different handle, or none, vouches for nothing.
fn subject_vouches(resolved: &Resolved, user: &User, acct: &str) -> bool {
    resolved
        .subject
        .as_deref()
        .is_some_and(|subject| acct_matches(strip_finger_prefixes(subject), acct))
        && user.ap_id == resolved.actor
}

/// The development-mode `WebFinger` origin for `acct`: while
/// [`FetchSettings::http_allow_private_hosts`](crate::config::FetchSettings::http_allow_private_hosts)
/// is on — the mode
/// `HTTP_ALLOW_PRIVATE_HOSTS` selects, documented as development and tests
/// only, and the same switch that disables the SSRF guard — a handle whose
/// domain carries an explicit `:port` is fingered at `http://` that
/// authority, so a federation peer running on loopback (or any host on a
/// non-standard port) is resolvable end to end. Every other handle keeps
/// the spec's `https://{domain}` base; production, with the flag off,
/// never returns one.
pub(in crate::federation::fetch_actor) fn dev_webfinger_base(
    config: &Config,
    acct: &str,
) -> Option<String> {
    if !config.fetch.http_allow_private_hosts {
        return None;
    }
    let domain = strip_finger_prefixes(acct).rsplit_once('@')?.1;
    domain.contains(':').then(|| format!("http://{domain}"))
}

/// The Mastodon round trip in handle terms: a mirror's nickname *is*
/// `{preferredUsername}@{host of the actor id}` (decision 7), so its
/// equality with the requested `acct` — case folded, `@`/`acct:` prefixes
/// stripped — is what proves the resolved actor vouches for the same
/// account the resolver was asked about.
fn acct_matches(nickname: &str, acct: &str) -> bool {
    nickname.eq_ignore_ascii_case(strip_finger_prefixes(acct))
}

#[cfg(test)]
mod tests {
    use std::sync::{Arc, atomic::Ordering};

    use axum::{Router, routing::get};
    use serde_json::{Value, json};

    use super::{super::fresh_since, *};
    use crate::{
        config::TEST_BASE_URL,
        federation::{fetch_actor::test_support::*, webfinger_client::WebfingerError},
        models::{
            activitypub_actor,
            user::{NewUser, get_by_nickname, insert, local_ap_id},
        },
    };

    /// Mirrors alice from the mock by her `acct`, then hands back the
    /// state, the running mock, its base and the first lookup's result.
    async fn mirror_alice(db: &crate::test_db::TestDb) -> (AppState, ServedAlice, String, User) {
        let listener = bind().await;
        let base = origin(&listener);
        let acct = format!("alice@{}", authority_of(&base));
        let jrd = jrd_for(&format!("acct:{acct}"), &base);
        let served = serve_alice(listener, &base, &jrd);
        let state = test_state(db.pool().clone());
        let first = get_or_fetch_by_acct_with_base(&state, &acct, Some(&base))
            .await
            .expect("the first lookup mirrors alice");
        assert_eq!(first.name, "First", "the first document");
        (state, served, base, first)
    }

    #[tokio::test]
    async fn a_fresh_mirror_answers_by_acct_without_the_network() {
        let db = crate::test_db::test_db().await.expect("test database slot");
        let (state, served, base, first) = mirror_alice(&db).await;
        let acct = format!("ALICE@{}", authority_of(&base).to_uppercase());

        let found = get_or_fetch_by_acct_with_base(&state, &acct, Some(&base))
            .await
            .unwrap();

        served.server.abort();
        assert_eq!(found.id, first.id, "the stored row, however spelled");
        assert_eq!(found.name, "First", "the database answer");
        assert_eq!(served.hits.load(Ordering::SeqCst), 1, "no second fetch");
    }

    #[tokio::test]
    async fn a_stale_mirror_is_refetched_by_acct() {
        let db = crate::test_db::test_db().await.expect("test database slot");
        let (state, served, base, first) = mirror_alice(&db).await;
        make_stale(&state.db, &first.ap_id).await;
        let acct = format!("alice@{}", authority_of(&base));

        let found = get_or_fetch_by_acct_with_base(&state, &acct, Some(&base))
            .await
            .unwrap();

        served.server.abort();
        assert_eq!(found.id, first.id, "the same row is updated");
        assert_eq!(found.name, "Second", "the refetch landed");
        assert_eq!(served.hits.load(Ordering::SeqCst), 2, "fetched again");
        let stamp = activitypub_actor::get(&state.db, first.id)
            .await
            .unwrap()
            .and_then(|row| row.last_fetched_at);
        assert!(fresh_since(stamp), "the refetch restamped the mirror");
    }

    #[tokio::test]
    async fn a_failed_refetch_by_acct_serves_the_stored_mirror() {
        let db = crate::test_db::test_db().await.expect("test database slot");
        let (state, served, base, first) = mirror_alice(&db).await;
        make_stale(&state.db, &first.ap_id).await;
        served.server.abort();
        drop(served.server.await);
        let acct = format!("alice@{}", authority_of(&base));

        let found = get_or_fetch_by_acct_with_base(&state, &acct, Some(&base))
            .await
            .expect("the peer is down, the stored row answers");

        assert_eq!(found.id, first.id, "the stored mirror");
        assert_eq!(found.name, "First", "as it was");
    }

    /// Answers alice's document once, then 410 Gone.
    async fn alice_then_gone(
        axum::extract::State(peer): axum::extract::State<Peer>,
    ) -> axum::response::Response {
        use axum::response::IntoResponse as _;
        if peer.hits.load(Ordering::SeqCst) == 0 {
            return self_actor(axum::extract::State(peer)).await;
        }
        axum::http::StatusCode::GONE.into_response()
    }

    #[tokio::test]
    async fn a_gone_refetch_by_acct_keeps_the_deletion_handling() {
        let db = crate::test_db::test_db().await.expect("test database slot");
        let listener = bind().await;
        let base = origin(&listener);
        let acct = format!("alice@{}", authority_of(&base));
        let peer = peer_at(&base);
        let server = spawn(
            listener,
            Router::new()
                .route(
                    "/.well-known/webfinger",
                    get(webfinger).with_state(jrd_for(&format!("acct:{acct}"), &base)),
                )
                .merge(
                    Router::new()
                        .route("/users/alice", get(alice_then_gone))
                        .with_state(peer),
                ),
        );
        let state = test_state(db.pool().clone());
        let first = get_or_fetch_by_acct_with_base(&state, &acct, Some(&base))
            .await
            .unwrap();
        make_stale(&state.db, &first.ap_id).await;

        let error = get_or_fetch_by_acct_with_base(&state, &acct, Some(&base))
            .await
            .unwrap_err();

        server.abort();
        assert!(
            matches!(error, FetchError::Gone),
            "410 is not swallowed: {error:?}"
        );
    }

    #[tokio::test]
    async fn a_refetch_by_acct_keeps_the_local_deactivation() {
        let db = crate::test_db::test_db().await.expect("test database slot");
        let (state, served, base, first) = mirror_alice(&db).await;
        sqlx::query(
            "UPDATE users SET is_active = false, local_deactivated = true,
                 remote_suspended = true WHERE id = $1",
        )
        .bind(first.id)
        .execute(&state.db)
        .await
        .map(drop)
        .unwrap();
        make_stale(&state.db, &first.ap_id).await;
        let acct = format!("alice@{}", authority_of(&base));

        let found = get_or_fetch_by_acct_with_base(&state, &acct, Some(&base))
            .await
            .unwrap();

        served.server.abort();
        assert_eq!(found.name, "Second", "the mirror was refetched");
        let flags: (bool, bool, bool) = sqlx::query_as(
            "SELECT is_active, local_deactivated, remote_suspended FROM users WHERE id = $1",
        )
        .bind(first.id)
        .fetch_one(&state.db)
        .await
        .unwrap();
        assert_eq!(
            flags,
            (false, true, false),
            "the origin lifted its suspension, the local deactivation stays"
        );
    }

    #[tokio::test]
    async fn a_local_handle_never_touches_the_network() {
        let db = crate::test_db::test_db().await.expect("test database slot");
        let pool = db.pool().clone();
        let alice = insert(
            &pool,
            &NewUser::local("alice", &local_ap_id(TEST_BASE_URL, "alice")),
        )
        .await
        .unwrap();
        let state = test_state(pool);
        let own = format!("alice@{}", state.config.domain());

        for acct in ["alice", own.as_str(), "ALICE"] {
            let found = get_or_fetch_by_acct(&state, acct).await.unwrap();
            assert_eq!(found.id, alice.id, "the local row answers ({acct})");
        }
    }

    #[tokio::test]
    async fn an_actor_that_denies_the_requested_handle_is_rejected() {
        let db = crate::test_db::test_db().await.expect("test database slot");
        let pool = db.pool().clone();
        let listener = bind().await;
        let base = origin(&listener);
        let peer = peer_at(&base);
        let hits = Arc::clone(&peer.hits);
        let jrd = json!({
            "subject": "acct:bob@mock.example",
            "links": [{
                "rel": "self",
                "type": "application/activity+json",
                "href": format!("{base}/users/alice"),
            }],
        });
        let app = Router::new()
            .route(
                "/.well-known/webfinger",
                get(webfinger).with_state(jrd.to_string()),
            )
            .merge(
                Router::new()
                    .route("/users/alice", get(self_actor))
                    .with_state(peer),
            );
        let server = spawn(listener, app);
        let state = test_state(pool);

        let error = get_or_fetch_by_acct_with_base(&state, "alice@mock.example", Some(&base))
            .await
            .unwrap_err();

        assert!(
            matches!(error, FetchError::AcctMismatch),
            "the actor's own handle is alice@127.0.0.1:port, not the asked one: {error:?}"
        );
        assert_eq!(
            hits.load(Ordering::SeqCst),
            1,
            "the actor URL came out of the webfinger answer and was fetched once"
        );
        let mirror = get_by_nickname(&state.db, &format!("alice@{}", authority_of(&base)))
            .await
            .unwrap()
            .expect("the fetched actor is mirrored under its own handle");
        assert_eq!(mirror.ap_id, format!("{base}/users/alice"), "by its id");
        assert!(
            get_by_nickname(&state.db, "alice@mock.example")
                .await
                .unwrap()
                .is_none(),
            "the claimed handle was never stored"
        );
        server.abort();
    }

    /// Serves `jrd` at the `WebFinger` path and the self-identifying alice
    /// actor, on `listener`; the task is the caller's to abort.
    fn serve_alice(listener: tokio::net::TcpListener, base: &str, jrd: &str) -> ServedAlice {
        let peer = peer_at(base);
        let hits = Arc::clone(&peer.hits);
        let app = Router::new()
            .route(
                "/.well-known/webfinger",
                get(webfinger).with_state(jrd.to_owned()),
            )
            .merge(
                Router::new()
                    .route("/users/alice", get(versioned_actor))
                    .with_state(peer),
            );
        ServedAlice {
            server: spawn(listener, app),
            hits,
        }
    }

    /// The running mock of [`serve_alice`] and its actor-fetch counter.
    struct ServedAlice {
        server: tokio::task::JoinHandle<()>,
        hits: Arc<std::sync::atomic::AtomicUsize>,
    }

    /// A JRD whose subject is `subject` and whose self link is alice's actor
    /// at `base`.
    fn jrd_for(subject: &str, base: &str) -> String {
        json!({
            "subject": subject,
            "links": [{
                "rel": "self",
                "type": "application/activity+json",
                "href": format!("{base}/users/alice"),
            }],
        })
        .to_string()
    }

    #[tokio::test]
    async fn a_split_domain_subject_vouches_and_renames_the_mirror() {
        let db = crate::test_db::test_db().await.expect("test database slot");
        let pool = db.pool().clone();
        let listener = bind().await;
        let base = origin(&listener);
        let served = serve_alice(listener, &base, &jrd_for("acct:Alice@mock.example", &base));
        let state = test_state(pool);

        let found = get_or_fetch_by_acct_with_base(&state, "@alice@mock.example", Some(&base))
            .await
            .expect("the subject names the handle and the actor is the one linked");

        served.server.abort();
        assert_eq!(found.nickname, "alice@mock.example", "the handle sticks");
        assert_eq!(found.ap_id, format!("{base}/users/alice"), "by its id");
        let stored = get_by_nickname(&state.db, "alice@mock.example")
            .await
            .unwrap()
            .expect("stored under the handle");
        assert_eq!(stored.id, found.id, "one row, renamed");
        assert!(
            get_by_nickname(&state.db, &format!("alice@{}", authority_of(&base)))
                .await
                .unwrap()
                .is_none(),
            "the host-derived nickname was replaced"
        );
        let again = get_or_fetch_by_acct_with_base(&state, "alice@mock.example", Some(&base))
            .await
            .unwrap();
        assert_eq!(again.id, found.id, "the next lookup answers from storage");
        assert_eq!(served.hits.load(Ordering::SeqCst), 1, "fetched once");
    }

    #[tokio::test]
    async fn a_subject_naming_another_handle_is_a_mismatch() {
        let db = crate::test_db::test_db().await.expect("test database slot");
        let pool = db.pool().clone();
        let listener = bind().await;
        let base = origin(&listener);
        let served = serve_alice(listener, &base, &jrd_for("acct:bob@mock.example", &base));
        let state = test_state(pool);

        let error = get_or_fetch_by_acct_with_base(&state, "alice@mock.example", Some(&base))
            .await
            .unwrap_err();

        served.server.abort();
        assert!(
            matches!(error, FetchError::AcctMismatch),
            "the subject is bob, not the asked alice: {error:?}"
        );
        assert!(
            get_by_nickname(&state.db, "alice@mock.example")
                .await
                .unwrap()
                .is_none(),
            "the claimed handle was never stored"
        );
    }

    /// Counts the `WebFinger` query and answers 404.
    async fn counted_not_found(
        axum::extract::State(hits): axum::extract::State<Arc<std::sync::atomic::AtomicUsize>>,
    ) -> axum::http::StatusCode {
        let _previous = hits.fetch_add(1, Ordering::SeqCst);
        axum::http::StatusCode::NOT_FOUND
    }

    /// Fingers `alice@{mock authority}` with the `simple` policy configured
    /// by `lists` (no MRF entry for `None`): the lookup's error and how
    /// many `WebFinger` queries the mock saw.
    async fn finger_under(lists: Option<&Value>) -> (FetchError, usize) {
        let db = crate::test_db::test_db().await.expect("test database slot");
        let pool = db.pool().clone();
        if let Some(lists) = lists {
            write_simple_lists(&pool, lists).await;
        }
        let listener = bind().await;
        let base = origin(&listener);
        let hits = Arc::new(std::sync::atomic::AtomicUsize::new(0));
        let server = spawn(
            listener,
            Router::new()
                .route("/.well-known/webfinger", get(counted_not_found))
                .with_state(Arc::clone(&hits)),
        );
        let state = test_state(pool);
        let acct = format!("alice@{}", authority_of(&base));

        let error = get_or_fetch_by_acct_with_base(&state, &acct, Some(&base))
            .await
            .unwrap_err();

        server.abort();
        (error, hits.load(Ordering::SeqCst))
    }

    /// Stores the runtime MRF entry enabling `simple` with `lists`.
    async fn write_simple_lists(pool: &sqlx::PgPool, lists: &Value) {
        crate::services::runtime_config::write(
            pool,
            ":pleroma",
            ":mrf",
            &json!({"policies": ["simple"], "simple": lists}),
        )
        .await
        .expect("the mrf entry writes");
    }

    /// A handle on a domain the `simple` policy refuses — by its `reject`
    /// list or by an allowlist that omits it — is not fingered at all.
    #[tokio::test]
    async fn a_handle_of_a_rejected_domain_is_not_fingered() {
        for lists in [
            json!({"reject": ["127.0.0.1"]}),
            json!({"accept": ["friend.example"]}),
        ] {
            let (error, hits) = finger_under(Some(&lists)).await;

            assert!(
                matches!(&error, FetchError::Invalid(detail) if detail.contains("127.0.0.1")),
                "the refusal names the host ({lists}): {error:?}"
            );
            assert_eq!(hits, 0, "no WebFinger query left ({lists})");
        }
    }

    /// With no MRF entry the same handle is fingered: the mock's 404 is
    /// the answer, and its counter proves the query was sent.
    #[tokio::test]
    async fn a_handle_with_no_mrf_entry_is_fingered() {
        let (error, hits) = finger_under(None).await;

        assert!(
            matches!(error, FetchError::Webfinger(_)),
            "the resolver ran and its 404 answers: {error:?}"
        );
        assert!(hits > 0, "the query reached the mock");
    }

    #[tokio::test]
    async fn an_unresolvable_handle_reports_the_webfinger_error() {
        let db = crate::test_db::test_db().await.expect("test database slot");
        let pool = db.pool().clone();
        let state = test_state(pool);

        let error = get_or_fetch_by_acct(&state, "alice@localhost")
            .await
            .unwrap_err();

        assert!(
            matches!(error, FetchError::Webfinger(WebfingerError::InvalidAcct)),
            "a handle the resolver refuses fails before any request: {error:?}"
        );
    }

    #[test]
    fn the_dev_base_serves_a_ported_handle_over_http_only_in_dev_mode() {
        let mut dev = Config::for_tests();
        assert!(
            dev.fetch.http_allow_private_hosts,
            "the test config is the development mode"
        );
        assert_eq!(
            dev_webfinger_base(&dev, "alice@127.0.0.1:4000").as_deref(),
            Some("http://127.0.0.1:4000"),
            "a handle naming an explicit port is fingered at that authority"
        );
        assert_eq!(
            dev_webfinger_base(&dev, "@alice@Remote.Example:8080").as_deref(),
            Some("http://Remote.Example:8080"),
            "prefixes fold; the domain part is taken verbatim"
        );
        assert_eq!(
            dev_webfinger_base(&dev, "alice@remote.example"),
            None,
            "a portless handle keeps the spec's https base even in dev mode"
        );
        assert_eq!(
            dev_webfinger_base(&dev, "alice"),
            None,
            "a bare nickname has no domain to build a base from"
        );
        dev.fetch.http_allow_private_hosts = false;
        assert_eq!(
            dev_webfinger_base(&dev, "alice@127.0.0.1:4000"),
            None,
            "with the guard on (production) no http base is ever offered"
        );
    }

    #[tokio::test]
    async fn a_ported_handle_resolves_through_the_dev_base_without_an_override() {
        // The production `get_or_fetch_by_acct` arm: no `webfinger_base`
        // override at all. The dev-mode config fingers the loopback mock at
        // its own `http://{host:port}` origin, the actor mirrors under the
        // handle it vouches for, and the vouch passes because the mirror
        // nickname carries the port like the requested acct does.
        let db = crate::test_db::test_db().await.expect("test database slot");
        let pool = db.pool().clone();
        let listener = bind().await;
        let base = origin(&listener);
        let acct = format!("alice@{}", authority_of(&base));
        let jrd = json!({
            "subject": format!("acct:{acct}"),
            "links": [{
                "rel": "self",
                "type": "application/activity+json",
                "href": format!("{base}/users/alice"),
            }],
        });
        let peer = peer_at(&base);
        let app = Router::new()
            .route(
                "/.well-known/webfinger",
                get(webfinger).with_state(jrd.to_string()),
            )
            .merge(
                Router::new()
                    .route("/users/alice", get(self_actor))
                    .with_state(peer),
            );
        let server = spawn(listener, app);
        let state = test_state(pool);

        let found = get_or_fetch_by_acct(&state, &acct)
            .await
            .expect("the ported handle resolves in dev mode");

        server.abort();
        assert_eq!(found.nickname, acct, "mirrored under the asked handle");
        assert_eq!(found.ap_id, format!("{base}/users/alice"), "by its id");
    }

    #[test]
    fn the_round_trip_compares_the_handle_case_insensitively() {
        assert!(acct_matches("alice@remote.example", "alice@remote.example"));
        assert!(acct_matches("Alice@Remote.Example", "ALICE@remote.example"));
        assert!(acct_matches(
            "alice@remote.example",
            "@alice@REMOTE.example"
        ));
        assert!(acct_matches(
            "alice@remote.example",
            "acct:alice@remote.example"
        ));
        assert!(!acct_matches("bob@remote.example", "alice@remote.example"));
        assert!(!acct_matches("alice@other.example", "alice@remote.example"));
    }
}
