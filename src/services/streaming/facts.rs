//! The stored-row lookups the audience computations share: `row`'s tag names
//! and whether it carries media — the facts that widen a status's stream set —
//! plus the audience and stream-name folds over their answers.

use sqlx::PgPool;
use uuid::Uuid;

use crate::{
    error::AppResult,
    models::{hashtag, media_attachment},
    streaming::hub::Audience,
};

/// The stored tag names of `row`, name-ordered as
/// [`hashtag::for_statuses`] groups them, empty when the status has none.
pub(super) async fn status_tag_names(pool: &PgPool, status_id: Uuid) -> AppResult<Vec<String>> {
    let mut grouped = hashtag::for_statuses(pool, &[status_id]).await?;
    let some_tags = grouped.remove(&status_id).unwrap_or_default();
    Ok(some_tags.into_iter().map(|tag| tag.name).collect())
}

/// Whether `row` carries at least one media attachment — the gate on the
/// `public*:media` streams.
pub(super) async fn has_attachments(pool: &PgPool, status_id: Uuid) -> AppResult<bool> {
    let grouped = media_attachment::for_statuses(pool, &[status_id]).await?;
    Ok(grouped.contains_key(&status_id))
}

/// The `Audience::Users` over `ids` extended with the list `owners` — the
/// list rule's "with the owner in `Audience::Users`" — sorted and dedup,
/// so the audience names each user once.
pub(super) fn users_audience(mut ids: Vec<Uuid>, owners: &[Uuid]) -> Audience {
    ids.extend(owners.iter().copied());
    ids.sort_unstable();
    ids.dedup();
    Audience::Users(ids)
}

/// `names` sorted and deduplicated: the canonical order the computed
/// stream list is handed over in, so a later publisher (and every test)
/// compares stream sets by comparing vectors.
pub(super) fn sorted_names(mut names: Vec<String>) -> Vec<String> {
    names.sort();
    names.dedup();
    names
}
