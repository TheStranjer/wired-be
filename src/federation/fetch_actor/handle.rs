//! [`confirm_nickname`]: keeping the handle a `WebFinger` answer gave a
//! split-domain actor when the actor is fetched (or refetched) by id.
//!
//! The parser derives `preferredUsername@<id host>`; an account whose actor
//! lives on `social.example.com` but whose handle is `alice@example.com`
//! would flip back to the derived one on every refresh. Pleroma's
//! `generate_nickname` asks `WebFinger` for the derived handle and adopts
//! the answer's `subject` when its self link is this very actor; so does
//! this module.

use tracing::debug;

use crate::{
    federation::{
        remote_actor::NewRemoteUser,
        webfinger_client::{
            Resolved, WebfingerError, resolve_acct_with_base, split_acct, strip_finger_prefixes,
        },
    },
    models::user::User,
    state::AppState,
};

use super::lookup::dev_webfinger_base;

/// Who settles a fetched actor's nickname before the mirror write.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum Naming {
    /// The fetch asks `WebFinger` itself, through [`confirm_nickname`]: the
    /// actor was reached by id, so no answer about its handle is at hand.
    Confirm,
    /// The caller just took the actor id from a `WebFinger` answer and
    /// settles the handle from that answer itself, so a second lookup
    /// would only repeat the question: [`keep_stored_nickname`] runs
    /// instead.
    Caller,
}

/// What asking `WebFinger` about an actor's derived handle came to.
#[derive(Debug, PartialEq, Eq)]
enum Verdict {
    /// The answer names this actor and carries this valid handle.
    Confirmed(String),
    /// The host answered, and the answer does not vouch for this actor
    /// under any handle (another actor, no subject, an invalid subject, or
    /// a subject on another domain that does not lead back here).
    Refuted,
    /// No answer was had: the lookup failed for transport, status or
    /// document reasons.
    Unavailable,
}

/// Settles `new_user.nickname` before the mirror write.
///
/// `new_user.nickname` arrives as the parser's `preferredUsername@<id
/// host>`. `stored` is the row already mirroring this actor, if any.
///
/// - A stored nickname equal to the derived one needs no question: the
///   lookup is skipped.
/// - Otherwise `WebFinger` is asked for the derived handle. A confirmed
///   answer replaces the nickname with the subject (the `domain` stays the
///   id host).
/// - Without a confirmation the derived nickname stands, except that a
///   lookup that merely *failed* leaves a stored nickname alone when its
///   local part is the actor's `preferredUsername`: one transient outage
///   must not undo a split-domain handle.
pub(super) async fn confirm_nickname(
    state: &AppState,
    stored: Option<&User>,
    new_user: &mut NewRemoteUser,
) {
    if stored.is_some_and(|row| row.nickname.eq_ignore_ascii_case(&new_user.nickname)) {
        return;
    }
    match ask_webfinger(state, &new_user.nickname, &new_user.ap_id).await {
        Verdict::Confirmed(handle) => new_user.nickname = handle,
        Verdict::Refuted => {}
        Verdict::Unavailable => keep_stored_nickname(stored, new_user),
    }
}

/// Settles `new_user.nickname` without asking `WebFinger`: a stored
/// nickname whose local part is the actor's `preferredUsername` is kept
/// (a split-domain handle survives the refetch), and otherwise the
/// parser's derived nickname stands.
pub(super) fn keep_stored_nickname(stored: Option<&User>, new_user: &mut NewRemoteUser) {
    if let Some(row) = stored
        && same_local_part(&row.nickname, &new_user.nickname)
    {
        new_user.nickname.clone_from(&row.nickname);
    }
}

/// Fingers `derived` and judges the answer for the actor `ap_id`.
///
/// The answer must point at `ap_id` itself and its subject must be a handle
/// [`split_acct`] accepts. A subject on a domain other than the one just
/// asked is a claim that domain never made, so it has to be vouched for by
/// that domain's own `WebFinger` leading back to `ap_id` — otherwise any
/// host could mirror its actors under another domain's handles.
async fn ask_webfinger(state: &AppState, derived: &str, ap_id: &str) -> Verdict {
    let resolved = match finger(state, derived).await {
        Ok(resolved) => resolved,
        Err(err) => return unavailable(derived, &err),
    };
    if resolved.actor != ap_id {
        return Verdict::Refuted;
    }
    let Some((local, domain)) = resolved
        .subject
        .as_deref()
        .and_then(|subject| split_acct(strip_finger_prefixes(subject)))
    else {
        return Verdict::Refuted;
    };
    let handle = format!("{local}@{domain}");
    if same_domain(&handle, derived) {
        return Verdict::Confirmed(handle);
    }
    match finger(state, &handle).await {
        Ok(vouched) if vouched.actor == ap_id => Verdict::Confirmed(handle),
        Ok(_) => Verdict::Refuted,
        Err(err) => unavailable(&handle, &err),
    }
}

/// One `WebFinger` lookup of `handle`, over the plain-HTTP development
/// origin a ported handle gets in development mode.
async fn finger(state: &AppState, handle: &str) -> Result<Resolved, WebfingerError> {
    let base = dev_webfinger_base(&state.config, handle);
    resolve_acct_with_base(&state.http_client, handle, base.as_deref()).await
}

/// Logs a failed lookup and answers [`Verdict::Unavailable`].
fn unavailable(handle: &str, err: &WebfingerError) -> Verdict {
    debug!(%err, handle, "webfinger could not confirm the actor's handle");
    Verdict::Unavailable
}

/// Whether two `local@domain` handles share their domain, case folded.
fn same_domain(left: &str, right: &str) -> bool {
    match (left.rsplit_once('@'), right.rsplit_once('@')) {
        (Some((_, left_domain)), Some((_, right_domain))) => {
            left_domain.eq_ignore_ascii_case(right_domain)
        }
        _ => false,
    }
}

/// Whether two `local@domain` handles share their local part, case folded.
fn same_local_part(left: &str, right: &str) -> bool {
    match (left.rsplit_once('@'), right.rsplit_once('@')) {
        (Some((left_local, _)), Some((right_local, _))) => {
            left_local.eq_ignore_ascii_case(right_local)
        }
        _ => false,
    }
}

#[cfg(test)]
mod tests {
    use std::sync::{
        Arc,
        atomic::{AtomicUsize, Ordering},
    };

    use axum::{Router, extract::State, http::StatusCode, routing::get};
    use serde_json::json;
    use tokio::net::TcpListener;

    use super::*;
    use crate::{
        federation::{
            fetch_actor::{get_or_fetch_by_ap_id, get_or_fetch_by_fingered_ap_id, test_support::*},
            remote_actor::{parse_actor, upsert_remote_user},
        },
        models::user::{get_by_nickname, set_nickname},
    };

    /// A JRD whose `subject` is `acct:{handle}` and whose self link is
    /// `href`.
    fn jrd(handle: &str, href: &str) -> String {
        json!({
            "subject": format!("acct:{handle}"),
            "links": [{
                "rel": "self",
                "type": "application/activity+json",
                "href": href,
            }],
        })
        .to_string()
    }

    /// Serves `jrd` at the `WebFinger` path of `listener`, plus the
    /// self-identifying alice actor when `actor_base` is given.
    fn serve(
        listener: TcpListener,
        jrd: &str,
        actor_base: Option<&str>,
    ) -> tokio::task::JoinHandle<()> {
        let mut app = Router::new().route(
            "/.well-known/webfinger",
            get(webfinger).with_state(jrd.to_owned()),
        );
        if let Some(base) = actor_base {
            app = app.merge(
                Router::new()
                    .route("/users/alice", get(self_actor))
                    .with_state(peer_at(base)),
            );
        }
        spawn(listener, app)
    }

    /// Counts the lookup and answers 404.
    async fn counting_not_found(State(counter): State<Arc<AtomicUsize>>) -> StatusCode {
        let _previous = counter.fetch_add(1, Ordering::SeqCst);
        StatusCode::NOT_FOUND
    }

    #[tokio::test]
    async fn a_subject_on_another_domain_that_leads_back_is_the_nickname() {
        let db = crate::test_db::test_db().await.expect("test database slot");
        let pool = db.pool().clone();
        let home = bind().await;
        let handle_host = bind().await;
        let base = origin(&home);
        let ap_id = format!("{base}/users/alice");
        let subject = format!("alice@{}", authority_of(&origin(&handle_host)));
        let answer = jrd(&subject, &ap_id);
        let actor_server = serve(home, &answer, Some(&base));
        let handle_server = serve(handle_host, &answer, None);
        let state = test_state(pool);

        let user = get_or_fetch_by_ap_id(&state, &ap_id).await.unwrap();

        actor_server.abort();
        handle_server.abort();
        assert_eq!(user.nickname, subject, "the confirmed handle is stored");
        assert_eq!(
            user.domain.as_deref(),
            Some(authority_of(&base)),
            "the domain stays the id host"
        );
        let found = get_by_nickname(&state.db, &subject).await.unwrap();
        assert_eq!(
            found.map(|row| row.id),
            Some(user.id),
            "the handle finds the mirror"
        );
    }

    #[tokio::test]
    async fn a_subject_the_other_domain_does_not_vouch_for_is_ignored() {
        let db = crate::test_db::test_db().await.expect("test database slot");
        let pool = db.pool().clone();
        let home = bind().await;
        let claimed_host = bind().await;
        let base = origin(&home);
        let ap_id = format!("{base}/users/alice");
        let subject = format!("alice@{}", authority_of(&origin(&claimed_host)));
        let actor_server = serve(home, &jrd(&subject, &ap_id), Some(&base));
        let claimed_server = spawn(claimed_host, Router::new());
        let state = test_state(pool);

        let user = get_or_fetch_by_ap_id(&state, &ap_id).await.unwrap();

        actor_server.abort();
        claimed_server.abort();
        assert_eq!(
            user.nickname,
            format!("alice@{}", authority_of(&base)),
            "a handle its own domain never confirmed is not adopted"
        );
    }

    #[tokio::test]
    async fn a_self_link_naming_another_actor_keeps_the_generated_nickname() {
        let db = crate::test_db::test_db().await.expect("test database slot");
        let pool = db.pool().clone();
        let listener = bind().await;
        let base = origin(&listener);
        let ap_id = format!("{base}/users/alice");
        let answer = jrd("alice@other.example", &format!("{base}/users/bob"));
        let server = serve(listener, &answer, Some(&base));
        let state = test_state(pool);

        let user = get_or_fetch_by_ap_id(&state, &ap_id).await.unwrap();

        server.abort();
        assert_eq!(
            user.nickname,
            format!("alice@{}", authority_of(&base)),
            "the answer is about somebody else"
        );
    }

    #[tokio::test]
    async fn a_failed_lookup_on_refresh_keeps_the_stored_split_nickname() {
        let db = crate::test_db::test_db().await.expect("test database slot");
        let pool = db.pool().clone();
        let listener = bind().await;
        let base = origin(&listener);
        let ap_id = format!("{base}/users/alice");
        let mirrored = upsert_remote_user(&pool, parse_actor(&actor_document(&ap_id)).unwrap())
            .await
            .unwrap();
        let _renamed = set_nickname(&pool, mirrored.id, "alice@split.example")
            .await
            .unwrap()
            .expect("the split nickname is free");
        make_stale(&pool, &ap_id).await;
        let peer = peer_at(&base);
        let hits = Arc::clone(&peer.hits);
        let server = spawn(
            listener,
            Router::new()
                .route("/users/alice", get(self_actor))
                .with_state(peer),
        );
        let state = test_state(pool);

        let refreshed = get_or_fetch_by_ap_id(&state, &ap_id).await.unwrap();

        server.abort();
        assert_eq!(hits.load(Ordering::SeqCst), 1, "the actor was refetched");
        assert_eq!(refreshed.id, mirrored.id, "the same row");
        assert_eq!(
            refreshed.nickname, "alice@split.example",
            "WebFinger's 404 does not undo the split handle"
        );
    }

    #[tokio::test]
    async fn a_refuting_answer_on_refresh_drops_the_stored_split_nickname() {
        let db = crate::test_db::test_db().await.expect("test database slot");
        let pool = db.pool().clone();
        let listener = bind().await;
        let base = origin(&listener);
        let ap_id = format!("{base}/users/alice");
        let mirrored = upsert_remote_user(&pool, parse_actor(&actor_document(&ap_id)).unwrap())
            .await
            .unwrap();
        let _renamed = set_nickname(&pool, mirrored.id, "alice@split.example")
            .await
            .unwrap()
            .expect("the split nickname is free");
        make_stale(&pool, &ap_id).await;
        let answer = jrd("alice@split.example", &format!("{base}/users/bob"));
        let server = serve(listener, &answer, Some(&base));
        let state = test_state(pool);

        let refreshed = get_or_fetch_by_ap_id(&state, &ap_id).await.unwrap();

        server.abort();
        assert_eq!(
            refreshed.nickname,
            format!("alice@{}", authority_of(&base)),
            "an answer that names another actor revokes the handle"
        );
    }

    #[tokio::test]
    async fn a_stored_generated_nickname_skips_webfinger() {
        let db = crate::test_db::test_db().await.expect("test database slot");
        let pool = db.pool().clone();
        let listener = bind().await;
        let base = origin(&listener);
        let ap_id = format!("{base}/users/alice");
        let mirrored = upsert_remote_user(&pool, parse_actor(&actor_document(&ap_id)).unwrap())
            .await
            .unwrap();
        make_stale(&pool, &ap_id).await;
        let lookups = Arc::new(AtomicUsize::new(0));
        let server = spawn(
            listener,
            Router::new()
                .route("/.well-known/webfinger", get(counting_not_found))
                .with_state(Arc::clone(&lookups))
                .merge(
                    Router::new()
                        .route("/users/alice", get(self_actor))
                        .with_state(peer_at(&base)),
                ),
        );
        let state = test_state(pool);

        let refreshed = get_or_fetch_by_ap_id(&state, &ap_id).await.unwrap();

        server.abort();
        assert_eq!(refreshed.nickname, mirrored.nickname, "nothing changed");
        assert_eq!(
            lookups.load(Ordering::SeqCst),
            0,
            "the derived nickname was already stored, so no lookup went out"
        );
    }

    /// Serves the self-identifying alice actor on `listener`, and a
    /// `WebFinger` path that counts each lookup into `lookups` and answers
    /// 404.
    fn serve_counting(
        listener: TcpListener,
        base: &str,
        lookups: &Arc<AtomicUsize>,
    ) -> tokio::task::JoinHandle<()> {
        spawn(
            listener,
            Router::new()
                .route("/.well-known/webfinger", get(counting_not_found))
                .with_state(Arc::clone(lookups))
                .merge(
                    Router::new()
                        .route("/users/alice", get(self_actor))
                        .with_state(peer_at(base)),
                ),
        )
    }

    #[tokio::test]
    async fn a_fingered_fetch_asks_webfinger_nothing_more() {
        let db = crate::test_db::test_db().await.expect("test database slot");
        let pool = db.pool().clone();
        let listener = bind().await;
        let base = origin(&listener);
        let ap_id = format!("{base}/users/alice");
        let lookups = Arc::new(AtomicUsize::new(0));
        let server = serve_counting(listener, &base, &lookups);
        let state = test_state(pool);

        let user = get_or_fetch_by_fingered_ap_id(&state, &ap_id)
            .await
            .unwrap();

        server.abort();
        assert_eq!(
            user.nickname,
            format!("alice@{}", authority_of(&base)),
            "the derived nickname, for the caller to settle"
        );
        assert_eq!(
            lookups.load(Ordering::SeqCst),
            0,
            "the caller's own answer was the one WebFinger lookup"
        );
    }

    #[tokio::test]
    async fn a_fingered_refresh_keeps_the_stored_split_nickname() {
        let db = crate::test_db::test_db().await.expect("test database slot");
        let pool = db.pool().clone();
        let listener = bind().await;
        let base = origin(&listener);
        let ap_id = format!("{base}/users/alice");
        let mirrored = upsert_remote_user(&pool, parse_actor(&actor_document(&ap_id)).unwrap())
            .await
            .unwrap();
        let _renamed = set_nickname(&pool, mirrored.id, "alice@split.example")
            .await
            .unwrap()
            .expect("the split nickname is free");
        make_stale(&pool, &ap_id).await;
        let lookups = Arc::new(AtomicUsize::new(0));
        let server = serve_counting(listener, &base, &lookups);
        let state = test_state(pool);

        let refreshed = get_or_fetch_by_fingered_ap_id(&state, &ap_id)
            .await
            .unwrap();

        server.abort();
        assert_eq!(refreshed.id, mirrored.id, "the same row");
        assert!(
            refreshed.last_fetched_at >= mirrored.last_fetched_at,
            "the stale (back-dated) mirror was refetched"
        );
        assert_eq!(
            refreshed.nickname, "alice@split.example",
            "the stored split handle survives the refetch"
        );
        assert_eq!(lookups.load(Ordering::SeqCst), 0, "and no lookup went out");
    }

    #[tokio::test]
    async fn a_fingered_refresh_drops_a_stored_nickname_of_another_local_part() {
        let db = crate::test_db::test_db().await.expect("test database slot");
        let pool = db.pool().clone();
        let listener = bind().await;
        let base = origin(&listener);
        let ap_id = format!("{base}/users/alice");
        let mirrored = upsert_remote_user(&pool, parse_actor(&actor_document(&ap_id)).unwrap())
            .await
            .unwrap();
        let _renamed = set_nickname(&pool, mirrored.id, "bob@split.example")
            .await
            .unwrap()
            .expect("the other nickname is free");
        make_stale(&pool, &ap_id).await;
        let lookups = Arc::new(AtomicUsize::new(0));
        let server = serve_counting(listener, &base, &lookups);
        let state = test_state(pool);

        let refreshed = get_or_fetch_by_fingered_ap_id(&state, &ap_id)
            .await
            .unwrap();

        server.abort();
        assert_eq!(
            refreshed.nickname,
            format!("alice@{}", authority_of(&base)),
            "a handle naming another local part is not this actor's"
        );
        assert_eq!(lookups.load(Ordering::SeqCst), 0, "no lookup went out");
    }

    #[test]
    fn handles_compare_by_part_case_insensitively() {
        assert!(same_local_part("Alice@a.example", "alice@b.example"));
        assert!(!same_local_part("bob@a.example", "alice@a.example"));
        assert!(!same_local_part("alice", "alice@a.example"));
        assert!(same_domain("a@Host.example", "b@host.example"));
        assert!(!same_domain("a@one.example", "a@two.example"));
        assert!(!same_domain("a", "a@two.example"));
    }
}
