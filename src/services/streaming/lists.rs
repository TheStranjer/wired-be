//! The list rule: the `list:{uuid}` streams a status rides — every list
//! whose owner follows the author and holds them as a member — with a reply
//! admitted only where the list's `replies_policy` admits the account being
//! replied to.

use sqlx::PgPool;
use uuid::Uuid;

use crate::{
    error::AppResult,
    models::{follow, list, status::Status, user::User},
};

/// The list rule's answer: the `list:{uuid}` stream of every list whose
/// owner follows the author and holds them as a member, and the owners'
/// ids to fold into the audience — each one a reply already checked
/// against its list's [`replies_policy`](list::List::replies_policy).
pub(super) async fn list_targets(
    pool: &PgPool,
    row: &Status,
    author: &User,
) -> AppResult<(Vec<String>, Vec<Uuid>)> {
    let mut streams = Vec::new();
    let mut owners = Vec::new();
    for list_row in list::lists_following_member(pool, author.id).await? {
        if reply_admitted(pool, row, &list_row).await? {
            streams.push(format!("list:{}", list_row.id));
            owners.push(list_row.user_id);
        }
    }
    Ok((streams, owners))
}

/// Whether `list_row` carries `row`: every list carries a root status, a
/// reply only where its policy admits the account being replied to.
async fn reply_admitted(pool: &PgPool, row: &Status, list_row: &list::List) -> AppResult<bool> {
    if row.in_reply_to_id.is_none() {
        return Ok(true);
    }
    match list_row.replies_policy.as_str() {
        "list" => replied_to_in_list(pool, row, list_row).await,
        "followed" => replied_to_is_followed(pool, row, list_row).await,
        // `"none"` carries no replies at all; anything else the column's
        // check constraint does not allow admits nothing either.
        _ => Ok(false),
    }
}

/// The `list` policy's answer: the replied-to account is the owner's own,
/// or a member of this same list. A reply to an account this instance does
/// not know the author of is neither.
async fn replied_to_in_list(pool: &PgPool, row: &Status, list_row: &list::List) -> AppResult<bool> {
    let Some(replied_to) = row.in_reply_to_account_id else {
        return Ok(false);
    };
    if replied_to == list_row.user_id {
        return Ok(true);
    }
    Ok(list::member_ids(pool, list_row.id)
        .await?
        .contains(&replied_to))
}

/// The `followed` policy's answer: the replied-to account is the owner's
/// own, or one the owner holds an accepted follow of. A reply to an
/// account this instance does not know the author of is neither.
async fn replied_to_is_followed(
    pool: &PgPool,
    row: &Status,
    list_row: &list::List,
) -> AppResult<bool> {
    let Some(replied_to) = row.in_reply_to_account_id else {
        return Ok(false);
    };
    if replied_to == list_row.user_id {
        return Ok(true);
    }
    follow::has_accepted_follow(pool, list_row.user_id, replied_to).await
}

#[cfg(test)]
mod tests {
    use crate::{
        services::streaming::test_support::{
            all_three_lists, follow_user, list_id_of, list_stream, local_user, post, published,
            reply_to,
        },
        streaming::hub::Audience,
        test_support::app_state,
    };

    use super::*;

    #[tokio::test]
    async fn lists_carry_root_statuses_under_every_policy() {
        let db = crate::test_db::test_db().await.expect("test database slot");
        let pool = db.pool().clone();
        let state = app_state(&pool);
        let alice = local_user(&pool, "alice").await;
        let dana = local_user(&pool, "dana").await;
        follow_user(&pool, &dana, &alice).await;
        let lists = all_three_lists(&pool, &dana, &alice).await;

        let row = post(&pool, &alice, "a root post").await;

        let (streams, audience, _) = published(&state, &row, &alice).await;
        for list_row in &lists {
            assert!(
                streams.contains(&list_stream(list_row.id)),
                "the {} policy carries a root post",
                list_row.replies_policy
            );
        }
        assert_eq!(
            audience,
            Audience::Public,
            "the lists ride along with the public audience"
        );
    }

    #[tokio::test]
    async fn list_reply_policies_admit_by_the_replied_to_account() {
        let db = crate::test_db::test_db().await.expect("test database slot");
        let pool = db.pool().clone();
        let state = app_state(&pool);
        let alice = local_user(&pool, "alice").await;
        let dana = local_user(&pool, "dana").await;
        follow_user(&pool, &dana, &alice).await;
        let lists = all_three_lists(&pool, &dana, &alice).await;
        let xan = local_user(&pool, "xan").await;
        let _member = list::add_members(&pool, list_id_of(&lists, "list"), &[xan.id])
            .await
            .expect("the list membership fixture adds");
        follow_user(&pool, &dana, &xan).await;
        let yanni = local_user(&pool, "yanni").await;

        let xan_row = post(&pool, &xan, "a listed account").await;
        let to_member = reply_to(&pool, &alice, &xan_row).await;
        let (member_streams, _, _) = published(&state, &to_member, &alice).await;
        assert!(
            member_streams.contains(&list_stream(list_id_of(&lists, "list"))),
            "the list policy carries a reply to one of its own members"
        );
        assert!(
            member_streams.contains(&list_stream(list_id_of(&lists, "followed"))),
            "the followed policy carries a reply to an account the owner follows"
        );
        assert!(
            !member_streams.contains(&list_stream(list_id_of(&lists, "none"))),
            "the none policy carries no reply at all"
        );

        let yanni_row = post(&pool, &yanni, "nobody to dana").await;
        let to_outsider = reply_to(&pool, &alice, &yanni_row).await;
        let (outsider_streams, _, _) = published(&state, &to_outsider, &alice).await;
        for list_row in &lists {
            assert!(
                !outsider_streams.contains(&list_stream(list_row.id)),
                "the {} policy drops a reply to an account outside it",
                list_row.replies_policy
            );
        }
    }
}
