//! Scenario 7: the operations round. An admin backup request answers the
//! queue's unbuilt shape, the worker writes a real `zip` to the uploads
//! dir and the listing's `/media/` url downloads the bytes, with its
//! `create_backup` ledger line. With the media proxy configured, a remote
//! note's attachment arrives rewritten into a signed `/proxy/` link —
//! origin kept in `remote_url` — and the proxy serves the mock origin's
//! exact bytes. The maintenance sweeps find their backdated stale rows —
//! notifications, tokens, job rows and tombstones — reclaim exactly them
//! and keep the control rows, and a throttled route answers `429` with
//! its headers the moment its burst is exhausted.

use axum::{Router, http::StatusCode};
use serde_json::json;
use sqlx::{PgPool, Row as _};
use tokio::net::TcpListener;
use uuid::Uuid;
use wired_be::{
    jobs::QUEUE_BACKGROUND,
    models::{
        job::{self, NewJob},
        user::User,
    },
    util::token::sha256_hex,
};

use super::mrf::{admin_entry, create_note, deliver};
use super::support::{BASE, BOB, P_ADMIN, PROXY_PNG, REPORTS, THROTTLED, World, assert_log};
use crate::{common, remote};

// ────────────────────  Scenario 7: backups + proxy + maintenance  ────────────────────

/// A tiny origin server answering `/pic.png` with [`PROXY_PNG`]; returns its
/// base URL.
async fn image_origin() -> String {
    let listener = TcpListener::bind("127.0.0.1:0")
        .await
        .expect("an image origin");
    let addr = listener.local_addr().expect("the origin address");
    let app = Router::new().route(
        "/pic.png",
        axum::routing::get(|| async {
            ([(axum::http::header::CONTENT_TYPE, "image/png")], PROXY_PNG)
        }),
    );
    let served = tokio::spawn(async move {
        axum::serve(listener, app).await.expect("the origin serves");
    });
    drop(served);
    format!("http://{addr}")
}

/// The origin url for `name` under `origin`.
fn origin_url(origin: &str, name: &str) -> String {
    format!("{origin}/{name}")
}

/// Scenario 7, first half: an admin backup request answers the queue's
/// `false`/empty-url shape, the worker writes a real archive to the uploads
/// dir, the user listing shows it processed with a `/media/` url, and the
/// bytes download over the router as a `zip` — with its `create_backup`
/// ledger.
#[tokio::test]
async fn an_admin_backup_is_queued_built_and_downloaded() {
    let db = common::test_db().await;
    let pool = db.pool().clone();
    let world = World::open(pool).await;

    let queued = common::api_post(
        &world.app,
        Some(&world.admin),
        &format!("{P_ADMIN}/backups"),
        &json!({"nickname": "root"}),
    )
    .await;
    common::assert_answer(&queued, StatusCode::OK, "the backup request");
    let first = queued
        .json_value()
        .as_array()
        .expect("the request answers the account's backup array")
        .first()
        .cloned()
        .expect("the new request stands in it");
    assert_eq!(
        first["processed"],
        json!(false),
        "the archive starts unbuilt"
    );
    assert_eq!(first["url"], json!(""), "with no url to hand out");

    let _built = world.drain().await;

    let listed = common::api_get(&world.app, Some(&world.admin), "/api/v1/pleroma/backups").await;
    common::assert_answer(&listed, StatusCode::OK, "the backup listing");
    let archive = listed
        .json_value()
        .as_array()
        .expect("the backup endpoint answers its array")
        .first()
        .cloned()
        .expect("the requested archive is listed");
    assert_eq!(archive["processed"], json!(true), "the worker built it");
    let url = archive["url"].as_str().expect("the archive has a url");
    assert!(
        url.starts_with(&format!("{BASE}/media/")),
        "the url is a /media/ link into the uploads dir: {url}",
    );

    let path = url.strip_prefix(BASE).expect("the url is under the base");
    let download = common::api_get(&world.app, Some(&world.admin), path).await;
    common::assert_answer(&download, StatusCode::OK, "the archive download");
    assert_eq!(
        &download.body[..4],
        b"PK\x03\x04",
        "the bytes are a real zip archive"
    );

    assert_log(
        &world.app,
        &world.admin,
        "create_backup",
        &["@root requested account backup for @root".to_owned()],
    )
    .await;
}

/// Scenario 7, second half: with the media proxy configured, a remote
/// note's attachment arrives rewritten into a signed `/proxy/` link with
/// the raw origin kept in `remote_url`, and the proxy serves the origin's
/// exact bytes.
#[tokio::test]
async fn remote_media_is_rewritten_into_signed_proxy_links_and_served() {
    let db = common::test_db().await;
    let pool = db.pool().clone();
    let config = common::test_media_proxy_config(1_000_000);
    let world = World::start(pool, config).await;
    let peer = remote::MockPeer::start().await;
    let origin = image_origin().await;

    let pic = origin_url(&origin, "pic.png");
    let actor = peer.actor(BOB);
    let note_id = peer.note("media-1");
    let pic_attachment = json!({
        "attachment": [{
            "type": "Document",
            "mediaType": "image/png",
            "url": pic,
        }],
    });
    let media_create = create_note(&peer, &actor, "media-1", "look at this", pic_attachment);
    let response = deliver(&world, &peer, &media_create).await;
    common::assert_answer(&response, StatusCode::ACCEPTED, "the media-bearing note");

    let entity = admin_entry(&world, &note_id)
        .await
        .expect("the note with the attachment landed");
    let attachment = &entity["media_attachments"][0];
    let proxied = attachment["url"]
        .as_str()
        .expect("the attachment has a url");
    assert!(
        proxied.starts_with(&format!("{BASE}/proxy/")),
        "the url is rewritten into the proxy: {proxied}"
    );
    assert!(
        proxied.ends_with("/pic.png"),
        "and keeps the origin file name"
    );
    assert_eq!(
        attachment["remote_url"],
        json!(pic),
        "the raw origin url rides as remote_url",
    );

    let path = proxied
        .strip_prefix(BASE)
        .expect("the proxied url is under the base");
    let served = common::api_get(&world.app, None, path).await;
    common::assert_answer(&served, StatusCode::OK, "the proxy fetch");
    assert_eq!(
        &*served.body, PROXY_PNG,
        "the proxy serves the origin's exact bytes"
    );
    assert_eq!(
        served.header("content-type"),
        Some("image/png"),
        "and the origin's content type",
    );
}

/// Scenario 7, third half: the maintenance sweep's four kinds each find
/// their stale rows — backdated in the database, not slept through — and
/// remove exactly them, while the rate limiter answers `429` with its
/// headers the moment a route's burst is exhausted.
#[tokio::test]
async fn maintenance_sweeps_the_stale_rows_and_the_throttle_answers_429() {
    let db = common::test_db().await;
    let pool = db.pool().clone();
    let config = common::test_config_with_rate_limits("statuses_actions=2:600");
    let world = World::start(pool.clone(), config).await;
    let reporter = world.register("reporter").await;
    let target = world.register("hunted").await;

    let seeds = seed_sweep_rows(&pool).await;
    let _swept = run_sweeps_and_probe(&pool, &world, seeds).await;

    // The throttle: the route's burst is two, the third call in the same
    // window answers 429 with the Mastodon headers.
    for _ in 0..2 {
        let filed = common::api_post(
            &world.app,
            Some(&reporter.token),
            REPORTS,
            &json!({"account_id": target.id, "comment": "burst"}),
        )
        .await;
        assert!(
            filed.status.is_success(),
            "the burst files: {}",
            filed.text()
        );
    }
    let throttled = common::api_post(
        &world.app,
        Some(&reporter.token),
        REPORTS,
        &json!({"account_id": target.id, "comment": "burst"}),
    )
    .await;
    assert_eq!(
        throttled.status,
        StatusCode::TOO_MANY_REQUESTS,
        "the third call is throttled",
    );
    common::assert_error(&throttled, THROTTLED, "the throttle body");
    assert_eq!(
        throttled.header("x-ratelimit-limit").unwrap(),
        "2",
        "the limit header"
    );
    assert_eq!(
        throttled.header("retry-after").unwrap(),
        "600",
        "the retry header"
    );
}

/// The stale rows the four maintenance sweeps read against, backdated in
/// the database rather than slept through.
struct Stale {
    /// The notification dismissed past the retention window.
    dismissed: Uuid,
    /// The token expired past its seven-day grace.
    expired: String,
    /// The completed job row past the prune window.
    doomed: Uuid,
    /// The local status soft-deleted past the purge window.
    tombstone: Uuid,
    /// The fresh notification the dismissal sweep must keep.
    control_note: Uuid,
    /// The still-valid token the expiry sweep must keep.
    live_token: String,
}

/// Seeds the swept tables through the models, then backdates the stale
/// rows: the ids and tokens a later probe needs.
async fn seed_sweep_rows(pool: &PgPool) -> Stale {
    let reporter_row = wired_be::models::user::get_by_nickname(pool, "reporter")
        .await
        .expect("the reporter reads")
        .expect("the reporter stands");
    let (control_note, stale_id) = seed_follow_notes(pool, &reporter_row).await;
    let live_token = common::create_app_and_token(pool, &reporter_row, "read").await;
    let stale_token = common::create_app_and_token(pool, &reporter_row, "read").await;
    let aged = sqlx::query(
        "UPDATE oauth_tokens SET valid_until = now() - interval '8 days' WHERE token = $1",
    )
    .bind(sha256_hex(&stale_token))
    .execute(pool)
    .await
    .expect("one token ages out");
    assert_eq!(aged.rows_affected(), 1, "exactly the stale token ages");
    let doomed = job::enqueue(
        pool,
        NewJob {
            queue: QUEUE_BACKGROUND,
            kind: "gone",
            args: json!({}),
            run_at: None,
            max_attempts: None,
            unique_key: None,
        },
    )
    .await
    .expect("a doomed job enters the queue")
    .expect("nothing dedups it");
    let aged_job = sqlx::query(
        "UPDATE jobs SET state = 'completed', updated_at = now() - interval '8 days' WHERE id = $1",
    )
    .bind(doomed.id)
    .execute(pool)
    .await
    .expect("the doomed job ages out");
    assert_eq!(aged_job.rows_affected(), 1, "the job row ages");
    let tombstone = wired_be::models::status::NewStatus::local(&reporter_row, "purged");
    let purged = wired_be::models::status::insert(pool, BASE, &tombstone)
        .await
        .expect("a fresh tombstone stands");
    let _gone = wired_be::models::status::soft_delete(pool, purged.id)
        .await
        .expect("the status is soft-deleted");
    let aged_purge =
        sqlx::query("UPDATE statuses SET deleted_at = now() - interval '31 days' WHERE id = $1")
            .bind(purged.id)
            .execute(pool)
            .await
            .expect("the tombstone ages out");
    assert_eq!(aged_purge.rows_affected(), 1, "the deletion date moves");
    Stale {
        dismissed: stale_id,
        expired: stale_token,
        doomed: doomed.id,
        tombstone: purged.id,
        control_note,
        live_token,
    }
}

/// One kept follow notification and one dismissed past the thirty-day
/// retention window.
async fn seed_follow_notes(pool: &PgPool, reporter_row: &User) -> (Uuid, Uuid) {
    let fresh_note = sqlx::query(
        "INSERT INTO notifications (id, user_id, from_account_id, type) \
         VALUES ($1, $2, $2, 'follow') RETURNING id",
    )
    .bind(Uuid::now_v7())
    .bind(reporter_row.id)
    .fetch_one(pool)
    .await
    .expect("the fresh notification");
    let control_note = fresh_note.get::<Uuid, _>("id");
    let stale_note = sqlx::query(
        "INSERT INTO notifications (id, user_id, from_account_id, type, dismissed_at) \
         VALUES ($1, $2, $2, 'follow', now() - interval '31 days') RETURNING id",
    )
    .bind(Uuid::now_v7())
    .bind(reporter_row.id)
    .fetch_one(pool)
    .await
    .expect("a stale notification");
    (control_note, stale_note.get::<Uuid, _>("id"))
}

/// Enqueues the four maintenance sweeps, drains the queue, and probes the
/// stale rows gone and the control rows kept.
async fn run_sweeps_and_probe(pool: &PgPool, world: &World, seeds: Stale) -> usize {
    for kind in [
        wired_be::jobs::maintenance::KIND_NOTIFICATIONS,
        wired_be::jobs::maintenance::KIND_TOKENS,
        wired_be::jobs::maintenance::KIND_JOBS,
        wired_be::jobs::maintenance::KIND_DELETED_STATUSES,
    ] {
        let queued = job::enqueue(
            pool,
            NewJob {
                queue: QUEUE_BACKGROUND,
                kind,
                args: json!({}),
                run_at: None,
                max_attempts: None,
                unique_key: None,
            },
        )
        .await
        .expect("the sweep job is enqueued");
        assert!(queued.is_some(), "the {kind} sweep enters the queue");
    }
    let drained = world.drain().await;

    let probes = [
        (
            "stale notification",
            sqlx::query("SELECT 1 FROM notifications WHERE id = $1").bind(seeds.dismissed),
        ),
        (
            "long-expired token",
            sqlx::query("SELECT 1 FROM oauth_tokens WHERE token = $1")
                .bind(sha256_hex(&seeds.expired)),
        ),
        (
            "stale job row",
            sqlx::query("SELECT 1 FROM jobs WHERE id = $1").bind(seeds.doomed),
        ),
        (
            "stale tombstone",
            sqlx::query("SELECT 1 FROM statuses WHERE id = $1").bind(seeds.tombstone),
        ),
    ];
    for (what, probe) in probes {
        let row = probe.fetch_optional(pool).await.expect("a probe runs");
        assert!(row.is_none(), "the sweep should have reclaimed the {what}");
    }
    let kept = [
        (
            "fresh notification",
            sqlx::query("SELECT 1 FROM notifications WHERE id = $1").bind(seeds.control_note),
        ),
        (
            "in-grace token",
            sqlx::query("SELECT 1 FROM oauth_tokens WHERE token = $1")
                .bind(sha256_hex(&seeds.live_token)),
        ),
    ];
    for (what, probe) in kept {
        let row = probe.fetch_optional(pool).await.expect("a probe runs");
        assert!(row.is_some(), "the {what} must survive the sweep");
    }
    drained
}
