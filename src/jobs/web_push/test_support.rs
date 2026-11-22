//! Fixtures the Web Push job's unit tests share: test state, accounts and
//! subscriptions, and the mock push service.

use std::sync::{Arc, Mutex};

use axum::{
    Router,
    body::Bytes,
    http::{HeaderMap, StatusCode},
    routing::post,
};
use base64::{Engine as _, engine::general_purpose::URL_SAFE_NO_PAD};
use p256::{
    SecretKey,
    elliptic_curve::{Generate as _, sec1::ToSec1Point as _},
};
use rand::RngCore as _;
use serde_json::{Value, json};
use sqlx::PgPool;
use tokio::{net::TcpListener, task::JoinHandle};
use uuid::Uuid;

use super::{MAX_ATTEMPTS, handle_deliver, job_args};
#[cfg(doc)]
use crate::services::web_push;
use crate::{
    config::{Config, TEST_BASE_URL},
    http_client::HttpClient,
    jobs::{AttemptInfo, JobError},
    models::{
        notification::{self, NewNotification, Notification},
        oauth_app::{NewOauthApp, create as create_app},
        oauth_token::{NewToken, OauthToken, create_token},
        user::{self, NewUser, User, insert},
        web_push_subscription::{self, NewSubscription, WebPushSubscription},
    },
    state::AppState,
    streaming::hub::Hub,
    uploads::UploaderBackend,
};

/// Test state over `pool` on the fixed test config, whose HTTP client
/// reaches the loopback mocks the tests bind (decision 13's fixture is
/// what the database-backed tests lease from).
pub(super) fn app_state(pool: &PgPool) -> AppState {
    app_state_with(pool, Config::for_tests())
}

/// [`app_state`] built on `config`, for the tests that read a setting
/// away from its test default.
pub(super) fn app_state_with(pool: &PgPool, config: Config) -> AppState {
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
        rate_limiter: Arc::new(crate::rate_limit::RateLimiter::from_config(config.as_ref())),
        mailer: Arc::new(crate::mailer::MemoryTransport::new()),
    }
}

/// The first attempt of the [`MAX_ATTEMPTS`] the queue hands the job.
pub(super) const FIRST_ATTEMPT: AttemptInfo = AttemptInfo {
    attempts: 1,
    max_attempts: MAX_ATTEMPTS,
};

/// A local user displaying as `name`.
pub(super) async fn local(pool: &PgPool, nickname: &str, name: &str) -> User {
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
pub(super) struct Account {
    /// The user row.
    pub(super) user: User,
    /// Its access token.
    pub(super) token: OauthToken,
}

/// A fresh local account named `nickname` with its `push` token.
pub(super) async fn account(pool: &PgPool, nickname: &str) -> Account {
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
pub(super) struct ClientKeys {
    /// The stored client public point.
    pub(super) p256dh: String,
    /// The stored auth secret.
    pub(super) auth: String,
    /// The private scalar, test-side only.
    pub(super) private_key: String,
}

/// A fresh P-256 client key pair and 16-octet auth secret.
pub(super) fn client_keys() -> ClientKeys {
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
pub(super) struct Pref {
    /// The stored alerts object.
    pub(super) alerts: Value,
    /// The stored policy.
    pub(super) policy: &'static str,
}

/// [`subscribe`]'s default: every alert on (the empty object —
/// [`web_push::alert_enabled`] reads unlisted types as on), policy `all`.
pub(super) fn open_pref() -> Pref {
    Pref {
        alerts: json!({}),
        policy: "all",
    }
}

/// Registers `acct`'s subscription at `endpoint` with `keys` and `pref`.
pub(super) async fn subscribe(
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
pub(super) type Seen = Arc<Mutex<Vec<(Vec<(String, String)>, Vec<u8>)>>>;

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
pub(super) async fn mock_push_service(status: u16) -> (String, Seen, JoinHandle<()>) {
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
pub(super) async fn note_about(
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
            chat_message_id: None,
        },
    )
    .await
    .expect("the notification fixture inserts")
}

/// Runs the job through the registered handler.
pub(super) async fn run(
    state: &AppState,
    note_id: Uuid,
    subscription_id: Uuid,
) -> Result<(), JobError> {
    handle_deliver(
        state.clone(),
        FIRST_ATTEMPT,
        job_args(note_id, subscription_id),
    )
    .await
}

/// The named header of the mock's last request.
pub(super) fn header(seen: &Seen, name: &str) -> String {
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
        .map_or_default(|(_key, value)| value)
}

/// The mock's last request body.
pub(super) fn captured_body(seen: &Seen) -> Vec<u8> {
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
pub(super) fn served(seen: &Seen) -> bool {
    !seen.lock().expect("an unpoisoned lock").is_empty()
}

/// Everything one delivery test drives.
pub(super) struct Setup {
    /// The job state.
    pub(super) state: AppState,
    /// The acting account (display name `Alice`).
    pub(super) actor: User,
    /// The notified recipient with its token.
    pub(super) recipient: Account,
    /// The follow notification joining them.
    pub(super) note: Notification,
    /// The recipient's registered subscription.
    pub(super) subscription: WebPushSubscription,
    /// The browser key pair behind it.
    pub(super) keys: ClientKeys,
    /// The mock's capture.
    pub(super) seen: Seen,
    /// The mock's URL.
    pub(super) url: String,
    /// The mock's server task.
    pub(super) server: JoinHandle<()>,
}

/// A follow-notification delivery against a mock answering `status`,
/// subscription open (`all`, every alert default-on).
pub(super) async fn setup(pool: &PgPool, status: u16) -> Setup {
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
