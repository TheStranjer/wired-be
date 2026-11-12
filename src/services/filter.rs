//! Applying a user's v1 phrase filters to already-rendered status pages.
//!
//! [`apply_filters`] is the one post-processing step every filtered
//! listing shares (`docs/ARCHITECTURE.md` decision 3: this is more than
//! any single table owns and less than an endpoint). It takes rendered
//! [`Status`] entities and the viewer's active [`Filter`] rows, and for
//! every status tests each filter that names the listing's
//! [`FilterContext`] against the status's plain text — [`strip_html`] of
//! the content, the spoiler text and every media description, of the
//! status *and* of its reblogged original. A matching `irreversible`
//! filter removes the status from the page; any other match keeps it and
//! sets its `filtered` array to one [`FilterResult`] per matched filter
//! ([`FilterResult::warn`]'s v2 shape). Statuses authored by the viewer
//! are never filtered, and with them their nested originals, which is
//! also what leaves an account owner's own profile listing in the
//! `account` context untouched.
//!
//! The step is pure: no database, no request. Callers load the viewer's
//! filters with [`models::filter::list_active`] — [`apply_active`] is the
//! one-read-and-apply shortcut for the status pages — and an
//! unauthenticated request has no filters to read, so its page comes back
//! exactly as it went in.
//!
//! Reference: Pleroma removes irreversible matches inside its query —
//! `restrict_filtered` over the composed word-boundary regex
//! (`lib/pleroma/filter.ex`,
//! `lib/pleroma/web/activity_pub/activity_pub.ex`) — and exempts the
//! viewer's own rows the same way (`activity.actor == user.ap_id`); the
//! `filtered` annotation itself follows the ticket, since Pleroma 2.10 has
//! no v2 `FilterResult` to point at.
//!
//! [`Status`]: crate::entities::status::Status
//! [`Filter`]: crate::models::filter::Filter
//! [`models::filter::list_active`]: crate::models::filter::list_active
//! [`FilterResult::warn`]: crate::entities::filter::FilterResult::warn

use chrono::Utc;
use uuid::Uuid;

use crate::{
    entities::{filter::FilterResult, status::Status as StatusEntity},
    error::AppResult,
    models::{
        filter::{self, Filter},
        user::User,
    },
    state::AppState,
    util::sanitize::strip_html,
};

/// The five Mastodon v1 filter contexts: where a filter applies. Every
/// filtered listing names one; the spellings are exactly the ones
/// [`crate::api::filters`] validates into the `context` column.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum FilterContext {
    /// The viewer's following timeline (`GET /api/v1/timelines/home`).
    Home,
    /// The public and hashtag timelines (`GET /api/v1/timelines/public`,
    /// `GET /api/v1/timelines/tag/{tag}`).
    Public,
    /// The notification listing (`GET /api/v1/notifications`).
    Notifications,
    /// A status's thread (`GET /api/v1/statuses/{id}/context`).
    Thread,
    /// One account's status listing (`GET /api/v1/accounts/{id}/statuses`).
    Account,
}

impl FilterContext {
    /// The stored name this context is called by inside a filter's
    /// `context` array.
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Home => "home",
            Self::Public => "public",
            Self::Notifications => "notifications",
            Self::Thread => "thread",
            Self::Account => "account",
        }
    }
}

/// Reads `viewer`'s active filters and post-processes their rendered
/// status page for `context` through [`apply_filters`].
///
/// An anonymous viewer has no filters to read, and their page comes back
/// untouched without a query. Every filtered *status page* endpoint shares
/// this one call.
///
/// # Errors
///
/// Any database failure behind
/// [`list_active`](crate::models::filter::list_active) arrives through
/// `?` as [`AppError::Database`](crate::error::AppError).
pub async fn apply_active(
    state: &AppState,
    viewer: Option<&User>,
    statuses: Vec<StatusEntity>,
    context: FilterContext,
) -> AppResult<Vec<StatusEntity>> {
    let Some(viewer) = viewer else {
        return Ok(statuses);
    };
    let filters = filter::list_active(&state.db, viewer.id).await?;
    Ok(apply_filters(statuses, &filters, context, Some(viewer.id)))
}

/// Post-processes an already-rendered page: returns the statuses that
/// survive `filters` in `context`, each carrying the `filtered` annotation
/// of the warning filters that matched it.
///
/// The rules, per the ticket:
///
/// - only filters whose `context` array names `context` and whose
///   [`expires_at`](Filter::expires_at) has not passed apply —
///   [`list_active`](crate::models::filter::list_active) already keeps
///   expired rows out of a caller's list, and the expiry test here makes
///   the step correct for any list it is handed;
/// - a status the viewer authored is never filtered: it keeps its place,
///   its `filtered` array untouched, and nothing of it (or of its nested
///   original) is tested; for the `account` context this is what leaves
///   the account owner's own profile listing unfiltered, because every
///   status on it is theirs;
/// - a status whose text — or whose reblogged original's text — matches
///   an `irreversible` filter leaves the page entirely;
/// - every other match keeps the status and sets its `filtered` array to
///   one [`FilterResult::warn`] per matching filter, in the order the
///   filters came in; nothing matched keeps the empty array.
///
/// With nothing to apply — no filters, or none active in this context —
/// the page is returned untouched, so an unauthenticated (or unfiltered)
/// request is the no-op the ticket asks for.
#[must_use]
pub fn apply_filters(
    mut statuses: Vec<StatusEntity>,
    filters: &[Filter],
    context: FilterContext,
    viewer_id: Option<Uuid>,
) -> Vec<StatusEntity> {
    let scoped: Vec<&Filter> = filters
        .iter()
        .filter(|filter| applies_in(filter, context))
        .collect();
    if scoped.is_empty() {
        return statuses;
    }
    statuses.retain_mut(|status| apply_to_status(status, &scoped, viewer_id));
    statuses
}

/// Whether `filter` governs `context` right now: it lists that context
/// among its own and is not past its expiry.
fn applies_in(filter: &Filter, context: FilterContext) -> bool {
    let live = filter.expires_at.is_none_or(|at| at > Utc::now());
    live && filter.context.iter().any(|name| name == context.as_str())
}

/// Applies the scoped `filters` to the one rendered `status`, reporting
/// whether it survives the page. See [`apply_filters`] for the rules.
fn apply_to_status(status: &mut StatusEntity, scoped: &[&Filter], viewer_id: Option<Uuid>) -> bool {
    if viewer_id.is_some_and(|id| status.account.id == id.to_string()) {
        return true;
    }
    let text = filter_text(status);
    let mut results: Vec<FilterResult> = Vec::new();
    for filter in scoped {
        if !filter::matches(filter, &text) {
            continue;
        }
        if filter.irreversible {
            return false;
        }
        results.push(FilterResult::warn(filter));
    }
    status.filtered = results;
    true
}

/// The plain text a phrase is tested against: [`strip_html`] of the
/// content, the spoiler text and every media description, and — for a
/// reblog, whose own content is empty — the same again for the nested
/// original. The parts are newline-joined, so no phrase can read across
/// one of the boundaries between them.
fn filter_text(status: &StatusEntity) -> String {
    let mut text = own_text(status);
    if let Some(original) = status.reblog.as_deref() {
        text.push('\n');
        text.push_str(&own_text(original));
    }
    text
}

/// The entity's own plain text: stripped content, spoiler and media
/// descriptions, newline-joined.
fn own_text(status: &StatusEntity) -> String {
    let mut text = strip_html(&status.content);
    text.push('\n');
    text.push_str(&status.spoiler_text);
    for attachment in &status.media_attachments {
        if let Some(description) = &attachment.description {
            text.push('\n');
            text.push_str(description);
        }
    }
    text
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;

    use chrono::{DateTime, Duration};
    use serde_json::{Value, json};
    use sqlx::PgPool;

    use super::*;
    use crate::{
        config::{Config, TEST_BASE_URL},
        http_client::HttpClient,
        mailer::MemoryTransport,
        models::{
            filter::NewFilter,
            media_attachment::{self, NewMediaAttachment},
            status::{self, NewStatus, Status as StatusRow, Visibility},
            user::{self, NewUser, local_ap_id},
        },
        services::render_status::render_statuses,
        uploads::UploaderBackend,
    };

    /// The instance origin the fixtures post under.
    const BASE: &str = TEST_BASE_URL;

    /// Every filter context beside its stored name — the scoping loops'
    /// shared fixture.
    const ALL_CONTEXTS: [(&str, FilterContext); 5] = [
        ("home", FilterContext::Home),
        ("public", FilterContext::Public),
        ("notifications", FilterContext::Notifications),
        ("thread", FilterContext::Thread),
        ("account", FilterContext::Account),
    ];

    /// The state under test: `pool` and the test config, with the
    /// collaborators the renderer behind these fixtures never touches.
    fn state(pool: &PgPool) -> AppState {
        let config = Arc::new(Config::for_tests());
        AppState {
            db: pool.clone(),
            hub: crate::streaming::hub::Hub::new(pool),
            config: Arc::clone(&config),
            http_client: HttpClient::from(config.as_ref()),
            uploader: UploaderBackend::from(config.as_ref()),
            instance_stats_cache: Arc::default(),
            mrf_config_cache: Arc::default(),
            translation_languages_cache: Arc::default(),
            rate_limiter: Arc::new(crate::rate_limit::RateLimiter::from_config(config.as_ref())),
            mailer: Arc::new(MemoryTransport::new()),
        }
    }

    /// Inserts a local user named `nickname`.
    async fn person(pool: &PgPool, nickname: &str) -> User {
        user::insert(
            pool,
            &NewUser::local(nickname, &local_ap_id(BASE, nickname)),
        )
        .await
        .expect("the fixture user inserts")
    }

    /// Inserts `text` as `author`'s local public status.
    async fn post(pool: &PgPool, author: &User, text: &str) -> StatusRow {
        status::insert(pool, BASE, &NewStatus::local(author, text))
            .await
            .expect("a fixture post inserts")
    }

    /// Inserts `text` as `author`'s local public status under the content
    /// warning `spoiler`, with one attachment described `description`.
    async fn warned_post(
        pool: &PgPool,
        author: &User,
        text: &str,
        spoiler: &str,
        description: &str,
    ) -> StatusRow {
        let row = status::insert(
            pool,
            BASE,
            &NewStatus {
                spoiler_text: spoiler.to_owned(),
                ..NewStatus::local(author, text)
            },
        )
        .await
        .expect("a fixture post inserts");
        let _attached: media_attachment::MediaAttachment = media_attachment::insert(
            pool,
            &NewMediaAttachment {
                user_id: Some(author.id),
                status_id: Some(row.id),
                url: format!("{BASE}/media/fixture.png"),
                content_type: "image/png".to_owned(),
                description: Some(description.to_owned()),
                ..NewMediaAttachment::default()
            },
        )
        .await
        .expect("the fixture attachment inserts");
        row
    }

    /// `reblogger`'s public reblog of `original`.
    async fn reblog(pool: &PgPool, reblogger: &User, original: &StatusRow) -> StatusRow {
        status::create_reblog(pool, BASE, reblogger, original, Visibility::Public)
            .await
            .expect("a fixture reblog writes")
            .0
    }

    /// Renders `rows` for `viewer` and applies `filters` in `context` as
    /// that viewer's page — the pair every assertion below starts from.
    async fn apply(
        test_state: &AppState,
        viewer: Option<&User>,
        rows: &[StatusRow],
        filters: &[Filter],
        context: FilterContext,
    ) -> Vec<StatusEntity> {
        let page = render_statuses(test_state, viewer, rows)
            .await
            .expect("the fixture page renders");
        apply_filters(page, filters, context, viewer.map(|user| user.id))
    }

    /// An instant `days` from now, negative ones for the past.
    fn offset(days: i64) -> DateTime<Utc> {
        Utc::now() + Duration::days(days)
    }

    /// A stored whole-word warning filter: `phrase` applying in `context`.
    fn warn(phrase: &str, context: &[&str]) -> Filter {
        Filter {
            id: Uuid::now_v7(),
            user_id: Uuid::now_v7(),
            phrase: phrase.to_owned(),
            context: context.iter().map(|name| (*name).to_owned()).collect(),
            whole_word: true,
            irreversible: false,
            expires_at: None,
            inserted_at: Utc::now(),
            updated_at: Utc::now(),
        }
    }

    /// The same filter as an `irreversible` one, hiding its matches.
    fn hidden(phrase: &str, context: &[&str]) -> Filter {
        Filter {
            irreversible: true,
            ..warn(phrase, context)
        }
    }

    /// The same filter with whole-word off: a plain substring search.
    fn loose(phrase: &str, context: &[&str]) -> Filter {
        Filter {
            whole_word: false,
            ..warn(phrase, context)
        }
    }

    /// The same filter expiring at `at`.
    fn expiring(filter: Filter, at: DateTime<Utc>) -> Filter {
        Filter {
            expires_at: Some(at),
            ..filter
        }
    }

    /// The ids of the rendered `statuses`, in order.
    fn ids(statuses: &[StatusEntity]) -> Vec<String> {
        statuses.iter().map(|status| status.id.clone()).collect()
    }

    /// The rendered `filtered` array of the entity with id `id`, as JSON.
    fn filtered(statuses: &[StatusEntity], id: &str) -> Value {
        let status = statuses
            .iter()
            .find(|candidate| candidate.id == id)
            .unwrap_or_else(|| panic!("`{id}` is on the page"));
        serde_json::to_value(status.filtered.clone()).expect("results serialize")
    }

    #[test]
    fn contexts_spell_their_stored_names() {
        assert_eq!(FilterContext::Home.as_str(), "home");
        assert_eq!(FilterContext::Public.as_str(), "public");
        assert_eq!(FilterContext::Notifications.as_str(), "notifications");
        assert_eq!(FilterContext::Thread.as_str(), "thread");
        assert_eq!(FilterContext::Account.as_str(), "account");
    }

    #[tokio::test]
    async fn a_warn_match_annotates_the_status_with_the_v2_result() {
        let db = crate::test_db::test_db().await.expect("test database slot");
        let pool = db.pool().clone();
        let test_state = state(&pool);
        let alice = person(&pool, "alice").await;
        let bob = person(&pool, "bob").await;
        let clean = post(&pool, &bob, "a nice day").await;
        let dirty = post(&pool, &bob, "read all about the spam today").await;
        let filter = warn("spam", &["home"]);
        let filter_id = filter.id.to_string();

        let kept = apply(
            &test_state,
            Some(&alice),
            &[clean.clone(), dirty.clone()],
            std::slice::from_ref(&filter),
            FilterContext::Home,
        )
        .await;

        assert_eq!(
            ids(&kept),
            vec![clean.id.to_string(), dirty.id.to_string()],
            "a warning keeps every status on the page"
        );
        assert_eq!(
            filtered(&kept, &clean.id.to_string()),
            json!([]),
            "the non-matching status keeps the empty array"
        );
        assert_eq!(
            filtered(&kept, &dirty.id.to_string()),
            json!([{
                "filter": {
                    "id": filter_id,
                    "title": "spam",
                    "context": ["home"],
                    "expires_at": Value::Null,
                    "filter_action": "warn",
                    "keywords": [{ "id": filter_id, "keyword": "spam", "whole_word": true }],
                    "statuses": [],
                },
                "keyword_matches": ["spam"],
                "status_matches": Value::Null,
            }]),
            "the matched status carries exactly the ticket's shape"
        );
    }

    #[tokio::test]
    async fn whole_word_needs_boundaries_substring_does_not() {
        let db = crate::test_db::test_db().await.expect("test database slot");
        let pool = db.pool().clone();
        let test_state = state(&pool);
        let alice = person(&pool, "alice").await;
        let carol = person(&pool, "carol").await;
        let foodie = post(&pool, &carol, "loved the foood").await;
        let standalone = post(&pool, &carol, "a foo bar").await;
        let rows = [foodie.clone(), standalone.clone()];

        let exact = warn("foo", &["home"]);
        let strict = apply(
            &test_state,
            Some(&alice),
            &rows,
            std::slice::from_ref(&exact),
            FilterContext::Home,
        )
        .await;
        assert_eq!(
            filtered(&strict, &foodie.id.to_string()),
            json!([]),
            "a glued occurrence is no whole word"
        );
        assert_eq!(
            filtered(&strict, &standalone.id.to_string())
                .as_array()
                .expect("an array")
                .len(),
            1,
            "the bounded occurrence is"
        );

        let substring = loose("foo", &["home"]);
        let broad = apply(
            &test_state,
            Some(&alice),
            &rows,
            std::slice::from_ref(&substring),
            FilterContext::Home,
        )
        .await;
        assert_ne!(
            filtered(&broad, &foodie.id.to_string()),
            json!([]),
            "whole-word off turns the glued occurrence into a substring match"
        );
    }

    #[tokio::test]
    async fn a_filter_annotates_every_context_it_names() {
        let db = crate::test_db::test_db().await.expect("test database slot");
        let pool = db.pool().clone();
        let test_state = state(&pool);
        let alice = person(&pool, "alice").await;
        let bob = person(&pool, "bob").await;
        let row = post(&pool, &bob, "the spam word").await;
        let row_id = row.id.to_string();

        for (name, context) in ALL_CONTEXTS {
            let scoped = warn("spam", &[name]);
            let annotated = apply(
                &test_state,
                Some(&alice),
                std::slice::from_ref(&row),
                std::slice::from_ref(&scoped),
                context,
            )
            .await;
            assert_eq!(
                ids(&annotated),
                vec![row_id.clone()],
                "a `{name}` filter never removes what it warns over"
            );
            assert_ne!(
                filtered(&annotated, &row_id),
                json!([]),
                "and it annotates in the `{name}` context"
            );
        }
    }

    #[tokio::test]
    async fn a_filter_ignores_the_contexts_it_does_not_name() {
        let db = crate::test_db::test_db().await.expect("test database slot");
        let pool = db.pool().clone();
        let test_state = state(&pool);
        let alice = person(&pool, "alice").await;
        let bob = person(&pool, "bob").await;
        let row = post(&pool, &bob, "the spam word").await;
        let row_id = row.id.to_string();

        for (name, context) in ALL_CONTEXTS.into_iter().filter(|(name, _)| *name != "home") {
            let home_only = hidden("spam", &["home"]);
            let untouched = apply(
                &test_state,
                Some(&alice),
                std::slice::from_ref(&row),
                std::slice::from_ref(&home_only),
                context,
            )
            .await;
            assert_eq!(
                ids(&untouched),
                vec![row_id.clone()],
                "a hidden `{name}`-context read keeps a home-only match"
            );
            assert_eq!(
                filtered(&untouched, &row_id),
                json!([]),
                "and the pass leaves its annotation empty"
            );
        }
    }

    #[tokio::test]
    async fn irreversible_removes_and_warn_annotates_side_by_side() {
        let db = crate::test_db::test_db().await.expect("test database slot");
        let pool = db.pool().clone();
        let test_state = state(&pool);
        let alice = person(&pool, "alice").await;
        let bob = person(&pool, "bob").await;
        let clean = post(&pool, &bob, "quiet").await;
        let warned = post(&pool, &bob, "a gentle spam").await;
        let doomed = post(&pool, &bob, "the hidden spam").await;
        let filters = vec![warn("gentle", &["home"]), hidden("hidden", &["home"])];

        let kept = apply(
            &test_state,
            Some(&alice),
            &[clean.clone(), warned.clone(), doomed.clone()],
            &filters,
            FilterContext::Home,
        )
        .await;
        assert_eq!(
            ids(&kept),
            vec![clean.id.to_string(), warned.id.to_string()],
            "the irreversible match leaves the page, the warn match stays, \
             and an unmatched status was never in question"
        );
        assert_eq!(
            filtered(&kept, &warned.id.to_string())
                .as_array()
                .expect("an array")
                .len(),
            1,
            "and it is annotated once"
        );
    }

    #[tokio::test]
    async fn an_irreversible_match_wins_over_a_warn_match() {
        // One status matched by both kinds: removal wins — no filtered
        // array is rendered for a status that is not there.
        let db = crate::test_db::test_db().await.expect("test database slot");
        let pool = db.pool().clone();
        let test_state = state(&pool);
        let alice = person(&pool, "alice").await;
        let bob = person(&pool, "bob").await;
        let row = post(&pool, &bob, "spam spam").await;
        let filters = vec![warn("spam", &["home"]), hidden("spam", &["home"])];

        let kept = apply(
            &test_state,
            Some(&alice),
            std::slice::from_ref(&row),
            &filters,
            FilterContext::Home,
        )
        .await;
        assert!(kept.is_empty(), "the irreversible match removed the status");
    }

    #[tokio::test]
    async fn an_expired_filter_hides_and_annotates_nothing() {
        let db = crate::test_db::test_db().await.expect("test database slot");
        let pool = db.pool().clone();
        let test_state = state(&pool);
        let alice = person(&pool, "alice").await;
        let bob = person(&pool, "bob").await;
        let row = post(&pool, &bob, "the spam word").await;
        let gone = expiring(hidden("spam", &["home"]), offset(-1));

        let kept = apply(
            &test_state,
            Some(&alice),
            std::slice::from_ref(&row),
            std::slice::from_ref(&gone),
            FilterContext::Home,
        )
        .await;
        assert_eq!(
            ids(&kept),
            vec![row.id.to_string()],
            "the expired row hides nothing"
        );
        assert_eq!(
            filtered(&kept, &row.id.to_string()),
            json!([]),
            "and an all-expired pass leaves the annotation empty"
        );
    }

    #[tokio::test]
    async fn a_filter_that_has_not_expired_yet_still_applies() {
        let db = crate::test_db::test_db().await.expect("test database slot");
        let pool = db.pool().clone();
        let test_state = state(&pool);
        let alice = person(&pool, "alice").await;
        let bob = person(&pool, "bob").await;
        let row = post(&pool, &bob, "the spam word").await;
        let soon = expiring(warn("spam", &["home"]), offset(1));

        let kept = apply(
            &test_state,
            Some(&alice),
            std::slice::from_ref(&row),
            std::slice::from_ref(&soon),
            FilterContext::Home,
        )
        .await;
        assert_ne!(
            filtered(&kept, &row.id.to_string()),
            json!([]),
            "the future expiry has not passed and the filter still applies"
        );
    }

    #[tokio::test]
    async fn a_reblog_matches_on_its_originals_text() {
        let db = crate::test_db::test_db().await.expect("test database slot");
        let pool = db.pool().clone();
        let test_state = state(&pool);
        let alice = person(&pool, "alice").await;
        let bob = person(&pool, "bob").await;
        let carol = person(&pool, "carol").await;
        let original = post(&pool, &bob, "the spam word").await;
        let boost = reblog(&pool, &carol, &original).await;
        let boost_id = boost.id.to_string();
        let rows = std::slice::from_ref(&boost).to_vec();

        let warned = apply(
            &test_state,
            Some(&alice),
            &rows,
            std::slice::from_ref(&warn("spam", &["home"])),
            FilterContext::Home,
        )
        .await;
        assert_eq!(
            ids(&warned),
            vec![boost_id.clone()],
            "the warn keeps the boost"
        );
        assert_eq!(
            filtered(&warned, &boost_id)
                .as_array()
                .expect("an array")
                .len(),
            1,
            "and annotates the rendered status whose original matched"
        );

        let removed = apply(
            &test_state,
            Some(&alice),
            &rows,
            std::slice::from_ref(&hidden("spam", &["home"])),
            FilterContext::Home,
        )
        .await;
        assert!(
            removed.is_empty(),
            "the irreversible match takes the boost with it"
        );
    }

    #[tokio::test]
    async fn the_viewers_own_statuses_are_never_filtered() {
        let db = crate::test_db::test_db().await.expect("test database slot");
        let pool = db.pool().clone();
        let test_state = state(&pool);
        let alice = person(&pool, "alice").await;
        let bob = person(&pool, "bob").await;
        let own = post(&pool, &alice, "my own spam").await;
        let theirs = post(&pool, &bob, "their spam").await;
        let hide = hidden("spam", &["home"]);

        let kept = apply(
            &test_state,
            Some(&alice),
            &[own.clone(), theirs.clone()],
            std::slice::from_ref(&hide),
            FilterContext::Home,
        )
        .await;
        assert_eq!(
            ids(&kept),
            vec![own.id.to_string()],
            "the viewer's own status stays even when its text matches; \
             the stranger's goes"
        );
        assert_eq!(
            filtered(&kept, &own.id.to_string()),
            json!([]),
            "and it stays unannotated — an exempt status is never touched"
        );
    }

    #[tokio::test]
    async fn an_own_reblog_shields_its_matched_original() {
        // The exemption reads the rendered status: the viewer's own boost
        // of a matched post stays whole, while somebody else's boost of
        // the same post — the same text on the wire — is filtered.
        let db = crate::test_db::test_db().await.expect("test database slot");
        let pool = db.pool().clone();
        let test_state = state(&pool);
        let alice = person(&pool, "alice").await;
        let bob = person(&pool, "bob").await;
        let carol = person(&pool, "carol").await;
        let matched = post(&pool, &bob, "the spam word").await;
        let alice_boost = reblog(&pool, &alice, &matched).await;
        let carol_boost = reblog(&pool, &carol, &matched).await;
        let hide = hidden("spam", &["home"]);

        let kept = apply(
            &test_state,
            Some(&alice),
            &[alice_boost.clone(), carol_boost.clone()],
            std::slice::from_ref(&hide),
            FilterContext::Home,
        )
        .await;
        assert_eq!(
            ids(&kept),
            vec![alice_boost.id.to_string()],
            "alice's own boost stands, carol's identical one is gone"
        );
    }

    #[tokio::test]
    async fn spoiler_text_and_media_descriptions_are_tested() {
        let db = crate::test_db::test_db().await.expect("test database slot");
        let pool = db.pool().clone();
        let test_state = state(&pool);
        let alice = person(&pool, "alice").await;
        let bob = person(&pool, "bob").await;
        let warned = warned_post(&pool, &bob, "a caption", "the spam warning", "a clean alt").await;
        let alted = warned_post(
            &pool,
            &bob,
            "a caption",
            "a clean warning",
            "spam in the alt",
        )
        .await;
        let clean = warned_post(&pool, &bob, "a caption", "a clean warning", "a clean alt").await;
        let hide = hidden("spam", &["public"]);

        let kept = apply(
            &test_state,
            Some(&alice),
            &[warned.clone(), alted.clone(), clean.clone()],
            std::slice::from_ref(&hide),
            FilterContext::Public,
        )
        .await;
        assert_eq!(
            ids(&kept),
            vec![clean.id.to_string()],
            "a phrase in the spoiler or in a media description is as good a \
             match as one in the body; the clean status rides on"
        );
    }

    #[tokio::test]
    async fn a_body_phrase_does_not_read_across_a_field_boundary() {
        // The parts are newline-joined: a two-word phrase cannot match the
        // end of one field and the start of the next.
        let db = crate::test_db::test_db().await.expect("test database slot");
        let pool = db.pool().clone();
        let test_state = state(&pool);
        let alice = person(&pool, "alice").await;
        let bob = person(&pool, "bob").await;
        let split = warned_post(&pool, &bob, "reads spam", "today and more", "clean").await;
        let straddle = hidden("spam today", &["public"]);

        let kept = apply(
            &test_state,
            Some(&alice),
            std::slice::from_ref(&split),
            std::slice::from_ref(&straddle),
            FilterContext::Public,
        )
        .await;
        assert_eq!(
            ids(&kept),
            vec![split.id.to_string()],
            "the phrase straddles the content/spoiler boundary and matches nothing"
        );
    }

    #[tokio::test]
    async fn every_warn_match_is_annotated_in_the_filters_order() {
        let db = crate::test_db::test_db().await.expect("test database slot");
        let pool = db.pool().clone();
        let test_state = state(&pool);
        let alice = person(&pool, "alice").await;
        let bob = person(&pool, "bob").await;
        let row = post(&pool, &bob, "alpha and beta and gamma").await;
        let row_id = row.id.to_string();
        let filters = vec![
            warn("beta", &["home"]),
            warn("delta", &["home"]),
            warn("alpha", &["home"]),
        ];

        let kept = apply(
            &test_state,
            Some(&alice),
            std::slice::from_ref(&row),
            &filters,
            FilterContext::Home,
        )
        .await;
        let entries = filtered(&kept, &row_id);
        let titles: Vec<&str> = entries
            .as_array()
            .expect("an array")
            .iter()
            .map(|entry| entry["filter"]["title"].as_str().expect("a title"))
            .collect();
        assert_eq!(
            titles,
            ["beta", "alpha"],
            "both warn matches annotate, in the order the filters came in, \
             the non-match never enters the array"
        );
    }

    #[tokio::test]
    async fn a_page_with_nothing_to_apply_comes_back_untouched() {
        let db = crate::test_db::test_db().await.expect("test database slot");
        let pool = db.pool().clone();
        let test_state = state(&pool);
        let alice = person(&pool, "alice").await;
        let bob = person(&pool, "bob").await;
        let row = post(&pool, &bob, "the spam word").await;
        let empty: Vec<Filter> = Vec::new();
        let other_context = vec![warn("spam", &["public"])];
        let expired = vec![expiring(hidden("spam", &["home"]), offset(-2))];

        for filters in [&empty, &other_context, &expired] {
            let kept = apply(
                &test_state,
                Some(&alice),
                std::slice::from_ref(&row),
                filters,
                FilterContext::Home,
            )
            .await;
            assert_eq!(ids(&kept), vec![row.id.to_string()], "the status stays");
            assert_eq!(
                filtered(&kept, &row.id.to_string()),
                json!([]),
                "and a nothing page carries the empty filtered array"
            );
        }
    }

    #[tokio::test]
    async fn apply_active_reads_the_viewers_own_active_filters() {
        let db = crate::test_db::test_db().await.expect("test database slot");
        let pool = db.pool().clone();
        let test_state = state(&pool);
        let alice = person(&pool, "alice").await;
        let bob = person(&pool, "bob").await;
        let doomed = post(&pool, &bob, "the spam word").await;
        let _seeded: Filter = filter::create(
            &pool,
            alice.id,
            &NewFilter {
                phrase: "spam".to_owned(),
                context: vec!["home".to_owned()],
                whole_word: true,
                irreversible: true,
                expires_at: None,
            },
        )
        .await
        .expect("the fixture filter creates");

        let page = render_statuses(&test_state, Some(&alice), std::slice::from_ref(&doomed))
            .await
            .expect("the fixture page renders");
        let kept = apply_active(&test_state, Some(&alice), page, FilterContext::Home)
            .await
            .expect("the active read succeeds");
        assert!(
            kept.is_empty(),
            "alice's active irreversible filter removes the stranger's post"
        );

        let anon_page = render_statuses(&test_state, Some(&alice), std::slice::from_ref(&doomed))
            .await
            .expect("the fixture page renders");
        let anonymous = apply_active(&test_state, None, anon_page, FilterContext::Home)
            .await
            .expect("the anonymous pass runs no query");
        assert_eq!(
            ids(&anonymous),
            vec![doomed.id.to_string()],
            "and missing filters (no auth) are the no-op"
        );
    }
}
