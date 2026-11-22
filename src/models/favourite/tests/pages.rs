//! The two paged reads: a status's users and an account's favourites.

use super::*;

#[tokio::test]
async fn the_users_of_a_status_page_over_the_like_ids() {
    let db = crate::test_db::test_db().await.expect("test database slot");
    let pool = db.pool().clone();
    let status_id = post_by(&pool, "author").await;
    let mut likers = Vec::new();
    for n in 0..PAGE_SEED {
        let liker = user(&pool, &format!("liker{n}")).await;
        favourite_as(&pool, &liker, status_id).await;
        likers.push(liker);
    }
    let all = stored(&pool).await;
    assert_eq!(all.len(), PAGE_SEED, "three fixture favourites");

    let first_page = users_for_status(&pool, status_id, page("limit=2"))
        .await
        .expect("the first page reads");
    assert_eq!(
        entry_ids(&first_page),
        vec![all[0].0, all[1].0],
        "the two newest likes"
    );
    assert_eq!(
        first_page[0].user.id, all[0].1,
        "and each entry carries the account behind its like"
    );

    let older = users_for_status(&pool, status_id, page(&format!("max_id={}", all[1].0)))
        .await
        .expect("the continuation reads");
    assert_eq!(
        entry_ids(&older),
        vec![all[2].0],
        "max_id continues where the page ended"
    );
    assert!(
        users_for_status(&pool, status_id, page(&format!("max_id={}", all[2].0)))
            .await
            .expect("the walk off the end reads")
            .is_empty(),
        "and past the oldest like there is nothing"
    );

    let newer = users_for_status(&pool, status_id, page(&format!("min_id={}", all[2].0)))
        .await
        .expect("the forward page reads");
    assert_eq!(
        entry_ids(&newer),
        vec![all[0].0, all[1].0],
        "min_id answers the newer page, newest-first after the flip"
    );

    assert!(
        users_for_status(&pool, post_by(&pool, "quiet").await, page(""))
            .await
            .expect("an unliked status reads")
            .is_empty(),
        "a status nobody favourited answers nothing"
    );
    assert_eq!(
        likers.len(),
        PAGE_SEED,
        "the fixture seeded three distinct accounts, one like each"
    );
}

#[tokio::test]
async fn a_user_page_pairs_each_like_with_its_status() {
    let db = crate::test_db::test_db().await.expect("test database slot");
    let pool = db.pool().clone();
    let alice = user(&pool, "alice").await;
    let bob = user(&pool, "bob").await;
    let first = post(&pool, &bob).await;
    let second = post(&pool, &bob).await;
    let third = post(&pool, &bob).await;
    favourite_as(&pool, &alice, first.id).await;
    favourite_as(&pool, &alice, second.id).await;
    favourite_as(&pool, &alice, third.id).await;
    let carol = user(&pool, "carol").await;
    favourite_as(&pool, &carol, first.id).await;

    let all = sqlx::query_as::<_, (Uuid,)>(
        "SELECT id FROM favourites WHERE user_id = $1 ORDER BY id DESC",
    )
    .bind(alice.id)
    .fetch_all(&pool)
    .await
    .expect("alice's rows read");
    let alice_ids: Vec<Uuid> = all.into_iter().map(|(id,)| id).collect();

    let pairs = page_for_user(&pool, alice.id, page("limit=2"))
        .await
        .expect("the first page reads");
    assert_eq!(
        pair_ids(&pairs),
        vec![alice_ids[0], alice_ids[1]],
        "newest favourite first, keyed on like ids"
    );
    let statuses: HashSet<Uuid> = [first.id, second.id, third.id].into_iter().collect();
    assert!(
        pairs
            .iter()
            .all(|(favourite, status)| statuses.contains(&status.id)
                && favourite.status_id == status.id),
        "every entry pairs its like with the status it likes"
    );

    let older = page_for_user(&pool, alice.id, page(&format!("max_id={}", alice_ids[1])))
        .await
        .expect("the continuation reads");
    assert_eq!(
        pair_ids(&older),
        vec![alice_ids[2]],
        "max_id walks alice's own like ids"
    );
    assert_eq!(
        older[0].0.user_id, alice.id,
        "carol's like of the same status is on no page of alice's"
    );

    assert!(
        page_for_user(&pool, user(&pool, "lonely").await.id, page(""))
            .await
            .expect("an account with no favourites reads")
            .is_empty(),
        "nobody's list is the empty page"
    );
}

#[tokio::test]
async fn a_pair_leaves_the_page_when_its_status_goes() {
    let db = crate::test_db::test_db().await.expect("test database slot");
    let pool = db.pool().clone();
    let alice = user(&pool, "alice").await;
    let bob = user(&pool, "bob").await;
    for _ in 0..PAGE_SEED {
        let liked = post(&pool, &bob).await;
        favourite_as(&pool, &alice, liked.id).await;
    }
    let full = page_for_user(&pool, alice.id, page(""))
        .await
        .expect("the whole list reads");
    assert_eq!(full.len(), PAGE_SEED, "one pair per favourite");

    // A hard-deleted status drops its whole pair from the page (not
    // the tombstone the endpoint's visibility step handles): a like of
    // a gone post is on no page.
    let gone = full[1].1.id;
    let erased = sqlx::query("DELETE FROM statuses WHERE id = $1")
        .bind(gone)
        .execute(&pool)
        .await
        .expect("the fixture status deletes")
        .rows_affected();
    assert_eq!(erased, 1, "the status was there to delete");
    let after = page_for_user(&pool, alice.id, page(""))
        .await
        .expect("the page after the delete reads");
    assert_eq!(
        after
            .iter()
            .map(|(_, status)| status.id)
            .collect::<Vec<Uuid>>(),
        vec![full[0].1.id, full[2].1.id],
        "the gone status contributes nothing and the survivors keep \
             the like-id order"
    );
    assert!(
        after
            .iter()
            .all(|(favourite, status)| status.id == favourite.status_id),
        "and every remaining pair still agrees"
    );
}
