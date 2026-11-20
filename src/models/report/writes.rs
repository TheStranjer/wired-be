//! Filing a report and reading one back: [`create`] writes the row and its
//! cited statuses in one transaction, [`get`] reads the one report behind a
//! primary key, [`set_ap_id`] lands the federated `Flag`'s id on a row
//! [`create`] left bare, and [`statuses_for`] reads the live statuses a
//! report cites.

use sqlx::PgPool;
use uuid::Uuid;

use crate::{
    error::{AppError, AppResult},
    models::user::User,
    util::id::new_id,
};

use super::{NewReport, Report, SELF_REPORT, local_activity_id};

/// [`create`]: the id this module mints (decision 4), what the reporter
/// filed, and the `ActivityPub` id of the `Flag` this report will federate as
/// (decision 6). `state`, `assigned_to`, `action_taken_at`,
/// `action_taken_by` and the two timestamps are left to the column defaults:
/// a filing opens the report, assigns nobody and acts on nothing.
/// `RETURNING *` brings the row back with those stamps.
const INSERT_SQL: &str = "INSERT INTO reports (
     id, reporter_id, target_id, comment, category, forwarded, rule_ids, ap_id
) VALUES ($1, $2, $3, $4, $5, $6, $7, $8) RETURNING *";

/// [`create`]'s second write: the statuses the report cites, in the same
/// transaction as the row they belong to, so a report is never visible with
/// half its context. `ON CONFLICT (report_id, status_id) DO NOTHING` makes a
/// status cited twice in one request one row (the pair is the table's key).
/// Unknown ids are the caller's question and this statement's failure: the
/// foreign key refuses them and the transaction rolls back.
const ATTACH_STATUSES_SQL: &str = "INSERT INTO report_statuses (report_id, status_id)
     SELECT $1::uuid, id FROM unnest($2::uuid[]) AS requested(id)
ON CONFLICT (report_id, status_id) DO NOTHING";

/// [`get`]: the one report behind a primary key.
const GET_SQL: &str = "SELECT * FROM reports WHERE id = $1";

/// [`set_ap_id`]: the mirror write of an inbound `Flag` — the filed report
/// keeps its identity, only the activity id it stands for arrives. Unknown
/// ids match no row and change nothing.
const SET_AP_ID_SQL: &str = "UPDATE reports SET ap_id = $2 WHERE id = $1";

/// [`statuses_for`]: the statuses one report cites, oldest status first —
/// the `UUIDv7` text sorts in creation order (decision 4), which is the order
/// Mastodon's `status_ids` array reads in. The join on `statuses` is what
/// keeps a deleted post out of the answer: its `report_statuses` row survives
/// the cascade-free tombstone, and a report does not cite a status nobody can
/// see.
const STATUSES_FOR_SQL: &str = "SELECT rs.status_id FROM report_statuses rs
     JOIN statuses s ON s.id = rs.status_id AND s.deleted_at IS NULL
     WHERE rs.report_id = $1 ORDER BY rs.status_id";

/// Files `reporter`'s report about `target` and returns the stored row.
///
/// The write is the row and its status context in one transaction, so a
/// report is never readable without the statuses it cites. Filing twice is
/// two reports: nothing here deduplicates a pair, and every filing mints its
/// own id (decision 4).
///
/// `base_url` is the instance origin, i.e. [`Config::base_url`], which the
/// model layer is handed rather than reading configuration itself; a reporter
/// this instance owns mints a fresh `{base_url}/activities/{uuid}` for the
/// `Flag` this report federates as (decision 6), a remote one stores `None`
/// until the federation ticket mirrors the activity it arrived with.
///
/// [`Config::base_url`]: crate::config::Config::base_url
///
/// # Errors
///
/// [`AppError::Unprocessable`] with `"Can not report yourself"` when the two
/// users are the same account. Any database failure (connection, encoding, a
/// cited status or rule that names no row) arrives as
/// [`AppError::Database`](crate::error::AppError).
pub async fn create(
    pool: &PgPool,
    base_url: &str,
    reporter: &User,
    target: &User,
    new: &NewReport,
) -> AppResult<Report> {
    if reporter.id == target.id {
        return Err(AppError::Unprocessable(SELF_REPORT.to_owned()));
    }
    let mut tx = pool.begin().await?;
    let row = sqlx::query_as::<_, Report>(INSERT_SQL)
        .bind(new_id())
        .bind(reporter.id)
        .bind(target.id)
        .bind(&new.comment)
        .bind(new.category)
        .bind(new.forwarded)
        .bind(&new.rule_ids)
        .bind(local_activity_id(base_url, reporter).as_deref())
        .fetch_one(&mut *tx)
        .await?;
    if !new.status_ids.is_empty() {
        let _attached = sqlx::query(ATTACH_STATUSES_SQL)
            .bind(row.id)
            .bind(&new.status_ids)
            .execute(&mut *tx)
            .await?;
    }
    tx.commit().await?;
    Ok(row)
}

/// The report behind the primary key `id`, `None` when no report carries it.
///
/// # Errors
///
/// Any database failure (connection, encoding) arrives as
/// [`AppError::Database`](crate::error::AppError).
pub async fn get(pool: &PgPool, id: Uuid) -> AppResult<Option<Report>> {
    let row = sqlx::query_as::<_, Report>(GET_SQL)
        .bind(id)
        .fetch_optional(pool)
        .await?;
    Ok(row)
}

/// Records `ap_id` as the `Flag` activity the stored report `id` stands
/// for: the federation mirror's write (decision 6).
///
/// [`create`] leaves a remote reporter's row without an id because that id
/// is the one the reporter's own instance minted, and this is where the
/// inbound `Flag` activity's id lands on it. The unique `ap_id` index
/// refuses a second report under one id, and an unknown `id` matches no
/// row and changes nothing.
///
/// # Errors
///
/// Any database failure (connection, encoding, an `ap_id` another row
/// already carries) arrives as [`AppError::Database`](crate::error::AppError).
pub async fn set_ap_id(pool: &PgPool, id: Uuid, ap_id: &str) -> AppResult<()> {
    sqlx::query(SET_AP_ID_SQL)
        .bind(id)
        .bind(ap_id)
        .execute(pool)
        .await
        .map(drop)?;
    Ok(())
}

/// The ids of the live statuses `report_id` cites, oldest status first.
///
/// What the entity's `status_ids` array and (later) a moderation view of the
/// report's context read. A report that cited nothing, or one whose every
/// cited post has since been deleted, answers the empty `Vec`; the
/// `report_statuses` rows themselves stay, because deleting a post does not
/// un-cite it for the record.
///
/// # Errors
///
/// Any database failure (connection, encoding) arrives as
/// [`AppError::Database`](crate::error::AppError).
pub async fn statuses_for(pool: &PgPool, report_id: Uuid) -> AppResult<Vec<Uuid>> {
    let rows = sqlx::query_as::<_, (Uuid,)>(STATUSES_FOR_SQL)
        .bind(report_id)
        .fetch_all(pool)
        .await?;
    Ok(rows.into_iter().map(|(status_id,)| status_id).collect())
}

#[cfg(test)]
mod tests {
    use chrono::Utc;

    use crate::{
        config::TEST_BASE_URL,
        models::{
            instance_rule,
            report::{Category, NewReport, ReportState, list, test_support::*},
        },
        util::id::parse_id,
    };

    use super::*;

    /// A filed report round-trips every column it was given, and the defaults
    /// the reporter left alone come back from the database.
    #[tokio::test]
    async fn a_report_is_filed_and_read_back() {
        let db = crate::test_db::test_db().await.expect("test database slot");
        let pool = db.pool().clone();
        let alice = local_user(&pool, "alice").await;
        let bob = local_user(&pool, "bob").await;
        let rule = instance_rule::create(&pool, "No harassment", "", 1)
            .await
            .expect("the rule fixture inserts");
        let cited = post(&pool, &bob, "the post being reported").await;

        let row = create(
            &pool,
            TEST_BASE_URL,
            &alice,
            &bob,
            &NewReport {
                comment: "this is harassment".to_owned(),
                category: Category::Violation,
                forwarded: true,
                rule_ids: vec![rule.id],
                status_ids: vec![cited.id],
            },
        )
        .await
        .expect("the report files");

        assert!(row.id.get_version_num() == 7, "the id is a UUIDv7");
        assert_eq!(row.reporter_id, alice.id, "who filed it");
        assert_eq!(row.target_id, bob.id, "who it is about");
        assert_eq!(row.comment, "this is harassment", "the comment verbatim");
        assert_eq!(row.category, Category::Violation, "the cited category");
        assert!(row.forwarded, "the forward request");
        assert_eq!(row.state, ReportState::Open, "a fresh report is open");
        assert_eq!(row.assigned_to, None, "and unassigned");
        assert_eq!(row.action_taken_at, None, "with nothing done about it");
        assert_eq!(row.rule_ids, vec![rule.id], "the cited rule");
        assert!(row.ap_id.is_some(), "a local reporter mints a Flag id");
        assert!(
            parse_id(&activity_id(&row)).is_some(),
            "the minted id is a UUID"
        );
        assert!(row.inserted_at <= Utc::now(), "the database stamps it");
        assert_eq!(
            stored(&pool, row.id).await,
            row,
            "get() reads back the row it wrote"
        );
        assert_eq!(
            statuses_for(&pool, row.id).await.unwrap(),
            vec![cited.id],
            "the cited status is attached"
        );

        let listed = list(&pool, page("")).await.unwrap();
        assert_eq!(ids_of(&listed), vec![row.id], "the listing holds it");
    }

    /// A filing names its reporter, its target and nothing else: the defaults
    /// the reporter left alone come back from the database.
    #[tokio::test]
    async fn a_bare_filing_takes_every_column_default() {
        let db = crate::test_db::test_db().await.expect("test database slot");
        let pool = db.pool().clone();
        let alice = local_user(&pool, "alice").await;
        let bob = local_user(&pool, "bob").await;

        let row = seed(&pool, &alice, &bob).await;

        assert_eq!(row.reporter_id, alice.id, "who filed it");
        assert_eq!(row.target_id, bob.id, "who it is about");
        assert_eq!(row.comment, "", "no comment is the empty comment");
        assert_eq!(row.category, Category::Other, "the default category");
        assert!(!row.forwarded, "reports are not forwarded by default");
        assert_eq!(row.state, ReportState::Open, "a fresh report is open");
        assert_eq!(row.assigned_to, None, "and nobody owns it yet");
        assert_eq!(row.action_taken_at, None, "with nothing done about it");
        assert!(row.rule_ids.is_empty(), "no rule cited is no rules");
        assert!(
            row.inserted_at <= Utc::now() && row.updated_at <= Utc::now(),
            "both timestamps are stamped"
        );
        assert!(
            statuses_for(&pool, row.id).await.unwrap().is_empty(),
            "and no status attached"
        );
        assert_eq!(citation_count(&pool).await, 0);
    }

    #[tokio::test]
    async fn a_remote_reporter_mints_no_activity_id() {
        let db = crate::test_db::test_db().await.expect("test database slot");
        let pool = db.pool().clone();
        let carol = remote_user(&pool, "carol").await;
        let alice = local_user(&pool, "alice").await;

        let row = seed(&pool, &carol, &alice).await;

        assert!(
            row.ap_id.is_none(),
            "a remote reporter's id is the one its own instance minted"
        );
        assert!(
            stored(&pool, row.id).await.ap_id.is_none(),
            "and nothing was stored in its place"
        );
    }

    /// The federation mirror's write: [`set_ap_id`] lands the delivered
    /// `Flag`'s id on the row [`create`] left bare, keeps the row itself,
    /// and is refused by the unique index when the id already stands.
    #[tokio::test]
    async fn set_ap_id_lands_the_flag_id_on_the_row() {
        let db = crate::test_db::test_db().await.expect("test database slot");
        let pool = db.pool().clone();
        let carol = remote_user(&pool, "carol").await;
        let alice = local_user(&pool, "alice").await;
        let bob = remote_user(&pool, "bob").await;
        let row = seed(&pool, &carol, &alice).await;
        let flag = "https://remote.example/activities/flag-42";

        set_ap_id(&pool, row.id, flag).await.unwrap();
        let mirrored = stored(&pool, row.id).await;
        assert_eq!(mirrored.ap_id.as_deref(), Some(flag), "the id landed");
        assert_eq!(mirrored.id, row.id, "the row itself did not move");
        assert_eq!(
            mirrored.reporter_id, row.reporter_id,
            "and nothing else did"
        );

        set_ap_id(&pool, row.id, flag)
            .await
            .expect("restating the same id is no error");

        let second = seed(&pool, &carol, &bob).await;
        let taken = set_ap_id(&pool, second.id, flag)
            .await
            .expect_err("one Flag ids one report");
        assert!(
            matches!(taken, AppError::Database(_)),
            "the unique index is the guard: {taken}"
        );

        set_ap_id(&pool, new_id(), "https://remote.example/activities/orphan")
            .await
            .expect("an unknown report id changes nothing and is no error");
    }

    /// Filing is not idempotent on the pair: a second report is a second row
    /// with a second id and a second `Flag` id (Mastodon allows repeat
    /// reports, and so does Pleroma's `Flag`).
    #[tokio::test]
    async fn the_same_pair_may_report_twice() {
        let db = crate::test_db::test_db().await.expect("test database slot");
        let pool = db.pool().clone();
        let alice = local_user(&pool, "alice").await;
        let bob = local_user(&pool, "bob").await;

        let first = seed(&pool, &alice, &bob).await;
        let second = seed(&pool, &alice, &bob).await;

        assert_ne!(first.id, second.id, "two filings, two ids");
        assert_ne!(first.ap_id, second.ap_id, "and two activity ids");
        assert_eq!(report_count(&pool).await, 2, "both are stored");
        assert_eq!(
            ids_of(&list(&pool, page("")).await.unwrap()),
            vec![second.id, first.id],
            "newest filing first"
        );
    }

    /// A status cited twice in one filing is one row, and the context reads
    /// back in the statuses' own creation order rather than the request's.
    #[tokio::test]
    async fn citing_one_status_twice_attaches_it_once() {
        let db = crate::test_db::test_db().await.expect("test database slot");
        let pool = db.pool().clone();
        let alice = local_user(&pool, "alice").await;
        let bob = local_user(&pool, "bob").await;
        let first = post(&pool, &bob, "first").await;
        let second = post(&pool, &bob, "second").await;

        let row = create(
            &pool,
            TEST_BASE_URL,
            &alice,
            &bob,
            &NewReport {
                status_ids: vec![second.id, first.id, second.id],
                ..bare()
            },
        )
        .await
        .expect("the repeat citation files");

        assert_eq!(
            statuses_for(&pool, row.id).await.unwrap(),
            vec![first.id, second.id],
            "oldest status first, each cited once"
        );
        assert_eq!(citation_count(&pool).await, 2);
    }

    /// A cited status that names no row is the foreign key's refusal, and the
    /// transaction's: the report row goes with it, so no report is ever stored
    /// with half its context.
    #[tokio::test]
    async fn citing_an_unknown_status_files_nothing() {
        let db = crate::test_db::test_db().await.expect("test database slot");
        let pool = db.pool().clone();
        let alice = local_user(&pool, "alice").await;
        let bob = local_user(&pool, "bob").await;

        let err = create(
            &pool,
            TEST_BASE_URL,
            &alice,
            &bob,
            &NewReport {
                status_ids: vec![Uuid::now_v7()],
                ..bare()
            },
        )
        .await
        .expect_err("the foreign key refuses");
        assert!(
            matches!(err, AppError::Database(_)),
            "a constraint failure is the database variant: {err}"
        );
        assert_eq!(report_count(&pool).await, 0, "the row rolled back");
        assert_eq!(citation_count(&pool).await, 0);
    }

    #[tokio::test]
    async fn reporting_yourself_is_refused() {
        let db = crate::test_db::test_db().await.expect("test database slot");
        let pool = db.pool().clone();
        let alice = local_user(&pool, "alice").await;

        let err = create(&pool, TEST_BASE_URL, &alice, &alice, &bare())
            .await
            .expect_err("nobody reports themselves");
        assert!(
            matches!(err, AppError::Unprocessable(_)),
            "a self-report is the 422 the endpoint renders: {err}"
        );
        assert_eq!(err.to_string(), "Can not report yourself", "its wording");
        assert_eq!(report_count(&pool).await, 0, "and it stored nothing");
    }

    #[tokio::test]
    async fn getting_an_unknown_report_is_no_row() {
        let db = crate::test_db::test_db().await.expect("test database slot");
        let pool = db.pool().clone();

        assert_eq!(
            get(&pool, Uuid::now_v7()).await.unwrap(),
            None,
            "an id no report carries is the None answer, not an error"
        );
        assert!(
            statuses_for(&pool, Uuid::now_v7())
                .await
                .unwrap()
                .is_empty(),
            "and its context is the empty list"
        );
        assert!(
            list(&pool, page("")).await.unwrap().is_empty(),
            "a fresh table lists nothing"
        );
    }

    /// A cited post that is deleted stops being part of the report's context
    /// while the report itself stands: [`statuses_for`] reads live rows.
    #[tokio::test]
    async fn a_deleted_status_leaves_the_context_it_was_cited_in() {
        let db = crate::test_db::test_db().await.expect("test database slot");
        let pool = db.pool().clone();
        let alice = local_user(&pool, "alice").await;
        let bob = local_user(&pool, "bob").await;
        let kept = post(&pool, &bob, "still here").await;
        let doomed = post(&pool, &bob, "about to go").await;
        let row = create(
            &pool,
            TEST_BASE_URL,
            &alice,
            &bob,
            &NewReport {
                status_ids: vec![kept.id, doomed.id],
                ..bare()
            },
        )
        .await
        .expect("the fixture report files");
        assert_eq!(statuses_for(&pool, row.id).await.unwrap().len(), 2);

        let tombstone = sqlx::query("UPDATE statuses SET deleted_at = now() WHERE id = $1")
            .bind(doomed.id)
            .execute(&pool)
            .await
            .expect("the fixture deletes the post");
        assert_eq!(tombstone.rows_affected(), 1, "one post went");

        assert_eq!(
            statuses_for(&pool, row.id).await.unwrap(),
            vec![kept.id],
            "the surviving citation is the answer"
        );
        assert_eq!(
            citation_count(&pool).await,
            2,
            "the record of what was cited stays"
        );
    }
}
