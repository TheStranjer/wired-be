//! End-to-end admin, moderation, MRF, config and operations scenarios
//! (WBE-225): whole journeys walked through the HTTP API with an admin
//! bearer and a moderator bearer, the background queues driven by hand
//! through [`common::run_all_jobs`], and the mock federation peer from
//! [`remote`] as the remote every MRF phase judges.
//!
//! Each scenario is its own `#[tokio::test]` so a failure names one
//! journey:
//!
//! 1. [`a_report_walks_both_admin_apis_and_the_staff_actions`] — the
//!    moderation flow: a report filed over `POST /api/v1/reports` read
//!    back through the Pleroma admin surface, the Mastodon admin surface
//!    and the legacy `/api/v0` twin; the notes/assign/resolve lifecycle
//!    walked through both APIs; the status edited and deleted through the
//!    admin statuses surface; and the account deactivated until its
//!    content vanishes from the public timeline, then reactivated. Every
//!    step's moderation-log entry is counted and worded inline.
//! 2. [`the_mrf_policies_shape_inbound_activities_as_configured`] and
//!    [`a_blocked_domain_is_refused_at_the_door_and_swept_on_deletion`] —
//!    the `SimplePolicy` (`reject`, `followers_only`, `media_removal`),
//!    the `Keyword`, `Hellthread`, `ObjectAge` and `Tag` policies
//!    configured through `POST /admin/config` and judged against inbound
//!    activities from the mock peer, the chain's transparency read through
//!    `/api/v1/instance`, `domain_blocks` and NodeInfo, and the remote
//!    domain's deletion through `DELETE /instances/{instance}`.
//! 3. [`runtime_config_rules_documents_and_announcements_reach_the_public_api`]
//!    — the runtime `:instance` overrides landing in the public instance
//!    endpoints, `GET /config/descriptions`, the instance-document
//!    upload/serve/delete cycle, and the rules and announcements CRUD read
//!    through the user-facing endpoints beside the `announcement` event.
//! 4. [`every_admin_write_records_exactly_one_moderation_log_entry`] —
//!    one write per admin surface, then the whole log: every write's
//!    action count and its full sentence.
//! 5. [`moderators_walk_the_staff_surfaces_and_are_refused_the_admin_ones`]
//!    — the moderator walks the reports and statuses surfaces and is
//!    refused every other admin group, while an `admin:read` grant never
//!    writes and an `admin:write` grant never reads, area scopes included.
//! 6. [`the_report_created_webhook_and_the_admin_relay_walk_their_handshakes`]
//!    — a subscription made through the admin API delivers its signed
//!    envelope when a real filing drains the queue, and the `/relay`
//!    lifecycle walks follow, ledger, withdrawal and the `Undo` against
//!    the mock peer's relay actor.
//! 7. [`an_admin_backup_is_queued_built_and_downloaded`],
//!    [`remote_media_is_rewritten_into_signed_proxy_links_and_served`],
//!    [`maintenance_sweeps_clean_the_stale_rows_and_a_throttled_bucket_answers_429`]
//!    — the operations round.
//!
//! What each route refuses on its own is pinned by the area files
//! (`admin_reports.rs`, `mastodon_admin_reports.rs`, `admin_statuses.rs`,
//! `admin_users.rs`, `admin.rs`, `admin_content.rs`, `admin_ops.rs`,
//! `admin_federation.rs`, `webhooks.rs`, …); this module owns the seams —
//! the state one admin endpoint leaves behind for the next and what the
//! API answers about it.

#![cfg(test)]

use std::{
    sync::{Arc, Mutex},
    time::Duration,
};

use axum::{
    Router,
    body::Bytes,
    extract::State,
    http::{HeaderMap, StatusCode},
    routing::post,
};
use hmac::{Hmac, KeyInit as _, Mac as _};
use serde_json::{Value, json};
use sqlx::{PgPool, Row as _};
use tempfile::TempDir;
use tokio::net::TcpListener;
use uuid::Uuid;
use wired_be::{
    config::{Config, TEST_BASE_URL},
    jobs::QUEUE_BACKGROUND,
    models::{
        job::{self, NewJob},
        user::User,
    },
    state::AppState,
    streaming::hub::Audience,
    util::token::to_hex,
};

use crate::{common, remote};

/// The password every scenario account is planted or registered with.
const PASSWORD: &str = "correct horse battery staple";
/// The scopes every scenario client registers with, and so every token it
/// wins: the whole client set plus the admin pair.
const CLIENT_SCOPES: &str = "read write follow push admin:read admin:write";
/// The instance origin [`common::test_config`] serves on.
const BASE: &str = TEST_BASE_URL;
/// The Pleroma admin API root.
const P_ADMIN: &str = "/api/v1/pleroma/admin";
/// The legacy Pleroma admin reports mount (WBE-200).
const V0_REPORTS: &str = "/api/v0/pleroma/reports";
/// The Mastodon admin reports root (WBE-214).
const M_ADMIN_REPORTS: &str = "/api/v1/admin/reports";
/// The status write.
const STATUSES: &str = "/api/v1/statuses";
/// The report filing.
const REPORTS: &str = "/api/v1/reports";
/// The caller's own account read.
const VERIFY: &str = "/api/v1/accounts/verify_credentials";
/// The anonymous public timeline.
const PUBLIC_TIMELINE: &str = "/api/v1/timelines/public";
/// The admin config store.
const CONFIG: &str = "/api/v1/pleroma/admin/config";
/// The admin users root.
const USERS: &str = "/api/v1/pleroma/admin/users";
/// The moderation-log listing.
const LOG: &str = "/api/v1/pleroma/admin/moderation_log";
/// The `Loopback` host the mock peer answers on, port-less the way the
/// MRF domain lists and the inbox door compare hosts.
const LOOPBACK: &str = "127.0.0.1";
/// The mock peer's author account every MRF phase delivers as.
const BOB: &str = "bob";
/// The 403 body of the staff routes for a non-staff account.
const NOT_STAFF: &str = "User is not a staff member.";
/// The 403 body of the admin-only routes for a staff non-admin.
const NOT_ADMIN: &str = "User is not an admin.";
/// The 403 body of a missing `admin:read` grant.
const NO_ADMIN_READ: &str = "Insufficient permissions: admin:read.";
/// The 403 body of a missing `admin:write` grant.
const NO_ADMIN_WRITE: &str = "Insufficient permissions: admin:write.";
/// The 403 body of the inbox door for a host the `simple` lists reject.
const REJECTED: &str = "Rejected";
/// The 429 body of every throttled bucket.
const THROTTLED: &str = "Throttled";
/// The 403 body of the bearer gate's refusal of a disabled account.
const DISABLED: &str = "Your account is currently disabled";
/// The past announcement instant.
const PAST: &str = "2000-06-01T12:00:00.000Z";
/// The far-future announcement instant.
const FUTURE: &str = "2100-06-01T12:00:00.000Z";
/// The `PNG` signature plus a tail, compared verbatim: the media proxy
/// never decodes what it serves.
const PROXY_PNG: &[u8] = b"\x89PNG\r\n\x1a\n-proxy-origin";
/// The multipart boundary the document and emoji uploads frame with.
const BOUNDARY: &str = "wired-e2e-admin-boundary";

/// One scenario's world: the router every step rides, the [`AppState`]
/// behind it that [`common::run_all_jobs`] drains, the client accounts
/// register through, and the two staff bearers the admin API is driven
/// with. The scratch directories the uploads, emoji packs and instance
/// documents land in live as long as the world does.
struct World {
    /// The application under test.
    app: Router,
    /// The state the router rides and the jobs run on.
    state: AppState,
    /// The scenario's OAuth client.
    client: common::TestClient,
    /// The admin's bearer.
    admin: String,
    /// The admin account [`admin`](Self::admin) belongs to.
    admin_user: User,
    /// The moderator's bearer.
    moderator: String,
    /// The moderator account [`moderator`](Self::moderator) belongs to.
    moderator_user: User,
    /// The scratch uploads root; nothing reads it, dropping it is the job.
    _uploads: TempDir,
    /// The scratch emoji-pack root, likewise.
    _emoji: TempDir,
    /// The scratch instance-document root, likewise.
    _docs: TempDir,
}

impl World {
    /// The world over `pool` and `config` with its three scratch
    /// directories mounted: the two staff accounts planted with the
    /// fixtures the end-to-end scenarios own (no public endpoint mints an
    /// admin or a moderator), their credentials won over HTTP by the
    /// password grant, and the client registered as the first call.
    async fn start(pool: PgPool, mut config: Config) -> Self {
        let uploads = TempDir::new().expect("a scratch uploads dir");
        let emoji = TempDir::new().expect("a scratch emoji dir");
        let docs = TempDir::new().expect("a scratch docs dir");
        config.uploads_dir = uploads.path().to_path_buf();
        config.emoji_dir = emoji.path().to_path_buf();
        config.statics.documents.dir = docs.path().to_path_buf();
        let admin_user = common::create_admin_with_password(&pool, "root", PASSWORD).await;
        let moderator_user = common::create_moderator_with_password(&pool, "moddy", PASSWORD).await;
        let state = common::test_app_state(pool.clone(), config);
        let app = wired_be::app(state.clone());
        let client = common::register_client(&app, "Admin Scenario Client", CLIENT_SCOPES).await;
        let admin = login(&app, &client, "root").await;
        let moderator = login(&app, &client, "moddy").await;
        Self {
            app,
            state,
            client,
            admin,
            admin_user,
            moderator,
            moderator_user,
            _uploads: uploads,
            _emoji: emoji,
            _docs: docs,
        }
    }

    /// The world on the suite's stock test config.
    async fn open(pool: PgPool) -> Self {
        Self::start(pool, common::test_config()).await
    }

    /// Drains the job queues the way the live worker would.
    async fn drain(&self) -> usize {
        common::run_all_jobs(&self.state).await
    }

    /// Registers `username` over the public `POST /api/v1/accounts`.
    async fn register(&self, username: &str) -> Account {
        let token = common::register_via_api(&self.app, &self.client, username, PASSWORD).await;
        Account::read(&self.app, username, token).await
    }
}

/// A scenario account: its id, nickname and the bearer it acts with.
struct Account {
    /// The account id.
    id: String,
    /// The nickname.
    nickname: String,
    /// The bearer.
    token: String,
}

impl Account {
    /// Reads `username`'s own account with `token` over
    /// `GET /api/v1/accounts/verify_credentials`.
    async fn read(app: &Router, username: &str, token: String) -> Self {
        let response = common::api_get(app, Some(&token), VERIFY).await;
        common::assert_answer(&response, StatusCode::OK, "the account reads itself");
        Self {
            id: id_of(&response.json_value()),
            nickname: username.to_owned(),
            token,
        }
    }
}

/// The `password` grant for `username`, asserted to its bearer.
async fn login(app: &Router, client: &common::TestClient, username: &str) -> String {
    let response = common::login_password(app, client, username, PASSWORD).await;
    common::assert_answer(&response, StatusCode::OK, "the scenario login");
    common::access_token_of(&response)
}

/// The string `id` of an entity answer.
fn id_of(entity: &Value) -> String {
    entity["id"]
        .as_str()
        .expect("the entity carries a string id")
        .to_owned()
}

/// `GET` of `path` as `token` (`None` anonymous), asserted 200 and
/// answered as a JSON array.
async fn get_array(app: &Router, token: Option<&str>, path: &str) -> Vec<Value> {
    let response = common::api_get(app, token, path).await;
    common::assert_answer(&response, StatusCode::OK, &format!("the listing of {path}"));
    response
        .json_value()
        .as_array()
        .unwrap_or_else(|| panic!("{path} answers a JSON array: {}", response.text()))
        .clone()
}

/// The anonymous public-timeline ids.
async fn public_ids(app: &Router) -> Vec<String> {
    get_array(app, None, PUBLIC_TIMELINE)
        .await
        .into_iter()
        .map(|entry| id_of(&entry))
        .collect()
}

/// Whether `entries` carry the entity whose `id` (or `uri`) is `id`.
fn carries(entries: &[Value], key: &str, id: &str) -> bool {
    entries.iter().any(|entry| entry[key].as_str() == Some(id))
}

/// `POST /api/v1/statuses` with `text` as `acct`, answered as the entity.
async fn post_status(world: &World, acct: &Account, text: &str) -> Value {
    let response = common::api_post(
        &world.app,
        Some(&acct.token),
        STATUSES,
        &json!({"status": text}),
    )
    .await;
    common::assert_answer(&response, StatusCode::OK, "the scenario post");
    response.json_value()
}

/// The whole moderation log as `(action, message)` pairs, newest first.
async fn log_entries(app: &Router, token: &str) -> Vec<(String, String)> {
    let response = common::api_get(app, Some(token), &format!("{LOG}?page_size=200")).await;
    common::assert_answer(&response, StatusCode::OK, "the moderation-log listing");
    response
        .json_value()
        .get("items")
        .and_then(Value::as_array)
        .unwrap_or_else(|| panic!("the log answers its items: {}", response.text()))
        .iter()
        .map(|entry| {
            (
                entry["data"]["action"]
                    .as_str()
                    .unwrap_or_else(|| panic!("an entry names its action: {entry}"))
                    .to_owned(),
                entry["message"]
                    .as_str()
                    .unwrap_or_else(|| panic!("an entry carries its message: {entry}"))
                    .to_owned(),
            )
        })
        .collect()
}

/// The moderation-log messages whose action is `action`, newest first.
async fn log_lines(app: &Router, token: &str, action: &str) -> Vec<String> {
    log_entries(app, token)
        .await
        .into_iter()
        .filter(|(entry, _)| entry == action)
        .map(|(_entry, message)| message)
        .collect()
}

/// The log's messages for `action` as a sorted set, compared to `expected`
/// sorted: the count is the assertion and every sentence is exact.
async fn assert_log(app: &Router, token: &str, action: &str, expected: &[String]) {
    let mut lines = log_lines(app, token, action).await;
    lines.sort();
    let mut want = expected.to_vec();
    want.sort();
    assert_eq!(lines, want, "the {action} sentences in the log");
}

/// `POST {CONFIG}` with `items`, answered raw.
async fn post_configs(app: &Router, token: &str, items: Value) -> common::TestResponse {
    common::api_post(app, Some(token), CONFIG, &json!({"configs": items})).await
}

/// The wire-form value of one `:pleroma` row: an array of
/// `{"tuple": [key, value]}` entries, Pleroma's `ConfigDB` spelling.
fn wire(pairs: &[(&str, Value)]) -> Value {
    pairs
        .iter()
        .map(|(key, value)| json!({"tuple": [key, value]}))
        .collect()
}

/// The config POST that sets `:mrf` to `policies` and rewrites every
/// `:mrf_*` policy row from scratch: the phases of the MRF scenario
/// replace the whole chain, not merge into the last one's rows.
async fn set_mrf(app: &Router, token: &str, policies: &[&str], rows: &[(&str, Value)]) {
    let mrf_write = json!({
        "group": ":pleroma",
        "key": ":mrf",
        "value": wire(&[("policies", json!(policies))]),
    });
    let mut items: Vec<Value> = vec![
        json!({"group": ":pleroma", "key": ":mrf", "delete": true}),
        json!({"group": ":pleroma", "key": ":mrf_simple", "delete": true}),
        json!({"group": ":pleroma", "key": ":mrf_keyword", "delete": true}),
        json!({"group": ":pleroma", "key": ":mrf_hellthread", "delete": true}),
        json!({"group": ":pleroma", "key": ":mrf_object_age", "delete": true}),
        mrf_write,
    ];
    for (key, value) in rows {
        items.push(json!({"group": ":pleroma", "key": key, "value": value}));
    }
    let response = post_configs(app, token, json!(items)).await;
    common::assert_answer(&response, StatusCode::OK, "the MRF config write");
}

/// The `pleroma.metadata.federation` block of `GET /api/v1/instance`.
async fn federation_block(app: &Router) -> Value {
    let response = common::api_get(app, None, "/api/v1/instance").await;
    common::assert_answer(&response, StatusCode::OK, "the instance document");
    response.json_value()["pleroma"]["metadata"]["federation"].clone()
}

// ───────────────────────  Scenario 2: the MRF  ───────────────────────

/// The `Note`-family object on the peer's origin named `name`, public by
/// default; every field of `extra` merges over the defaults.
fn note_object(
    peer: &remote::MockPeer,
    actor: &str,
    name: &str,
    content: &str,
    extra: Value,
) -> Value {
    let mut object = json!({
        "id": peer.note(name),
        "type": "Note",
        "attributedTo": actor,
        "to": [remote::PUBLIC],
        "cc": [],
        "content": content,
        "published": PAST,
    });
    merge(&mut object, extra);
    object
}

/// A `Create` of a public `Note` named `name` with `content`; `object_extra`
/// merges over the note.
fn create_note(
    peer: &remote::MockPeer,
    actor: &str,
    name: &str,
    content: &str,
    object_extra: Value,
) -> Value {
    let object = note_object(peer, actor, name, content, object_extra);
    json!({
        "id": peer.activity_id(name),
        "type": "Create",
        "actor": actor,
        "object": object,
    })
}

/// Merges `addressing` over the envelope — where a hellthread test hangs its
/// recipient list, since the policy counts the *activity's* `to` and `cc`.
fn wrap_create(envelope: Value, addressing: Value) -> Value {
    let mut envelope = envelope;
    merge(&mut envelope, addressing);
    envelope
}

/// Merges `extra`'s fields over `target`.
fn merge(target: &mut Value, extra: Value) {
    let (Some(map), Value::Object(additions)) = (target.as_object_mut(), extra) else {
        panic!("both documents are JSON objects");
    };
    for (key, value) in additions {
        let _previous = map.insert(key, value);
    }
}

/// Delivers `activity` to the shared inbox as the peer's `BOB`, drains the
/// fold, and hands the raw inbox answer back to the caller.
async fn deliver(world: &World, peer: &remote::MockPeer, activity: &Value) -> common::TestResponse {
    let response = peer
        .deliver(&world.app, &peer.actor(BOB), remote::INBOX, activity)
        .await;
    let _drained = world.drain().await;
    response
}

/// Whether `entries` carry the status whose `uri` is `uri`.
fn carries_uri(entries: &[Value], uri: &str) -> bool {
    carries(entries, "uri", uri)
}

/// The `GET {P_ADMIN}/statuses?godmode=true` ledger, narrowed to the entry
/// whose `uri` is `uri`.
async fn admin_entry(world: &World, uri: &str) -> Option<Value> {
    let entries = get_array(
        &world.app,
        Some(&world.admin),
        &format!("{P_ADMIN}/statuses?godmode=true"),
    )
    .await;
    entries
        .into_iter()
        .find(|entry| entry["uri"].as_str() == Some(uri))
}

/// Scenario 2, part 1: the `simple` policy's three ticket-lists. A public
/// note with an attachment first folds untouched; `followers_only` and
/// `media_removal` then rewrite the next one to a private, media-less
/// post; `reject` refuses the following delivery at the inbox door; and
/// the chain's transparency reads through `GET /api/v1/instance` and
/// `NodeInfo` beside the domain's own `domain_blocks` telling.
#[tokio::test]
async fn simple_policy_rewrites_refuses_and_reports_itself() {
    let db = common::test_db().await;
    let pool = db.pool().clone();
    let world = World::open(pool).await;
    let peer = remote::MockPeer::start().await;
    let actor = peer.actor(BOB);

    let clean_uri = peer.note("mrf-clean");
    let baseline_create = create_note(&peer, &actor, "mrf-clean", "<p>plain hello</p>", json!({}));
    let baseline_door = deliver(&world, &peer, &baseline_create).await;
    common::assert_answer(
        &baseline_door,
        StatusCode::ACCEPTED,
        "the baseline inbox answer",
    );
    let clean = admin_entry(&world, &clean_uri)
        .await
        .expect("the baseline note folded");
    assert_eq!(
        clean["visibility"],
        json!("public"),
        "the baseline is public"
    );

    simple_policy_followers_only_and_media_removal_rewrite(&world, &peer, &actor).await;
    simple_policy_refuses_and_reports_itself(&world, &peer, &actor).await;
}

/// The `followers_only` + `media_removal` lists: the next note arrives
/// private, media-less and off the public timeline.
async fn simple_policy_followers_only_and_media_removal_rewrite(
    world: &World,
    peer: &remote::MockPeer,
    actor: &str,
) {
    let rewrite_lists = json!({
        "followers_only": [[LOOPBACK, "press"]],
        "media_removal": [LOOPBACK],
    });
    let rewrite_rows: Vec<(&str, Value)> = vec![(":mrf_simple", wire_from(&rewrite_lists))];
    set_mrf(&world.app, &world.admin, &["simple"], &rewrite_rows).await;
    let limited_uri = peer.note("mrf-limited");
    let note_attachment = json!({
        "attachment": [{
            "type": "Document",
            "mediaType": "image/png",
            "url": "https://media.example/pic.png",
        }],
    });
    let rewrite_create = create_note(
        peer,
        actor,
        "mrf-limited",
        "<p>look, a photo</p>",
        note_attachment,
    );
    let rewrite_door = deliver(world, peer, &rewrite_create).await;
    common::assert_answer(
        &rewrite_door,
        StatusCode::ACCEPTED,
        "the rewrite is a 202 at the door",
    );
    let limited = admin_entry(world, &limited_uri)
        .await
        .expect("the rewritten note folded");
    assert_eq!(
        limited["visibility"],
        json!("private"),
        "followers_only lands the note private"
    );
    assert_eq!(
        limited["media_attachments"],
        json!([]),
        "media_removal strips the attachment"
    );
    assert!(
        !carries_uri(
            &get_array(&world.app, None, PUBLIC_TIMELINE).await,
            &limited_uri
        ),
        "the followers-only note is off the public timeline"
    );
}

/// Scenario 2, part 1: the `simple` policy with no list writes folds a
/// plain note in untouched; a `reject` entry then refuses the next delivery at
/// the inbox door; and the chain's transparency reads through
/// `GET /api/v1/instance`, `NodeInfo` and `domain_blocks` alike.
async fn simple_policy_refuses_and_reports_itself(
    world: &World,
    peer: &remote::MockPeer,
    actor: &str,
) {
    // The rejection list refuses the *next* delivery at the door.
    let reject_lists = json!({"reject": [[LOOPBACK, "spammers"]]});
    let reject_rows: Vec<(&str, Value)> = vec![(":mrf_simple", wire_from(&reject_lists))];
    set_mrf(&world.app, &world.admin, &["simple"], &reject_rows).await;
    let refuse_create = create_note(peer, actor, "mrf-refused", "<p>spam</p>", json!({}));
    let refuse_door = deliver(world, peer, &refuse_create).await;
    common::assert_answer(
        &refuse_door,
        StatusCode::FORBIDDEN,
        "the blocked host is refused",
    );
    common::assert_error(&refuse_door, REJECTED, "the door's word");
    assert!(
        admin_entry(world, &peer.note("mrf-refused"))
            .await
            .is_none(),
        "the refused note never folded"
    );
    // Transparency: the instance's federation block, the domain_blocks
    // twin and the NodeInfo copy all tell the configured chain.
    let federation = federation_block(&world.app).await;
    assert_eq!(
        federation["mrf_policies"],
        json!(["SimplePolicy"]),
        "the named chain"
    );
    assert_eq!(
        federation["mrf_simple"]["reject"],
        json!([LOOPBACK]),
        "the published reject list"
    );
    assert_eq!(
        federation["mrf_simple_info"]["reject"][LOOPBACK]["reason"],
        json!("spammers"),
        "the reason rides beside it"
    );
    let blocks = get_array(&world.app, None, "/api/v1/instance/domain_blocks").await;
    assert!(
        blocks
            .iter()
            .any(|row| row["domain"].as_str() == Some(LOOPBACK)
                && row["comment"].as_str() == Some("spammers")),
        "domain_blocks renders the reject list: {blocks:?}"
    );
    let nodeinfo = common::api_get(&world.app, None, "/nodeinfo/2.0").await;
    common::assert_answer(&nodeinfo, StatusCode::OK, "the NodeInfo document");
    assert_eq!(
        nodeinfo.json_value()["metadata"]["federation"]["mrf_policies"],
        json!(["SimplePolicy"]),
        "NodeInfo tells the same chain"
    );
}

/// The wire form of a whole policy-parameter object: its keys as
/// `{"tuple": [key, value]}` entries.
fn wire_from(object: &Value) -> Value {
    object
        .as_object()
        .expect("a policy parameter object")
        .iter()
        .map(|(key, value)| json!({"tuple": [key, value]}))
        .collect()
}

/// Scenario 2, part 2: the `Keyword`, `Hellthread`, `ObjectAge` and `Tag`
/// policies, each configured through `POST /admin/config` and judged
/// against one delivered activity — the keyword note replaced, the
/// over-mention hellthread delisted, the over-age note rejected, and the
/// tagged author's public note forced unlisted.
#[tokio::test]
async fn keyword_hellthread_object_age_and_tag_policies_judge_the_inbound_chain() {
    let db = common::test_db().await;
    let pool = db.pool().clone();
    let world = World::open(pool).await;
    let peer = remote::MockPeer::start().await;
    let actor = peer.actor(BOB);

    keyword_replaces_a_note(&world, &peer, &actor).await;
    hellthread_delists_and_drops(&world, &peer, &actor).await;
    object_age_rejects_an_old_note(&world, &peer, &actor).await;
    tag_forces_unlisted(&world, &peer, &actor).await;

    // The final chain: all five policies named, and the transparency
    // reports every name in order.
    set_mrf(
        &world.app,
        &world.admin,
        &["simple", "keyword", "hellthread", "object_age", "tag"],
        &[],
    )
    .await;
    let federation = federation_block(&world.app).await;
    assert_eq!(
        federation["mrf_policies"],
        json!([
            "SimplePolicy",
            "KeywordPolicy",
            "HellthreadPolicy",
            "ObjectAgePolicy",
            "TagPolicy"
        ]),
        "the instance answers all five policies in chain order"
    );
}

/// The keyword `replace` list: a delivered note's matched word arrives
/// rewritten in the folded status.
async fn keyword_replaces_a_note(world: &World, peer: &remote::MockPeer, actor: &str) {
    let kw_rows: Vec<(&str, Value)> = vec![(
        ":mrf_keyword",
        wire_from(&json!({"replace": [["bad word", "b*tch"]]})),
    )];
    set_mrf(&world.app, &world.admin, &["keyword"], &kw_rows).await;
    let uri = peer.note("kw-replace");
    let kw_create = create_note(
        peer,
        actor,
        "kw-replace",
        "<p>say bad word out loud</p>",
        json!({}),
    );
    let kw_door = deliver(world, peer, &kw_create).await;
    common::assert_answer(
        &kw_door,
        StatusCode::ACCEPTED,
        "the replaced note is accepted",
    );
    let entry = admin_entry(world, &uri)
        .await
        .expect("the rewritten note folded");
    assert_eq!(
        entry["content"],
        json!("<p>say b*tch out loud</p>"),
        "the keyword policy rewrote the note's content"
    );
}

/// The hellthread thresholds: three recipients over the delist threshold
/// demote the note to unlisted; four over the reject threshold drop it
/// entirely.
async fn hellthread_delists_and_drops(world: &World, peer: &remote::MockPeer, actor: &str) {
    let ht_rows: Vec<(&str, Value)> = vec![(
        ":mrf_hellthread",
        wire_from(&json!({"delist_threshold": 2, "reject_threshold": 3})),
    )];
    set_mrf(&world.app, &world.admin, &["hellthread"], &ht_rows).await;

    let mentions = |count: usize| -> Vec<Value> {
        (0..count)
            .map(|i| json!(format!("https://mentioned.example/user{i}")))
            .collect()
    };
    let delist_uri = peer.note("ht-delist");
    let htd_create = wrap_create(
        create_note(
            peer,
            actor,
            "ht-delist",
            "<p>a crowd of mentions</p>",
            json!({}),
        ),
        json!({"to": [remote::PUBLIC], "cc": mentions(3)}),
    );
    let htd_door = deliver(world, peer, &htd_create).await;
    common::assert_answer(&htd_door, StatusCode::ACCEPTED, "the delisted note arrives");
    let entry = admin_entry(world, &delist_uri)
        .await
        .expect("the delisted note folded");
    assert_eq!(
        entry["visibility"],
        json!("unlisted"),
        "over the delist threshold: unlisted"
    );

    let hdr_create = wrap_create(
        create_note(peer, actor, "ht-drop", "<p>a larger crowd</p>", json!({})),
        json!({"to": [remote::PUBLIC], "cc": mentions(4)}),
    );
    let hdr_door = deliver(world, peer, &hdr_create).await;
    common::assert_answer(
        &hdr_door,
        StatusCode::ACCEPTED,
        "the door still 202s the chain drop",
    );
    assert!(
        admin_entry(world, &peer.note("ht-drop")).await.is_none(),
        "over the reject threshold the note never folds"
    );
}

/// The object-age gate: a note published before the configured timestamp
/// is rejected by the chain and never stored.
async fn object_age_rejects_an_old_note(world: &World, peer: &remote::MockPeer, actor: &str) {
    let oa_rows: Vec<(&str, Value)> = vec![(
        ":mrf_object_age",
        wire_from(&json!({"threshold": 604_800, "actions": ["reject"]})),
    )];
    set_mrf(&world.app, &world.admin, &["object_age"], &oa_rows).await;
    let oa_old_create = create_note(
        peer,
        actor,
        "oa-old",
        "<p>ancient</p>",
        json!({"published": "2020-01-01T00:00:00Z"}),
    );
    let oa_old_door = deliver(world, peer, &oa_old_create).await;
    common::assert_answer(
        &oa_old_door,
        StatusCode::ACCEPTED,
        "the door does not weigh ages",
    );
    assert!(
        admin_entry(world, &peer.note("oa-old")).await.is_none(),
        "the over-age note is rejected by the chain"
    );
    // A note published after the timestamp survives the same chain.
    let fresh_uri = peer.note("oa-fresh");
    let oa_fresh_create = create_note(
        peer,
        actor,
        "oa-fresh",
        "<p>recent</p>",
        json!({"published": "2026-11-15T00:00:00Z"}),
    );
    let oa_fresh_door = deliver(world, peer, &oa_fresh_create).await;
    common::assert_answer(
        &oa_fresh_door,
        StatusCode::ACCEPTED,
        "a fresh delivery arrives",
    );
    assert!(
        admin_entry(world, &fresh_uri).await.is_some(),
        "the under-age note folds untouched"
    );
}

/// The tag switches: the author's mirror tagged `mrf_tag:force-unlisted`
/// (through the admin users surface, its `tag` entry logged) has its next
/// public note demoted to unlisted by the chain.
async fn tag_forces_unlisted(world: &World, peer: &remote::MockPeer, actor: &str) {
    set_mrf(&world.app, &world.admin, &["tag"], &[]).await;
    let tagged = common::api_put(
        &world.app,
        Some(&world.admin),
        &format!("{USERS}/tag"),
        &json!({"nicknames": [peer.handle(BOB)], "tags": ["mrf_tag:force-unlisted"]}),
    )
    .await;
    common::assert_answer(&tagged, StatusCode::NO_CONTENT, "the admin tag write");
    let uri = peer.note("tagged-public");
    let tag_create = create_note(
        peer,
        actor,
        "tagged-public",
        "<p>loud but hidden</p>",
        json!({}),
    );
    let tag_door = deliver(world, peer, &tag_create).await;
    common::assert_answer(
        &tag_door,
        StatusCode::ACCEPTED,
        "the tagged author's note arrives",
    );
    let entry = admin_entry(world, &uri)
        .await
        .expect("the tagged note folded");
    assert_eq!(
        entry["visibility"],
        json!("unlisted"),
        "the author's tag forces it unlisted"
    );
    assert_log(
        &world.app,
        &world.admin,
        "tag",
        &[format!(
            "@root added tags: mrf_tag:force-unlisted to users: @{}",
            peer.handle(BOB)
        )],
    )
    .await;
}

/// Scenario 2, part 3: the remote domain's deletion. A delivered mirror
/// and its post stand until `DELETE /instances/{domain}` marks the domain
/// gone; the staged `users.delete_remote` sweeps then run through the
/// queue and the post leaves the timelines.
#[tokio::test]
async fn a_blocked_domain_is_refused_at_the_door_and_swept_on_deletion() {
    let db = common::test_db().await;
    let pool = db.pool().clone();
    let world = World::open(pool).await;
    let peer = remote::MockPeer::start().await;
    let actor = peer.actor(BOB);

    let uri = peer.note("sweep-1");
    let domain_create = create_note(
        &peer,
        &actor,
        "sweep-1",
        "<p>until the sweep</p>",
        json!({}),
    );
    let door = deliver(&world, &peer, &domain_create).await;
    common::assert_answer(
        &door,
        StatusCode::ACCEPTED,
        "the note lands before the sweep",
    );
    assert!(
        carries_uri(&get_array(&world.app, None, PUBLIC_TIMELINE).await, &uri),
        "on the timeline"
    );

    let response = common::api_delete(
        &world.app,
        Some(&world.admin),
        &format!("{P_ADMIN}/instances/{}", peer.host()),
        &Value::Null,
    )
    .await;
    common::assert_answer(&response, StatusCode::OK, "the instance deletion lands");
    assert_eq!(
        response.json_value(),
        json!(peer.host()),
        "the domain answers as a JSON string"
    );
    let ran = world.drain().await;
    assert!(ran >= 1, "the staged sweeps ran: {ran}");
    assert!(
        !carries_uri(&get_array(&world.app, None, PUBLIC_TIMELINE).await, &uri),
        "the swept domain's post left the timeline"
    );
    assert_log(
        &world.app,
        &world.admin,
        "delete_instance",
        &[format!("@root deleted instance: {}", peer.host())],
    )
    .await;

    // The local instance cannot delete itself.
    let local = common::api_delete(
        &world.app,
        Some(&world.admin),
        &format!("{P_ADMIN}/instances/test.example"),
        &Value::Null,
    )
    .await;
    common::assert_answer(
        &local,
        StatusCode::BAD_REQUEST,
        "the local domain is refused",
    );
    common::assert_error(
        &local,
        "Can't delete the local instance",
        "the local domain is refused",
    );
}

// ────────────────  Scenario 3: config, docs, rules, announcements  ───────────────

/// Scenario 3: the runtime `:instance` override lands in the public
/// instance endpoints and `NodeInfo`; the descriptions endpoint answers the
/// whole whitelist; an instance document round-trips upload/serve/delete;
/// the rules CRUD reads back through the public `instance/rules`; and an
/// announcement created by an admin reaches the user-facing list, is
/// dismissed, and its create published the `announcement` stream event —
/// each admin write logged.
#[tokio::test]
async fn runtime_config_rules_documents_and_announcements_reach_the_public_api() {
    let db = common::test_db().await;
    let pool = db.pool().clone();
    let world = World::open(pool).await;

    instance_override(&world).await;
    descriptions_answer_the_whitelist(&world.app, &world.admin).await;
    instance_document_round_trip(&world).await;
    rules_walk(&world).await;
    announcements_walk(&world).await;
}

/// The `:instance` runtime override: rename the instance, and the change
/// reads through `GET /api/v1/instance`, `/api/v2/instance` and `NodeInfo`;
/// a restart-requiring `:media_proxy` write raises the per-request
/// `need_reboot` flag on the answer.
async fn instance_override(world: &World) {
    let written = post_configs(
        &world.app,
        &world.admin,
        json!([{
            "group": ":pleroma",
            "key": ":instance",
            "value": wire(&[
                ("name", json!("Renamed Adminery")),
                ("registrations_open", json!(false)),
            ]),
        }]),
    )
    .await;
    common::assert_answer(&written, StatusCode::OK, "the :instance write");
    assert_eq!(
        written.json_value()["need_reboot"],
        json!(false),
        "the :instance row is live"
    );

    let v1 = common::api_get(&world.app, None, "/api/v1/instance").await;
    assert_eq!(
        v1.json_value()["title"],
        json!("Renamed Adminery"),
        "the v1 title moved"
    );
    let v2 = common::api_get(&world.app, None, "/api/v2/instance").await;
    assert_eq!(
        v2.json_value()["registrations"]["enabled"],
        json!(false),
        "the v2 gate moved"
    );
    let nodeinfo = common::api_get(&world.app, None, "/nodeinfo/2.0").await;
    assert_eq!(
        nodeinfo.json_value()["metadata"]["nodeName"],
        json!("Renamed Adminery"),
        "NodeInfo tells the same name"
    );

    // A restart-requiring key reports its own need_reboot on the answer.
    let reboot = post_configs(
        &world.app,
        &world.admin,
        json!([{
            "group": ":pleroma",
            "key": ":media_proxy",
            "value": wire(&[("enabled", json!(false))]),
        }]),
    )
    .await;
    common::assert_answer(&reboot, StatusCode::OK, "the :media_proxy write");
    assert_eq!(
        reboot.json_value()["need_reboot"],
        json!(true),
        "the proxy row needs a restart"
    );

    // The override is logged once, naming the pair.
    assert_log(
        &world.app,
        &world.admin,
        "config_change",
        &[
            "@root changed config: :pleroma/:media_proxy".to_owned(),
            "@root changed config: :pleroma/:instance".to_owned(),
        ],
    )
    .await;
}

/// The descriptions endpoint answers one object per whitelisted key, in
/// registry order, the first being `:instance` with its typed children.
async fn descriptions_answer_the_whitelist(app: &Router, token: &str) {
    let response = common::api_get(app, Some(token), &format!("{CONFIG}/descriptions")).await;
    common::assert_answer(&response, StatusCode::OK, "the descriptions payload");
    let body = response.json_value();
    let entries = body.as_array().expect("the descriptions are an array");
    assert_eq!(entries.len(), 8, "the whole whitelist is documented");
    assert_eq!(
        entries[0]["key"],
        json!(":instance"),
        "the first is :instance"
    );
    assert_eq!(entries[0]["type"], json!("group"), "documented as a group");
    let children = entries[0]["children"]
        .as_array()
        .expect("the instance children");
    assert!(
        children
            .iter()
            .any(|c| c["key"] == json!("name") && c["type"] == json!("string")),
        "with a typed `name` child"
    );
}

/// The instance-document cycle: the multipart upload answers the public
/// url, the public route serves the bytes with the document CSP, and the
/// delete takes it back down.
async fn instance_document_round_trip(world: &World) {
    let upload =
        common::TestRequest::patch(&format!("{P_ADMIN}/instance_document/terms-of-service"))
            .bearer(&world.admin)
            .multipart(
                BOUNDARY,
                &[common::MultipartPart::file(
                    "file",
                    "terms.html",
                    "text/html",
                    Bytes::from_static(b"<h1>Terms</h1>"),
                )],
            )
            .send(&world.app)
            .await;
    common::assert_answer(&upload, StatusCode::OK, "the document upload");
    assert_eq!(
        upload.json_value()["url"],
        json!(format!("{BASE}/instance/terms-of-service.html")),
        "the answer names the public url"
    );

    let served = common::api_get(&world.app, None, "/instance/terms-of-service.html").await;
    common::assert_answer(&served, StatusCode::OK, "the public document read");
    assert_eq!(served.text(), "<h1>Terms</h1>", "the bytes round-trip");
    assert_eq!(
        served.header("content-type"),
        Some("text/html; charset=utf-8"),
        "served as html"
    );

    let deleted = common::api_delete(
        &world.app,
        Some(&world.admin),
        &format!("{P_ADMIN}/instance_document/terms-of-service"),
        &Value::Null,
    )
    .await;
    common::assert_answer(&deleted, StatusCode::OK, "the document delete");
    let gone = common::api_get(&world.app, None, "/instance/terms-of-service.html").await;
    common::assert_answer(
        &gone,
        StatusCode::NOT_FOUND,
        "the deleted document is a 404",
    );
}

/// The rules quartet: create, read through the public three-key
/// `instance/rules`, re-order by priority, delete — and the public
/// instance document carries the same rules. Every write is logged.
async fn rules_walk(world: &World) {
    let keep = common::api_post(
        &world.app,
        Some(&world.admin),
        &format!("{P_ADMIN}/rules"),
        &json!({"text": "Be kind", "hint": "say hi first"}),
    )
    .await;
    common::assert_answer(&keep, StatusCode::OK, "the rule create");
    let keep_id = id_of(&keep.json_value());
    let drop_me = common::api_post(
        &world.app,
        Some(&world.admin),
        &format!("{P_ADMIN}/rules"),
        &json!({"text": "No spam", "priority": 2}),
    )
    .await;
    common::assert_answer(&drop_me, StatusCode::OK, "the second rule create");
    let drop_id = id_of(&drop_me.json_value());

    let public = get_array(&world.app, None, "/api/v1/instance/rules").await;
    assert_eq!(
        public,
        vec![
            json!({"id": keep_id, "text": "Be kind", "hint": "say hi first"}),
            json!({"id": drop_id, "text": "No spam", "hint": ""}),
        ],
        "the public listing is the admin order, priority then creation, the priority left out"
    );

    let requeued = common::api_patch(
        &world.app,
        Some(&world.admin),
        &format!("{P_ADMIN}/rules/{keep_id}"),
        &json!({"priority": 9}),
    )
    .await;
    common::assert_answer(&requeued, StatusCode::OK, "the rule update");
    assert_eq!(
        requeued.json_value()["priority"],
        json!(9),
        "the priority moved"
    );

    let deleted = common::api_delete(
        &world.app,
        Some(&world.admin),
        &format!("{P_ADMIN}/rules/{drop_id}"),
        &Value::Null,
    )
    .await;
    common::assert_answer(&deleted, StatusCode::OK, "the rule delete");
    assert_eq!(
        deleted.json_value(),
        json!({}),
        "the delete answers the empty object"
    );

    assert_log(
        &world.app,
        &world.admin,
        "rule_create",
        &[
            format!("@root created rule: {drop_id}"),
            format!("@root created rule: {keep_id}"),
        ],
    )
    .await;
    assert_log(
        &world.app,
        &world.admin,
        "rule_update",
        &[format!("@root updated rule: {keep_id}")],
    )
    .await;
    assert_log(
        &world.app,
        &world.admin,
        "rule_delete",
        &[format!("@root deleted rule: {drop_id}")],
    )
    .await;
}

/// The announcements quintet end to end: an admin creates a notice whose
/// window holds, its create publishes the `announcement` stream event, the
/// user-facing list shows it unread, the user dismisses it, and the admin
/// update/delete read back. Every write is logged.
async fn announcements_walk(world: &World) {
    let mut rx = world.state.hub.subscribe();
    let created = common::api_post(
        &world.app,
        Some(&world.admin),
        &format!("{P_ADMIN}/announcements"),
        &json!({"content": "Down at five", "starts_at": PAST, "ends_at": FUTURE}),
    )
    .await;
    common::assert_answer(&created, StatusCode::OK, "the announcement create");
    let announcement_id = id_of(&created.json_value());
    assert_eq!(
        created.json_value()["pleroma"]["raw_content"],
        json!("Down at five"),
        "the raw content rides back"
    );

    let event = tokio::time::timeout(Duration::from_secs(5), rx.recv())
        .await
        .expect("the create published a stream event")
        .expect("the hub holds it");
    assert_eq!(event.event, "announcement", "Mastodon's announcement event");
    assert!(
        matches!(event.audience, Audience::Users(_)),
        "addressed to the local users"
    );

    let reader = world.register("reader").await;
    let list = get_array(&world.app, Some(&reader.token), "/api/v1/announcements").await;
    let notice = list
        .iter()
        .find(|row| row["id"].as_str() == Some(announcement_id.as_str()))
        .expect("the user-facing list carries the notice");
    assert_eq!(notice["read"], json!(false), "unseen by this reader");

    let dismissed = common::api_post(
        &world.app,
        Some(&reader.token),
        &format!("/api/v1/announcements/{announcement_id}/dismiss"),
        &Value::Null,
    )
    .await;
    common::assert_answer(&dismissed, StatusCode::OK, "the dismissal");
    let with_dismissed = get_array(
        &world.app,
        Some(&reader.token),
        "/api/v1/announcements?with_dismissed=true",
    )
    .await;
    assert!(
        with_dismissed
            .iter()
            .any(|row| row["id"].as_str() == Some(announcement_id.as_str())
                && row["read"] == json!(true)),
        "the dismissed notice reads back as read for its dismisser"
    );

    announcements_edit_and_delete(world, &announcement_id).await;
    assert_log(
        &world.app,
        &world.admin,
        "announcement_create",
        &[format!("@root created announcement: {announcement_id}")],
    )
    .await;
}

/// The admin update and delete of `announcement_id`, each answered and
/// each logged with its own sentence.
async fn announcements_edit_and_delete(world: &World, announcement_id: &str) {
    let updated = common::api_patch(
        &world.app,
        Some(&world.admin),
        &format!("{P_ADMIN}/announcements/{announcement_id}"),
        &json!({"content": "Moved to six"}),
    )
    .await;
    common::assert_answer(&updated, StatusCode::OK, "the announcement update");
    let deleted = common::api_delete(
        &world.app,
        Some(&world.admin),
        &format!("{P_ADMIN}/announcements/{announcement_id}"),
        &Value::Null,
    )
    .await;
    common::assert_answer(&deleted, StatusCode::OK, "the announcement delete");
    assert_log(
        &world.app,
        &world.admin,
        "announcement_update",
        &[format!("@root updated announcement: {announcement_id}")],
    )
    .await;
    assert_log(
        &world.app,
        &world.admin,
        "announcement_delete",
        &[format!("@root deleted announcement: {announcement_id}")],
    )
    .await;
}

// ────────────────────────  Scenario 4: log completeness  ────────────────────────

/// Scenario 4: a clean walk of one write per admin surface — `POST /config`, a rule
/// create/update/delete, announcement, tag and emoji reload — reads back
/// through `GET /admin/moderation_log` as exactly one entry each, in the
/// expected counts and worded with Pleroma's sentences, and nothing else.
#[tokio::test]
async fn every_admin_write_records_exactly_one_moderation_log_entry() {
    let db = common::test_db().await;
    let pool = db.pool().clone();
    let world = World::open(pool).await;
    let reporter = world.register("witness").await;

    let config_write = post_configs(
        &world.app,
        &world.admin,
        json!([{"group": ":pleroma", "key": ":instance", "value": wire(&[("name", json!("Logged"))])}]),
    )
    .await;
    common::assert_answer(&config_write, StatusCode::OK, "the config write");
    let rule = common::api_post(
        &world.app,
        Some(&world.admin),
        &format!("{P_ADMIN}/rules"),
        &json!({"text": "Only this"}),
    )
    .await;
    let rule_id = id_of(&rule.json_value());
    let rule_update = common::api_patch(
        &world.app,
        Some(&world.admin),
        &format!("{P_ADMIN}/rules/{rule_id}"),
        &json!({"priority": 1}),
    )
    .await;
    common::assert_answer(&rule_update, StatusCode::OK, "the rule update");
    let rule_delete = common::api_delete(
        &world.app,
        Some(&world.admin),
        &format!("{P_ADMIN}/rules/{rule_id}"),
        &Value::Null,
    )
    .await;
    common::assert_answer(&rule_delete, StatusCode::OK, "the rule delete");
    let notice = common::api_post(
        &world.app,
        Some(&world.admin),
        &format!("{P_ADMIN}/announcements"),
        &json!({"content": "One notice", "starts_at": PAST, "ends_at": FUTURE}),
    )
    .await;
    let notice_id = id_of(&notice.json_value());
    let tag_write = common::api_put(
        &world.app,
        Some(&world.admin),
        &format!("{USERS}/tag"),
        &json!({"nicknames": [reporter.nickname], "tags": ["vip"]}),
    )
    .await;
    common::assert_answer(&tag_write, StatusCode::NO_CONTENT, "the tag write");
    let reloaded = common::api_post(
        &world.app,
        Some(&world.admin),
        &format!("{P_ADMIN}/reload_emoji"),
        &Value::Null,
    )
    .await;
    common::assert_answer(&reloaded, StatusCode::OK, "the emoji reload");

    assert_log_contents(
        &world,
        &[
            "@root reloaded emoji",
            "@root added tags: vip to users: @witness",
            &format!("@root deleted rule: {rule_id}"),
            &format!("@root updated rule: {rule_id}"),
            &format!("@root created announcement: {notice_id}"),
            &format!("@root created rule: {rule_id}"),
            "@root changed config: :pleroma/:instance",
        ],
    )
    .await;
}

/// Reads the whole moderation log back newest-first and compares its
/// messages, sorted, to `expected`.
async fn assert_log_contents(world: &World, expected: &[&str]) {
    let log = common::api_get(
        &world.app,
        Some(&world.admin),
        &format!("{LOG}?page_size=200"),
    )
    .await;
    common::assert_answer(&log, StatusCode::OK, "the moderation-log listing");
    let items = log.json_value()["items"]
        .as_array()
        .expect("the log answers its items")
        .clone();
    let dates: Vec<i64> = items
        .iter()
        .map(|entry| {
            entry["time"]
                .as_i64()
                .unwrap_or_else(|| panic!("an entry carries its time: {entry}"))
        })
        .collect();
    let mut newest_first = dates.clone();
    newest_first.sort_by(|a, b| b.cmp(a));
    assert_eq!(dates, newest_first, "the log lists newest first");
    let mut lines: Vec<String> = items
        .iter()
        .map(|entry| {
            entry["data"]["message"]
                .as_str()
                .unwrap_or_else(|| panic!("an entry carries its message: {entry}"))
                .to_owned()
        })
        .collect();
    lines.sort();
    let mut want: Vec<String> = expected.iter().map(|line| (*line).to_owned()).collect();
    want.sort();
    assert_eq!(lines, want, "exactly one entry per admin write");
}

// ────────────────────  Scenario 5: the admin/staff boundary  ────────────────────

/// The staff half of the admin API: reports and statuses, both directions —
/// a moderator bearer walks it end to end.
async fn staff_half(world: &World, target: &Account, status_id: &str) {
    let base = format!("{P_ADMIN}/reports");
    let filed = common::api_post(
        &world.app,
        Some(&target.token),
        REPORTS,
        &json!({"account_id": world.admin_user.id.to_string(), "comment": "staff probe"}),
    )
    .await;
    common::assert_answer(&filed, StatusCode::OK, "the staff-probe filing");
    let report_id = id_of(&filed.json_value());
    for (what, response) in [
        (
            "the report listing",
            common::api_get(&world.app, Some(&world.moderator), &base).await,
        ),
        (
            "the status listing",
            common::api_get(
                &world.app,
                Some(&world.moderator),
                &format!("{P_ADMIN}/statuses"),
            )
            .await,
        ),
        (
            "a status edit",
            common::api_put(
                &world.app,
                Some(&world.moderator),
                &format!("{P_ADMIN}/statuses/{status_id}"),
                &json!({"sensitive": true}),
            )
            .await,
        ),
        (
            "a note",
            common::api_post(
                &world.app,
                Some(&world.moderator),
                &format!("{base}/{report_id}/notes"),
                &json!({"content": "staff note"}),
            )
            .await,
        ),
    ] {
        assert!(
            response.status.is_success(),
            "a moderator was refused {what}: {}",
            response.text(),
        );
    }
}

/// The admin half of the admin API: a moderator is refused every one of
/// these with `403` and the admin sentence, whatever direction the call
/// goes in.
async fn admin_half_denied(world: &World) {
    let refusals = [
        (
            "the config read",
            common::api_get(&world.app, Some(&world.moderator), CONFIG).await,
        ),
        (
            "the config write",
            common::api_post(
                &world.app,
                Some(&world.moderator),
                CONFIG,
                &json!({"configs": [{
                    "group": ":pleroma",
                    "key": ":instance",
                    "value": [{"tuple": ["registrations_open", false]}],
                }]}),
            )
            .await,
        ),
        (
            "the user list",
            common::api_get(&world.app, Some(&world.moderator), USERS).await,
        ),
        (
            "a deactivation",
            common::api_patch(
                &world.app,
                Some(&world.moderator),
                &format!("{USERS}/deactivate"),
                &json!({"nicknames": ["root"]}),
            )
            .await,
        ),
        (
            "the log",
            common::api_get(&world.app, Some(&world.moderator), LOG).await,
        ),
        (
            "the stats",
            common::api_get(
                &world.app,
                Some(&world.moderator),
                &format!("{P_ADMIN}/stats"),
            )
            .await,
        ),
    ];
    assert_refused(&refusals);
    admin_half_denied_writes(world).await;
}

/// The write side of the admin half: relay, webhooks, rules,
/// announcements and the emoji reload are all `403` for a moderator.
async fn admin_half_denied_writes(world: &World) {
    let refusals = [
        (
            "a relay follow",
            common::api_post(
                &world.app,
                Some(&world.moderator),
                &format!("{P_ADMIN}/relay"),
                &json!({"relay_url": "https://relay.example/inbox"}),
            )
            .await,
        ),
        (
            "a webhook",
            common::api_post(
                &world.app,
                Some(&world.moderator),
                &format!("{P_ADMIN}/webhooks"),
                &json!({"url": "https://hooks.example/night", "events": ["report.created"]}),
            )
            .await,
        ),
        (
            "a rule",
            common::api_post(
                &world.app,
                Some(&world.moderator),
                &format!("{P_ADMIN}/rules"),
                &json!({"text": "moderators may not"}),
            )
            .await,
        ),
        (
            "an announcement",
            common::api_post(
                &world.app,
                Some(&world.moderator),
                &format!("{P_ADMIN}/announcements"),
                &json!({"content": "moderators may not"}),
            )
            .await,
        ),
        (
            "an emoji reload",
            common::api_post(
                &world.app,
                Some(&world.moderator),
                &format!("{P_ADMIN}/reload_emoji"),
                &Value::Null,
            )
            .await,
        ),
    ];
    assert_refused(&refusals);
}

/// Every refusal answers `403` and the admin sentence.
fn assert_refused(refusals: &[(&str, common::TestResponse)]) {
    for (what, response) in refusals {
        assert_eq!(
            response.status,
            StatusCode::FORBIDDEN,
            "a moderator was allowed {what}: {}",
            response.text(),
        );
        common::assert_error(response, NOT_ADMIN, &format!("the refusal of {what}"));
    }
}

/// Scenario 5: the staff surface accepts moderators, the admin surface
/// refuses them, scope-bearing tokens answer their exact sentences, an
/// app-only token cannot reach the admin API at all, and an anonymous
/// request is refused before the gate is even considered.
#[tokio::test]
async fn the_admin_surface_refuses_moderators_scopes_and_the_anonymous() {
    let db = common::test_db().await;
    let pool = db.pool().clone();
    let world = World::open(pool.clone()).await;
    let reporter = world.register("reporter").await;
    let post = post_status(&world, &reporter, "in scope").await;
    let post_id = id_of(&post);

    staff_half(&world, &reporter, &post_id).await;
    admin_half_denied(&world).await;

    let plain = common::register_via_api(&world.app, &world.client, "plainstaff", PASSWORD).await;
    role_gate_refusals(&world, &pool, &plain).await;
    scope_gate_refusals(&world, &pool).await;
    gate_refusals_for_the_anonymous_and_the_machine(&world, &pool).await;
}

/// A registered user's bearer comes back with the client's own scopes, so
/// this plain account is refused the staff surface with the staff sentence
/// and the admin surface with the admin one — and the same user with real
/// admin scopes still fails the role gate both ways.
async fn role_gate_refusals(world: &World, pool: &PgPool, plain: &str) {
    let staff_refusal = common::api_get(&world.app, Some(plain), M_ADMIN_REPORTS).await;
    assert_eq!(
        staff_refusal.status,
        StatusCode::FORBIDDEN,
        "a plain user is refused the staff reports surface: {}",
        staff_refusal.text(),
    );
    common::assert_error(&staff_refusal, NOT_STAFF, "the plain staff refusal");
    let admin_refusal = common::api_get(&world.app, Some(plain), CONFIG).await;
    assert_eq!(
        admin_refusal.status,
        StatusCode::FORBIDDEN,
        "a plain user is refused the admin config surface: {}",
        admin_refusal.text(),
    );
    assert!(
        admin_refusal.text().contains(NOT_ADMIN) || admin_refusal.text().contains("admin:read"),
        "the config refusal names the admin role or scope: {}",
        admin_refusal.text(),
    );
    let outsider_row = wired_be::models::user::get_by_nickname(pool, "plainstaff")
        .await
        .expect("the outsider reads")
        .expect("the outsider stands");
    let scoped = common::create_app_and_token(pool, &outsider_row, "admin:read admin:write").await;
    let still_staff = common::api_get(&world.app, Some(&scoped), M_ADMIN_REPORTS).await;
    assert_eq!(
        still_staff.status,
        StatusCode::FORBIDDEN,
        "scopes do not mint staff"
    );
    common::assert_error(&still_staff, NOT_STAFF, "the staff refusal by role");
    let still_admin = common::api_get(&world.app, Some(&scoped), CONFIG).await;
    assert_eq!(
        still_admin.status,
        StatusCode::FORBIDDEN,
        "scopes do not mint admins"
    );
    common::assert_error(&still_admin, NOT_ADMIN, "the admin refusal by role");
}

/// Scope-bearing tokens answer their exact sentences in both directions:
/// `admin:read` writes are refused, `admin:write` reads are refused, and a
/// per-area scope never crosses into another area.
async fn scope_gate_refusals(world: &World, pool: &PgPool) {
    let root = &world.admin_user;
    let read_only = common::create_app_and_token(pool, root, "admin:read").await;
    let write_only = common::create_app_and_token(pool, root, "admin:write").await;
    let reports_only = common::create_app_and_token(pool, root, "admin:read:reports").await;

    let denied_write = common::api_post(
        &world.app,
        Some(&read_only),
        &format!("{P_ADMIN}/reports/1/notes"),
        &json!({"content": "no"}),
    )
    .await;
    assert_eq!(
        denied_write.status,
        StatusCode::FORBIDDEN,
        "the read-only write is refused",
    );
    common::assert_error(&denied_write, NO_ADMIN_WRITE, "the read-only refusal");

    let denied_read =
        common::api_get(&world.app, Some(&write_only), &format!("{P_ADMIN}/reports")).await;
    assert_eq!(
        denied_read.status,
        StatusCode::FORBIDDEN,
        "the write-only read is refused",
    );
    common::assert_error(&denied_read, NO_ADMIN_READ, "the write-only refusal");

    let area_read = common::api_get(
        &world.app,
        Some(&reports_only),
        &format!("{P_ADMIN}/reports"),
    )
    .await;
    assert!(
        area_read.status.is_success(),
        "the reports scope reads its area: {}",
        area_read.text(),
    );
    let other_area = common::api_get(
        &world.app,
        Some(&reports_only),
        &format!("{P_ADMIN}/statuses"),
    )
    .await;
    assert_eq!(
        other_area.status,
        StatusCode::FORBIDDEN,
        "the reports scope cannot read statuses",
    );
    common::assert_error(&other_area, NO_ADMIN_READ, "the cross-area refusal");
}

/// The anonymous request is refused before the admin gate, and the
/// app-only token — no user behind it — never reaches the admin surface.
async fn gate_refusals_for_the_anonymous_and_the_machine(world: &World, pool: &PgPool) {
    let anon = common::api_get(&world.app, None, &format!("{P_ADMIN}/reports")).await;
    assert_eq!(
        anon.status,
        StatusCode::UNAUTHORIZED,
        "the anonymous admin read is a 401",
    );
    common::assert_error(&anon, "Invalid credentials", "the anonymous refusal");

    let app_only = common::create_app_only_token(pool, "admin").await;
    let machine = common::api_get(&world.app, Some(&app_only), &format!("{P_ADMIN}/reports")).await;
    assert_eq!(
        machine.status,
        StatusCode::FORBIDDEN,
        "the app-only admin read is refused",
    );
    common::assert_error(
        &machine,
        "This API requires an authenticated user",
        "the app-only refusal",
    );
}

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

/// Scenario 6, second half: following a relay mirrors the subscription in
/// the `{actor, followed_back}` ledger — the flag true only once the relay
/// has accepted the instance actor's follow back — delivers the `Follow`
/// outbound, and the `force` unfollow answers the address as a JSON string
/// while it clears the ledger and sends the `Undo`; a second withdrawal of
/// the same relay is the `400` miss.
#[tokio::test]
async fn following_a_relay_mirrors_the_subscription_and_the_ledger() {
    let db = common::test_db().await;
    let pool = db.pool().clone();
    let world = World::open(pool.clone()).await;
    let peer = remote::MockPeer::start().await;
    let relay_url = peer.relay();

    let followed = common::api_post(
        &world.app,
        Some(&world.admin),
        &format!("{P_ADMIN}/relay"),
        &json!({"relay_url": relay_url}),
    )
    .await;
    common::assert_answer(&followed, StatusCode::OK, "the relay follow");
    assert_eq!(
        followed.json_value(),
        json!({"actor": relay_url, "followed_back": false}),
        "a fresh follow answers its actor, not yet followed back",
    );

    let listed = common::api_get(&world.app, Some(&world.admin), &format!("{P_ADMIN}/relay")).await;
    common::assert_answer(&listed, StatusCode::OK, "the subscription listing");
    assert_eq!(
        listed.json_value(),
        json!({"relays": [{"actor": relay_url, "followed_back": false}]}),
        "the ledger mirrors the subscription",
    );

    relay_read_backs_its_accepted_flag(&world, &peer, &relay_url).await;

    relay_withdrawal_walks_the_ledger_and_the_undo(&world, &peer, &relay_url).await;
}

/// Stage the relay's follow of the instance actor through the model and
/// read the flipped `followed_back` flag back through the ledger.
async fn relay_read_backs_its_accepted_flag(
    world: &World,
    peer: &remote::MockPeer,
    relay_url: &str,
) {
    let pool = &world.state.db;
    let mirror = wired_be::models::user::get_by_ap_id(pool, relay_url)
        .await
        .expect("the mirror reads")
        .expect("the follow mirrored the relay actor");
    let instance =
        wired_be::services::instance_actor::ensure_instance_actor(pool, &world.state.config)
            .await
            .expect("the instance actor stands");
    let row = wired_be::models::follow::follow(pool, BASE, &mirror, &instance)
        .await
        .expect("the relay's follow of the instance actor records");
    let _granted = wired_be::models::follow::accept(pool, row.id)
        .await
        .expect("the instance actor grants it");
    let back = common::api_get(&world.app, Some(&world.admin), &format!("{P_ADMIN}/relay")).await;
    assert_eq!(
        back.json_value()["relays"][0]["followed_back"],
        json!(true),
        "the accepted follow reads back in the ledger",
    );

    let _drain = world.drain().await;
    assert!(
        !peer.delivered("Follow").is_empty(),
        "the relay follow goes out over the federator",
    );
}

/// The `force` unfollow echoes the address as a JSON string, logs both
/// moves, sends the `Undo`, empties the ledger and answers the second
/// withdrawal with the `400` miss.
async fn relay_withdrawal_walks_the_ledger_and_the_undo(
    world: &World,
    peer: &remote::MockPeer,
    relay_url: &str,
) {
    let unfollowed = common::api_delete(
        &world.app,
        Some(&world.admin),
        &format!("{P_ADMIN}/relay"),
        &json!({"relay_url": relay_url, "force": true}),
    )
    .await;
    common::assert_answer(&unfollowed, StatusCode::OK, "the relay unfollow");
    assert_eq!(
        unfollowed.json::<String>(),
        relay_url,
        "the unfollow echoes the address as a JSON string",
    );

    assert_log(
        &world.app,
        &world.admin,
        "relay_follow",
        &[format!("@root followed relay: {relay_url}")],
    )
    .await;
    assert_log(
        &world.app,
        &world.admin,
        "relay_unfollow",
        &[format!("@root unfollowed relay: {relay_url}")],
    )
    .await;

    let _undo_drain = world.drain().await;
    assert!(
        !peer.delivered("Undo").is_empty(),
        "the relay unfollow goes out as an Undo",
    );

    let twice = common::api_delete(
        &world.app,
        Some(&world.admin),
        &format!("{P_ADMIN}/relay"),
        &json!({"relay_url": relay_url, "force": true}),
    )
    .await;
    assert_eq!(
        twice.status,
        StatusCode::BAD_REQUEST,
        "unfollowing twice is a miss"
    );
    common::assert_error(&twice, "Not found", "the unfollow miss");
    let empty = common::api_get(&world.app, Some(&world.admin), &format!("{P_ADMIN}/relay")).await;
    assert_eq!(
        empty.json_value(),
        json!({"relays": []}),
        "the ledger reads empty again"
    );
}

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
    .bind(&stale_token)
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
            sqlx::query("SELECT 1 FROM oauth_tokens WHERE token = $1").bind(&seeds.expired),
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
            sqlx::query("SELECT 1 FROM oauth_tokens WHERE token = $1").bind(&seeds.live_token),
        ),
    ];
    for (what, probe) in kept {
        let row = probe.fetch_optional(pool).await.expect("a probe runs");
        assert!(row.is_some(), "the {what} must survive the sweep");
    }
    drained
}

/// Scenario 1, with scenario 4's moderation-surface half walked inline.
#[tokio::test]
async fn a_report_walks_both_admin_apis_and_the_staff_actions() {
    let db = common::test_db().await;
    let pool = db.pool().clone();
    let world = World::open(pool).await;
    let reporter = world.register("witness").await;
    let target = world.register("target").await;

    let flagged = post_status(&world, &target, "the reported post").await;
    let flagged_id = id_of(&flagged);
    let survivor = post_status(&world, &target, "the survivor").await;
    let survivor_id = id_of(&survivor);

    let filed = common::api_post(
        &world.app,
        Some(&reporter.token),
        REPORTS,
        &json!({
            "account_id": target.id,
            "status_ids": [flagged_id],
            "comment": "this is harassment",
            "category": "spam",
        }),
    )
    .await;
    common::assert_answer(&filed, StatusCode::OK, "the filing");
    let report_id = id_of(&filed.json_value());
    assert_eq!(
        filed.json_value()["action_taken"],
        json!(false),
        "a fresh report acts on nothing"
    );

    report_tellings(&world, &report_id, &reporter, &target).await;
    notes_assign_and_resolve(&world, &report_id).await;
    admin_edits_and_deletes(&world, &flagged_id, &survivor_id).await;
    deactivation_blinds_then_restores(&world, &target).await;
}

/// The one filing, told three ways: the Pleroma admin list entity, the
/// Mastodon admin list entity, and the legacy `/api/v0` twin's total.
async fn report_tellings(world: &World, report_id: &str, reporter: &Account, target: &Account) {
    let pleroma = listed_pleroma_report(&world.app, &world.moderator, report_id).await;
    assert_eq!(
        pleroma["content"],
        json!("this is harassment"),
        "the Pleroma comment"
    );
    assert_eq!(pleroma["state"], json!("open"), "a fresh report is open");
    assert_eq!(
        pleroma["actor"]["id"],
        json!(reporter.id),
        "the Pleroma actor is the reporter"
    );
    assert_eq!(
        pleroma["account"]["id"],
        json!(target.id),
        "the Pleroma account is the target"
    );
    assert_eq!(
        pleroma["statuses"].as_array().map(Vec::len),
        Some(1),
        "the cited status rides the admin entity"
    );

    let mastodon = get_array(&world.app, Some(&world.admin), M_ADMIN_REPORTS)
        .await
        .into_iter()
        .find(|row| row["id"] == json!(report_id))
        .expect("the filing shows in the Mastodon admin list");
    assert_eq!(
        mastodon["target_account"]["id"],
        json!(target.id),
        "the Mastodon target"
    );
    assert_eq!(
        mastodon["assigned_account"],
        json!(null),
        "the Mastodon queue starts unassigned"
    );
    assert_eq!(
        mastodon["statuses"].as_array().map(Vec::len),
        Some(1),
        "the Mastodon entity cites the status too"
    );

    let legacy = common::api_get(&world.app, Some(&world.admin), V0_REPORTS).await;
    common::assert_answer(&legacy, StatusCode::OK, "the legacy admin listing");
    assert!(
        legacy.json_value()["total"].as_i64() >= Some(1),
        "the legacy twin counts it"
    );
}

/// `GET {P_ADMIN}/reports?state=open` narrowed to `report_id`, asserted
/// present.
async fn listed_pleroma_report(app: &Router, token: &str, report_id: &str) -> Value {
    let response =
        common::api_get(app, Some(token), &format!("{P_ADMIN}/reports?state=open")).await;
    common::assert_answer(&response, StatusCode::OK, "the Pleroma admin listing");
    let body = response.json_value();
    let rows = body["reports"]
        .as_array()
        .expect("the admin page answers its reports array");
    rows.iter()
        .find(|row| row["id"].as_str() == Some(report_id))
        .cloned()
        .unwrap_or_else(|| panic!("the filing shows in the Pleroma admin list: {body}"))
}

/// The notes lifecycle, the assignment through both APIs, the unassign
/// through both, and the resolve/reopen/resolve walk — every move with its
/// exact moderation sentence.
async fn notes_assign_and_resolve(world: &World, report_id: &str) {
    let base = format!("{P_ADMIN}/reports");
    let note_head = format!("@moddy added note 'looked at it' to report ##{report_id}");
    let assigned_head =
        format!("@moddy assigned report ##{report_id} ( on user @target) to user moddy");
    let noted = common::api_post(
        &world.app,
        Some(&world.moderator),
        &format!("{base}/{report_id}/notes"),
        &json!({"content": "looked at it"}),
    )
    .await;
    common::assert_answer(&noted, StatusCode::NO_CONTENT, "the note write");
    let shown = common::api_get(
        &world.app,
        Some(&world.moderator),
        &format!("{base}/{report_id}"),
    )
    .await;
    let note_id = shown.json_value()["notes"][0]["id"]
        .as_str()
        .expect("the note carries its id")
        .to_owned();
    assert_log(
        &world.app,
        &world.admin,
        "report_note",
        &[format!("{note_head} on user @target")],
    )
    .await;

    let assigned = common::api_post(
        &world.app,
        Some(&world.moderator),
        &format!("{base}/assign_account"),
        &json!({"reports": [{"id": report_id, "assigned_account": "moddy"}]}),
    )
    .await;
    common::assert_answer(&assigned, StatusCode::OK, "the Pleroma assignment");
    assert_eq!(
        assigned.json_value()[0]["assigned_account"]["id"],
        json!(world.moderator_user.id.to_string()),
        "the Pleroma entity names its new owner"
    );

    let to_self = common::api_post(
        &world.app,
        Some(&world.moderator),
        &format!("{M_ADMIN_REPORTS}/{report_id}/assign_to_self"),
        &Value::Null,
    )
    .await;
    common::assert_answer(&to_self, StatusCode::OK, "the Mastodon assign-to-self");
    assert_eq!(
        to_self.json_value()["assigned_account"]["id"],
        json!(world.moderator_user.id.to_string()),
        "the Mastodon entity names its owner too"
    );
    assert_log(
        &world.app,
        &world.admin,
        "report_assigned",
        &[assigned_head.clone(), assigned_head],
    )
    .await;

    unassign_report(world, report_id, &base).await;
    resolve_reopen_resolve(world, report_id).await;
    delete_report_note(world, report_id, &note_id, &base).await;
}

/// The unassignment through both admin APIs, logged once per move.
async fn unassign_report(world: &World, report_id: &str, base: &str) {
    let unassigned_head =
        format!("@moddy unassigned report ##{report_id} ( on user @target) from a user");
    let unassigned = common::api_post(
        &world.app,
        Some(&world.moderator),
        &format!("{base}/assign_account"),
        &json!({"reports": [{"id": report_id, "assigned_account": null}]}),
    )
    .await;
    common::assert_answer(&unassigned, StatusCode::OK, "the Pleroma unassignment");
    assert!(
        unassigned.json_value()[0].get("assigned_account").is_none(),
        "the key comes back off once unassigned: {}",
        unassigned.text()
    );
    let m_unassign = common::api_post(
        &world.app,
        Some(&world.moderator),
        &format!("{M_ADMIN_REPORTS}/{report_id}/unassign"),
        &Value::Null,
    )
    .await;
    common::assert_answer(&m_unassign, StatusCode::OK, "the Mastodon unassign");
    assert_eq!(
        m_unassign.json_value()["assigned_account"],
        json!(null),
        "the Mastodon null"
    );
    assert_log(
        &world.app,
        &world.admin,
        "report_unassigned",
        &[unassigned_head.clone(), unassigned_head],
    )
    .await;
}

/// The note delete and its `report_note_delete` sentence.
async fn delete_report_note(world: &World, report_id: &str, note_id: &str, base: &str) {
    let removed = common::api_delete(
        &world.app,
        Some(&world.moderator),
        &format!("{base}/{report_id}/notes/{note_id}"),
        &Value::Null,
    )
    .await;
    common::assert_answer(&removed, StatusCode::NO_CONTENT, "the note delete");
    assert_log(
        &world.app,
        &world.admin,
        "report_note_delete",
        &[format!(
            "@moddy deleted note 'looked at it' from report ##{report_id} on user @target"
        )],
    )
    .await;
}

/// The suspended account's own surfaces: the 404 profile, the 403 bearer,
/// and the admin god-mode ledger that still reaches the hidden post.
async fn assert_hidden_from_the_client(world: &World, target: &Account, fresh_id: &str) {
    let profile = common::api_get(
        &world.app,
        None,
        &format!("{}/{}", "/api/v1/accounts", target.id),
    )
    .await;
    common::assert_answer(
        &profile,
        StatusCode::NOT_FOUND,
        "the deactivated profile is a 404",
    );
    let refused = common::api_get(&world.app, Some(&target.token), VERIFY).await;
    common::assert_answer(
        &refused,
        StatusCode::FORBIDDEN,
        "the disabled bearer is refused",
    );
    common::assert_error(&refused, DISABLED, "the disabled bearer is refused");
    assert!(
        carries(
            &get_array(
                &world.app,
                Some(&world.admin),
                &format!("{P_ADMIN}/statuses?godmode=true")
            )
            .await,
            "id",
            fresh_id,
        ),
        "the admin ledger still reaches the hidden post"
    );
}

/// The state-change trio: the Pleroma `PATCH` resolve, the Mastodon
/// reopen (which clears the action stamp), and the Mastodon resolve —
/// three `report_update` sentences, and the Pleroma entity reading
/// `resolved` again at the end.
async fn resolve_reopen_resolve(world: &World, report_id: &str) {
    let patched = common::api_patch(
        &world.app,
        Some(&world.admin),
        &format!("{P_ADMIN}/reports"),
        &json!({"reports": [{"id": report_id, "state": "resolved"}]}),
    )
    .await;
    common::assert_answer(&patched, StatusCode::NO_CONTENT, "the Pleroma resolve");

    let reopened = common::api_post(
        &world.app,
        Some(&world.moderator),
        &format!("{M_ADMIN_REPORTS}/{report_id}/reopen"),
        &Value::Null,
    )
    .await;
    common::assert_answer(&reopened, StatusCode::OK, "the Mastodon reopen");
    assert_eq!(
        reopened.json_value()["action_taken"],
        json!(false),
        "the reopen clears the action"
    );

    let resolved = common::api_post(
        &world.app,
        Some(&world.moderator),
        &format!("{M_ADMIN_REPORTS}/{report_id}/resolve"),
        &Value::Null,
    )
    .await;
    common::assert_answer(&resolved, StatusCode::OK, "the Mastodon resolve");
    assert_eq!(
        resolved.json_value()["action_taken"],
        json!(true),
        "the resolve acts"
    );
    assert_eq!(
        resolved.json_value()["action_taken_by_account"]["id"],
        json!(world.moderator_user.id.to_string()),
        "the resolving staffer is named"
    );
    assert_log(
        &world.app,
        &world.admin,
        "report_update",
        &[
            format!("@moddy updated report ##{report_id} ( on user @target) with 'resolved' state"),
            format!("@moddy updated report ##{report_id} ( on user @target) with 'open' state"),
            format!("@root updated report ##{report_id} ( on user @target) with 'resolved' state"),
        ],
    )
    .await;

    let closed_page = common::api_get(
        &world.app,
        Some(&world.moderator),
        &format!("{P_ADMIN}/reports?state=resolved"),
    )
    .await;
    common::assert_answer(&closed_page, StatusCode::OK, "the resolved filter");
    assert!(
        closed_page.json_value()["reports"]
            .as_array()
            .is_some_and(|rows| rows.iter().any(|row| row["id"] == json!(report_id))),
        "the resolved report reads back through the state filter"
    );
}

/// The admin statuses surface's edit and delete: the reported status
/// re-scoped with its `status_update`, then the survivor deleted with its
/// `status_delete` and the client read after it a 404.
async fn admin_edits_and_deletes(world: &World, reported_id: &str, survivor_id: &str) {
    let edited = common::api_put(
        &world.app,
        Some(&world.moderator),
        &format!("{P_ADMIN}/statuses/{reported_id}"),
        &json!({"sensitive": true, "visibility": "private"}),
    )
    .await;
    common::assert_answer(&edited, StatusCode::OK, "the admin status edit");
    assert_eq!(
        edited.json_value()["sensitive"],
        json!(true),
        "the entity reads sensitive"
    );
    assert_eq!(
        edited.json_value()["visibility"],
        json!("private"),
        "and private"
    );
    assert!(
        !public_ids(&world.app)
            .await
            .contains(&reported_id.to_owned()),
        "a re-scoped private post leaves the public timeline"
    );
    assert_log(
        &world.app,
        &world.admin,
        "status_update",
        &[format!(
            "@moddy updated status ##{reported_id}, set sensitive: 'true', visibility: 'private'"
        )],
    )
    .await;

    let deleted = common::api_delete(
        &world.app,
        Some(&world.moderator),
        &format!("{P_ADMIN}/statuses/{survivor_id}"),
        &Value::Null,
    )
    .await;
    common::assert_answer(&deleted, StatusCode::OK, "the admin status delete");
    assert_eq!(
        deleted.json_value(),
        json!({}),
        "the delete answers the empty object"
    );
    let gone = common::api_get(&world.app, None, &format!("{STATUSES}/{survivor_id}")).await;
    common::assert_answer(&gone, StatusCode::NOT_FOUND, "the deleted status is a 404");
    assert_log(
        &world.app,
        &world.admin,
        "status_delete",
        &[format!("@moddy deleted status ##{survivor_id}")],
    )
    .await;
}

/// The deactivation leg: a fresh public post rides the timeline, the
/// account's deactivation takes it off (and 403s its own bearer and 404s
/// its profile) while the admin ledger still reaches it, and the
/// reactivation returns it — both moves logged.
async fn deactivation_blinds_then_restores(world: &World, target: &Account) {
    let fresh = post_status(world, target, "the live one").await;
    let fresh_id = id_of(&fresh);
    assert!(
        public_ids(&world.app).await.contains(&fresh_id),
        "the fresh post rides the timeline"
    );

    let off = common::api_patch(
        &world.app,
        Some(&world.admin),
        &format!("{USERS}/deactivate"),
        &json!({"nicknames": [target.nickname]}),
    )
    .await;
    common::assert_answer(&off, StatusCode::OK, "the deactivation");
    assert_eq!(
        off.json_value()["users"][0]["is_active"],
        json!(false),
        "answered deactivated"
    );
    assert!(
        !public_ids(&world.app).await.contains(&fresh_id),
        "the deactivated author's post leaves the public timeline"
    );
    assert_hidden_from_the_client(world, target, &fresh_id).await;
    assert_log(
        &world.app,
        &world.admin,
        "deactivate",
        &["@root deactivated users: @target".to_owned()],
    )
    .await;

    let on = common::api_patch(
        &world.app,
        Some(&world.admin),
        &format!("{USERS}/activate"),
        &json!({"nicknames": [target.nickname]}),
    )
    .await;
    common::assert_answer(&on, StatusCode::OK, "the reactivation");
    assert!(
        public_ids(&world.app).await.contains(&fresh_id),
        "the reactivated author's post returns to the timeline"
    );
    let ok_again = common::api_get(&world.app, Some(&target.token), VERIFY).await;
    common::assert_answer(
        &ok_again,
        StatusCode::OK,
        "the released account reads itself",
    );
    assert_log(
        &world.app,
        &world.admin,
        "activate",
        &["@root activated users: @target".to_owned()],
    )
    .await;
}
