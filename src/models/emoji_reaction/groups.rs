//! Folding the flat reaction rows of [`reactors`] into one group per status
//! and emoji, each carrying its reactors in order.

use uuid::Uuid;

use crate::util::emoji::display_name;

#[cfg(doc)]
use super::read::{REACTORS_SQL, reactors};

/// One emoji on one status with *every* account that reacted: what
/// [`reactors`] answers for the reactions endpoint, which renders all of them
/// instead of summarising.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ReactorGroup {
    /// The emoji's API name: the stored text, or `shortcode@domain` for a
    /// custom emoji of another instance.
    pub name: String,
    /// The custom emoji's image URL, `None` for a unicode emoji: the image of
    /// this emoji's earliest reaction.
    pub url: Option<String>,
    /// The reactors, earliest reaction first.
    pub reactor_ids: Vec<Uuid>,
}

/// One fetched row of [`REACTORS_SQL`]: a single reaction, in the order that
/// statement returns them (the window column it orders by is not part of it).
#[derive(Debug, sqlx::FromRow)]
pub(super) struct ReactionRow {
    /// The reacted-to status: the outer key of the grouping.
    status_id: Uuid,
    /// The emoji: the inner key.
    emoji: String,
    /// The emoji's origin, the other half of the inner key.
    emoji_domain: Option<String>,
    /// The custom emoji's image URL, if any.
    url: Option<String>,
    /// The reactor: one entry of its group's reactor list.
    user_id: Uuid,
}

/// Folds [`reactors`]' flat rows into one [`ReactorGroup`] per
/// `(status_id, emoji)`, in the order the statement returned them: a group
/// starts at its first row and a later row joins the group the walk stands in,
/// which the loader's `ORDER BY` keeps contiguous.
pub(super) fn group(rows: Vec<ReactionRow>) -> Vec<(Uuid, ReactorGroup)> {
    let mut grouped: Vec<(Uuid, ReactorGroup)> = Vec::with_capacity(rows.len());
    for row in rows {
        let name = display_name(&row.emoji, row.emoji_domain.as_deref());
        if !join_open_group(&mut grouped, &row, &name) {
            grouped.push((
                row.status_id,
                ReactorGroup {
                    name,
                    url: row.url,
                    reactor_ids: vec![row.user_id],
                },
            ));
        }
    }
    grouped
}

/// Appends `row`'s reactor to the group `grouped` last holds, answering whether
/// it fitted; `name` is the row's emoji as [`display_name`] names it.
///
/// Only that last group can take a row: the statement orders a status's rows by
/// their emoji's first reaction time and then by the emoji text and origin, so
/// the rows of one `(status_id, emoji, origin)` arrive together and a row that
/// does not match the open group starts a new one — which is what [`group`]
/// does on `false`.
fn join_open_group(grouped: &mut [(Uuid, ReactorGroup)], row: &ReactionRow, name: &str) -> bool {
    let Some((status, open)) = grouped.last_mut() else {
        return false;
    };
    if *status != row.status_id || open.name != name {
        return false;
    }
    open.reactor_ids.push(row.user_id);
    true
}

#[cfg(test)]
mod tests {
    use super::super::{
        read::{for_statuses, reactors},
        test_support::{
            CAT, CAT_URL, REMOTE_CAT, REMOTE_CAT_URL, REMOTE_DOMAIN, THUMBS, instant, names,
            post_by, react_remote, react_with, stamp, user,
        },
    };
    use super::*;

    #[tokio::test]
    async fn emojis_order_by_first_reaction_then_name() {
        let db = crate::test_db::test_db().await.expect("test database slot");
        let pool = db.pool().clone();
        let alice = user(&pool, "alice").await;
        let bob = user(&pool, "bob").await;
        let status_id = post_by(&pool, "carol").await;
        react_with(&pool, &alice, status_id, ":zebra:", None).await;
        react_with(&pool, &bob, status_id, ":zebra:", None).await;
        react_with(&pool, &alice, status_id, ":apple:", None).await;
        react_with(&pool, &bob, status_id, THUMBS, None).await;

        // :apple: is alice's second row yet the third group: what orders a
        // group is its *first* reaction, not any later one, and zebra's later
        // second reaction cannot move it either.
        stamp(
            &pool,
            status_id,
            &alice,
            ":zebra:",
            instant("2024-01-01T00:00:00.000Z"),
        )
        .await;
        stamp(
            &pool,
            status_id,
            &bob,
            ":zebra:",
            instant("2024-05-01T00:00:00.000Z"),
        )
        .await;
        stamp(
            &pool,
            status_id,
            &bob,
            THUMBS,
            instant("2024-02-01T00:00:00.000Z"),
        )
        .await;
        stamp(
            &pool,
            status_id,
            &alice,
            ":apple:",
            instant("2024-03-01T00:00:00.000Z"),
        )
        .await;

        let rollups = for_statuses(&pool, &[status_id], Some(alice.id))
            .await
            .expect("the roll-up reads");
        assert_eq!(
            names(&rollups[&status_id]),
            vec![":zebra:", THUMBS, ":apple:"],
            "january's zebra leads, then february's thumb, then march's \
             apple, which nothing later moved"
        );
        assert_eq!(
            rollups[&status_id][0].account_ids,
            vec![alice.id, bob.id],
            "and a group's reactors stay in their own reaction order"
        );

        // Two groups that began at the same instant order by emoji text.
        let twin = post_by(&pool, "dave").await;
        let same = instant("2024-04-01T00:00:00.000Z");
        react_with(&pool, &alice, twin, ":b:", None).await;
        react_with(&pool, &alice, twin, ":a:", None).await;
        for emoji in [":a:", ":b:"] {
            stamp(&pool, twin, &alice, emoji, same).await;
        }
        let twins = for_statuses(&pool, &[twin], Some(alice.id))
            .await
            .expect("the second roll-up reads");
        assert_eq!(
            names(&twins[&twin]),
            vec![":a:", ":b:"],
            "one first-reaction time leaves the emoji text to order"
        );
    }

    #[tokio::test]
    async fn reactors_groups_keep_every_reactor_in_order() {
        let db = crate::test_db::test_db().await.expect("test database slot");
        let pool = db.pool().clone();
        let alice = user(&pool, "alice").await;
        let bob = user(&pool, "bob").await;
        let status_id = post_by(&pool, "carol").await;
        react_with(&pool, &alice, status_id, THUMBS, None).await;
        react_with(&pool, &bob, status_id, THUMBS, None).await;
        react_with(&pool, &bob, status_id, CAT, Some(CAT_URL)).await;
        stamp(
            &pool,
            status_id,
            &alice,
            THUMBS,
            instant("2024-01-01T00:00:00.000Z"),
        )
        .await;
        stamp(
            &pool,
            status_id,
            &bob,
            THUMBS,
            instant("2024-02-01T00:00:00.000Z"),
        )
        .await;
        stamp(
            &pool,
            status_id,
            &bob,
            CAT,
            instant("2024-03-01T00:00:00.000Z"),
        )
        .await;

        let groups = reactors(&pool, status_id, None, None)
            .await
            .expect("the reactors read");
        assert_eq!(groups.len(), 2, "one group per emoji");
        assert_eq!(groups[0].name, THUMBS, "the earlier emoji first");
        assert_eq!(
            groups[0].reactor_ids,
            vec![alice.id, bob.id],
            "every reactor, not the capped five, in reaction order"
        );
        assert_eq!(groups[0].url, None, "a unicode group carries no image");
        assert_eq!(
            groups[1],
            ReactorGroup {
                name: CAT.to_owned(),
                url: Some(CAT_URL.to_owned()),
                reactor_ids: vec![bob.id],
            },
            "the custom group keeps its image and its one reactor"
        );

        assert_eq!(
            reactors(&pool, status_id, Some(CAT), None)
                .await
                .expect("the filtered read runs"),
            vec![ReactorGroup {
                name: CAT.to_owned(),
                url: Some(CAT_URL.to_owned()),
                reactor_ids: vec![bob.id],
            }],
            "the single-emoji filter answers that emoji only"
        );
        assert!(
            reactors(&pool, status_id, Some(":none:"), None)
                .await
                .expect("an unreacted emoji reads")
                .is_empty(),
            "an emoji nobody reacted with answers nothing"
        );
        assert!(
            reactors(&pool, post_by(&pool, "dave").await, None, None)
                .await
                .expect("an unreacted status reads")
                .is_empty(),
            "a status nobody reacted to answers nothing"
        );
    }

    #[tokio::test]
    async fn a_remote_emoji_is_its_own_group_named_for_its_origin() {
        let db = crate::test_db::test_db().await.expect("test database slot");
        let pool = db.pool().clone();
        let alice = user(&pool, "alice").await;
        let bob = user(&pool, "bob").await;
        let status_id = post_by(&pool, "carol").await;
        let _first = react_remote(&pool, &alice, status_id).await;
        let _second = react_remote(&pool, &bob, status_id).await;
        react_with(&pool, &bob, status_id, REMOTE_CAT, Some(CAT_URL)).await;

        let rollup = for_statuses(&pool, &[status_id], Some(alice.id))
            .await
            .expect("the roll-up reads");
        let entries = &rollup[&status_id];
        assert_eq!(
            names(entries),
            vec!["misskeycat@misskey.test", REMOTE_CAT],
            "the remote emoji is named shortcode@domain, the local one keeps its text"
        );
        assert_eq!(entries[0].count, 2, "both remote reactors share one entry");
        assert!(entries[0].me, "and alice is one of them");
        assert_eq!(
            entries[0].url.as_deref(),
            Some(REMOTE_CAT_URL),
            "the remote image rides along"
        );
        assert_eq!(entries[1].count, 1, "the local twin is a separate emoji");

        let remote = reactors(&pool, status_id, Some(REMOTE_CAT), Some(REMOTE_DOMAIN))
            .await
            .expect("the remote filter runs");
        assert_eq!(
            remote,
            vec![ReactorGroup {
                name: "misskeycat@misskey.test".to_owned(),
                url: Some(REMOTE_CAT_URL.to_owned()),
                reactor_ids: vec![alice.id, bob.id],
            }],
            "the filter names the emoji and its origin"
        );
        let local = reactors(&pool, status_id, Some(REMOTE_CAT), None)
            .await
            .expect("the local filter runs");
        assert_eq!(
            local
                .iter()
                .map(|group| group.name.as_str())
                .collect::<Vec<_>>(),
            vec![REMOTE_CAT],
            "without a domain the same text is the local emoji"
        );
        assert_eq!(local[0].reactor_ids, vec![bob.id], "and only bob's");
    }

    #[test]
    fn group_folds_contiguous_rows_and_starts_a_new_group() {
        let status = Uuid::now_v7();
        let other = Uuid::now_v7();
        let (a, b) = (Uuid::now_v7(), Uuid::now_v7());
        let rows = vec![
            row(status, THUMBS, None, a),
            row(status, THUMBS, Some(CAT_URL), b),
            row(status, CAT, Some(CAT_URL), b),
            row(other, THUMBS, None, a),
        ];

        let grouped = group(rows);
        assert_eq!(
            grouped.len(),
            3,
            "two emojis on one status, one on the other"
        );
        assert_eq!(
            grouped[0].1.reactor_ids,
            vec![a, b],
            "the two 👍 rows folded into one group"
        );
        assert_eq!(
            grouped[0].1.url, None,
            "and the group keeps the image of its earliest reaction"
        );
        assert_eq!(
            (grouped[1].0, grouped[1].1.name.clone()),
            (status, CAT.to_owned()),
            "the cat group is its own entry of the same status"
        );
        assert_eq!(
            (grouped[2].0, grouped[2].1.name.clone()),
            (other, THUMBS.to_owned()),
            "and the same emoji on another status starts over"
        );
        assert!(
            group(Vec::new()).is_empty(),
            "nothing in answers nothing out"
        );
    }

    /// One fetched [`ReactionRow`], for [`group`]'s unit test.
    fn row(status_id: Uuid, emoji: &str, url: Option<&str>, user_id: Uuid) -> ReactionRow {
        ReactionRow {
            status_id,
            emoji: emoji.to_owned(),
            emoji_domain: None,
            url: url.map(str::to_owned),
            user_id,
        }
    }
}
