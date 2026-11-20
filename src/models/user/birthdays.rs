//! The birthday listing behind `GET /api/v1/pleroma/birthdays`:
//! [`birthday_accounts`] answers the viewer's own and their accepted
//! follows' accounts whose birthday falls on the queried calendar day.

use sqlx::PgPool;
use uuid::Uuid;

use super::User;
use crate::error::AppResult;

/// The `SELECT` behind [`birthday_accounts`]: the `month`/`day` match is on
/// the calendar parts of the `birthday` column, so a `2000-02-29` birthday
/// lands only on a `2`/`29` query — a leap day, and no other day, ever
/// matches it (Pleroma's `birthday_month`/`birthday_day` filters of
/// `User.Query` compare the same two extracted parts). A `NULL` birthday
/// extracts to `NULL` and fails the comparison, so the explicit
/// `birthday IS NOT NULL` states what the comparison already implies.
const BIRTHDAYS_SQL: &str = "SELECT * FROM users
      WHERE is_active AND show_birthday AND birthday IS NOT NULL
        AND EXTRACT(MONTH FROM birthday)::int = $2
        AND EXTRACT(DAY FROM birthday)::int = $3
        AND (id = $1 OR id IN (
                 SELECT followee_id FROM follows
                  WHERE follower_id = $1 AND state = 'accepted'))
   ORDER BY id DESC";

/// The accounts whose birthday `GET /api/v1/pleroma/birthdays` answers for
/// `viewer_id`: the accounts it names.
///
/// That is the viewer itself and the accounts of their *accepted* follows,
/// each with [`show_birthday`](User::show_birthday) set and a
/// [`birthday`](User::birthday) falling on `month`/`day`, newest account
/// first.
///
/// A deactivated account is out (the flag the whole API hides suspended
/// rows behind), and a remote mirror is in: the follows of a local account
/// are local and remote alike.
///
/// # Errors
///
/// Any database failure (connection, encoding) arrives as
/// [`AppError::Database`](crate::error::AppError).
pub async fn birthday_accounts(
    pool: &PgPool,
    viewer_id: Uuid,
    month: i32,
    day: i32,
) -> AppResult<Vec<User>> {
    let users = sqlx::query_as::<_, User>(BIRTHDAYS_SQL)
        .bind(viewer_id)
        .bind(month)
        .bind(day)
        .fetch_all(pool)
        .await?;
    Ok(users)
}

#[cfg(test)]
mod tests {
    use chrono::NaiveDate;

    use super::*;
    use crate::models::user::{Flag, NewUser, insert, test_support::*};

    /// A local user named `nickname` born on `birthday`, showing it only
    /// when `show` says so.
    async fn born(pool: &PgPool, nickname: &str, birthday: Option<NaiveDate>, show: bool) -> User {
        insert(
            pool,
            &NewUser {
                birthday,
                show_birthday: Flag::new(show),
                ..local_new(nickname)
            },
        )
        .await
        .expect("the birthday fixture inserts")
    }

    /// `year-month-day` as a date, from the parts a test names.
    fn date(year: i32, month: u32, day: u32) -> NaiveDate {
        NaiveDate::from_ymd_opt(year, month, day).expect("the fixture date is a real day")
    }

    /// Deactivates `target` — the state a suspended or closed account is in.
    async fn deactivate(pool: &PgPool, target: &User) {
        let gone = sqlx::query("UPDATE users SET is_active = false WHERE id = $1")
            .bind(target.id)
            .execute(pool)
            .await
            .expect("the fixture deactivates");
        assert_eq!(gone.rows_affected(), 1, "the row was there to close");
    }

    /// Records `follower`'s accepted follow of the local `target`.
    async fn follow(pool: &PgPool, follower: &User, target: &User) {
        let row = crate::models::follow::follow(pool, BASE, follower, target)
            .await
            .expect("the fixture follow records");
        assert_eq!(
            row.state,
            crate::models::follow::FollowState::Accepted,
            "an open local account answers on the spot"
        );
    }

    #[tokio::test]
    async fn birthday_accounts_answers_the_viewer_and_their_follows() {
        let db = crate::test_db::test_db().await.expect("test database slot");
        let pool = db.pool().clone();
        let day = date(1990, 5, 17);
        let alice = born(&pool, "alice", Some(day), true).await;
        let followed = born(&pool, "followed", Some(day), true).await;
        let unfollowed = born(&pool, "unfollowed", Some(day), true).await;
        let private = born(&pool, "private", Some(day), false).await;
        let closed = born(&pool, "closed", Some(day), true).await;
        let blank = born(&pool, "blank", None, true).await;
        let wrong_day = born(&pool, "wrong-day", Some(date(1990, 5, 18)), true).await;
        deactivate(&pool, &closed).await;
        // A remote follow counts once it is accepted: the request a mirror
        // leaves pending is no follow for the listing.
        let ap_id = "https://remote.example/users/mirror";
        let mirror = insert(
            &pool,
            &NewUser {
                nickname: "mirror@remote.example".to_owned(),
                ap_id: ap_id.to_owned(),
                domain: Some("remote.example".to_owned()),
                birthday: Some(day),
                show_birthday: Flag::new(true),
                ..NewUser::default()
            },
        )
        .await
        .expect("the remote birthday fixture inserts");
        follow(&pool, &alice, &followed).await;

        let seen = ids_of(&birthday_accounts(&pool, alice.id, 5, 17).await.unwrap());
        assert!(
            seen.contains(&alice.id),
            "the viewer is in their own birthday list"
        );
        assert!(seen.contains(&followed.id), "so is an accepted follow");
        for absent in [unfollowed.id, private.id, closed.id, blank.id, wrong_day.id] {
            assert!(
                !seen.contains(&absent),
                "{absent} is not on the list: an account the viewer does not \
                 follow, one that hides its birthday, a closed one, one with \
                 no birthday and one born on another day are all out"
            );
        }

        let request = crate::models::follow::follow(&pool, BASE, &alice, &mirror)
            .await
            .expect("the outgoing request records");
        assert_eq!(
            request.state,
            crate::models::follow::FollowState::Pending,
            "a mirror answers nothing on the spot"
        );
        assert_eq!(
            ids_of(&birthday_accounts(&pool, alice.id, 5, 17).await.unwrap()),
            seen,
            "a pending request to a remote account adds nobody"
        );
        let accepted = crate::models::follow::accept(&pool, request.id)
            .await
            .expect("the fixture's other side accepts it");
        assert_eq!(
            accepted.state,
            crate::models::follow::FollowState::Accepted,
            "the request is answered"
        );
        let after = ids_of(&birthday_accounts(&pool, alice.id, 5, 17).await.unwrap());
        assert!(
            after.contains(&mirror.id),
            "and the accepted follow of a remote account is in: local and \
             remote follows count alike"
        );
    }

    #[tokio::test]
    async fn a_leap_day_birthday_matches_the_leap_day_only() {
        let db = crate::test_db::test_db().await.expect("test database slot");
        let pool = db.pool().clone();
        let alice = born(&pool, "alice", Some(date(2000, 2, 29)), true).await;

        let on = ids_of(&birthday_accounts(&pool, alice.id, 2, 29).await.unwrap());
        assert_eq!(on, vec![alice.id], "2/29 finds the account born on it");
        for (month, day) in [(2, 28), (3, 29), (2, 30), (13, 29)] {
            assert!(
                birthday_accounts(&pool, alice.id, month, day)
                    .await
                    .unwrap()
                    .is_empty(),
                "and {month}/{day} does not: the match is on both calendar \
                 parts, so in a year without a 29th of February the list is \
                 simply empty"
            );
        }
    }

    #[tokio::test]
    async fn birthday_accounts_orders_newest_account_first() {
        let db = crate::test_db::test_db().await.expect("test database slot");
        let pool = db.pool().clone();
        let day = date(1990, 12, 25);
        let alice = born(&pool, "alice", Some(day), true).await;
        let first = born(&pool, "first", Some(day), true).await;
        let second = born(&pool, "second", Some(day), true).await;
        follow(&pool, &alice, &first).await;
        follow(&pool, &alice, &second).await;

        let mut expected = vec![alice.id, first.id, second.id];
        // `ORDER BY id DESC` on `UUIDv7` text is creation order (decision 4);
        // the fixture ids are minted in the same millisecond, so the
        // expectation is the byte order the database sorts on, not the
        // insert order.
        expected.sort_by(|left, right| right.cmp(left));
        let seen = ids_of(&birthday_accounts(&pool, alice.id, 12, 25).await.unwrap());
        assert_eq!(
            seen, expected,
            "newest account first, the viewer among them"
        );
        assert!(
            birthday_accounts(&pool, alice.id, 1, 1)
                .await
                .unwrap()
                .is_empty(),
            "a day nobody the viewer knows is born on answers nothing, \
             which is no error"
        );
    }
}
