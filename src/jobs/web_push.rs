//! The Web Push delivery job: one notification to one subscription.
//!
//! `docs/ARCHITECTURE.md` decision 11 keeps background work in Postgres
//! rows; this is the consumer of the `push.deliver` jobs the [`notify`]
//! hook queues when a notification lands for a local account (the
//! counterpart of Pleroma's `Pleroma.Web.Push.Impl.build` + `deliver`
//! fan-out, `lib/pleroma/web/push/impl.ex`). [`handle_deliver`] (job kind
//! [`KIND_DELIVER`], queue [`QUEUE_DEFAULT`](super::QUEUE_DEFAULT),
//! [`MAX_ATTEMPTS`] tries) takes
//! the `{notification_id, subscription_id}` pair, re-reads both rows and
//! runs the same two gates Pleroma's fan-out runs: the subscription's
//! [`alerts`](web_push::alert_enabled) flag for the notification's type,
//! and its policy — `all` wakes for anyone, `followed` only for actors the
//! recipient follows, `follower` only for the recipient's followers,
//! `none` for nobody.
//!
//! Past the gates the payload is the Mastodon-shaped JSON document
//! [`web_push::payload_json`] builds — the subscription's access-token
//! string, the notification id and type, the actor's avatar as `icon`, the
//! `"<actor display name> <verb>"` title, and the status's plain text as
//! `body`, blank when the recipient set
//! [`hide_notification_contents`](crate::models::user::notification_settings::NotificationSettings::hide_notification_contents)
//! — encrypted and VAPID-signed by [`crate::services::web_push`] and
//! `POST`ed.
//!
//! The failure mapping follows the ticket: a
//! [`Gone`](web_push::PushError::Gone) delivery (404/410 or keys that can
//! decrypt nothing) deletes the subscription and completes the job — there
//! is nothing to retry and nobody to wake again; a
//! [`Retryable`](web_push::PushError::Retryable) one (5xx, 429, transport,
//! or a database hiccup on the reads) hands the job back to the queue,
//! whose third failure discards it. A vanished notification, subscription
//! or token is not an error: whatever deleted them had the delivery's own
//! consent, so the job completes on nothing.
//!
//! [`notify`]: crate::services::notify

use serde::Deserialize;
use serde_json::{Value, json};
use sqlx::PgPool;
use tracing::debug;
use uuid::Uuid;

use crate::{
    entities::account::DEFAULT_AVATAR_PATH,
    error::AppError,
    jobs::{AttemptInfo, HandlerFuture, JobError},
    models::{
        follow,
        notification::{self, Notification},
        oauth_token, status,
        user::{self, User, notification_settings::NotificationSettings},
        web_push_subscription::{self, WebPushSubscription},
    },
    services::{vapid, web_push},
    state::AppState,
    util::{format, sanitize},
};

/// The job kind of one Web Push delivery, handled by [`handle_deliver`] on
/// [`QUEUE_DEFAULT`](super::QUEUE_DEFAULT).
pub const KIND_DELIVER: &str = "push.deliver";
/// The attempt budget the [`notify`](crate::services::notify) hook queues
/// each delivery with: push services flap, and three tries with the
/// queue's backoff is a real chance to land without an infinite chain on
/// a dead peer.
pub const MAX_ATTEMPTS: i32 = 3;
/// The longest delivered `body`, in characters — Pleroma's `@body_chars`
/// in `lib/pleroma/web/push/impl.ex`.
const BODY_CHARS: usize = 140;

/// The `{notification_id, subscription_id}` arguments of a delivery job,
/// as [`job_args`] writes and [`handle_deliver`] reads them.
#[derive(Debug, Deserialize)]
struct DeliverArgs {
    /// The notification whose facts the payload reports.
    notification_id: Uuid,
    /// The subscription to wake.
    subscription_id: Uuid,
}

/// The job-arguments document for one delivery: the pair [`handle_deliver`]
/// re-reads, so the job survives either row's deletion between enqueue and
/// attempt.
#[must_use]
pub fn job_args(notification_id: Uuid, subscription_id: Uuid) -> Value {
    json!({
        "notification_id": notification_id,
        "subscription_id": subscription_id,
    })
}

/// Reads [`DeliverArgs`] out of a claimed job's payload; arguments this
/// shape cannot deserialize are permanent — the job is discarded.
fn parse_args(args: &Value) -> Result<DeliverArgs, JobError> {
    serde_json::from_value::<DeliverArgs>(args.clone())
        .map_err(|err| JobError::Discard(format!("web push job arguments are unusable: {err}")))
}

/// Lifts a database failure into the queue's retry arm; every read the job
/// does is idempotent, so another attempt costs nothing but the wait.
fn retry(err: &AppError) -> JobError {
    JobError::Retry(format!("web push delivery failed: {err}"))
}

/// The [`JobRegistry`](super::JobRegistry) handler for [`KIND_DELIVER`].
pub fn handle_deliver(state: AppState, _attempt: AttemptInfo, args: Value) -> HandlerFuture {
    Box::pin(async move {
        let DeliverArgs {
            notification_id,
            subscription_id,
        } = parse_args(&args)?;
        deliver(&state, notification_id, subscription_id).await
    })
}

/// The job's body: load the subscription, then run [`deliver_for`]. A
/// subscription the queue still holds a job for but the database no longer
/// has is already retired — complete quietly.
///
/// # Errors
///
/// [`JobError::Retry`] on database failure, and whatever [`deliver_for`]
/// reports.
async fn deliver(
    state: &AppState,
    notification_id: Uuid,
    subscription_id: Uuid,
) -> Result<(), JobError> {
    let found = web_push_subscription::find_by_id(&state.db, subscription_id)
        .await
        .map_err(|err| retry(&err))?;
    let Some(subscription) = found else {
        debug!(subscription = %subscription_id, "push subscription is already gone");
        return Ok(());
    };
    deliver_for(state, &subscription, notification_id).await
}

/// The gates: the subscription's owner, its notification, its alert flag
/// and its policy. Each way of having nothing to deliver is a silent
/// completion, not an error.
///
/// # Errors
///
/// [`JobError::Retry`] on database failure, and whatever [`send_note`]
/// reports.
async fn deliver_for(
    state: &AppState,
    subscription: &WebPushSubscription,
    notification_id: Uuid,
) -> Result<(), JobError> {
    let Some(recipient_id) = subscription.user_id else {
        debug!(subscription = %subscription.id, "app-only subscription has nobody to wake");
        return Ok(());
    };
    let found = notification::get(&state.db, recipient_id, notification_id)
        .await
        .map_err(|err| retry(&err))?;
    let Some(note) = found else {
        debug!(notification = %notification_id, "notification is gone before delivery");
        return Ok(());
    };
    if !web_push::alert_enabled(&subscription.alerts, &note.event_type) {
        debug!(
            subscription = %subscription.id,
            notification_type = %note.event_type,
            "the subscription's alert flag is off"
        );
        return Ok(());
    }
    if !policy_permits(&state.db, subscription, recipient_id, note.from_account_id).await? {
        debug!(
            subscription = %subscription.id,
            policy = %subscription.policy,
            "the subscription's policy keeps this actor out"
        );
        return Ok(());
    }
    send_note(state, subscription, recipient_id, &note).await
}

/// Whether `subscription`'s policy lets `actor_id` wake `recipient_id`:
/// the ticket's reading of Mastodon's four — `followed` asks whether the
/// recipient follows the actor, `follower` whether the actor follows the
/// recipient, and any other stored spelling (`none` included) stays dark.
///
/// # Errors
///
/// [`JobError::Retry`] on database failure.
async fn policy_permits(
    pool: &PgPool,
    subscription: &WebPushSubscription,
    recipient_id: Uuid,
    actor_id: Uuid,
) -> Result<bool, JobError> {
    match subscription.policy.as_str() {
        "all" => Ok(true),
        "followed" => follow::has_accepted_follow(pool, recipient_id, actor_id)
            .await
            .map_err(|err| retry(&err)),
        "follower" => follow::has_accepted_follow(pool, actor_id, recipient_id)
            .await
            .map_err(|err| retry(&err)),
        _ => Ok(false),
    }
}

/// The delivery itself: assemble the payload from the actor, the
/// recipient's settings, the status text and the token string, sign the
/// VAPID header, and send. A `Gone` answer retires the subscription and
/// completes; a `Retryable` one becomes the queue's retry.
///
/// # Errors
///
/// [`JobError::Retry`] on a database failure or a retryable push failure,
/// [`JobError::Discard`] for VAPID material that cannot sign at all.
async fn send_note(
    state: &AppState,
    subscription: &WebPushSubscription,
    recipient_id: Uuid,
    note: &Notification,
) -> Result<(), JobError> {
    let pool = &state.db;
    let Some(actor) = user::get_by_id(pool, note.from_account_id)
        .await
        .map_err(|err| retry(&err))?
    else {
        debug!(account = %note.from_account_id, "the notification's actor is gone");
        return Ok(());
    };
    let Some(recipient) = user::get_by_id(pool, recipient_id)
        .await
        .map_err(|err| retry(&err))?
    else {
        return Ok(());
    };
    let found = oauth_token::get_by_id(pool, subscription.token_id)
        .await
        .map_err(|err| retry(&err))?;
    let Some(token) = found else {
        debug!(token = %subscription.token_id, "the subscription's token is gone");
        return Ok(());
    };
    let body = body_text(pool, &recipient, note).await?;
    let icon = icon_for(state, &actor);
    let payload = web_push::payload_json(&web_push::PushPayload {
        access_token: &token.token,
        notification_id: note.id,
        notification_type: &note.event_type,
        icon: &icon,
        actor_display_name: display_name(&actor),
        body: &body,
    });
    let authorization = vapid_authorization(state, &subscription.endpoint).await?;
    match web_push::send(&state.http_client, subscription, &payload, &authorization).await {
        Ok(()) => {
            debug!(
                subscription = %subscription.id,
                notification = %note.id,
                "web push delivered"
            );
            Ok(())
        }
        Err(web_push::PushError::Gone) => {
            let deleted = web_push_subscription::delete_for_token(pool, subscription.token_id)
                .await
                .map_err(|err| retry(&err))?;
            debug!(
                subscription = %subscription.id,
                deleted,
                "push subscription retired by the push service"
            );
            Ok(())
        }
        Err(web_push::PushError::Retryable(detail)) => Err(JobError::Retry(detail)),
    }
}

/// The delivered `body`: the notification's status reduced to plain text
/// and cut to [`BODY_CHARS`] (Pleroma's `scrub_html_and_truncate`), empty
/// when the recipient hides notification contents or the notification
/// names no (living) status.
///
/// # Errors
///
/// [`JobError::Retry`] on database failure.
async fn body_text(
    pool: &PgPool,
    recipient: &User,
    note: &Notification,
) -> Result<String, JobError> {
    if NotificationSettings::for_user(recipient).hide_notification_contents {
        return Ok(String::new());
    }
    let Some(status_id) = note.status_id else {
        return Ok(String::new());
    };
    let Some(row) = status::get_by_id(pool, status_id)
        .await
        .map_err(|err| retry(&err))?
    else {
        return Ok(String::new());
    };
    let plain = sanitize::strip_html(&row.content);
    Ok(format::truncate_chars(plain.trim(), BODY_CHARS))
}

/// The delivered `icon`: the actor's avatar URL, or the same instance
/// default the [`Account`](crate::entities::account::Account) entity
/// renders when the actor has none.
fn icon_for(state: &AppState, actor: &User) -> String {
    actor
        .avatar_url
        .clone()
        .unwrap_or_else(|| format!("{}{DEFAULT_AVATAR_PATH}", state.config.base_url))
}

/// The title's first half: the actor's display name, its nickname when the
/// account never set one (what a client shows where a name belongs).
fn display_name(actor: &User) -> &str {
    if actor.name.is_empty() {
        &actor.nickname
    } else {
        &actor.name
    }
}

/// The `Authorization` header for one send: the stored VAPID pair (seeded
/// on first use, see [`vapid::vapid_public_key`]) signed for `endpoint`
/// with the instance's [`web_push::vapid_subject`].
///
/// # Errors
///
/// [`JobError::Retry`] on a database failure reading the pair,
/// [`JobError::Discard`] when the stored material cannot sign — a broken
/// key or endpoint no retry fixes.
async fn vapid_authorization(state: &AppState, endpoint: &str) -> Result<String, JobError> {
    use crate::models::instance_secret;

    let public_key = vapid::vapid_public_key(&state.db)
        .await
        .map_err(|err| retry(&err))?;
    let stored = instance_secret::get(&state.db, vapid::VAPID_PRIVATE_NAME)
        .await
        .map_err(|err| retry(&err))?;
    let private_pem = stored.ok_or_else(|| {
        JobError::Discard("the instance has no VAPID private key to sign with".to_owned())
    })?;
    web_push::vapid_headers(
        endpoint,
        &private_pem,
        &public_key,
        &web_push::vapid_subject(&state.config),
    )
    .map(|(authorization, _expires_at)| authorization)
    .map_err(|err| JobError::Discard(format!("the VAPID JWT could not be signed: {err}")))
}

#[cfg(test)]
mod tests {
    use std::sync::{Arc, Mutex};

    use axum::{
        Router,
        body::Bytes,
        http::{HeaderMap, StatusCode},
        routing::post,
    };
    use base64::{Engine as _, engine::general_purpose::URL_SAFE_NO_PAD};
    use chrono::Utc;
    use p256::{
        SecretKey,
        ecdsa::{Signature, VerifyingKey, signature::Verifier as _},
        elliptic_curve::{Generate as _, sec1::ToSec1Point as _},
    };
    use rand::RngCore as _;
    use tokio::{net::TcpListener, task::JoinHandle};

    use super::*;
    use crate::{
        config::{Config, TEST_BASE_URL},
        http_client::HttpClient,
        models::{
            notification::NewNotification,
            oauth_app::{NewOauthApp, create as create_app},
            oauth_token::{NewToken, OauthToken, create_token},
            user::{NewUser, insert},
            web_push_subscription::NewSubscription,
        },
        streaming::hub::Hub,
        uploads::UploaderBackend,
    };

    /// Test state over `pool` on the fixed test config, whose HTTP client
    /// reaches the loopback mocks the tests bind (decision 13's fixture is
    /// what the database-backed tests lease from).
    fn app_state(pool: &PgPool) -> AppState {
        let config = Arc::new(Config::for_tests());
        AppState {
            db: pool.clone(),
            hub: Hub::new(pool),
            config: Arc::clone(&config),
            http_client: HttpClient::from(config.as_ref()),
            uploader: UploaderBackend::from(config.as_ref()),
            instance_stats_cache: Arc::default(),
            mrf_config_cache: Arc::default(),
            translation_languages_cache: Arc::default(),
            rate_limiter: Arc::new(crate::rate_limit::RateLimiter::from_config(config.as_ref())),
            mailer: Arc::new(crate::mailer::MemoryTransport::new()),
        }
    }

    /// The first attempt of the [`MAX_ATTEMPTS`] the queue hands the job.
    const FIRST_ATTEMPT: AttemptInfo = AttemptInfo {
        attempts: 1,
        max_attempts: MAX_ATTEMPTS,
    };

    /// A local user displaying as `name`.
    async fn local(pool: &PgPool, nickname: &str, name: &str) -> User {
        insert(
            pool,
            &NewUser {
                name: name.to_owned(),
                ..NewUser::local(nickname, &user::local_ap_id(TEST_BASE_URL, nickname))
            },
        )
        .await
        .expect("the local fixture inserts")
    }

    /// An account with one push-scoped access token: the pair a
    /// subscription belongs to.
    struct Account {
        /// The user row.
        user: User,
        /// Its access token.
        token: OauthToken,
    }

    /// A fresh local account named `nickname` with its `push` token.
    async fn account(pool: &PgPool, nickname: &str) -> Account {
        let user = local(pool, nickname, "").await;
        let app = create_app(
            pool,
            &NewOauthApp {
                client_name: format!("Push Job {nickname}"),
                redirect_uris: "https://client.example/callback".to_owned(),
                scopes: "push".to_owned(),
                website: None,
                user_id: Some(user.id),
            },
        )
        .await
        .expect("the app fixture inserts");
        let token = create_token(
            pool,
            &NewToken {
                user_id: Some(user.id),
                app_id: app.id,
                scopes: "push".to_owned(),
                ttl: None,
            },
        )
        .await
        .expect("the token fixture inserts");
        Account { user, token }
    }

    /// The browser side of the subscription: the stored key pair beside
    /// the private scalar the test decrypts with.
    #[derive(Clone, Debug)]
    struct ClientKeys {
        /// The stored client public point.
        p256dh: String,
        /// The stored auth secret.
        auth: String,
        /// The private scalar, test-side only.
        private_key: String,
    }

    /// A fresh P-256 client key pair and 16-octet auth secret.
    fn client_keys() -> ClientKeys {
        let secret = SecretKey::try_generate().expect("a fresh P-256 client key");
        let mut auth = [0_u8; 16];
        rand::rngs::OsRng.fill_bytes(&mut auth);
        ClientKeys {
            p256dh: URL_SAFE_NO_PAD.encode(secret.public_key().to_sec1_point(false).as_bytes()),
            auth: URL_SAFE_NO_PAD.encode(auth),
            private_key: URL_SAFE_NO_PAD.encode(secret.to_bytes()),
        }
    }

    /// The delivery preferences a fixture registers with.
    struct Pref {
        /// The stored alerts object.
        alerts: Value,
        /// The stored policy.
        policy: &'static str,
    }

    /// [`subscribe`]'s default: every alert on (the empty object —
    /// [`web_push::alert_enabled`] reads unlisted types as on), policy `all`.
    fn open_pref() -> Pref {
        Pref {
            alerts: json!({}),
            policy: "all",
        }
    }

    /// Registers `acct`'s subscription at `endpoint` with `keys` and `pref`.
    async fn subscribe(
        pool: &PgPool,
        acct: &Account,
        endpoint: &str,
        keys: &ClientKeys,
        pref: &Pref,
    ) -> WebPushSubscription {
        web_push_subscription::create(
            pool,
            &NewSubscription {
                user_id: acct.user.id,
                token_id: acct.token.id,
                endpoint: endpoint.to_owned(),
                key_p256dh: keys.p256dh.clone(),
                key_auth: keys.auth.clone(),
                alerts: pref.alerts.clone(),
                policy: pref.policy.to_owned(),
            },
        )
        .await
        .expect("the subscription registers")
    }

    /// What one mock push service remembers of each request: its headers
    /// and body.
    type Seen = Arc<Mutex<Vec<(Vec<(String, String)>, Vec<u8>)>>>;

    /// The mock push service's state: the status it answers with and the
    /// capture requests land in.
    #[derive(Clone)]
    struct Mock {
        /// Status code every answer carries.
        status: u16,
        /// Where each request is recorded.
        seen: Seen,
    }

    /// The mock's handler: record the request, answer with the state's
    /// status.
    async fn catch_request(
        axum::extract::State(mock): axum::extract::State<Mock>,
        headers: HeaderMap,
        body: Bytes,
    ) -> StatusCode {
        let pairs = headers
            .iter()
            .map(|(name, value)| {
                (
                    name.as_str().to_owned(),
                    String::from_utf8_lossy(value.as_bytes()).into_owned(),
                )
            })
            .collect();
        mock.seen
            .lock()
            .expect("an unpoisoned lock")
            .push((pairs, body.to_vec()));
        StatusCode::from_u16(mock.status).expect("the test statuses are representable")
    }

    /// Serves `app` on `listener` until the test aborts the task.
    async fn serve_mock(listener: TcpListener, app: Router) {
        if let Err(err) = axum::serve(listener, app).await {
            tracing::debug!("test push service stopped: {err}");
        }
    }

    /// A mock push service answering every POST with `status`, on a fresh
    /// loopback port: `(url, capture, server)`.
    async fn mock_push_service(status: u16) -> (String, Seen, JoinHandle<()>) {
        let listener = TcpListener::bind("127.0.0.1:0")
            .await
            .expect("a loopback port to serve on");
        let url = format!(
            "http://{}/push",
            listener.local_addr().expect("the bound address")
        );
        let seen: Seen = Arc::default();
        let app = Router::new()
            .route("/push", post(catch_request))
            .with_state(Mock {
                status,
                seen: Arc::clone(&seen),
            });
        let server = tokio::spawn(serve_mock(listener, app));
        (url, seen, server)
    }

    /// The notification fixture: `recipient` told that `actor` did
    /// `event_type` about `status_id`.
    async fn note_about(
        pool: &PgPool,
        recipient: &User,
        actor: &User,
        event_type: &str,
        status_id: Option<Uuid>,
    ) -> Notification {
        notification::create(
            pool,
            &NewNotification {
                user_id: recipient.id,
                from_account_id: actor.id,
                notification_type: event_type,
                status_id,
                emoji: None,
                report_id: None,
            },
        )
        .await
        .expect("the notification fixture inserts")
    }

    /// Runs the job through the registered handler.
    async fn run(state: &AppState, note_id: Uuid, subscription_id: Uuid) -> Result<(), JobError> {
        handle_deliver(
            state.clone(),
            FIRST_ATTEMPT,
            job_args(note_id, subscription_id),
        )
        .await
    }

    /// The named header of the mock's last request.
    fn header(seen: &Seen, name: &str) -> String {
        let last = seen
            .lock()
            .expect("an unpoisoned lock")
            .last()
            .cloned()
            .expect("the mock served a request");
        let (headers, _body) = last;
        headers
            .into_iter()
            .find(|(key, _value)| key == name)
            .map(|(_key, value)| value)
            .unwrap_or_default()
    }

    /// The mock's last request body.
    fn captured_body(seen: &Seen) -> Vec<u8> {
        let last = seen
            .lock()
            .expect("an unpoisoned lock")
            .last()
            .cloned()
            .expect("the mock served a request");
        let (_headers, body) = last;
        body
    }

    /// Whether the mock saw any request.
    fn served(seen: &Seen) -> bool {
        !seen.lock().expect("an unpoisoned lock").is_empty()
    }

    /// Everything one delivery test drives.
    struct Setup {
        /// The job state.
        state: AppState,
        /// The acting account (display name `Alice`).
        actor: User,
        /// The notified recipient with its token.
        recipient: Account,
        /// The follow notification joining them.
        note: Notification,
        /// The recipient's registered subscription.
        subscription: WebPushSubscription,
        /// The browser key pair behind it.
        keys: ClientKeys,
        /// The mock's capture.
        seen: Seen,
        /// The mock's URL.
        url: String,
        /// The mock's server task.
        server: JoinHandle<()>,
    }

    /// A follow-notification delivery against a mock answering `status`,
    /// subscription open (`all`, every alert default-on).
    async fn setup(pool: &PgPool, status: u16) -> Setup {
        let actor = local(pool, &format!("a{status}-alice"), "Alice").await;
        let recipient = account(pool, &format!("a{status}-bob")).await;
        let (url, seen, server) = mock_push_service(status).await;
        let keys = client_keys();
        let subscription = subscribe(pool, &recipient, &url, &keys, &open_pref()).await;
        let note = note_about(
            pool,
            &recipient.user,
            &actor,
            notification::TYPE_FOLLOW,
            None,
        )
        .await;
        Setup {
            state: app_state(pool),
            actor,
            recipient,
            note,
            subscription,
            keys,
            seen,
            url,
            server,
        }
    }

    #[test]
    fn the_job_shape_round_trips_and_bad_args_discard() {
        let (note_id, subscription_id) = (Uuid::now_v7(), Uuid::now_v7());
        let args = job_args(note_id, subscription_id);
        assert_eq!(
            args["notification_id"],
            note_id.to_string(),
            "ids travel as decision 4's text"
        );
        assert_eq!(args["subscription_id"], subscription_id.to_string());
        let parsed = parse_args(&args).expect("the written shape reads back");
        assert_eq!(parsed.notification_id, note_id);
        assert_eq!(parsed.subscription_id, subscription_id);
        let err = parse_args(&json!({})).expect_err("a pair-less payload cannot run");
        assert!(
            matches!(err, JobError::Discard(_)),
            "unusable arguments are permanent: {err}"
        );
    }

    #[tokio::test]
    async fn a_missing_subscription_or_notification_completes_quietly() {
        let db = crate::test_db::test_db().await.expect("test database slot");
        let pool = db.pool().clone();
        let state = app_state(&pool);
        assert_eq!(
            run(&state, Uuid::now_v7(), Uuid::now_v7()).await,
            Ok(()),
            "rows that are gone retire the delivery"
        );
        let acct = account(&pool, "wp-dead-bob").await;
        let subscription = subscribe(
            &pool,
            &acct,
            "https://push.invalid/x",
            &client_keys(),
            &open_pref(),
        )
        .await;
        assert_eq!(
            run(&state, Uuid::now_v7(), subscription.id).await,
            Ok(()),
            "and so does a notification that never existed for the owner"
        );
    }

    #[tokio::test]
    async fn a_follow_wakes_the_subscription_with_a_signed_encrypted_payload() {
        let db = crate::test_db::test_db().await.expect("test database slot");
        let pool = db.pool().clone();
        let s = setup(&pool, 201).await;

        let outcome = run(&s.state, s.note.id, s.subscription.id).await;

        assert_eq!(outcome, Ok(()), "the mock accepted the delivery");
        s.server.abort();
        assert_eq!(
            header(&s.seen, "content-encoding"),
            "aes128gcm",
            "the coding is announced"
        );
        assert_eq!(
            header(&s.seen, "content-type"),
            "application/octet-stream",
            "the body is opaque to the service"
        );
        assert_eq!(header(&s.seen, "ttl"), web_push::TTL_HEADER, "four weeks");
        assert_eq!(header(&s.seen, "urgency"), "normal", "the middle urgency");
        let authorization = header(&s.seen, "authorization");
        let stored_public = vapid::vapid_public_key(&pool)
            .await
            .expect("the instance key pair");
        assert!(
            authorization.starts_with("vapid t=")
                && authorization.ends_with(&format!(", k={stored_public}")),
            "the signed header advertises the instance key: {authorization}"
        );
        let decrypted =
            web_push::decrypt_payload(&captured_body(&s.seen), &s.keys.private_key, &s.keys.auth)
                .expect("the body is a record the client's keys read");
        let payload: Value = serde_json::from_slice(&decrypted).expect("the payload is JSON");
        assert_eq!(
            payload["access_token"], s.recipient.token.token,
            "the wake token is the subscription's own"
        );
        assert_eq!(payload["notification_id"], s.note.id.to_string());
        assert_eq!(payload["notification_type"], notification::TYPE_FOLLOW);
        assert_eq!(payload["preferred_locale"], "en", "the fixed locale");
        assert_eq!(
            payload["icon"],
            format!("{TEST_BASE_URL}{DEFAULT_AVATAR_PATH}"),
            "an actor with no avatar wakes with the instance default"
        );
        assert_eq!(
            payload["title"], "Alice followed you",
            "display name + verb"
        );
        assert_eq!(payload["body"], "", "a status-less telling carries no text");
        assert_eq!(s.actor.name, "Alice", "the fixture actor's name");
    }

    #[tokio::test]
    async fn the_delivered_vapid_jwt_verifies_and_names_the_endpoint_origin() {
        let db = crate::test_db::test_db().await.expect("test database slot");
        let pool = db.pool().clone();
        let s = setup(&pool, 201).await;
        run(&s.state, s.note.id, s.subscription.id)
            .await
            .expect("delivered");
        s.server.abort();

        let authorization = header(&s.seen, "authorization");
        let jwt = authorization
            .strip_prefix("vapid t=")
            .and_then(|rest| rest.split_once(", k="))
            .expect("the captured header is a vapid pair")
            .0
            .to_owned();
        let parts: Vec<&str> = jwt.split('.').collect();
        let claims: Value = serde_json::from_slice(
            &URL_SAFE_NO_PAD
                .decode(parts[1])
                .expect("the claims are base64url"),
        )
        .expect("the claims are JSON");
        let origin = s.url.strip_suffix("/push").expect("the mock url shape");
        assert_eq!(claims["aud"], origin, "aud is the endpoint origin");
        assert_eq!(
            claims["sub"], TEST_BASE_URL,
            "with no CONTACT_EMAIL configured the instance origin is the subject"
        );
        let now = Utc::now().timestamp();
        let exp = claims["exp"].as_i64().expect("exp is numeric");
        assert!(
            exp > now + 43_000 && exp < now + 44_000,
            "exp is twelve hours out: {exp} against now {now}"
        );
        let pair = vapid::vapid_public_key(&pool).await.expect("the key pair");
        let verifying = VerifyingKey::from_sec1_bytes(
            &URL_SAFE_NO_PAD
                .decode(pair)
                .expect("the public half decodes"),
        )
        .expect("the instance point parses");
        let signature = Signature::from_slice(
            &URL_SAFE_NO_PAD
                .decode(parts[2])
                .expect("the signature is base64url"),
        )
        .expect("the JWT signature is the fixed r||s form");
        verifying
            .verify(format!("{}.{}", parts[0], parts[1]).as_bytes(), &signature)
            .expect("the delivered JWT verifies against the stored public key");
    }

    #[tokio::test]
    async fn a_mention_delivers_the_status_plain_text_under_a_nickname_title() {
        let db = crate::test_db::test_db().await.expect("test database slot");
        let pool = db.pool().clone();
        // An actor with no display name falls back to the nickname; the
        // mention names its status, whose text becomes the body.
        let actor = local(&pool, "wp-m-alice", "").await;
        let recipient = account(&pool, "wp-m-bob").await;
        let post = status::insert(
            &pool,
            TEST_BASE_URL,
            &status::NewStatus::local(&actor, "look at this"),
        )
        .await
        .expect("the status fixture inserts");
        let (url, seen, server) = mock_push_service(201).await;
        let keys = client_keys();
        let subscription = subscribe(
            &pool,
            &recipient,
            &url,
            &keys,
            &Pref {
                alerts: json!({"mention": true}),
                policy: "all",
            },
        )
        .await;
        let note = note_about(
            &pool,
            &recipient.user,
            &actor,
            notification::TYPE_MENTION,
            Some(post.id),
        )
        .await;

        run(&app_state(&pool), note.id, subscription.id)
            .await
            .expect("the delivery runs");
        server.abort();
        let decrypted =
            web_push::decrypt_payload(&captured_body(&seen), &keys.private_key, &keys.auth)
                .expect("the client reads the body");
        let payload: Value = serde_json::from_slice(&decrypted).expect("the payload is JSON");
        assert_eq!(payload["notification_type"], notification::TYPE_MENTION);
        assert_eq!(
            payload["title"], "wp-m-alice mentioned you",
            "a nameless actor wakes by nickname"
        );
        assert_eq!(
            payload["body"], "look at this",
            "the body is the status's plain text"
        );
    }

    #[tokio::test]
    async fn a_hidden_account_gets_no_body_but_still_a_title() {
        let db = crate::test_db::test_db().await.expect("test database slot");
        let pool = db.pool().clone();
        let actor = local(&pool, "wp-h-alice", "Alice").await;
        let recipient = account(&pool, "wp-h-bob").await;
        user::notification_settings::save(
            &pool,
            recipient.user.id,
            &NotificationSettings {
                hide_notification_contents: true,
                ..NotificationSettings::default()
            },
        )
        .await
        .expect("the settings save");
        let post = status::insert(
            &pool,
            TEST_BASE_URL,
            &status::NewStatus::local(&actor, "a secret mention"),
        )
        .await
        .expect("the status fixture inserts");
        let (url, seen, server) = mock_push_service(201).await;
        let keys = client_keys();
        let subscription = subscribe(&pool, &recipient, &url, &keys, &open_pref()).await;
        let note = note_about(
            &pool,
            &recipient.user,
            &actor,
            notification::TYPE_MENTION,
            Some(post.id),
        )
        .await;

        run(&app_state(&pool), note.id, subscription.id)
            .await
            .expect("the delivery runs");
        server.abort();
        let decrypted =
            web_push::decrypt_payload(&captured_body(&seen), &keys.private_key, &keys.auth)
                .expect("the client reads the body");
        let payload: Value = serde_json::from_slice(&decrypted).expect("the payload is JSON");
        assert_eq!(payload["body"], "", "the hidden contents never leave here");
        assert_eq!(
            payload["title"], "Alice mentioned you",
            "the title still says who"
        );
    }

    #[tokio::test]
    async fn a_disabled_alert_sends_nothing() {
        let db = crate::test_db::test_db().await.expect("test database slot");
        let pool = db.pool().clone();
        let s = setup(&pool, 201).await;
        // Re-register the token's subscription with the follow alert off.
        let off = Pref {
            alerts: json!({"follow": false}),
            policy: "all",
        };
        let off_sub = subscribe(&pool, &s.recipient, &s.url, &s.keys, &off).await;

        let outcome = run(&s.state, s.note.id, off_sub.id).await;

        assert_eq!(outcome, Ok(()), "an off flag is a clean skip");
        assert!(!served(&s.seen), "nothing reached the push service");
        s.server.abort();
    }

    /// The follow edges a policy case draws between actor and recipient.
    #[derive(Clone, Copy)]
    struct Edges {
        /// Whether the recipient follows the actor.
        recipient_follows_actor: bool,
        /// Whether the actor follows the recipient.
        actor_follows_recipient: bool,
    }

    /// Nobody follows anybody: the both-edges-off corner of the matrix.
    const NO_EDGES: Edges = Edges {
        recipient_follows_actor: false,
        actor_follows_recipient: false,
    };
    /// Only the recipient follows the actor.
    const RECIPIENT_FOLLOWS: Edges = Edges {
        recipient_follows_actor: true,
        actor_follows_recipient: false,
    };
    /// Only the actor follows the recipient.
    const ACTOR_FOLLOWS: Edges = Edges {
        recipient_follows_actor: false,
        actor_follows_recipient: true,
    };
    /// A mutual follow.
    const BOTH_WAYS: Edges = Edges {
        recipient_follows_actor: true,
        actor_follows_recipient: true,
    };

    /// The policy fixture: a `follow` notification from a named actor to
    /// a subscription with `policy`, over the `edges`' follow relationships,
    /// returning the job's outcome beside the mock's capture.
    async fn policy_case(
        pool: &PgPool,
        policy: &'static str,
        nickname: &str,
        edges: Edges,
    ) -> (Result<(), JobError>, Seen, JoinHandle<()>) {
        let actor = local(pool, &format!("{nickname}-actor"), "Actor").await;
        let recipient = account(pool, &format!("{nickname}-target")).await;
        let (url, seen, server) = mock_push_service(201).await;
        let subscription = subscribe(
            pool,
            &recipient,
            &url,
            &client_keys(),
            &Pref {
                alerts: json!({}),
                policy,
            },
        )
        .await;
        if edges.recipient_follows_actor {
            let _edge = follow::follow(pool, TEST_BASE_URL, &recipient.user, &actor)
                .await
                .expect("the recipient's follow writes");
        }
        if edges.actor_follows_recipient {
            let _edge = follow::follow(pool, TEST_BASE_URL, &actor, &recipient.user)
                .await
                .expect("the actor's follow writes");
        }
        let note = note_about(
            pool,
            &recipient.user,
            &actor,
            notification::TYPE_FOLLOW,
            None,
        )
        .await;
        let outcome = run(&app_state(pool), note.id, subscription.id).await;
        (outcome, seen, server)
    }

    #[tokio::test]
    async fn policies_gate_the_wake_by_the_follow_edges() {
        let db = crate::test_db::test_db().await.expect("test database slot");
        let pool = db.pool().clone();

        let (all_out, all_seen, all_server) = policy_case(&pool, "all", "plall", NO_EDGES).await;
        assert_eq!(all_out, Ok(()), "`all` answers for anyone");
        assert!(served(&all_seen), "and the stranger got through");
        all_server.abort();

        let (none_out, none_seen, none_server) =
            policy_case(&pool, "none", "plnon", BOTH_WAYS).await;
        assert_eq!(none_out, Ok(()), "`none` answers for nobody");
        assert!(!served(&none_seen), "not even a mutual follow wakes it");
        none_server.abort();

        let (_f1_out, f1_seen, f1_server) =
            policy_case(&pool, "followed", "plf1", RECIPIENT_FOLLOWS).await;
        assert!(
            served(&f1_seen),
            "`followed` wakes when the recipient follows"
        );
        f1_server.abort();
        let (_f2_out, f2_seen, f2_server) =
            policy_case(&pool, "followed", "plf2", ACTOR_FOLLOWS).await;
        assert!(
            !served(&f2_seen),
            "`followed` stays dark when only the actor follows"
        );
        f2_server.abort();

        let (_r1_out, r1_seen, r1_server) =
            policy_case(&pool, "follower", "plr1", ACTOR_FOLLOWS).await;
        assert!(
            served(&r1_seen),
            "`follower` wakes for the recipient's followers"
        );
        r1_server.abort();
        let (_r2_out, r2_seen, r2_server) =
            policy_case(&pool, "follower", "plr2", RECIPIENT_FOLLOWS).await;
        assert!(
            !served(&r2_seen),
            "`follower` stays dark when only the recipient follows"
        );
        r2_server.abort();
    }

    #[tokio::test]
    async fn a_410_deletes_the_subscription_and_completes() {
        let db = crate::test_db::test_db().await.expect("test database slot");
        let pool = db.pool().clone();
        let s = setup(&pool, 410).await;

        let outcome = run(&s.state, s.note.id, s.subscription.id).await;

        assert_eq!(
            outcome,
            Ok(()),
            "a retired subscription is not a failed job"
        );
        assert!(
            web_push_subscription::find_by_id(&pool, s.subscription.id)
                .await
                .expect("the lookup runs")
                .is_none(),
            "the 410 took the row with it"
        );
        server_gone_stays_gone(&pool, &s).await;
        s.server.abort();
    }

    /// The 410's delete is final for the token: the token's lookup reads
    /// nothing back either (the job deleted by `token_id`, the row's
    /// unique key).
    async fn server_gone_stays_gone(pool: &PgPool, s: &Setup) {
        assert!(
            web_push_subscription::find(pool, s.recipient.token.id)
                .await
                .expect("the lookup runs")
                .is_none(),
            "the delete went through the token, the row's unique key"
        );
    }

    #[tokio::test]
    async fn a_server_error_retries_the_job_and_keeps_the_subscription() {
        let db = crate::test_db::test_db().await.expect("test database slot");
        let pool = db.pool().clone();
        let s = setup(&pool, 503).await;

        let outcome = run(&s.state, s.note.id, s.subscription.id).await;

        match outcome {
            Err(JobError::Retry(detail)) => assert!(
                detail.contains("503"),
                "the retry names the peer's answer: {detail}"
            ),
            other => panic!("a 503 must retry the job, got {other:?}"),
        }
        assert!(
            web_push_subscription::find_by_id(&pool, s.subscription.id)
                .await
                .expect("the lookup runs")
                .is_some(),
            "a retryable failure keeps the subscription"
        );
        s.server.abort();
    }

    #[test]
    fn the_kind_is_registered_with_the_ticket_budget() {
        let registry = crate::job_registry();
        assert!(
            registry.get(KIND_DELIVER).is_some(),
            "the worker knows `{KIND_DELIVER}`"
        );
        assert_eq!(MAX_ATTEMPTS, 3, "the ticket's three attempts");
    }
}
