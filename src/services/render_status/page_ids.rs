//! The id collections a page render asks its keyed loaders about: the
//! order-preserving [`distinct`] helper and [`PageIds`], the five id sets
//! walked once from the page and its nested rows.

use std::collections::{HashMap, HashSet};

use uuid::Uuid;

use crate::models::status::Status as StatusRow;

/// The distinct ids of `ids`, in order of first appearance: one ask of a
/// batch loader is enough, whatever references an id on the page.
pub(super) fn distinct<I>(ids: I) -> Vec<Uuid>
where
    I: IntoIterator<Item = Uuid>,
{
    let mut seen = HashSet::new();
    ids.into_iter().filter(|id| seen.insert(*id)).collect()
}

/// The id sets [`Batch::load`] asks its keyed loaders about, walked once
/// from the page and the two nested-row maps: the same five `distinct`
/// collections for every render, named instead of spelled out inline.
///
/// [`Batch::load`]: super::batch::Batch::load
#[derive(Debug)]
pub(super) struct PageIds {
    /// Every row this render emits an entity for — the page's own ids with
    /// the reblogged originals and quoted originals beside them. The key of
    /// every status-keyed loader.
    pub(super) rendered: Vec<Uuid>,
    /// Every author and replied-to author the page and its nested rows
    /// name.
    pub(super) accounts: Vec<Uuid>,
    /// The authors whose relationship to the viewer renders: the `user_id`
    /// of every emitted row, nested ones included.
    pub(super) authors: Vec<Uuid>,
    /// The threads the page touches, for the viewer's muted-thread read.
    pub(super) conversations: Vec<Uuid>,
    /// The direct-message threads the page touches, for the viewer's
    /// participation read.
    pub(super) directs: Vec<Uuid>,
}

impl PageIds {
    /// Collects the five sets in first-seen order over `rows`, the
    /// `originals` they reblog and the `quoted` they quote.
    pub(super) fn of(
        rows: &[&StatusRow],
        originals: &HashMap<Uuid, StatusRow>,
        quoted: &HashMap<Uuid, StatusRow>,
    ) -> Self {
        Self {
            rendered: distinct(
                rows.iter()
                    .copied()
                    .map(|row| row.id)
                    .chain(originals.keys().copied())
                    .chain(quoted.keys().copied()),
            ),
            accounts: distinct(
                rows.iter()
                    .copied()
                    .chain(originals.values())
                    .chain(quoted.values())
                    .flat_map(|row| [Some(row.user_id), row.in_reply_to_account_id])
                    .flatten(),
            ),
            authors: distinct(
                rows.iter()
                    .copied()
                    .map(|row| row.user_id)
                    .chain(originals.values().map(|row| row.user_id))
                    .chain(quoted.values().map(|row| row.user_id)),
            ),
            conversations: distinct(
                rows.iter()
                    .copied()
                    .chain(originals.values())
                    .chain(quoted.values())
                    .map(|row| row.conversation_id),
            ),
            directs: distinct(
                rows.iter()
                    .copied()
                    .chain(originals.values())
                    .filter_map(|row| row.direct_conversation_id),
            ),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn distinct_collapses_repeats_and_keeps_first_seen_order() {
        let a = Uuid::now_v7();
        let b = Uuid::now_v7();
        let ids = distinct([a, b, a, a, b]);
        assert_eq!(ids, vec![a, b], "one ask per id, in first-seen order");
        assert!(distinct(std::iter::empty()).is_empty(), "nothing in");
    }
}
