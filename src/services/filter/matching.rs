//! The per-status step of filter application: what one rendered status trips.
//!
//! A filter matches a status when any of its keywords occurs in the
//! status's plain text — [`strip_html`] of the content, the spoiler text and
//! every media description, of the status *and* of its reblogged original —
//! or when the status (or the original it boosts) is one of the filter's
//! status entries, as Mastodon's `CustomFilter.apply_cached_filters` does.

use uuid::Uuid;

use crate::{
    entities::{filter::FilterResult, status::Status as StatusEntity},
    models::filter::{Filter, FilterAction, keyword},
    util::sanitize::strip_html,
};

/// Applies the scoped `filters` to the one rendered `status`, reporting
/// whether it survives the page.
///
/// A status the viewer authored is never touched. A match of a `hide` filter
/// drops the status (`false`); every other match keeps it and the status's
/// `filtered` array becomes one [`FilterResult`] per matching filter, in the
/// order the filters came in. Nothing matched keeps the empty array.
pub(super) fn apply_to_status(
    status: &mut StatusEntity,
    scoped: &[&Filter],
    viewer_id: Option<Uuid>,
) -> bool {
    if viewer_id.is_some_and(|id| status.account.id == id.to_string()) {
        return true;
    }
    let text = filter_text(status);
    let ids = status_ids(status);
    let mut results: Vec<FilterResult> = Vec::new();
    for filter in scoped {
        let keyword_matches = matched_keywords(filter, &text);
        let status_matches = matched_statuses(filter, &ids);
        if keyword_matches.is_empty() && status_matches.is_empty() {
            continue;
        }
        if filter.action == FilterAction::Hide {
            return false;
        }
        results.push(FilterResult::new(filter, keyword_matches, status_matches));
    }
    status.filtered = results;
    true
}

/// The phrases of `filter` that occur in `text`, in keyword order.
fn matched_keywords(filter: &Filter, text: &str) -> Vec<String> {
    filter
        .keywords
        .iter()
        .filter(|found| keyword::matches(found, text))
        .map(|found| found.phrase.clone())
        .collect()
}

/// The ids among `ids` that `filter` holds as status entries.
fn matched_statuses(filter: &Filter, ids: &[String]) -> Vec<String> {
    filter
        .statuses
        .iter()
        .map(|entry| entry.status_id.to_string())
        .filter(|id| ids.contains(id))
        .collect()
}

/// The ids a status entry may name for `status`: its own and, for a reblog,
/// its original's.
fn status_ids(status: &StatusEntity) -> Vec<String> {
    let mut ids = vec![status.id.clone()];
    if let Some(original) = status.reblog.as_deref() {
        ids.push(original.id.clone());
    }
    ids
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
    use chrono::Utc;
    use serde_json::{Value, json};
    use sqlx::PgPool;

    use super::*;
    use crate::{
        config::{Config, TEST_BASE_URL},
        models::{
            filter::{
                keyword::{Keyword, NewKeyword},
                status::FilterStatus,
            },
            status::{self, NewStatus, Status as StatusRow, Visibility},
            user::User,
        },
        services::{
            filter::{FilterContext, apply_filters},
            render_status::{ViewerMode, render_statuses},
        },
        state::AppState,
        test_fixtures::local_user as person,
    };

    /// Inserts `text` as `author`'s local public status.
    async fn post(pool: &PgPool, author: &User, text: &str) -> StatusRow {
        status::insert(pool, TEST_BASE_URL, &NewStatus::local(author, text))
            .await
            .expect("a fixture post inserts")
    }

    /// A home filter with `action`, the given keyword phrases and one entry
    /// per id of `statuses`.
    fn filter_of(action: FilterAction, words: &[&str], statuses: &[Uuid]) -> Filter {
        let id = Uuid::now_v7();
        let keywords = words
            .iter()
            .map(|word| {
                let fresh = NewKeyword::new(word);
                Keyword {
                    id: Uuid::now_v7(),
                    filter_id: id,
                    phrase: fresh.keyword,
                    whole_word: fresh.whole_word,
                    inserted_at: Utc::now(),
                    updated_at: Utc::now(),
                }
            })
            .collect();
        let entries = statuses
            .iter()
            .map(|status_id| FilterStatus {
                id: Uuid::now_v7(),
                filter_id: id,
                status_id: *status_id,
                inserted_at: Utc::now(),
                updated_at: Utc::now(),
            })
            .collect();
        Filter {
            id,
            user_id: Uuid::now_v7(),
            title: "fixture".to_owned(),
            context: vec!["home".to_owned()],
            action,
            expires_at: None,
            inserted_at: Utc::now(),
            updated_at: Utc::now(),
            keywords,
            statuses: entries,
        }
    }

    /// Renders `rows` for `viewer` and applies `filters` in the home context.
    async fn apply(
        pool: &PgPool,
        viewer: &User,
        rows: &[StatusRow],
        filters: &[Filter],
    ) -> Vec<StatusEntity> {
        let state = AppState::for_tests(pool.clone(), Config::for_tests());
        let page = render_statuses(&state, ViewerMode::User(Some(viewer)), rows)
            .await
            .expect("the fixture page renders");
        apply_filters(page, filters, FilterContext::Home, Some(viewer.id))
    }

    /// The `filtered` array of the first status of `page`, as JSON.
    fn first_filtered(page: &[StatusEntity]) -> Value {
        let status = page.first().expect("the page holds a status");
        serde_json::to_value(status.filtered.clone()).expect("results serialize")
    }

    #[tokio::test]
    async fn a_status_entry_matches_without_any_keyword() {
        let db = crate::test_db::test_db().await.expect("test database slot");
        let pool = db.pool().clone();
        let alice = person(&pool, "alice").await;
        let bob = person(&pool, "bob").await;
        let target = post(&pool, &bob, "nothing to see").await;
        let warner = filter_of(FilterAction::Warn, &[], &[target.id]);

        let kept = apply(&pool, &alice, std::slice::from_ref(&target), &[warner]).await;
        let results = first_filtered(&kept);
        assert_eq!(
            results[0]["status_matches"],
            json!([target.id.to_string()]),
            "the matched status id is reported"
        );
        assert_eq!(
            results[0]["keyword_matches"],
            Value::Null,
            "and no keyword matched"
        );
        assert_eq!(
            results[0]["filter"]["statuses"][0]["status_id"],
            json!(target.id.to_string()),
            "the embedded filter lists its status entry"
        );

        let hider = filter_of(FilterAction::Hide, &[], &[target.id]);
        let gone = apply(&pool, &alice, std::slice::from_ref(&target), &[hider]).await;
        assert!(gone.is_empty(), "a hiding entry removes the status");
    }

    #[tokio::test]
    async fn a_status_entry_matches_a_boost_of_its_status() {
        let db = crate::test_db::test_db().await.expect("test database slot");
        let pool = db.pool().clone();
        let alice = person(&pool, "alice").await;
        let bob = person(&pool, "bob").await;
        let carol = person(&pool, "carol").await;
        let original = post(&pool, &bob, "quiet").await;
        let boost =
            status::create_reblog(&pool, TEST_BASE_URL, &carol, &original, Visibility::Public)
                .await
                .expect("the boost writes")
                .0;
        let warner = filter_of(FilterAction::Warn, &[], &[original.id]);

        let kept = apply(&pool, &alice, std::slice::from_ref(&boost), &[warner]).await;
        assert_eq!(
            first_filtered(&kept)[0]["status_matches"],
            json!([original.id.to_string()]),
            "the original's entry matches the boost that carries it"
        );
    }

    #[tokio::test]
    async fn a_blur_filter_annotates_and_keeps_every_matching_keyword() {
        let db = crate::test_db::test_db().await.expect("test database slot");
        let pool = db.pool().clone();
        let alice = person(&pool, "alice").await;
        let bob = person(&pool, "bob").await;
        let row = post(&pool, &bob, "alpha and beta and gamma").await;
        let blurry = filter_of(FilterAction::Blur, &["gamma", "delta", "alpha"], &[]);

        let kept = apply(&pool, &alice, std::slice::from_ref(&row), &[blurry]).await;
        assert_eq!(kept.len(), 1, "blur never removes a status");
        let results = first_filtered(&kept);
        assert_eq!(
            results[0]["filter"]["filter_action"], "blur",
            "the action rides on the embedded filter"
        );
        assert_eq!(
            results[0]["keyword_matches"],
            json!(["gamma", "alpha"]),
            "the matching keywords, in keyword order; the miss is absent"
        );
        assert_eq!(
            results[0]["status_matches"],
            Value::Null,
            "no entry matched"
        );
    }
}
