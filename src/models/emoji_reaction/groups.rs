//! Folding the flat reaction rows of [`reactors`] into one group per status
//! and emoji, each carrying its reactors in order.

use uuid::Uuid;

#[cfg(doc)]
use super::read::{REACTORS_SQL, reactors};

/// One emoji on one status with *every* account that reacted: what
/// [`reactors`] answers for the reactions endpoint, which renders all of them
/// instead of summarising.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ReactorGroup {
    /// The emoji as stored.
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
        if !join_open_group(&mut grouped, &row) {
            grouped.push((
                row.status_id,
                ReactorGroup {
                    name: row.emoji,
                    url: row.url,
                    reactor_ids: vec![row.user_id],
                },
            ));
        }
    }
    grouped
}

/// Appends `row`'s reactor to the group `grouped` last holds, answering whether
/// it fitted.
///
/// Only that last group can take a row: the statement orders a status's rows by
/// their emoji's first reaction time and then by the emoji text, so the rows of
/// one `(status_id, emoji)` arrive together and a row that does not match the
/// open group starts a new one — which is what [`group`] does on `false`.
fn join_open_group(grouped: &mut [(Uuid, ReactorGroup)], row: &ReactionRow) -> bool {
    let Some((status, open)) = grouped.last_mut() else {
        return false;
    };
    if *status != row.status_id || open.name != row.emoji {
        return false;
    }
    open.reactor_ids.push(row.user_id);
    true
}

#[cfg(test)]
mod tests {
    use super::super::{
        read::{for_statuses, reactors},
        test_support::{CAT, CAT_URL, THUMBS, instant, names, post_by, react_with, stamp, user},
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

        let groups = reactors(&pool, status_id, None)
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
            reactors(&pool, status_id, Some(CAT))
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
            reactors(&pool, status_id, Some(":none:"))
                .await
                .expect("an unreacted emoji reads")
                .is_empty(),
            "an emoji nobody reacted with answers nothing"
        );
        assert!(
            reactors(&pool, post_by(&pool, "dave").await, None)
                .await
                .expect("an unreacted status reads")
                .is_empty(),
            "a status nobody reacted to answers nothing"
        );
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
            url: url.map(str::to_owned),
            user_id,
        }
    }
}
