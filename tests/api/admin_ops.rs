//! Admin ops endpoints over HTTP (`/api/v1/pleroma/admin/…`, WBE-208): the
//! media-proxy cache ledger (`media_proxy_caches`, its `delete` and `purge`
//! actions, the optional `PURGE` forwarding), the frontends catalogue and
//! installer (`frontends`, `frontends/install`), and the admin backup
//! trigger (`backups`) — against the real router, through mock upstream,
//! archive and cache servers on loopback.

#![cfg(test)]

use std::{
    io::{Cursor, Write as _},
    sync::{Arc, Mutex},
};

use axum::{
    Router,
    extract::State,
    http::{Method, StatusCode, Uri, header},
    response::{IntoResponse as _, Response},
};
use bytes::Bytes;
use serde_json::{Value, json};
use sqlx::PgPool;
use tempfile::TempDir;
use tokio::net::TcpListener;
use wired_be::{
    config::{Config, FrontendConfig, FrontendSettings},
    models::user::{Flag, NewUser},
    services::media_proxy,
};

use crate::common;

/// The bytes the mock upstream serves for every proxied request.
const CACHED: &[u8] = b"cached-bytes";
/// Where these tests expect the admin API.
const ADMIN: &str = "/api/v1/pleroma/admin";

/// A [`Config`] with the media proxy on (loopback allowed, rate limiting
/// off) and `purge_url` as `MEDIA_PROXY_PURGE_URL`.
fn ops_config(purge_url: Option<&str>) -> Config {
    Config::from_lookup(|key| match key {
        wired_be::config::DATABASE_URL_VAR => Some(wired_be::config::TEST_DATABASE_URL.to_owned()),
        wired_be::config::BASE_URL_VAR => Some(wired_be::config::TEST_BASE_URL.to_owned()),
        wired_be::config::HTTP_ALLOW_PRIVATE_HOSTS_VAR
        | wired_be::config::MEDIA_PROXY_ENABLED_VAR => Some("true".to_owned()),
        wired_be::config::MEDIA_PROXY_SECRET_VAR => {
            Some(common::MEDIA_PROXY_TEST_SECRET.to_owned())
        }
        wired_be::config::RATE_LIMITS_ENABLED_VAR => Some("false".to_owned()),
        wired_be::config::MEDIA_PROXY_PURGE_URL_VAR => purge_url.map(str::to_owned),
        _ => None,
    })
    .expect("the fixed admin-ops test config is valid")
}

/// [`ops_config`] with `FRONTENDS_DIR` moved to `dir` and one installable
/// frontend configured: `pleroma-fe` at ref `main`, its `dist` subtree
/// built from `build_url`.
fn frontend_config(dir: &std::path::Path, build_url: &str) -> Config {
    let mut config = ops_config(None);
    config.statics.frontends = FrontendSettings {
        dir: dir.to_path_buf(),
        available: vec![FrontendConfig {
            name: "pleroma-fe".to_owned(),
            git: Some("https://example.com/pleroma-fe".to_owned()),
            build_url: Some(build_url.to_owned()),
            reference: Some("main".to_owned()),
            build_dir: Some("dist".to_owned()),
        }],
    };
    config
}

/// Binds `app` on loopback and returns its origin; the task lives for the
/// rest of the test runtime, as the fetching endpoints need it to.
async fn serve(app: Router) -> String {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    let _server: tokio::task::JoinHandle<()> = tokio::spawn(async move {
        if let Err(err) = axum::serve(listener, app).await {
            tracing::debug!("mock server stopped: {err}");
        }
    });
    format!("http://{addr}")
}

/// The mock media upstream: every request answers `image/png` with
/// [`CACHED`].
async fn spawn_upstream() -> String {
    let app = Router::new().fallback(upstream_media);
    serve(app).await
}

/// Serves [`CACHED`] as `image/png` for any request.
async fn upstream_media() -> Response {
    (
        [(header::CONTENT_TYPE, "image/png")],
        Bytes::from_static(CACHED),
    )
        .into_response()
}

/// The mock build server: its known archives and the request lines it saw.
#[derive(Debug)]
struct ArchiveServer {
    /// The archives by request path (`/name.zip`).
    archives: Vec<(String, Vec<u8>)>,
    /// Every `"{METHOD} {path}"` line the server answered.
    asked: Arc<Mutex<Vec<String>>>,
}

impl ArchiveServer {
    /// The bytes of the archive stored under `path`, if any.
    fn archive_at(&self, path: &str) -> Option<Vec<u8>> {
        self.archives
            .iter()
            .find(|(name, _)| name == path)
            .map(|(_, bytes)| bytes.clone())
    }
}

/// A mock build server: every `GET` answers the archive stored under its
/// path (404 for no path it knows) and every request line is recorded.
async fn spawn_archive_server(archives: Vec<(&str, Vec<u8>)>) -> (String, Arc<Mutex<Vec<String>>>) {
    let server = Arc::new(ArchiveServer {
        archives: archives
            .into_iter()
            .map(|(name, bytes)| (format!("/{name}"), bytes))
            .collect(),
        asked: Arc::new(Mutex::new(Vec::new())),
    });
    let app = Router::new()
        .fallback(record_and_serve)
        .with_state(Arc::clone(&server));
    let origin = serve(app).await;
    (origin, Arc::clone(&server.asked))
}

/// Records the request line and answers the matching archive.
async fn record_and_serve(
    State(server): State<Arc<ArchiveServer>>,
    method: Method,
    uri: Uri,
) -> Response {
    server
        .asked
        .lock()
        .unwrap()
        .push(format!("{method} {}", uri.path()));
    server.archive_at(uri.path()).map_or_else(
        || StatusCode::NOT_FOUND.into_response(),
        |bytes| ([(header::CONTENT_TYPE, "application/zip")], bytes).into_response(),
    )
}

/// The mock CDN cache: records every request line and always accepts it.
async fn spawn_purge_server() -> (String, Arc<Mutex<Vec<String>>>) {
    let recorded: Arc<Mutex<Vec<String>>> = Arc::new(Mutex::new(Vec::new()));
    let app = Router::new()
        .fallback(record_purge)
        .with_state(Arc::clone(&recorded));
    let origin = serve(app).await;
    (origin, recorded)
}

/// Records the invalidation request and accepts it.
async fn record_purge(
    State(recorded): State<Arc<Mutex<Vec<String>>>>,
    method: Method,
    uri: Uri,
) -> (StatusCode, &'static str) {
    recorded
        .lock()
        .unwrap()
        .push(format!("{method} {}", uri.path()));
    (StatusCode::OK, "purged")
}

/// The signed proxy path for `url` under `config`, as a client would ask
/// it (the instance origin stripped).
fn proxy_path(config: &Config, url: &str) -> String {
    let link = media_proxy::proxy_url(config, url);
    link.strip_prefix(wired_be::config::TEST_BASE_URL)
        .unwrap_or_else(|| panic!("the link should hang off the test origin: {link}"))
        .to_owned()
}

/// Fetches every proxied URL in `urls` through `app`, asserting each is
/// served — which is what records it into the ledger.
async fn seed_cache(app: &Router, config: &Config, urls: &[String]) {
    for url in urls {
        let path = proxy_path(config, url);
        let response = common::TestRequest::get(&path).send(app).await;
        common::assert_answer(&response, StatusCode::OK, &format!("proxied {url}"));
        assert_eq!(response.body, CACHED, "served bytes for {url}");
    }
}

/// An admin named `nickname` holding `scopes`.
async fn admin_token(pool: &PgPool, nickname: &str, scopes: &str) -> String {
    let created = common::create_local_user_with(pool, nickname, |user| NewUser {
        is_admin: Flag::new(true),
        ..user
    })
    .await;
    common::create_app_and_token(pool, &created, scopes).await
}

/// `GET {ADMIN}/media_proxy_caches{query}` as `token`, answered as JSON.
async fn caches(app: &Router, token: &str, query: &str) -> Value {
    let response = common::TestRequest::get(&format!("{ADMIN}/media_proxy_caches{query}"))
        .bearer(token)
        .send(app)
        .await;
    common::assert_answer(&response, StatusCode::OK, "the cache listing reads");
    response.json_value()
}

/// A zip holding `files` as `(entry name, contents)`.
fn zip_bytes(files: &[(&str, &[u8])]) -> Vec<u8> {
    let mut buffer = Vec::new();
    {
        let mut writer = zip::ZipWriter::new(Cursor::new(&mut buffer));
        let options = zip::write::SimpleFileOptions::default();
        for (name, contents) in files {
            writer.start_file(*name, options).unwrap();
            writer.write_all(contents).unwrap();
        }
        let _closed = writer.finish().unwrap();
    }
    buffer
}

#[tokio::test]
async fn the_proxy_records_every_served_url_for_the_admin_ledger() {
    let db = common::test_db().await;
    let pool = db.pool().clone();
    let upstream = spawn_upstream().await;
    let config = ops_config(None);
    let app = common::test_app_with_config(pool.clone(), config.clone());
    let token = admin_token(&pool, "cache-reader", "admin:read").await;

    let urls: Vec<String> = ["zeta.png", "alpha.png"]
        .iter()
        .map(|name| format!("{upstream}/{name}"))
        .collect();
    seed_cache(&app, &config, &urls).await;
    // A second fetch of one URL must not double-record it.
    seed_cache(&app, &config, &urls[..1]).await;

    let body = caches(&app, &token, "").await;
    assert_eq!(body["page_size"], 20, "the ticket default");
    assert_eq!(body["count"], 2, "each served url is in the ledger once");
    let listed: Vec<&str> = body["urls"]
        .as_array()
        .unwrap()
        .iter()
        .map(Value::as_str)
        .map(Option::unwrap)
        .collect();
    assert_eq!(
        listed,
        [&urls[1], &urls[0]],
        "the page is url-ordered, not insert-ordered"
    );

    let filtered = caches(&app, &token, "?query=alph").await;
    assert_eq!(filtered["count"], 1);
    assert_eq!(filtered["urls"], json!([urls[1]]));

    let window = caches(&app, &token, "?page=2&page_size=1").await;
    assert_eq!(window["page_size"], 1);
    assert_eq!(window["count"], 2, "the count spans pages");
    assert_eq!(
        window["urls"],
        json!([urls[0]]),
        "page 2 holds the last url"
    );
}

#[tokio::test]
async fn cache_delete_removes_the_named_urls_and_echoes_them() {
    let db = common::test_db().await;
    let pool = db.pool().clone();
    let upstream = spawn_upstream().await;
    let config = ops_config(None);
    let app = common::test_app_with_config(pool.clone(), config.clone());
    let token = admin_token(&pool, "cache-deleter", "admin:read admin:write").await;

    let gone = format!("{upstream}/gone.png");
    let kept = format!("{upstream}/kept.png");
    seed_cache(&app, &config, &[gone.clone(), kept.clone()]).await;

    let response = common::TestRequest::post(&format!("{ADMIN}/media_proxy_caches/delete"))
        .bearer(&token)
        .json(&json!({ "urls": [gone, "https://never-cached.example/x.png"] }))
        .send(&app)
        .await;
    common::assert_answer(&response, StatusCode::OK, "the delete lands");
    assert_eq!(
        response.json_value()["urls"]
            .as_array()
            .expect("the answer carries urls")
            .len(),
        2,
        "the answer echoes the requested set, cached or not"
    );

    let body = caches(&app, &token, "").await;
    assert_eq!(body["count"], 1);
    assert_eq!(body["urls"], json!([kept]));
}

#[tokio::test]
async fn cache_purge_sends_a_signed_purge_per_url_and_clears_the_ledger() {
    let db = common::test_db().await;
    let pool = db.pool().clone();
    let upstream = spawn_upstream().await;
    let (cache, purged) = spawn_purge_server().await;
    let config = ops_config(Some(&cache));
    let app = common::test_app_with_config(pool.clone(), config.clone());
    let token = admin_token(&pool, "cache-purger", "admin:read admin:write").await;

    let first = format!("{upstream}/first.png");
    let second = format!("{upstream}/second.png");
    seed_cache(&app, &config, &[first.clone(), second.clone()]).await;

    let response = common::TestRequest::post(&format!("{ADMIN}/media_proxy_caches/purge"))
        .bearer(&token)
        .json(&json!({ "urls": [first, second.clone()], "ban": true }))
        .send(&app)
        .await;
    common::assert_answer(&response, StatusCode::OK, "the purge lands");
    assert_eq!(
        response.json_value(),
        json!({ "urls": [first, second] }),
        "the answer echoes the requested urls"
    );

    let sent = purged.lock().unwrap().clone();
    assert_eq!(sent.len(), 2, "one PURGE per url: {sent:?}");
    for line in sent {
        let (method, path) = line.split_once(' ').expect("a request line");
        assert_eq!(method, "PURGE", "the invalidation verb");
        assert!(path.starts_with("/proxy/"), "a signed proxy link: {path}");
        assert!(
            path.ends_with("/first.png") || path.ends_with("/second.png"),
            "the link carries the filename: {path}"
        );
    }

    assert_eq!(
        caches(&app, &token, "").await["count"],
        0,
        "the ledger is clear"
    );
}

#[tokio::test]
async fn cache_purge_without_a_purge_url_only_clears_the_ledger() {
    let db = common::test_db().await;
    let pool = db.pool().clone();
    let upstream = spawn_upstream().await;
    let config = ops_config(None);
    let app = common::test_app_with_config(pool.clone(), config.clone());
    let token = admin_token(&pool, "cache-purger-off", "admin:read admin:write").await;

    let url = format!("{upstream}/solo.png");
    seed_cache(&app, &config, std::slice::from_ref(&url)).await;

    let response = common::TestRequest::post(&format!("{ADMIN}/media_proxy_caches/purge"))
        .bearer(&token)
        .json(&json!({ "urls": [url.clone()] }))
        .send(&app)
        .await;
    common::assert_answer(&response, StatusCode::OK, "the purge still lands");
    assert_eq!(response.json_value()["urls"], json!([url]));
    assert_eq!(caches(&app, &token, "").await["count"], 0);
}

#[tokio::test]
async fn cache_actions_without_urls_answer_invalid_parameters() {
    let db = common::test_db().await;
    let pool = db.pool().clone();
    let app = common::test_app_with_config(pool.clone(), ops_config(None));
    let token = admin_token(&pool, "cache-body", "admin:write").await;

    for action in ["delete", "purge"] {
        for body in [json!({}), json!({ "urls": null })] {
            let response =
                common::TestRequest::post(&format!("{ADMIN}/media_proxy_caches/{action}"))
                    .bearer(&token)
                    .json(&body)
                    .send(&app)
                    .await;
            common::assert_answer(
                &response,
                StatusCode::BAD_REQUEST,
                &format!("{action} body"),
            );
            common::assert_error(&response, "Invalid parameters", &format!("{action} body"));
        }
    }
}

#[tokio::test]
async fn the_admin_ops_routes_guard_their_scopes() {
    let db = common::test_db().await;
    let pool = db.pool().clone();
    let app = common::test_app_with_config(pool.clone(), ops_config(None));

    let plain = common::create_local_user(&pool, "ops-plain").await;
    let user_token = common::create_app_and_token(&pool, &plain, "admin:read").await;
    let no_scope = admin_token(&pool, "ops-noscope", "read").await;
    let wrong_area = admin_token(&pool, "ops-other-area", "admin:read:statuses").await;
    let read_only = admin_token(&pool, "ops-read-only", "admin:read:media_proxy_caches").await;

    let unauthenticated = common::TestRequest::get(&format!("{ADMIN}/media_proxy_caches"))
        .send(&app)
        .await;
    common::assert_answer(
        &unauthenticated,
        StatusCode::UNAUTHORIZED,
        "no token at all",
    );
    common::assert_error(&unauthenticated, "Invalid credentials", "no token at all");

    let not_admin = common::TestRequest::get(&format!("{ADMIN}/frontends"))
        .bearer(&user_token)
        .send(&app)
        .await;
    common::assert_answer(&not_admin, StatusCode::FORBIDDEN, "a non-admin");
    common::assert_error(&not_admin, "User is not an admin.", "a non-admin");

    let no_scope_answer = common::TestRequest::get(&format!("{ADMIN}/media_proxy_caches"))
        .bearer(&no_scope)
        .send(&app)
        .await;
    common::assert_answer(
        &no_scope_answer,
        StatusCode::FORBIDDEN,
        "an admin without admin scopes",
    );
    common::assert_error(
        &no_scope_answer,
        "Insufficient permissions: admin:read.",
        "an admin without admin scopes",
    );

    let other_area_answer = common::TestRequest::get(&format!("{ADMIN}/media_proxy_caches"))
        .bearer(&wrong_area)
        .send(&app)
        .await;
    common::assert_answer(
        &other_area_answer,
        StatusCode::FORBIDDEN,
        "a statuses-scoped admin",
    );
    common::assert_error(
        &other_area_answer,
        "Insufficient permissions: admin:read.",
        "a statuses-scoped admin",
    );

    // The narrow cache-read alias reaches the list …
    let narrow = common::TestRequest::get(&format!("{ADMIN}/media_proxy_caches"))
        .bearer(&read_only)
        .send(&app)
        .await;
    common::assert_answer(&narrow, StatusCode::OK, "the area read alias");

    // … but not the write actions, which ask for `admin:write`.
    let write_answer = common::TestRequest::post(&format!("{ADMIN}/media_proxy_caches/delete"))
        .bearer(&read_only)
        .json(&json!({ "urls": [] }))
        .send(&app)
        .await;
    common::assert_answer(
        &write_answer,
        StatusCode::FORBIDDEN,
        "read alias on a write route",
    );
    common::assert_error(
        &write_answer,
        "Insufficient permissions: admin:write.",
        "read alias on a write route",
    );
}

#[tokio::test]
async fn frontends_list_the_catalogue_with_their_installed_flag() {
    let dir = TempDir::new().unwrap();
    std::fs::create_dir_all(dir.path().join("pleroma-fe/main")).unwrap();
    let db = common::test_db().await;
    let pool = db.pool().clone();
    let app = common::test_app_with_config(
        pool.clone(),
        frontend_config(dir.path(), "https://build.invalid/fe.zip"),
    );
    let token = admin_token(&pool, "fe-reader", "admin:read").await;

    let response = common::TestRequest::get(&format!("{ADMIN}/frontends"))
        .bearer(&token)
        .send(&app)
        .await;
    common::assert_answer(&response, StatusCode::OK, "the catalogue reads");
    let body = response.json_value();
    let frontends = body.as_array().expect("the answer is an array");
    assert_eq!(frontends.len(), 1, "exactly the configured entries");
    assert_eq!(
        frontends[0],
        json!({
            "name": "pleroma-fe",
            "git": "https://example.com/pleroma-fe",
            "build_url": "https://build.invalid/fe.zip",
            "ref": "main",
            "installed": true,
        })
    );

    // Removing the ref directory flips the flag back.
    std::fs::remove_dir_all(dir.path().join("pleroma-fe/main")).unwrap();
    let relisted = common::TestRequest::get(&format!("{ADMIN}/frontends"))
        .bearer(&token)
        .send(&app)
        .await;
    assert_eq!(relisted.json_value()[0]["installed"], json!(false));
}

#[tokio::test]
async fn frontend_installs_download_extract_and_land_a_build() {
    let dir = TempDir::new().unwrap();
    let (server, asked) = spawn_archive_server(vec![
        (
            "fe-main.zip",
            zip_bytes(&[
                ("dist/index.html", b"<!doctype html>fe".as_slice()),
                ("dist/assets/app.js", b"console.log(1)".as_slice()),
            ]),
        ),
        (
            "fe-dev.zip",
            zip_bytes(&[("dist/index.html", b"dev build".as_slice())]),
        ),
    ])
    .await;
    let db = common::test_db().await;
    let pool = db.pool().clone();
    let app = common::test_app_with_config(
        pool.clone(),
        frontend_config(dir.path(), &format!("{server}/fe-${{ref}}.zip")),
    );
    let token = admin_token(&pool, "fe-installer", "admin:read admin:write").await;

    let install = |body: Value| {
        let app = app.clone();
        let token = token.clone();
        async move {
            common::TestRequest::post(&format!("{ADMIN}/frontends/install"))
                .bearer(&token)
                .json(&body)
                .send(&app)
                .await
        }
    };

    let main_answer = install(json!({ "name": "pleroma-fe" })).await;
    common::assert_answer(&main_answer, StatusCode::OK, "the config ref installs");
    assert_eq!(main_answer.json_value(), json!({ "status": "ok" }));
    assert_eq!(
        std::fs::read(dir.path().join("pleroma-fe/main/index.html")).unwrap(),
        b"<!doctype html>fe",
        "the dist subtree lands at the destination, not inside it"
    );
    assert_eq!(
        std::fs::read(dir.path().join("pleroma-fe/main/assets/app.js")).unwrap(),
        b"console.log(1)"
    );
    assert!(!dir.path().join("pleroma-fe/main/dist").exists());

    // `ref` from the request re-selects the archive through ${ref}.
    let dev_answer = install(json!({ "name": "pleroma-fe", "ref": "dev" })).await;
    common::assert_answer(&dev_answer, StatusCode::OK, "the request ref installs");
    assert_eq!(dev_answer.json_value(), json!({ "status": "ok" }));
    assert_eq!(
        std::fs::read(dir.path().join("pleroma-fe/dev/index.html")).unwrap(),
        b"dev build"
    );
    assert_eq!(
        *asked.lock().unwrap(),
        ["GET /fe-main.zip".to_owned(), "GET /fe-dev.zip".to_owned()],
        "each install fetched its own ref's archive"
    );

    // The listing reports the frontend installed.
    let listed = common::TestRequest::get(&format!("{ADMIN}/frontends"))
        .bearer(&token)
        .send(&app)
        .await;
    assert_eq!(listed.json_value()[0]["installed"], json!(true));
}

/// Reinstalling replaces the target, an unknown name with a `build_url`
/// installs off-catalogue, and every source-less or name-less request is
/// the ticket's refusal.
#[tokio::test]
async fn frontend_install_validates_names_and_sources() {
    let dir = TempDir::new().unwrap();
    let (server, _asked) = spawn_archive_server(vec![(
        "fe-main.zip",
        zip_bytes(&[("dist/index.html", b"<!doctype html>fe".as_slice())]),
    )])
    .await;
    let db = common::test_db().await;
    let pool = db.pool().clone();
    let app = common::test_app_with_config(
        pool.clone(),
        frontend_config(dir.path(), &format!("{server}/fe-main.zip")),
    );
    let token = admin_token(&pool, "fe-validator", "admin:write").await;

    let install = |body: Value| {
        let app = app.clone();
        let token = token.clone();
        async move {
            common::TestRequest::post(&format!("{ADMIN}/frontends/install"))
                .bearer(&token)
                .json(&body)
                .send(&app)
                .await
        }
    };

    // A first install and a second one over the same target: the files
    // come back intact, not doubled or half-landed.
    let first = install(json!({ "name": "pleroma-fe" })).await;
    common::assert_answer(&first, StatusCode::OK, "the first install lands");
    let again = install(json!({ "name": "pleroma-fe" })).await;
    common::assert_answer(&again, StatusCode::OK, "a reinstall lands too");
    assert_eq!(
        std::fs::read(dir.path().join("pleroma-fe/main/index.html")).unwrap(),
        b"<!doctype html>fe",
        "the target holds exactly the fresh build"
    );

    // An unknown name with a build_url installs anyway; without one it is
    // the ticket's refusal.
    let off_catalogue = install(json!({
        "name": "my-fe",
        "build_url": format!("{server}/fe-main.zip"),
    }))
    .await;
    common::assert_answer(
        &off_catalogue,
        StatusCode::OK,
        "an off-catalogue build installs",
    );
    assert_eq!(
        std::fs::read(dir.path().join("my-fe/master/index.html")).unwrap(),
        b"<!doctype html>fe",
        "the ref defaults to master"
    );

    let ghost = install(json!({ "name": "ghost" })).await;
    common::assert_answer(&ghost, StatusCode::BAD_REQUEST, "an unknown name");
    common::assert_error(
        &ghost,
        r#"Could not install frontend: unknown frontend "ghost""#,
        "an unknown name",
    );

    let no_name = install(json!({})).await;
    common::assert_answer(&no_name, StatusCode::BAD_REQUEST, "a body without a name");
    common::assert_error(&no_name, "Missing parameter: name", "a body without a name");
}

#[tokio::test]
async fn frontend_install_refuses_a_traversing_archive() {
    let dir = TempDir::new().unwrap();
    let archive = zip_bytes(&[
        ("dist/index.html", b"fine".as_slice()),
        ("dist/../../escape.txt", b"escaped".as_slice()),
    ]);
    let (server, _asked) = spawn_archive_server(vec![("evil.zip", archive)]).await;
    let db = common::test_db().await;
    let pool = db.pool().clone();
    let app = common::test_app_with_config(
        pool.clone(),
        frontend_config(dir.path(), &format!("{server}/evil.zip")),
    );
    let token = admin_token(&pool, "fe-slip", "admin:write").await;

    let response = common::TestRequest::post(&format!("{ADMIN}/frontends/install"))
        .bearer(&token)
        .json(&json!({ "name": "pleroma-fe" }))
        .send(&app)
        .await;
    common::assert_answer(&response, StatusCode::BAD_REQUEST, "the zip-slip archive");
    let error = response.json_value()["error"]
        .as_str()
        .expect("an error body")
        .to_owned();
    assert!(
        error.starts_with("Could not install frontend: "),
        "the ticket's wording: {error}"
    );
    assert!(
        error.contains("escape.txt"),
        "the refusal names the traversing entry: {error}"
    );

    assert!(
        !dir.path().join("pleroma-fe").exists(),
        "nothing landed in the frontends directory"
    );
    assert!(
        !std::env::temp_dir().join("escape.txt").exists(),
        "nothing escaped the destination"
    );
    let leftovers: Vec<_> = std::fs::read_dir(dir.path())
        .unwrap()
        .filter_map(Result::ok)
        .filter(|entry| {
            entry
                .file_name()
                .to_string_lossy()
                .starts_with(".tmp-install-")
        })
        .collect();
    assert!(leftovers.is_empty(), "the staging directory went too");
}

#[tokio::test]
async fn frontend_install_accepts_an_uploaded_archive() {
    let dir = TempDir::new().unwrap();
    let db = common::test_db().await;
    let pool = db.pool().clone();
    // The configured build_url is unreachable on purpose: the uploaded
    // file must win over it.
    let app = common::test_app_with_config(
        pool.clone(),
        frontend_config(dir.path(), "http://127.0.0.1:1/unreachable.zip"),
    );
    let token = admin_token(&pool, "fe-uploader", "admin:write").await;

    let archive = zip_bytes(&[("index.html", b"uploaded fe".as_slice())]);
    let response = common::TestRequest::post(&format!("{ADMIN}/frontends/install"))
        .bearer(&token)
        .multipart(
            "----wired-admin-ops-boundary",
            &[
                common::MultipartPart::text("name", "pleroma-fe"),
                common::MultipartPart::file(
                    "file",
                    "fe.zip",
                    "application/zip",
                    Bytes::from(archive),
                ),
            ],
        )
        .send(&app)
        .await;
    common::assert_answer(&response, StatusCode::OK, "the upload installs");
    assert_eq!(response.json_value(), json!({ "status": "ok" }));
    assert_eq!(
        std::fs::read(dir.path().join("pleroma-fe/main/index.html")).unwrap(),
        b"uploaded fe",
        "a flat archive lands directly in the target"
    );
}

#[tokio::test]
async fn an_admin_creates_a_backup_for_any_account() {
    let dir = TempDir::new().unwrap();
    let db = common::test_db().await;
    let pool = db.pool().clone();
    let mut config = ops_config(None);
    config.uploads_dir = dir.path().to_path_buf();
    let state = common::test_app_state(pool.clone(), config);
    let app = wired_be::app(state.clone());

    let admin = admin_token(&pool, "backup-admin", "admin:write").await;
    let member = common::create_local_user(&pool, "backup-member").await;
    let member_token = common::create_app_and_token(&pool, &member, "read:backups").await;

    let triggered = common::TestRequest::post(&format!("{ADMIN}/backups"))
        .bearer(&admin)
        .json(&json!({ "nickname": "backup-member" }))
        .send(&app)
        .await;
    common::assert_answer(&triggered, StatusCode::OK, "the admin trigger lands");
    let created = triggered.json_value();
    let backups = created.as_array().expect("the user endpoint's shape");
    assert_eq!(backups.len(), 1, "the member's fresh backup");
    assert_eq!(backups[0]["processed"], json!(false), "the job has not run");
    assert_eq!(backups[0]["url"], json!(""));

    // A second trigger for the same account rides along: the user
    // endpoint's BACKUP_LIMIT_DAYS gate is the account's own, not the
    // admin's.
    let second = common::TestRequest::post(&format!("{ADMIN}/backups"))
        .bearer(&admin)
        .json(&json!({ "nickname": "backup-member" }))
        .send(&app)
        .await;
    common::assert_answer(&second, StatusCode::OK, "the limit does not gate admins");
    assert_eq!(second.json_value().as_array().expect("a list").len(), 2);

    // Draining the queue builds the archives; the member's own listing
    // shows both, processed and downloadable.
    let ran = common::run_all_jobs(&state).await;
    assert!(ran >= 2, "the backup jobs ran: {ran}");
    let member_list = common::TestRequest::get("/api/v1/pleroma/backups")
        .bearer(&member_token)
        .send(&app)
        .await;
    common::assert_answer(
        &member_list,
        StatusCode::OK,
        "the member reads their backups",
    );
    let listed = member_list.json_value();
    let rows = listed.as_array().expect("the answer is an array");
    assert_eq!(rows.len(), 2);
    for row in rows {
        assert_eq!(row["processed"], json!(true), "the job processed {row}");
        assert!(
            row["url"]
                .as_str()
                .unwrap_or_default()
                .starts_with("https://test.example/media/"),
            "the archive is served: {row}"
        );
        assert!(row["file_size"].as_i64().unwrap_or_default() > 0);
    }
}

/// The trigger's refusals: an unknown nickname is the 404, a body without
/// one the `Missing parameter: nickname`, and a read-scoped admin never
/// reaches either.
#[tokio::test]
async fn the_admin_backup_trigger_checks_nickname_and_scope() {
    let db = common::test_db().await;
    let pool = db.pool().clone();
    let app = common::test_app_with_config(pool.clone(), ops_config(None));
    let admin = admin_token(&pool, "backup-checker", "admin:write").await;
    let reader = admin_token(&pool, "backup-reader", "admin:read").await;

    let unknown_nick = common::TestRequest::post(&format!("{ADMIN}/backups"))
        .bearer(&admin)
        .json(&json!({ "nickname": "nobody" }))
        .send(&app)
        .await;
    common::assert_answer(&unknown_nick, StatusCode::NOT_FOUND, "an unknown nickname");
    common::assert_error(&unknown_nick, "Record not found", "an unknown nickname");

    let no_nick = common::TestRequest::post(&format!("{ADMIN}/backups"))
        .bearer(&admin)
        .json(&json!({}))
        .send(&app)
        .await;
    common::assert_answer(
        &no_nick,
        StatusCode::BAD_REQUEST,
        "a body without a nickname",
    );
    common::assert_error(
        &no_nick,
        "Missing parameter: nickname",
        "a body without a nickname",
    );

    // The trigger asks for `admin:write`; a read-scoped admin is refused.
    let reader_answer = common::TestRequest::post(&format!("{ADMIN}/backups"))
        .bearer(&reader)
        .json(&json!({ "nickname": "backup-reader" }))
        .send(&app)
        .await;
    common::assert_answer(&reader_answer, StatusCode::FORBIDDEN, "a read-scoped admin");
}
