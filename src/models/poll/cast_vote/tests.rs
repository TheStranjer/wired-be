//! The ballot's write put through its paces: which counts a recorded
//! ballot moves, which ballots are refused and with what effect (none),
//! and what a poll deleted under the lock answers.

use super::*;
use crate::{
    models::poll::{PollParams, get_by_id, options, test_support::*, voted_option_positions},
    util::id::new_id,
};

/// The options of `poll_id` as `(position, votes_count)`, the shape the
/// bump assertions compare.
async fn option_counts(pool: &PgPool, poll_id: Uuid) -> Vec<(i32, i32)> {
    options(pool, poll_id)
        .await
        .expect("the options read")
        .into_iter()
        .map(|option| (option.position, option.votes_count))
        .collect()
}

#[tokio::test]
async fn cast_vote_records_the_ballot_and_moves_every_count() {
    let db = crate::test_db::test_db().await.expect("test database slot");
    let pool = db.pool().clone();
    let alice = create_user(&pool, "alice").await;
    let bob = create_user(&pool, "bob").await;
    let posted = create_status(&pool, &alice).await;
    let stand = create_for(
        &pool,
        posted.id,
        &PollParams {
            options: vec!["tea".to_owned(), "coffee".to_owned(), "water".to_owned()],
            multiple: true,
            ..PollParams::default()
        },
    )
    .await;
    let choices = options(&pool, stand.id).await.expect("options read");
    let ids: Vec<Uuid> = choices.iter().map(|choice| choice.id).collect();

    // Bob marks the first and the third option in one ballot.
    assert!(
        cast_vote(&pool, stand.id, bob.id, &[ids[0], ids[2]])
            .await
            .expect("the ballot is recorded"),
        "a first ballot answers recorded, not refused"
    );
    let after = get_by_id(&pool, stand.id)
        .await
        .expect("the poll reads")
        .expect("the poll stands");
    assert_eq!(after.votes_count, 2, "one vote per mark");
    assert_eq!(after.voters_count, 1, "one voter however many marks");
    assert_eq!(
        option_counts(&pool, stand.id).await,
        vec![(0, 1), (1, 0), (2, 1)],
        "each chosen option gained exactly one"
    );
    assert_eq!(
        voted_option_positions(&pool, stand.id, bob.id)
            .await
            .expect("the marks read"),
        vec![0, 2],
        "the marks sit on the chosen options under the voter"
    );
    assert_eq!(
        count_for(
            &pool,
            "SELECT count(*) FROM poll_votes WHERE poll_id = $1",
            stand.id
        )
        .await,
        2,
        "one row per mark"
    );
    let stamped = sqlx::query_scalar::<_, bool>(
        "SELECT bool_and(inserted_at <= now() AND inserted_at >= now() - interval '1 minute')
           FROM poll_votes WHERE poll_id = $1",
    )
    .bind(stand.id)
    .fetch_one(&pool)
    .await
    .expect("the timestamps read");
    assert!(
        stamped,
        "the write stamps `inserted_at`, which has no default"
    );
}

#[tokio::test]
async fn cast_vote_sums_separate_ballots_into_the_same_counts() {
    let db = crate::test_db::test_db().await.expect("test database slot");
    let pool = db.pool().clone();
    let alice = create_user(&pool, "alice").await;
    let bob = create_user(&pool, "bob").await;
    let carol = create_user(&pool, "carol").await;
    let posted = create_status(&pool, &alice).await;
    let stand = create_for(&pool, posted.id, &params(&["tea", "coffee"])).await;
    let choices = options(&pool, stand.id).await.expect("options read");
    let ids: Vec<Uuid> = choices.iter().map(|choice| choice.id).collect();

    assert!(
        cast_vote(&pool, stand.id, bob.id, &[ids[0]])
            .await
            .expect("the first ballot is recorded"),
        "bob's mark lands"
    );
    assert!(
        cast_vote(&pool, stand.id, carol.id, &[ids[1]])
            .await
            .expect("the second ballot is recorded"),
        "a different voter is no collision"
    );
    let summed = get_by_id(&pool, stand.id)
        .await
        .expect("the poll reads")
        .expect("the poll stands");
    assert_eq!(summed.votes_count, 2, "one vote per ballot");
    assert_eq!(summed.voters_count, 2, "each voter counts once");
    assert_eq!(
        option_counts(&pool, stand.id).await,
        vec![(0, 1), (1, 1)],
        "and the options sum the ballots"
    );
}

#[tokio::test]
async fn cast_vote_refuses_the_second_ballot_of_one_voter_without_effects() {
    let db = crate::test_db::test_db().await.expect("test database slot");
    let pool = db.pool().clone();
    let alice = create_user(&pool, "alice").await;
    let bob = create_user(&pool, "bob").await;
    let posted = create_status(&pool, &alice).await;
    let stand = create_for(&pool, posted.id, &params(&["tea", "coffee"])).await;
    let choices = options(&pool, stand.id).await.expect("options read");
    let ids: Vec<Uuid> = choices.iter().map(|choice| choice.id).collect();

    assert!(
        cast_vote(&pool, stand.id, bob.id, &[ids[0]])
            .await
            .expect("the first ballot is recorded"),
        "the first ballot lands"
    );

    assert!(
        !cast_vote(&pool, stand.id, bob.id, &[ids[1]])
            .await
            .expect("the second ballot is answered"),
        "the same voter's second ballot is refused, whichever option it names"
    );
    let after = get_by_id(&pool, stand.id)
        .await
        .expect("the poll reads")
        .expect("the poll stands");
    assert_eq!(after.votes_count, 1, "the refusal changed no count");
    assert_eq!(after.voters_count, 1, "in either column");
    assert_eq!(
        option_counts(&pool, stand.id).await,
        vec![(0, 1), (1, 0)],
        "nor any option's share"
    );
    assert_eq!(
        voted_option_positions(&pool, stand.id, bob.id)
            .await
            .expect("the marks read"),
        vec![0],
        "and wrote no second mark"
    );
}

/// A [`params`] two-option poll that lets one voter mark both options —
/// the shape an inbound federated ballot arrives against, one `Create`
/// per chosen option.
fn many_params(titles: &[&str]) -> PollParams {
    PollParams {
        multiple: true,
        ..params(titles)
    }
}

#[tokio::test]
async fn a_multiple_poll_counts_a_voter_s_later_ballot_once() {
    let db = crate::test_db::test_db().await.expect("test database slot");
    let pool = db.pool().clone();
    let alice = create_user(&pool, "alice").await;
    let bob = create_user(&pool, "bob").await;
    let posted = create_status(&pool, &alice).await;
    let stand = create_for(&pool, posted.id, &many_params(&["tea", "coffee"])).await;
    let choices = options(&pool, stand.id).await.expect("options read");
    let ids: Vec<Uuid> = choices.iter().map(|choice| choice.id).collect();

    assert!(
        cast_vote(&pool, stand.id, bob.id, &[ids[0]])
            .await
            .expect("the first option is recorded"),
        "bob's first ballot lands"
    );
    assert!(
        cast_vote(&pool, stand.id, bob.id, &[ids[1]])
            .await
            .expect("the later option is recorded"),
        "a multiple poll takes the same voter's second option"
    );
    let after = get_by_id(&pool, stand.id)
        .await
        .expect("the poll reads")
        .expect("the poll stands");
    assert_eq!(after.votes_count, 2, "one vote per distinct option");
    assert_eq!(
        after.voters_count, 1,
        "the voter is counted once, not twice"
    );
    assert_eq!(
        option_counts(&pool, stand.id).await,
        vec![(0, 1), (1, 1)],
        "and each of the voter's two options gained one"
    );
    assert_eq!(
        voted_option_positions(&pool, stand.id, bob.id)
            .await
            .expect("the marks read"),
        vec![0, 1],
        "both options stand marked under one ballot-set"
    );
}

#[tokio::test]
async fn a_multiple_poll_refuses_an_option_the_voter_ticked_already() {
    let db = crate::test_db::test_db().await.expect("test database slot");
    let pool = db.pool().clone();
    let alice = create_user(&pool, "alice").await;
    let bob = create_user(&pool, "bob").await;
    let posted = create_status(&pool, &alice).await;
    let stand = create_for(&pool, posted.id, &many_params(&["tea", "coffee", "water"])).await;
    let choices = options(&pool, stand.id).await.expect("options read");
    let ids: Vec<Uuid> = choices.iter().map(|choice| choice.id).collect();

    assert!(
        cast_vote(&pool, stand.id, bob.id, &[ids[0]])
            .await
            .expect("the first ballot lands"),
        "the first option is recorded"
    );

    assert!(
        !cast_vote(&pool, stand.id, bob.id, &[ids[0]])
            .await
            .expect("the repeat is answered"),
        "re-marking a ticked option records nothing new"
    );
    let after = get_by_id(&pool, stand.id)
        .await
        .expect("the poll reads")
        .expect("the poll stands");
    assert_eq!(after.votes_count, 1, "the refusal doubled no count");
    assert_eq!(after.voters_count, 1, "in either column");
}

#[tokio::test]
async fn a_multiple_ballot_writes_only_the_fresh_options() {
    let db = crate::test_db::test_db().await.expect("test database slot");
    let pool = db.pool().clone();
    let alice = create_user(&pool, "alice").await;
    let bob = create_user(&pool, "bob").await;
    let posted = create_status(&pool, &alice).await;
    let stand = create_for(&pool, posted.id, &many_params(&["tea", "coffee", "water"])).await;
    let choices = options(&pool, stand.id).await.expect("options read");
    let ids: Vec<Uuid> = choices.iter().map(|choice| choice.id).collect();

    // One option already marked; the whole three-option ballot then adds
    // only the two it has not ticked, never re-counting the first.
    assert!(
        cast_vote(&pool, stand.id, bob.id, &[ids[0]])
            .await
            .expect("the first option lands"),
        "the first option is recorded"
    );
    assert!(
        cast_vote(&pool, stand.id, bob.id, &ids)
            .await
            .expect("the full ballot is recorded"),
        "the two fresh options land on the second ballot"
    );
    let after = get_by_id(&pool, stand.id)
        .await
        .expect("the poll reads")
        .expect("the poll stands");
    assert_eq!(
        after.votes_count, 3,
        "each of the three options, counted once"
    );
    assert_eq!(after.voters_count, 1, "still a single voter");
    assert_eq!(
        option_counts(&pool, stand.id).await,
        vec![(0, 1), (1, 1), (2, 1)],
        "the already-marked option was not bumped a second time"
    );
}

#[tokio::test]
async fn cast_vote_needs_a_poll_that_is_still_stored() {
    let db = crate::test_db::test_db().await.expect("test database slot");
    let pool = db.pool().clone();
    let alice = create_user(&pool, "alice").await;
    let bob = create_user(&pool, "bob").await;

    let err = cast_vote(&pool, new_id(), bob.id, &[])
        .await
        .expect_err("a poll that is not stored cannot take a ballot");
    assert!(
        matches!(err, AppError::NotFound),
        "the gone poll is the 404, not a database failure: {err}"
    );

    // And a poll deleted after the endpoint read it: the lock catches
    // it, and the half-open transaction leaves nothing behind.
    let posted = create_status(&pool, &alice).await;
    let stand = create_for(&pool, posted.id, &params(&["tea"])).await;
    let choice = options(&pool, stand.id)
        .await
        .expect("options read")
        .first()
        .expect("the fixture poll has its option")
        .id;
    let removed = sqlx::query("DELETE FROM statuses WHERE id = $1")
        .bind(posted.id)
        .execute(&pool)
        .await
        .expect("the status delete runs");
    assert_eq!(removed.rows_affected(), 1, "the fixture status was live");
    let cascaded = cast_vote(&pool, stand.id, bob.id, &[choice])
        .await
        .expect_err("the cascaded poll is gone at the lock");
    assert!(
        matches!(cascaded, AppError::NotFound),
        "a poll deleted under the endpoint is the 404: {cascaded}"
    );
}
