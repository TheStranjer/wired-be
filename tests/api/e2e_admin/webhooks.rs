//! Scenario 6's first half: the signed `report.created` webhook. A hook
//! made through the admin API over a loopback endpoint that catches every
//! delivery answers a real filing with one envelope — event header, JSON
//! body and an HMAC signature recomputed from the shared secret — and the
//! disable silences the next filing, with the `webhook_update` log line
//! read back.

use std::sync::{Arc, Mutex};

use axum::{
    Router,
    body::Bytes,
    extract::State,
    http::{HeaderMap, StatusCode},
    routing::post,
};
use hmac::{Hmac, KeyInit as _, Mac as _};
use serde_json::{Value, json};
use tokio::net::TcpListener;
use wired_be::util::token::to_hex;

use super::support::{Account, P_ADMIN, REPORTS, World, assert_log};
use crate::common;

// ────────────────────  Scenario 6: webhook + relay  ────────────────────

/// The caught hook deliveries: headers and raw body, as the endpoint
/// records them.
type Caught = Arc<Mutex<Vec<(HeaderMap, Vec<u8>)>>>;

/// The hook endpoint: every delivery it catches is pushed onto `caught`, and
/// it answers `202` — accepted, never retried. Returns its `/night` URL.
async fn hook_endpoint(sinks: Caught) -> String {
    let listener = TcpListener::bind("127.0.0.1:0")
        .await
        .expect("a hook endpoint");
    let addr = listener.local_addr().expect("the hook address");
    let app = Router::new().route(
        "/night",
        post(
            |State(hook_box): State<Caught>, headers: HeaderMap, body: Bytes| async move {
                hook_box
                    .lock()
                    .expect("the hook box")
                    .push((headers, body.to_vec()));
                StatusCode::ACCEPTED
            },
        ),
    );
    let served = tokio::spawn(async move {
        axum::serve(listener, app.with_state(sinks))
            .await
            .expect("the hook endpoint serves");
    });
    drop(served);
    format!("http://{addr}/night")
}

/// The `x-wired-signature` value the delivery job computes: `sha256=<hex>`
/// of the HMAC over `"{timestamp}.{body}"`.
fn hook_signature(secret: &str, timestamp: &str, body: &str) -> String {
    let mut mac = Hmac::<sha2_011::Sha256>::new_from_slice(secret.as_bytes())
        .expect("the HMAC accepts a secret of any length");
    mac.update(format!("{timestamp}.{body}").as_bytes());
    format!("sha256={}", to_hex(&mac.finalize().into_bytes()))
}

/// Scenario 6, first half: filing a report enqueues exactly one delivery to
/// a configured webhook; the caught request carries the event header, the
/// envelope and a matching signature; disabling the hook silences the next
/// filing.
#[tokio::test]
async fn a_filed_report_delivers_its_signed_hook_and_disable_silences_it() {
    let db = common::test_db().await;
    let pool = db.pool().clone();
    let world = World::open(pool).await;
    let reporter = world.register("reporter").await;
    let target = world.register("hunted").await;

    let caught: Caught = Arc::new(Mutex::new(Vec::new()));
    let url = hook_endpoint(Arc::clone(&caught)).await;
    let (hook_id, secret) = create_report_hook(&world, &url).await;
    file_report(&world, &reporter, &target, "hook me").await;
    let _hook_drain = world.drain().await;
    assert_caught_delivery(&caught, &secret, "hook me");

    // Disable, file again, drain: the endpoint stays silent.
    let disabled = common::api_patch(
        &world.app,
        Some(&world.admin),
        &format!("{P_ADMIN}/webhooks/{hook_id}"),
        &json!({"enabled": false}),
    )
    .await;
    common::assert_answer(&disabled, StatusCode::OK, "the webhook disable");
    file_report(&world, &reporter, &target, "silence").await;
    let _silence_drain = world.drain().await;
    assert_eq!(
        caught.lock().expect("the hook box").len(),
        1,
        "a disabled hook delivers nothing",
    );
    assert_log(
        &world.app,
        &world.admin,
        "webhook_update",
        &[format!("@root updated webhook: {hook_id}")],
    )
    .await;
}

/// `POST /admin/webhooks` for `report.created` deliveries to `url`,
/// answered as the hook's `(id, secret)` pair.
async fn create_report_hook(world: &World, url: &str) -> (String, String) {
    let created = common::api_post(
        &world.app,
        Some(&world.admin),
        &format!("{P_ADMIN}/webhooks"),
        &json!({"url": url, "events": ["report.created"]}),
    )
    .await;
    common::assert_answer(&created, StatusCode::OK, "the webhook creation");
    let hook = created.json_value();
    let hook_id = hook["id"]
        .as_str()
        .expect("the hook carries its id")
        .to_owned();
    let secret = hook["secret"]
        .as_str()
        .expect("the hook carries its secret")
        .to_owned();
    assert_eq!(
        hook["events"].as_array().map(Vec::len),
        Some(1),
        "the hook is subscribed to the one event"
    );
    (hook_id, secret)
}

/// `POST /api/v1/reports` with `comment` from `reporter` about `target`.
async fn file_report(world: &World, reporter: &Account, target: &Account, comment: &str) {
    let filed = common::api_post(
        &world.app,
        Some(&reporter.token),
        REPORTS,
        &json!({"account_id": target.id, "comment": comment}),
    )
    .await;
    common::assert_answer(&filed, StatusCode::OK, "the filing");
}

/// The one caught delivery: event header, JSON body, envelope and a
/// signature that recomputes from the shared secret.
fn assert_caught_delivery(caught: &Caught, secret: &str, comment: &str) {
    let deliveries = caught.lock().expect("the hook box").clone();
    assert_eq!(deliveries.len(), 1, "the filing delivered exactly once");
    let (headers, body) = &deliveries[0];
    assert_eq!(
        headers.get("x-wired-event").and_then(|v| v.to_str().ok()),
        Some("report.created"),
        "the event header names the triggering event"
    );
    assert_eq!(
        headers.get("content-type").and_then(|v| v.to_str().ok()),
        Some("application/json"),
        "the delivery is JSON"
    );
    let envelope: Value = serde_json::from_slice(body).expect("the delivery parses");
    assert_eq!(
        envelope["type"],
        json!("report.created"),
        "the envelope names its event",
    );
    assert_eq!(
        envelope["data"]["report"]["comment"],
        json!(comment),
        "the payload carries the filing",
    );
    let timestamp = headers
        .get("x-wired-timestamp")
        .and_then(|v| v.to_str().ok())
        .expect("the delivery stamps its signature")
        .to_owned();
    assert_eq!(
        headers
            .get("x-wired-signature")
            .and_then(|v| v.to_str().ok())
            .unwrap_or_default(),
        hook_signature(
            secret,
            &timestamp,
            std::str::from_utf8(body).expect("utf-8 body")
        ),
        "the signature proves the shared secret over timestamp and body",
    );
}
