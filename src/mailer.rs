//! Outbound email: the `mailer.send` job, the transports that deliver it and
//! the templates later tickets build messages from.
//!
//! The reference is Pleroma's mail pipeline (`lib/pleroma/emails/mailer.ex`
//! and `lib/pleroma/workers/mailer_worker.ex`): an email is never sent
//! inside a request, it is queued as a background job and the worker's
//! handler performs the SMTP conversation, retrying a dead relay on the
//! queue's backoff. `docs/ARCHITECTURE.md` decision 11 keeps that queue in
//! Postgres, so [`deliver_later`] enqueues a [`KIND_SEND`] job on
//! [`QUEUE_MAILER`] and [`handle`] — registered by [`crate::serve`] — runs
//! one attempt.
//!
//! Mail is off until an operator sets `SMTP_HOST` (decision 12): while it is
//! unset, [`deliver_later`] logs at debug and drops the email without
//! queueing anything, and [`transport_from_config`] hands the state a
//! [`DisabledTransport`] that refuses everything. When it is set, the state
//! carries the real [`SmtpTransport`]; tests inject a [`MemoryTransport`],
//! which records messages instead of sending them, and read it back with
//! [`AppState::mailbox`](crate::state::AppState::mailbox).
//!
//! The failure split follows the job runtime's rules (the [`JobError`]
//! vocabulary in [`crate::jobs`]): a recipient that is not an address can
//! never send, so the handler discards; anything the relay says — timeouts,
//! refusals, a lost connection — is retried.

pub mod templates;

use std::{
    any::Any,
    error::Error,
    fmt,
    future::Future,
    pin::Pin,
    sync::{Arc, Mutex, MutexGuard, PoisonError},
};

use lettre::{
    Address, AsyncTransport as _, Message, Tokio1Executor,
    message::{Mailbox, MultiPart},
    transport::smtp::{AsyncSmtpTransport, authentication::Credentials},
};
use serde_json::Value;
use tracing::debug;

use crate::{
    config::{Config, SmtpTls},
    error::{AppError, AppResult},
    jobs::{AttemptInfo, HandlerFuture, JobError, QUEUE_MAILER},
    models::job::{NewJob, enqueue},
    state::AppState,
};

/// The job kind of one queued email, on [`QUEUE_MAILER`].
pub const KIND_SEND: &str = "mailer.send";

/// One outbound email: what [`deliver_later`] queues, what the [`KIND_SEND`]
/// job carries and what a [`MailTransport`] sends.
///
/// The field names are the job's JSON argument vocabulary: an email
/// serializes to `{"to", "subject", "text_body", "html_body"}`.
#[derive(Clone, Debug, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct Email {
    /// Recipient address, the `To:` header.
    to: String,
    /// Subject line, the `Subject:` header.
    subject: String,
    /// Plain-text body.
    text_body: String,
    /// Optional HTML body; a message with one sends the pair as an
    /// alternative, leaving the client's reader pick the text.
    #[serde(default)]
    html_body: Option<String>,
}

impl Email {
    /// A text-only email to `to` with `subject` and `text_body`.
    #[must_use]
    pub fn new(to: &str, subject: &str, text_body: &str) -> Self {
        Self {
            to: to.to_owned(),
            subject: subject.to_owned(),
            text_body: text_body.to_owned(),
            html_body: None,
        }
    }

    /// The same email with `html_body` as its alternative HTML part.
    #[must_use]
    pub fn with_html_body(mut self, html_body: &str) -> Self {
        self.html_body = Some(html_body.to_owned());
        self
    }

    /// The recipient address.
    #[must_use]
    pub fn to(&self) -> &str {
        &self.to
    }

    /// The subject line.
    #[must_use]
    pub fn subject(&self) -> &str {
        &self.subject
    }

    /// The plain-text body.
    #[must_use]
    pub fn text_body(&self) -> &str {
        &self.text_body
    }

    /// The optional HTML body.
    #[must_use]
    pub fn html_body(&self) -> Option<&str> {
        self.html_body.as_deref()
    }
}

/// Why a message did not leave this instance.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum MailError {
    /// No relay is configured (`SMTP_HOST` unset), so nothing can be sent.
    NotConfigured,
    /// An address cannot be used as given; the detail says which and why.
    /// Retrying an unreadable address never helps, so this failure is
    /// permanent.
    Address(String),
    /// The SMTP conversation failed; the detail is the relay's or the
    /// client's own words.
    Smtp(String),
}

impl fmt::Display for MailError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::NotConfigured => f.write_str("mail is not configured: SMTP_HOST is unset"),
            Self::Address(detail) => write!(f, "invalid email address: {detail}"),
            Self::Smtp(detail) => write!(f, "SMTP delivery failed: {detail}"),
        }
    }
}

impl Error for MailError {}

/// The boxed future a [`MailTransport`] delivers on.
pub type MailFuture<'a> = Pin<Box<dyn Future<Output = Result<(), MailError>> + Send + 'a>>;

/// How an email leaves this instance.
///
/// The production implementation is [`SmtpTransport`], built from the
/// `SMTP_*` settings; [`MemoryTransport`] is the test stand-in that records
/// instead of sending. The [`KIND_SEND`] handler always goes through the
/// state's transport, so a test asserts on what was handed to it, never on
/// network traffic.
///
/// `Debug` is a supertrait so the `Arc<dyn MailTransport + Send + Sync>` in
/// [`AppState`] keeps its derived `Debug` — with the
/// relay's password redacted by the hand-written impl on [`SmtpTransport`].
pub trait MailTransport: fmt::Debug {
    /// Sends `email`, reporting a [`MailError`] on failure.
    fn deliver<'a>(&'a self, email: &'a Email) -> MailFuture<'a>;

    /// Self as a `dyn Any`, so the test-support
    /// [`AppState::mailbox`](crate::state::AppState::mailbox) can find the
    /// [`MemoryTransport`] behind the state's `Arc<dyn MailTransport>`.
    fn as_any(&self) -> &dyn Any;
}

/// The production transport: hands each message to the `SMTP_HOST` relay
/// over SMTP, as the `SMTP_*` and `MAIL_FROM` settings describe.
///
/// One lettre transport is built per send; without lettre's `pool` feature
/// there is no shared state to keep, and the queue gives every message a job
/// of its own anyway.
#[derive(Clone, PartialEq, Eq)]
pub struct SmtpTransport {
    /// Relay host (`SMTP_HOST`), the only reason this exists.
    host: String,
    /// Relay port (`SMTP_PORT`).
    port: u16,
    /// Relay login name (`SMTP_USERNAME`).
    username: Option<String>,
    /// Relay login secret (`SMTP_PASSWORD`). Secret material: the manual
    /// `Debug` impl below redacts it.
    password: Option<String>,
    /// How the connection is encrypted (`SMTP_TLS`).
    tls: SmtpTls,
    /// The `From:` address (`MAIL_FROM`).
    from: String,
}

impl SmtpTransport {
    /// The transport `config` describes, `None` while `SMTP_HOST` is unset —
    /// the same gate [`deliver_later`] applies before queueing.
    #[must_use]
    pub fn from_config(config: &Config) -> Option<Self> {
        let host = config.smtp_host.clone()?;
        Some(Self {
            host,
            port: config.smtp_port,
            username: config.smtp_username.clone(),
            password: config.smtp_password.clone(),
            tls: config.smtp_tls,
            from: config.mail_from.clone(),
        })
    }

    /// The lettre transport for one send: the [`SmtpTls`] mode picks the
    /// builder (`relay` for SMTPS, `starttls_relay` for required STARTTLS,
    /// `builder_dangerous` for plaintext), then the configured port and —
    /// only when both halves are present — the login.
    fn build(&self) -> Result<AsyncSmtpTransport<Tokio1Executor>, lettre::transport::smtp::Error> {
        let mut builder = match self.tls {
            SmtpTls::Tls => AsyncSmtpTransport::<Tokio1Executor>::relay(&self.host)?,
            SmtpTls::Starttls => AsyncSmtpTransport::<Tokio1Executor>::starttls_relay(&self.host)?,
            SmtpTls::None => AsyncSmtpTransport::<Tokio1Executor>::builder_dangerous(&self.host),
        };
        builder = builder.port(self.port);
        if let (Some(username), Some(password)) = (&self.username, &self.password) {
            builder = builder.credentials(Credentials::new(username.clone(), password.clone()));
        }
        Ok(builder.build())
    }
}

/// `password` is secret material and renders as `<redacted>`; everything
/// else shows.
impl fmt::Debug for SmtpTransport {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("SmtpTransport")
            .field("host", &self.host)
            .field("port", &self.port)
            .field("username", &self.username)
            .field("password", &"<redacted>")
            .field("tls", &self.tls)
            .field("from", &self.from)
            .finish()
    }
}

impl MailTransport for SmtpTransport {
    fn deliver<'a>(&'a self, email: &'a Email) -> MailFuture<'a> {
        Box::pin(async move {
            let message = build_message(&self.from, email)?;
            let transport = self.build().map_err(smtp_failure)?;
            let response = transport.send(message).await.map_err(smtp_failure)?;
            debug!(
                to = %email.to(),
                answer = response.first_line().unwrap_or_default(),
                "mailer: relay accepted the message"
            );
            Ok(())
        })
    }

    fn as_any(&self) -> &dyn Any {
        self
    }
}

/// The test transport: records every message it is handed and reports
/// success. Inject it as the state's transport in tests; a [`SmtpTransport`]
/// never runs in one.
#[derive(Clone, Debug, Default)]
pub struct MemoryTransport {
    /// Everything handed to [`deliver`](MailTransport::deliver), oldest
    /// first. Shared by clone, so any handle sees every delivery.
    messages: Arc<Mutex<Vec<Email>>>,
}

impl MemoryTransport {
    /// An empty mailbox.
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// A snapshot of the recorded messages, oldest delivery first.
    #[must_use]
    pub fn messages(&self) -> Vec<Email> {
        lock_recorded(&self.messages).clone()
    }

    /// The number of messages recorded so far.
    #[must_use]
    pub fn len(&self) -> usize {
        lock_recorded(&self.messages).len()
    }

    /// Whether nothing has been recorded yet.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }
}

impl MailTransport for MemoryTransport {
    fn deliver<'a>(&'a self, email: &'a Email) -> MailFuture<'a> {
        Box::pin(async move {
            lock_recorded(&self.messages).push(email.clone());
            Ok(())
        })
    }

    fn as_any(&self) -> &dyn Any {
        self
    }
}

/// The stand-in the production state carries while `SMTP_HOST` is unset.
///
/// Nothing should ever ask it to send — [`deliver_later`] drops emails
/// before queueing when mail is disabled — but a job that outlived its
/// configuration gets the honest [`MailError::NotConfigured`] instead of a
/// silent success.
#[derive(Clone, Copy, Debug, Default)]
pub struct DisabledTransport;

impl MailTransport for DisabledTransport {
    fn deliver<'a>(&'a self, _email: &'a Email) -> MailFuture<'a> {
        Box::pin(async move { Err(MailError::NotConfigured) })
    }

    fn as_any(&self) -> &dyn Any {
        self
    }
}

/// Locks a [`MemoryTransport`]'s records, taking the guard from a poisoned
/// lock as-is: a panicking task cannot corrupt the recorded `Vec`, it can
/// only leave it shorter than the deliveries attempted.
fn lock_recorded(messages: &Arc<Mutex<Vec<Email>>>) -> MutexGuard<'_, Vec<Email>> {
    messages.lock().unwrap_or_else(PoisonError::into_inner)
}

/// The transport [`crate::serve`] puts in [`AppState::mailer`]:
/// [`SmtpTransport`] when `SMTP_HOST` is configured, [`DisabledTransport`]
/// while it is not.
#[must_use]
pub fn transport_from_config(config: &Config) -> Arc<dyn MailTransport + Send + Sync> {
    match SmtpTransport::from_config(config) {
        Some(transport) => Arc::new(transport),
        None => Arc::new(DisabledTransport),
    }
}

/// Queues `email` for delivery: a [`KIND_SEND`] job on [`QUEUE_MAILER`]
/// whose JSON arguments are the email itself.
///
/// Nothing touches the relay here, exactly like Pleroma's
/// `Mailer.deliver_async`. And while `SMTP_HOST` is unset mail is disabled:
/// this logs at debug and reports success without queueing anything — an
/// instance with no relay is not a broken instance, and dropping a
/// notification email is not a request failure.
///
/// # Errors
///
/// A database failure arrives as [`AppError::Database`]; the only other
/// failure is an email that cannot serialize to JSON, reported as
/// [`AppError::Internal`].
pub async fn deliver_later(state: &AppState, email: Email) -> AppResult<()> {
    if state.config.smtp_host.is_none() {
        debug!(to = %email.to(), "mailer disabled: no SMTP_HOST, the email is not queued");
        return Ok(());
    }
    let args = serde_json::to_value(&email)
        .map_err(|err| AppError::Internal(format!("could not serialize an email: {err}")))?;
    let queued = enqueue(
        &state.db,
        NewJob {
            queue: QUEUE_MAILER,
            kind: KIND_SEND,
            args,
            run_at: None,
            max_attempts: None,
            unique_key: None,
        },
    )
    .await?;
    if queued.is_none() {
        debug!(to = %email.to(), "mailer: a send job is already live for this email");
    }
    Ok(())
}

/// Sends `email` through the relay `config` describes, right now, on the
/// calling task.
///
/// The synchronous door around [`SmtpTransport`] — Pleroma's
/// `Mailer.deliver`: for the rare caller that must see the SMTP answer
/// itself instead of queueing it. [`deliver_later`] is the path every
/// request handler should take.
///
/// # Errors
///
/// [`MailError::NotConfigured`] while `SMTP_HOST` is unset,
/// [`MailError::Address`] for a sender or recipient that is not an address,
/// [`MailError::Smtp`] for anything the relay says.
pub async fn send_now(config: &Config, email: &Email) -> Result<(), MailError> {
    let transport = SmtpTransport::from_config(config).ok_or(MailError::NotConfigured)?;
    transport.deliver(email).await
}

/// The [`KIND_SEND`] handler, registered by [`crate::serve`]: decodes the
/// queued email and sends it through the state's transport.
pub fn handle(state: AppState, _attempt: AttemptInfo, args: Value) -> HandlerFuture {
    Box::pin(async move { send_queued(state, args).await })
}

/// Runs one send attempt and classifies its failure: a payload that can
/// never work — arguments that are not an [`Email`], a recipient that is not
/// an address — is discarded, anything the transport fails transiently is
/// retried on the queue's backoff.
async fn send_queued(state: AppState, args: Value) -> Result<(), JobError> {
    let email = serde_json::from_value::<Email>(args).map_err(|err| {
        JobError::Discard(format!("mailer.send arguments are not an Email: {err}"))
    })?;
    if let Err(err) = checked_recipient(&email) {
        return Err(JobError::Discard(err.to_string()));
    }
    state.mailer.deliver(&email).await.map_err(|err| {
        let detail = err.to_string();
        match err {
            MailError::Address(_) => JobError::Discard(format!("unusable address: {detail}")),
            MailError::NotConfigured | MailError::Smtp(_) => JobError::Retry(detail),
        }
    })
}

/// The email's recipient as a bare [`Address`] — the handler's gate before
/// any transport sees the message: a recipient that is not an address can
/// never be sent to, whatever the transport is.
fn checked_recipient(email: &Email) -> Result<Address, MailError> {
    email
        .to
        .parse::<Address>()
        .map_err(|err| MailError::Address(format!("{}: {err}", email.to)))
}

/// The lettre message for `email`, sent as `from`: both addresses are
/// parsed (a bad one is [`MailError::Address`], permanent), and the body is
/// plain text or a text/HTML alternative pair.
fn build_message(from: &str, email: &Email) -> Result<Message, MailError> {
    let sender = from
        .parse::<Mailbox>()
        .map_err(|err| MailError::Address(format!("invalid sender {from}: {err}")))?;
    let recipient = checked_recipient(email)?;
    let builder = Message::builder()
        .from(sender)
        .to(Mailbox::from(recipient))
        .subject(email.subject.clone());
    match &email.html_body {
        Some(html) => builder
            .multipart(MultiPart::alternative_plain_html(
                email.text_body.clone(),
                html.clone(),
            ))
            .map_err(smtp_failure),
        None => builder.body(email.text_body.clone()).map_err(smtp_failure),
    }
}

/// Wraps a lettre failure (the builder's or the transport's) as the
/// retryable [`MailError::Smtp`].
fn smtp_failure<E: fmt::Display>(err: E) -> MailError {
    MailError::Smtp(err.to_string())
}

#[cfg(test)]
mod tests {
    use sqlx::{PgPool, postgres::PgPoolOptions};

    use super::*;
    use crate::{
        config::Config, http_client::HttpClient, jobs::AttemptInfo, models::job::Job,
        uploads::UploaderBackend,
    };

    /// A queued email, distinct in every field the job must carry.
    fn sample_email() -> Email {
        Email::new(
            "someone@example.org",
            "Confirm your account",
            "Open the link to confirm.",
        )
        .with_html_body("<p>Open the link to confirm.</p>")
    }

    /// A [`Config`] whose `SMTP_HOST` is set (mail enabled).
    fn enabled_config() -> Config {
        let mut config = Config::for_tests();
        config.smtp_host = Some("smtp.example.org".to_owned());
        config
    }

    /// A lazy pool for handler tests that never touch the database.
    fn lazy_pool() -> PgPool {
        PgPoolOptions::new()
            .connect_lazy("postgres://localhost/unused")
            .unwrap()
    }

    /// An [`AppState`] over `pool` with `config` and a fresh
    /// [`MemoryTransport`], returned together so the test can assert on the
    /// same mailbox through both handles.
    fn state_with_mailbox(pool: PgPool, config: Config) -> (AppState, MemoryTransport) {
        let config = Arc::new(config);
        let hub = crate::streaming::hub::Hub::new(&pool);
        let mailbox = MemoryTransport::new();
        let state = AppState {
            db: pool,
            hub,
            config: Arc::clone(&config),
            http_client: HttpClient::from(config.as_ref()),
            uploader: UploaderBackend::from(config.as_ref()),
            instance_stats_cache: Arc::default(),
            translation_languages_cache: Arc::default(),
            rate_limiter: Arc::new(crate::rate_limit::RateLimiter::from_config(config.as_ref())),
            mailer: Arc::new(mailbox.clone()),
        };
        (state, mailbox)
    }

    /// The same over the lazy pool, for tests no database backs.
    fn mailbox_state() -> (AppState, MemoryTransport) {
        state_with_mailbox(lazy_pool(), enabled_config())
    }

    /// The same with `transport` in place of the memory mailbox.
    fn state_with_transport(transport: Arc<dyn MailTransport + Send + Sync>) -> AppState {
        let config = Arc::new(enabled_config());
        let db = lazy_pool();
        AppState {
            hub: crate::streaming::hub::Hub::new(&db),
            db,
            config: Arc::clone(&config),
            http_client: HttpClient::from(config.as_ref()),
            uploader: UploaderBackend::from(config.as_ref()),
            instance_stats_cache: Arc::default(),
            translation_languages_cache: Arc::default(),
            rate_limiter: Arc::new(crate::rate_limit::RateLimiter::from_config(config.as_ref())),
            mailer: transport,
        }
    }

    /// The job rows on the mailer queue (every mailer test expects at most
    /// one).
    async fn fetch_job(pool: &PgPool) -> Option<Job> {
        sqlx::query_as::<_, Job>("SELECT * FROM jobs WHERE queue = $1")
            .bind(QUEUE_MAILER)
            .fetch_optional(pool)
            .await
            .unwrap()
    }

    /// The handler's view of `email` as stored job arguments.
    fn args_for(email: &Email) -> Value {
        serde_json::to_value(email).unwrap()
    }

    /// A handler-level call: run [`handle`] over `args` on `state`.
    async fn run_handler(state: AppState, args: Value) -> Result<(), JobError> {
        let attempt = AttemptInfo {
            attempts: 1,
            max_attempts: 5,
        };
        handle(state, attempt, args).await
    }

    /// The JSON an [`Email`] serializes to: the field names are the job's
    /// argument vocabulary.
    fn json_expected(email: &Email) -> Value {
        serde_json::json!({
            "to": email.to(),
            "subject": email.subject(),
            "text_body": email.text_body(),
            "html_body": email.html_body(),
        })
    }

    #[tokio::test]
    async fn email_round_trips_through_its_job_arguments() {
        let email = sample_email();
        let decoded: Email = serde_json::from_value(json_expected(&email)).unwrap();
        assert_eq!(decoded, email, "the queued shape re-reads verbatim");
    }

    #[test]
    fn email_without_html_serializes_the_key_as_null() {
        let email = Email::new("a@b.example", "s", "t");
        assert_eq!(
            serde_json::to_value(&email).unwrap(),
            serde_json::json!({
                "to": "a@b.example",
                "subject": "s",
                "text_body": "t",
                "html_body": null,
            }),
            "the stored shape always carries all four keys"
        );
        let decoded: Email = serde_json::from_value(serde_json::json!({
            "to": "a@b.example",
            "subject": "s",
            "text_body": "t",
        }))
        .unwrap();
        assert_eq!(decoded, email, "html_body is optional when decoding");
    }

    #[sqlx::test(migrator = "crate::MIGRATOR")]
    async fn deliver_later_enqueues_a_mailer_send_job(pool: PgPool) {
        let (state, _mailbox) = state_with_mailbox(pool.clone(), enabled_config());
        let email = sample_email();

        deliver_later(&state, email.clone()).await.unwrap();

        let job = fetch_job(&pool).await.expect("the job row exists");
        assert_eq!(job.queue, QUEUE_MAILER, "queued on the mailer queue");
        assert_eq!(job.kind, KIND_SEND, "job kind");
        assert_eq!(job.args, json_expected(&email), "the email as arguments");
    }

    #[sqlx::test(migrator = "crate::MIGRATOR")]
    async fn deliver_later_without_smtp_host_queues_nothing(pool: PgPool) {
        let (state, _mailbox) = state_with_mailbox(pool.clone(), Config::for_tests());

        deliver_later(&state, sample_email()).await.unwrap();

        assert!(
            fetch_job(&pool).await.is_none(),
            "a disabled mailer drops the email without queueing"
        );
    }

    #[tokio::test]
    async fn handler_delivers_through_the_state_transport() {
        let (state, mailbox) = mailbox_state();
        let email = sample_email();

        run_handler(state.clone(), args_for(&email)).await.unwrap();

        assert_eq!(
            mailbox.messages(),
            vec![email],
            "the mailbox holds the email"
        );
        let via_accessor = state.mailbox().expect("the state carries a mailbox");
        assert_eq!(
            via_accessor.messages(),
            mailbox.messages(),
            "AppState::mailbox finds the state's recording transport"
        );
    }

    #[tokio::test]
    async fn handler_discards_arguments_that_are_not_an_email() {
        let (state, mailbox) = mailbox_state();

        let outcome = run_handler(state, serde_json::json!({"nonsense": true})).await;

        match outcome {
            Err(JobError::Discard(detail)) => assert!(
                detail.contains("not an Email"),
                "the discard says why: {detail}"
            ),
            other => panic!("malformed arguments must discard, got {other:?}"),
        }
        assert!(mailbox.is_empty(), "nothing was delivered");
    }

    #[tokio::test]
    async fn handler_discards_an_invalid_recipient() {
        let (state, mailbox) = mailbox_state();
        let email = Email::new("not-an-address", "s", "t");

        let outcome = run_handler(state, args_for(&email)).await;

        match outcome {
            Err(JobError::Discard(detail)) => assert!(
                detail.contains("not-an-address"),
                "the discard names the address: {detail}"
            ),
            other => panic!("an invalid address must discard, got {other:?}"),
        }
        assert!(
            mailbox.is_empty(),
            "a doomed address never reaches the transport"
        );
    }

    /// A transport that always fails the way a dead relay does.
    #[derive(Debug)]
    struct FailingTransport;

    impl MailTransport for FailingTransport {
        fn deliver<'a>(&'a self, _email: &'a Email) -> MailFuture<'a> {
            Box::pin(async move { Err(MailError::Smtp("connection refused".to_owned())) })
        }

        fn as_any(&self) -> &dyn Any {
            self
        }
    }

    #[tokio::test]
    async fn handler_retries_smtp_failures() {
        let state = state_with_transport(Arc::new(FailingTransport));

        let outcome = run_handler(state, args_for(&sample_email())).await;

        match outcome {
            Err(JobError::Retry(detail)) => assert!(
                detail.contains("connection refused"),
                "the retry carries the relay's words: {detail}"
            ),
            other => panic!("a relay failure must retry, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn handler_retries_mail_that_is_not_configured() {
        let state = state_with_transport(Arc::new(DisabledTransport));

        let outcome = run_handler(state, args_for(&sample_email())).await;

        match outcome {
            Err(JobError::Retry(detail)) => assert!(
                detail.contains("SMTP_HOST"),
                "the retry names the missing setting: {detail}"
            ),
            other => panic!("a vanished relay must retry, got {other:?}"),
        }
    }

    #[test]
    fn smtp_transport_is_none_while_mail_is_disabled() {
        assert!(
            SmtpTransport::from_config(&Config::for_tests()).is_none(),
            "no SMTP_HOST, no transport"
        );
        let transport = SmtpTransport::from_config(&enabled_config()).expect("enabled");
        assert_eq!(transport.host, "smtp.example.org", "host");
        assert_eq!(transport.port, 587, "default port");
    }

    #[test]
    fn smtp_transport_debug_redacts_the_password() {
        let mut config = enabled_config();
        config.smtp_username = Some("relay-user".to_owned());
        config.smtp_password = Some("relay-secret".to_owned());
        let debug = format!(
            "{:?}",
            SmtpTransport::from_config(&config).expect("enabled")
        );
        assert!(
            !debug.contains("relay-secret"),
            "the secret leaked: {debug}"
        );
        assert!(
            debug.contains(r#"password: "<redacted>""#),
            "the marker names the field: {debug}"
        );
    }

    #[tokio::test]
    async fn send_now_without_smtp_host_reports_not_configured() {
        let config = Config::for_tests();
        let outcome = send_now(&config, &sample_email()).await;
        assert_eq!(
            outcome,
            Err(MailError::NotConfigured),
            "there is no relay to ask"
        );
    }

    #[tokio::test]
    async fn disabled_transport_refuses_everything() {
        let outcome = DisabledTransport.deliver(&sample_email()).await;
        assert_eq!(
            outcome,
            Err(MailError::NotConfigured),
            "the stand-in is honest"
        );
    }

    #[test]
    fn transport_from_config_picks_by_smtp_host() {
        let enabled = transport_from_config(&enabled_config());
        assert!(
            enabled.as_any().downcast_ref::<SmtpTransport>().is_some(),
            "a configured relay gets the SMTP transport"
        );
        let disabled = transport_from_config(&Config::for_tests());
        assert!(
            disabled
                .as_any()
                .downcast_ref::<DisabledTransport>()
                .is_some(),
            "an unset relay gets the stand-in"
        );
    }

    #[test]
    fn mail_error_display_states_the_failure() {
        assert_eq!(
            MailError::NotConfigured.to_string(),
            "mail is not configured: SMTP_HOST is unset",
            "the disabled case"
        );
        assert_eq!(
            MailError::Address("who: no at sign".to_owned()).to_string(),
            "invalid email address: who: no at sign",
            "an address failure"
        );
        assert_eq!(
            MailError::Smtp("connection refused".to_owned()).to_string(),
            "SMTP delivery failed: connection refused",
            "a relay failure"
        );
    }

    #[tokio::test]
    async fn memory_transport_records_in_delivery_order() {
        let mailbox = MemoryTransport::new();
        let first = Email::new("a@b.example", "first", "one");
        let second = Email::new("c@d.example", "second", "two");

        mailbox.deliver(&first).await.unwrap();
        mailbox.deliver(&second).await.unwrap();

        assert_eq!(
            mailbox.messages(),
            vec![first, second],
            "both recorded, in order"
        );
        assert_eq!(mailbox.len(), 2, "len tracks the records");
        assert!(!mailbox.is_empty(), "and emptiness says so");
    }

    #[test]
    fn checked_recipient_accepts_bare_addresses_only() {
        assert!(
            checked_recipient(&Email::new("someone@example.org", "s", "t")).is_ok(),
            "a bare address is sendable"
        );
        assert!(
            checked_recipient(&Email::new("Misspelled Address", "s", "t")).is_err(),
            "free text is not"
        );
    }
}
