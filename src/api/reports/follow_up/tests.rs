//! Tests for the post-filing hook: the letters to the staff.

use std::sync::Arc;

use sqlx::PgPool;

use super::*;
use crate::{
    config::{Config, TEST_BASE_URL},
    http_client::HttpClient,
    mailer::MemoryTransport,
    models::{
        job::Job,
        report::{self, Category},
        user::User,
    },
    rate_limit::RateLimiter,
    streaming::hub::Hub,
    uploads::UploaderBackend,
};

/// Test state over `pool` on `config`, with the recording transport: the
/// shape both helpers below build.
fn state_with(pool: &PgPool, config: Config) -> AppState {
    let config = Arc::new(config);
    AppState {
        db: pool.clone(),
        hub: Hub::new(pool),
        config: Arc::clone(&config),
        http_client: HttpClient::from(config.as_ref()),
        uploader: UploaderBackend::from(config.as_ref()),
        instance_stats_cache: Arc::default(),
        mrf_config_cache: Arc::default(),
        translation_languages_cache: Arc::default(),
        rate_limiter: Arc::new(RateLimiter::from_config(config.as_ref())),
        mailer: Arc::new(MemoryTransport::new()),
    }
}

/// The mail-enabled state: [`email_staff`] queues a letter only when a
/// relay is configured, and the suite's default config names none.
///
/// [`email_staff`]: super::email_staff
fn mail_state(pool: &PgPool) -> AppState {
    let mut config = Config::for_tests();
    config.smtp_host = Some("smtp.example.org".to_owned());
    state_with(pool, config)
}

/// Every queued `mailer.send` job, oldest first.
async fn letters(pool: &PgPool) -> Vec<Job> {
    sqlx::query_as::<_, Job>("SELECT * FROM jobs WHERE kind = $1 ORDER BY id")
        .bind(mailer::KIND_SEND)
        .fetch_all(pool)
        .await
        .expect("the job read runs")
}

/// A local account named `nickname` with `address` as its email and the
/// admin role when `admin` says so.
async fn person(pool: &PgPool, nickname: &str, address: &str, admin: bool) -> User {
    user::insert(
        pool,
        &user::NewUser {
            email: Some(address.to_owned()),
            is_admin: user::Flag::new(admin),
            ..user::NewUser::local(nickname, &user::local_ap_id(TEST_BASE_URL, nickname))
        },
    )
    .await
    .expect("the account fixture inserts")
}

/// `reporter`'s filed report about `target`, with a comment the letter
/// quotes.
async fn filed_row(pool: &PgPool, reporter: &User, target: &User) -> ReportRow {
    report::create(
        pool,
        TEST_BASE_URL,
        reporter,
        target,
        &report::NewReport {
            comment: "a letter, please".to_owned(),
            category: Category::Other,
            forwarded: false,
            rule_ids: vec![],
            status_ids: vec![],
        },
    )
    .await
    .expect("the report fixture files")
}

/// The recipients the queued letters name, oldest first.
async fn recipients(pool: &PgPool) -> Vec<String> {
    letters(pool)
        .await
        .iter()
        .map(|job| {
            job.args["to"]
                .as_str()
                .expect("a queued letter names its recipient")
                .to_owned()
        })
        .collect()
}

#[tokio::test]
async fn a_filed_report_letters_every_mailable_staff_account() {
    let db = crate::test_db::test_db().await.expect("test database slot");
    let pool = db.pool().clone();
    let state = mail_state(&pool);
    let boss = person(&pool, "letter-boss", "boss@example.org", true).await;
    let _deputy = person(&pool, "letter-deputy", "deputy@example.org", true).await;
    let reporter = person(&pool, "letter-reporter", "ring@example.org", false).await;
    let target = person(&pool, "letter-target", "target@example.org", false).await;
    let row = filed_row(&pool, &reporter, &target).await;

    email_staff(&state, &row).await;

    assert_eq!(
        recipients(&pool).await,
        vec![
            "boss@example.org".to_owned(),
            "deputy@example.org".to_owned()
        ],
        "one letter per mailable staff account, in creation order"
    );
    let queued = letters(&pool).await;
    let first = queued.first().expect("the admin's letter");
    assert_eq!(
        first.queue,
        crate::jobs::QUEUE_MAILER,
        "on the mailer queue"
    );
    assert_eq!(first.args["subject"], "Wired Report", "the subject");
    assert!(
        first.args["text_body"]
            .as_str()
            .is_some_and(|body| body.contains("@letter-reporter")
                && body.contains("@letter-target")
                && body.contains("a letter, please")),
        "the body names both accounts and quotes the comment: {}",
        first.args["text_body"]
    );
    assert!(boss.is_admin.get(), "the fixture's own admin");
}

#[tokio::test]
async fn the_reporter_is_left_out_of_its_own_reports_letters() {
    let db = crate::test_db::test_db().await.expect("test database slot");
    let pool = db.pool().clone();
    let state = mail_state(&pool);
    let boss = person(&pool, "self-boss", "boss2@example.org", true).await;
    let target = person(&pool, "self-target", "target2@example.org", false).await;
    let row = filed_row(&pool, &boss, &target).await;

    email_staff(&state, &row).await;

    assert!(
        recipients(&pool).await.is_empty(),
        "the instance's only staffer filed the report, so nobody is written to"
    );
}

#[tokio::test]
async fn a_staff_account_without_an_address_gets_no_letter() {
    let db = crate::test_db::test_db().await.expect("test database slot");
    let pool = db.pool().clone();
    let state = mail_state(&pool);
    let _silent = user::insert(
        &pool,
        &user::NewUser {
            is_moderator: user::Flag::new(true),
            ..user::NewUser::local(
                "silent-mod",
                &user::local_ap_id(TEST_BASE_URL, "silent-mod"),
            )
        },
    )
    .await
    .expect("the addressless moderator inserts");
    let reporter = person(&pool, "silent-reporter", "ring2@example.org", false).await;
    let target = person(&pool, "silent-target", "target3@example.org", false).await;
    let row = filed_row(&pool, &reporter, &target).await;

    email_staff(&state, &row).await;

    assert!(
        recipients(&pool).await.is_empty(),
        "a staffer with no mailbox is not written to"
    );
}

#[tokio::test]
async fn a_report_that_names_no_accounts_letters_nobody() {
    let db = crate::test_db::test_db().await.expect("test database slot");
    let pool = db.pool().clone();
    let state = mail_state(&pool);
    let boss = person(&pool, "orphan-boss", "boss3@example.org", true).await;
    let reporter = person(&pool, "orphan-reporter", "ring3@example.org", false).await;
    let target = person(&pool, "orphan-target", "target4@example.org", false).await;
    let row = filed_row(&pool, &reporter, &target).await;

    email_staff(
        &state,
        &ReportRow {
            target_id: Uuid::now_v7(),
            ..row.clone()
        },
    )
    .await;

    assert!(
        recipients(&pool).await.is_empty(),
        "a report whose target the cascade already took names nobody to write about"
    );
    email_staff(&state, &row).await;
    assert_eq!(
        recipients(&pool).await,
        vec!["boss3@example.org".to_owned()],
        "and the same hook on a whole row writes the letter"
    );
    assert!(boss.is_admin.get(), "the fixture's own admin");
}

#[tokio::test]
async fn no_relay_means_no_letter_and_no_reads() {
    let db = crate::test_db::test_db().await.expect("test database slot");
    let pool = db.pool().clone();
    let state = state_with(&pool, Config::for_tests());
    let boss = person(&pool, "quiet-boss", "boss4@example.org", true).await;
    let reporter = person(&pool, "quiet-reporter", "ring4@example.org", false).await;
    let target = person(&pool, "quiet-target", "target5@example.org", false).await;
    let row = filed_row(&pool, &reporter, &target).await;

    email_staff(&state, &row).await;

    assert!(
        letters(&pool).await.is_empty(),
        "`SMTP_HOST` unset drops the letter before anything is read"
    );
    assert_eq!(
        boss.email.as_deref(),
        Some("boss4@example.org"),
        "the staffer was there all along"
    );
}

#[tokio::test]
async fn a_failing_staff_read_is_logged_and_not_raised() {
    let pool = sqlx::postgres::PgPoolOptions::new()
        .connect_lazy("postgres://localhost/wired_no_such_database")
        .expect("a lazy pool needs no server");
    let state = mail_state(&pool);
    let now = chrono::Utc::now();
    let row = ReportRow {
        id: Uuid::now_v7(),
        reporter_id: Uuid::now_v7(),
        target_id: Uuid::now_v7(),
        comment: String::new(),
        category: Category::Other,
        forwarded: false,
        state: report::ReportState::Open,
        assigned_to: None,
        action_taken_at: None,
        action_taken_by: None,
        rule_ids: vec![],
        ap_id: None,
        inserted_at: now,
        updated_at: now,
    };

    email_staff(&state, &row).await;

    assert!(
        report::get(&pool, row.id).await.is_err(),
        "the pool this test ran on really cannot answer a read"
    );
}
