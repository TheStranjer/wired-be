//! The `rich_media.fetch` job: the background card behind a status's first
//! link.
//!
//! `docs/ARCHITECTURE.md` decision 11 keeps background work in Postgres rows;
//! this module is the consumer side of the card fetch the create hook queues.
//! The producer is [`after_status_created`](crate::services::create_status):
//! every stored status schedules one [`KIND_FETCH`] job on
//! [`QUEUE_BACKGROUND`] under the dedupe key `rich_media:<status_id>` — the
//! shape of Pleroma's card pipeline, whose `Card.get_by_activity` takes the
//! activity's first external URL and, on a cache miss, inserts a
//! `Pleroma.Workers.RichMediaWorker` backfill job on the `background` queue
//! (`lib/pleroma/workers/rich_media_worker.ex`) to parse the page
//! off-request. The gate and the cache policy here are Pleroma's too:
//! `RICH_MEDIA_REMOTE` decides whether mirrored posts get cards at all, and
//! `link_cards` is what keeps a URL from being fetched on every post that
//! links it.
//!
//! The handler runs the pieces the [rich media
//! service](crate::services::rich_media) already provides: [`first_link`]
//! picks the one URL the status card is made from, [`fetch_card`] and
//! [`card_json`] turn the page into the cached JSON, and the
//! [`link_card`] rows the cache policy here reads
//! and writes are that model's. A failed fetch is cached as `failed`, never
//! retried by the queue: a dead URL is remembered for an hour, not re-probed
//! on every attempt.

use chrono::{DateTime, TimeDelta, Utc};
use scraper::{ElementRef, Html, Selector};
use serde::Deserialize;
use serde_json::{Value, json};
use tracing::{debug, warn};
use url::Url;
use uuid::Uuid;

use crate::{
    config::Config,
    error::AppError,
    jobs::{AttemptInfo, HandlerFuture, JobError, QUEUE_BACKGROUND},
    models::{
        job::{NewJob, enqueue},
        link_card::{self, LinkCard},
        status::{self, Status, Visibility},
    },
    services::rich_media::{card_json, fetch_card, host_ignored},
    state::AppState,
};

/// The job kind of a status's link-card fetch, handled by [`handle_fetch`]
/// on [`QUEUE_BACKGROUND`].
pub const KIND_FETCH: &str = "rich_media.fetch";

/// How long a cached `ok` card answers without a refetch: seven days, the
/// window after which a page's preview is stale enough to be worth another
/// look (Pleroma expires a card when the page's own cache age runs out;
/// here the window is one fixed span for every card).
const OK_TTL: TimeDelta = TimeDelta::days(7);

/// How long a cached `failed` row suppresses refetching: one hour. Dead
/// URLs stay remembered briefly instead of being re-probed on every post,
/// but a page that came back is not frozen out for a week.
const FAILED_TTL: TimeDelta = TimeDelta::hours(1);

/// The [`link_cards.status`](LinkCard::status) text of a successful fetch.
const STATUS_OK: &str = "ok";

/// The [`link_cards.status`](LinkCard::status) text of a failed fetch.
const STATUS_FAILED: &str = "failed";

/// The `{status_id}` arguments of the fetch job, as [`job_args`] writes and
/// [`handle_fetch`] reads them.
#[derive(Debug, Deserialize)]
struct FetchArgs {
    /// The status whose first link gets the card.
    status_id: Uuid,
}

/// The `{status_id}` job-arguments document for status `status_id`: the one
/// shape [`schedule_fetch`] enqueues and [`handle_fetch`] deserialises.
#[must_use]
pub fn job_args(status_id: Uuid) -> Value {
    json!({ "status_id": status_id })
}

/// The dedupe key of status `status_id`'s fetch job.
///
/// At most one queued card fetch per status, however many times its create
/// is replayed ([`enqueue`]'s `None` swallows the second claim).
#[must_use]
pub fn unique_key(status_id: Uuid) -> String {
    format!("rich_media:{status_id}")
}

/// Queues `row`'s link-card fetch: one [`KIND_FETCH`] job due now on
/// [`QUEUE_BACKGROUND`], keyed [`unique_key`] so a replayed create cannot
/// queue the same card twice.
///
/// A failed enqueue is logged, not returned: the status is stored and the
/// card is a rendering nicety, not something the client's post depends on
/// (the same rule as
/// [`schedule_expiration`](crate::jobs::statuses::schedule_expiration)).
pub async fn schedule_fetch(state: &AppState, row: &Status) {
    let key = unique_key(row.id);
    let request = NewJob {
        queue: QUEUE_BACKGROUND,
        kind: KIND_FETCH,
        args: job_args(row.id),
        run_at: None,
        max_attempts: None,
        unique_key: Some(&key),
    };
    match enqueue(&state.db, request).await {
        Ok(Some(job)) => debug!(
            job = %job.id,
            status = %row.id,
            "rich media card fetch scheduled"
        ),
        Ok(None) => debug!(status = %row.id, "rich media card fetch already scheduled"),
        Err(err) => warn!(
            %err,
            status = %row.id,
            "could not schedule rich media card fetch"
        ),
    }
}

/// The first ordinary link in sanitized status HTML, the URL its card is
/// made from.
///
/// Anchors whose class list contains `mention` or `hashtag` are links to
/// accounts and tags, not pages, and are skipped; so is every `href` that
/// is not an absolute `http(s)` URL (relative references, `mailto:` and the
/// like). Of what is left the first in document order wins: a status shows
/// one card, the one describing the first page it links.
#[must_use]
pub fn first_link(content_html: &str) -> Option<String> {
    let selector = Selector::parse("a[href]").ok()?;
    Html::parse_document(content_html)
        .select(&selector)
        .filter(|anchor| !social_anchor(anchor))
        .find_map(|anchor| absolute_http(anchor.attr("href")))
}

/// Whether an anchor is a mention or hashtag link rather than a page link:
/// its `class` token list contains `mention` or `hashtag` (mixed class
/// lists included — Pleroma writes `mention hashtag` and `mention u-url`).
fn social_anchor(anchor: &ElementRef<'_>) -> bool {
    anchor
        .attr("class")
        .is_some_and(|classes| classes.split_whitespace().any(is_social_class))
}

/// Whether one `class` token marks an anchor as a mention or hashtag link.
fn is_social_class(token: &str) -> bool {
    matches!(token, "mention" | "hashtag")
}

/// The `href` text verbatim when it is an absolute `http(s)` URL; `None`
/// for a relative reference, another scheme, or unparsable text. The cache
/// key and the fetched URL stay exactly what the post links.
fn absolute_http(href: Option<&str>) -> Option<String> {
    let raw = href?;
    Url::parse(raw)
        .is_ok_and(|parsed| matches!(parsed.scheme(), "http" | "https"))
        .then(|| raw.to_owned())
}

/// Whether `cached` is fresh enough to answer without a fetch: an `ok` row
/// within [`OK_TTL`] of `now`, a `failed` row within [`FAILED_TTL`]. Any
/// other state (the column's check constraint cannot hold one, but a row
/// nobody has audited might) is stale and gets refetched. `now` is a
/// parameter so the TTL arithmetic is testable without sleeping.
fn is_fresh(cached: &LinkCard, now: DateTime<Utc>) -> bool {
    let ttl = match cached.status.as_str() {
        STATUS_OK => OK_TTL,
        STATUS_FAILED => FAILED_TTL,
        _ => return false,
    };
    cached.fetched_at > now - ttl
}

/// Whether the row passes the card gate: only a public or unlisted post
/// gets a card, and a post this instance does not own only while
/// [`rich_media_remote`](Config::rich_media_remote) is on.
const fn card_wanted(row: &Status, config: &Config) -> bool {
    let shared = matches!(row.visibility, Visibility::Public | Visibility::Unlisted);
    shared && (row.local || config.rich_media_remote.get())
}

/// The URL `row`'s card is fetched from, `None` when nothing about the row
/// asks for a fetch: the gate refuses it, its content holds no eligible
/// `http(s)` link, or the link's host is on the ignore list.
fn card_url(row: &Status, config: &Config) -> Option<String> {
    if !card_wanted(row, config) {
        return None;
    }
    let url = first_link(&row.content)?;
    (!host_ignored(config, &url)).then_some(url)
}

/// Reads [`FetchArgs`] out of a claimed job's payload; arguments this shape
/// cannot deserialize are permanent — the job is discarded, never retried.
fn parse_args(args: &Value) -> Result<Uuid, JobError> {
    serde_json::from_value::<FetchArgs>(args.clone())
        .map(|parsed| parsed.status_id)
        .map_err(|err| {
            JobError::Discard(format!(
                "rich media fetch job arguments are unusable: {err}"
            ))
        })
}

/// Lifts a database failure into the queue's retry arm; every write the
/// handler makes is idempotent (the store rewrites its own row, the link
/// keeps what stands), so another pass costs nothing but the wait.
fn retry(err: &AppError) -> JobError {
    JobError::Retry(format!("rich media card fetch pass failed: {err}"))
}

/// The [`JobRegistry`](super::JobRegistry) handler for [`KIND_FETCH`]:
/// fetch, cache and link the card of the status the arguments name.
///
/// A row the gate refuses, or one with no fetchable link, completes the job
/// silently — there is nothing to cache and a retry can not change that, as
/// there is nothing to cache for a fetch that ends in an error: the failure
/// itself is the cached answer. A status the database does not answer is
/// [`JobError::Discard`]: the id names nothing this job can ever fetch.
pub fn handle_fetch(state: AppState, _attempt: AttemptInfo, args: Value) -> HandlerFuture {
    Box::pin(async move {
        let status_id = parse_args(&args)?;
        fetch_one(&state, status_id).await
    })
}

/// The fetch job's body: gate the row, pick its link, answer from the cache
/// or a fresh fetch, and write both cache rows.
///
/// # Errors
///
/// [`JobError::Discard`] when the arguments name a status that is not
/// stored; [`JobError::Retry`] on a database failure. Fetch errors are
/// cached, not returned.
async fn fetch_one(state: &AppState, status_id: Uuid) -> Result<(), JobError> {
    let row = status::get_by_id(&state.db, status_id)
        .await
        .map_err(|err| retry(&err))?;
    let Some(row) = row else {
        return Err(JobError::Discard(format!(
            "rich media fetch names status {status_id}, which is not stored"
        )));
    };
    let Some(url) = card_url(&row, &state.config) else {
        return Ok(());
    };
    let cached = link_card::get(&state.db, &url)
        .await
        .map_err(|err| retry(&err))?;
    let (outcome, card) = match cached.filter(|entry| is_fresh(entry, Utc::now())) {
        Some(entry) => (entry.status, entry.card),
        None => fetch_and_classify(state, &url).await,
    };
    store_and_link(state, status_id, &url, &outcome, card.as_ref()).await
}

/// Runs the one network fetch and classifies its outcome into the pair
/// [`link_card::store`] takes: `ok` with the rendered card, or `failed`
/// with nothing. A fetch error is never a queue retry — the page is
/// refused, gone, blocked or titleless, and the same call would answer the
/// same way.
async fn fetch_and_classify(state: &AppState, url: &str) -> (String, Option<Value>) {
    match fetch_card(&state.http_client, url).await {
        Ok(card) => (STATUS_OK.to_owned(), Some(card_json(&card))),
        Err(err) => {
            debug!(%err, url, "rich media fetch failed; caching the failure");
            (STATUS_FAILED.to_owned(), None)
        }
    }
}

/// Writes the outcome to the cache and points the status at it. Every
/// branch of the cache policy ends here, so a status links its card whether
/// the row behind it was just fetched, refreshed from a hit, or remembers a
/// failure.
///
/// # Errors
///
/// [`JobError::Retry`] on a database failure.
async fn store_and_link(
    state: &AppState,
    status_id: Uuid,
    url: &str,
    outcome: &str,
    card: Option<&Value>,
) -> Result<(), JobError> {
    link_card::store(&state.db, url, outcome, card)
        .await
        .map_err(|err| retry(&err))?;
    link_card::link_status(&state.db, status_id, url)
        .await
        .map_err(|err| retry(&err))?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use std::sync::{
        Arc,
        atomic::{AtomicUsize, Ordering},
    };

    use axum::{
        Router,
        extract::State,
        http::{StatusCode, header},
        response::IntoResponse,
        routing::get,
    };
    use sqlx::PgPool;
    use tokio::net::TcpListener;

    use super::*;
    use crate::{
        config::{Config, TEST_BASE_URL, Toggle},
        http_client::HttpClient,
        models::{
            job::Job,
            status::NewStatus,
            user::{NewUser, User, insert, local_ap_id},
        },
        streaming::hub::Hub,
        uploads::UploaderBackend,
        util::format::text_to_html,
    };

    /// `User-Agent` of the test fetch clients.
    const TEST_UA: &str = "Wired (https://test.example)";

    /// The minimal Open Graph page the mock card servers serve.
    const MOCK_PAGE: &str = r#"<html><head>
        <meta property="og:title" content="Mock Title">
    </head><body></body></html>"#;

    /// The handler's attempt info: the first of however many the job gets.
    const FIRST_ATTEMPT: AttemptInfo = AttemptInfo {
        attempts: 1,
        max_attempts: 5,
    };

    /// Test state over `pool` with `config` and the loopback-tolerant
    /// client the mock servers answer.
    fn state_with(pool: &PgPool, config: Config) -> AppState {
        let config = Arc::new(config);
        AppState {
            db: pool.clone(),
            hub: Hub::new(pool),
            config: Arc::clone(&config),
            http_client: HttpClient::new_for_tests(TEST_UA),
            uploader: UploaderBackend::from(config.as_ref()),
            instance_stats_cache: Arc::default(),
            mrf_config_cache: Arc::default(),
            translation_languages_cache: Arc::default(),
            rate_limiter: Arc::new(crate::rate_limit::RateLimiter::from_config(config.as_ref())),
            mailer: Arc::new(crate::mailer::MemoryTransport::new()),
        }
    }

    /// Test state over `pool` with the fixed test config: rich media
    /// fetching for remote posts off, nothing on the ignore list.
    fn app_state(pool: &PgPool) -> AppState {
        state_with(pool, Config::for_tests())
    }

    /// Test state over `pool` with `rich_media_remote` switched on.
    fn remote_enabled_state(pool: &PgPool) -> AppState {
        let mut config = Config::for_tests();
        config.rich_media_remote = Toggle::new(true);
        state_with(pool, config)
    }

    /// Inserts a local user named `nickname`.
    async fn user(pool: &PgPool, nickname: &str) -> User {
        insert(
            pool,
            &NewUser::local(nickname, &local_ap_id(TEST_BASE_URL, nickname)),
        )
        .await
        .expect("the local fixture inserts")
    }

    /// A remote mirror user named `nickname` on `remote.example`.
    async fn remote_user(pool: &PgPool, nickname: &str) -> User {
        insert(
            pool,
            &NewUser {
                nickname: format!("{nickname}@remote.example"),
                ap_id: format!("https://remote.example/users/{nickname}"),
                domain: Some("remote.example".to_owned()),
                ..NewUser::default()
            },
        )
        .await
        .expect("the remote fixture inserts")
    }

    /// A local post by `author` at `visibility`, linking `url`.
    async fn linking(pool: &PgPool, author: &User, visibility: Visibility, url: &str) -> Status {
        let new = NewStatus {
            visibility,
            ..NewStatus::local(author, &format!("look: {url}"))
        };
        status::insert(pool, TEST_BASE_URL, &new)
            .await
            .expect("the status fixture inserts")
    }

    /// A mirrored (not `local`) public post by `author`, linking `url`.
    async fn mirrored_linking(pool: &PgPool, author: &User, url: &str) -> Status {
        let raw = format!("look: {url}");
        let new = NewStatus {
            uri: Some(format!(
                "https://remote.example/activities/{}",
                Uuid::now_v7()
            )),
            user_id: author.id,
            content: text_to_html(&raw),
            raw_text: raw,
            ..NewStatus::default()
        };
        status::insert(pool, TEST_BASE_URL, &new)
            .await
            .expect("the mirrored fixture inserts")
    }

    /// The cached `link_cards` row for `url`, if the cache holds one.
    async fn cached(pool: &PgPool, url: &str) -> Option<LinkCard> {
        link_card::get(pool, url).await.expect("the cache reads")
    }

    /// The URL `status_id` links in `status_cards`, if it links one.
    async fn linked_url(pool: &PgPool, status_id: Uuid) -> Option<String> {
        sqlx::query_scalar::<_, String>("SELECT url FROM status_cards WHERE status_id = $1")
            .bind(status_id)
            .fetch_optional(pool)
            .await
            .expect("the link table reads")
    }

    /// Total rows in the card cache (the nothing-was-stored assertion).
    async fn cache_count(pool: &PgPool) -> i64 {
        sqlx::query_scalar::<_, i64>("SELECT count(*) FROM link_cards")
            .fetch_one(pool)
            .await
            .expect("the cache counts")
    }

    /// Starts a mock server routing `path` to `app` and returns the page
    /// URL. The serving task is detached: it lives until the test's runtime
    /// ends with it.
    async fn mock_server(path: &str, app: Router) -> String {
        let listener = TcpListener::bind("127.0.0.1:0")
            .await
            .expect("free loopback port");
        let url = format!(
            "http://{}{path}",
            listener.local_addr().expect("bound address")
        );
        let _serving = tokio::spawn(async move {
            if let Err(err) = axum::serve(listener, app).await {
                tracing::debug!("mock card server stopped: {err}");
            }
        });
        url
    }

    /// Serves [`MOCK_PAGE`] as HTML, counting every hit.
    async fn counted_page(State(hits): State<Arc<AtomicUsize>>) -> impl IntoResponse {
        let _before = hits.fetch_add(1, Ordering::SeqCst);
        (
            [(header::CONTENT_TYPE, "text/html; charset=utf-8")],
            MOCK_PAGE,
        )
    }

    /// A mock HTML page and its hit counter, as the URL a status can link.
    async fn mock_page() -> (String, Arc<AtomicUsize>) {
        let hits = Arc::new(AtomicUsize::new(0));
        let app = Router::new()
            .route("/page", get(counted_page))
            .with_state(Arc::clone(&hits));
        let url = mock_server("/page", app).await;
        (url, hits)
    }

    /// A mock server whose page answers `404 Not Found`.
    async fn mock_missing_page() -> String {
        let app = Router::new().route("/gone", get(|| async { StatusCode::NOT_FOUND }));
        mock_server("/gone", app).await
    }

    /// A `link_cards` row of `status` fetched `age` ago (for [`is_fresh`]).
    fn cached_row(status: &str, age: TimeDelta) -> LinkCard {
        LinkCard {
            url: "https://example.test".to_owned(),
            status: status.to_owned(),
            card: None,
            fetched_at: Utc::now() - age,
        }
    }

    #[test]
    fn first_link_returns_the_first_plain_http_link_verbatim() {
        let html = r#"<p>see <a href="http://first.test/a" rel="ugc">first</a>
            and <a href="https://second.test/b">second</a></p>"#;
        assert_eq!(
            first_link(html).as_deref(),
            Some("http://first.test/a"),
            "the first anchor in document order wins"
        );
    }

    #[test]
    fn first_link_skips_mention_and_hashtag_anchors() {
        let html = r#"<p><a class="mention u-url" href="https://remote.test/@bob">@bob</a>
            <a class="hashtag" href="https://test.example/tags/rust">#rust</a>
            <a class="mention hashtag" href="https://test.example/tags/go">#go</a>
            <a href="https://real.test/page">real</a></p>"#;
        assert_eq!(
            first_link(html).as_deref(),
            Some("https://real.test/page"),
            "mixed class lists are skipped too"
        );
        let only_social =
            r#"<p><a class="u-url mention" href="https://remote.test/@bob">@bob</a></p>"#;
        assert_eq!(
            first_link(only_social),
            None,
            "with only social anchors nothing is left"
        );
    }

    #[test]
    fn first_link_skips_hrefs_that_are_not_absolute_http_urls() {
        let html = r##"<p><a href="/relative">r</a>
            <a href="mailto:someone@example.test">m</a>
            <a href="#fragment">f</a>
            <a href="ftp://files.test/pub">ftp</a></p>"##;
        assert_eq!(first_link(html), None, "none of these is a fetchable page");
        let with_real = r#"<a href="/relative">r</a> <a href="https://late.test/x">late</a>"#;
        assert_eq!(
            first_link(with_real).as_deref(),
            Some("https://late.test/x"),
            "the first real link after the skipped ones"
        );
    }

    #[test]
    fn first_link_of_html_without_links_is_none() {
        assert_eq!(first_link("<p>plain text, no links</p>"), None);
        assert_eq!(first_link(""), None, "empty content");
        assert_eq!(first_link("<a>an anchor without href</a>"), None);
    }

    #[test]
    fn is_fresh_holds_seven_days_for_ok_and_one_hour_for_failed() {
        let now = Utc::now();
        assert!(
            is_fresh(&cached_row(STATUS_OK, TimeDelta::days(6)), now),
            "an ok card six days old is still fresh"
        );
        assert!(
            !is_fresh(&cached_row(STATUS_OK, TimeDelta::days(8)), now),
            "eight days old is past the week"
        );
        assert!(
            is_fresh(&cached_row(STATUS_FAILED, TimeDelta::minutes(30)), now),
            "a failure half an hour old still suppresses the retry"
        );
        assert!(
            !is_fresh(&cached_row(STATUS_FAILED, TimeDelta::hours(2)), now),
            "two hours old the URL may be tried again"
        );
        assert!(
            !is_fresh(&cached_row("pending", TimeDelta::seconds(1)), now),
            "a state outside the two is never fresh"
        );
    }

    #[tokio::test]
    async fn a_public_local_link_fetches_once_and_a_second_run_answers_from_cache() {
        let db = crate::test_db::test_db().await.expect("test database slot");
        let pool = db.pool().clone();
        let alice = user(&pool, "alice").await;
        let (url, hits) = mock_page().await;
        let row = linking(&pool, &alice, Visibility::Public, &url).await;
        let state = app_state(&pool);

        let first = handle_fetch(state.clone(), FIRST_ATTEMPT, job_args(row.id)).await;
        assert_eq!(first, Ok(()), "the fetching pass completes");

        let entry = cached(&pool, &url).await.expect("the card is cached");
        assert_eq!(entry.status, STATUS_OK, "a served page caches ok");
        let card = entry.card.expect("an ok row carries its card");
        assert_eq!(card["title"], "Mock Title", "the parsed card JSON");
        assert_eq!(
            linked_url(&pool, row.id).await.as_deref(),
            Some(url.as_str()),
            "the status links its card"
        );
        assert_eq!(hits.load(Ordering::SeqCst), 1, "the mock was fetched once");

        let second = handle_fetch(state, FIRST_ATTEMPT, job_args(row.id)).await;
        assert_eq!(second, Ok(()), "the cached pass completes too");
        assert_eq!(
            hits.load(Ordering::SeqCst),
            1,
            "the fresh ok row is a hit: the mock is never asked again"
        );
    }

    #[tokio::test]
    async fn an_unlisted_post_gets_its_card_too() {
        let db = crate::test_db::test_db().await.expect("test database slot");
        let pool = db.pool().clone();
        let alice = user(&pool, "alice").await;
        let (url, _hits) = mock_page().await;
        let row = linking(&pool, &alice, Visibility::Unlisted, &url).await;

        let outcome = handle_fetch(app_state(&pool), FIRST_ATTEMPT, job_args(row.id)).await;

        assert_eq!(outcome, Ok(()), "unlisted passes the gate");
        assert_eq!(
            cached(&pool, &url).await.map(|entry| entry.status),
            Some(STATUS_OK.to_owned()),
            "and its card is cached"
        );
    }

    #[tokio::test]
    async fn a_pre_seeded_fresh_ok_row_means_the_fetch_is_never_attempted() {
        let db = crate::test_db::test_db().await.expect("test database slot");
        let pool = db.pool().clone();
        let alice = user(&pool, "alice").await;
        let (url, hits) = mock_page().await;
        let row = linking(&pool, &alice, Visibility::Public, &url).await;
        link_card::store(&pool, &url, STATUS_OK, Some(&json!({"title": "Seeded"})))
            .await
            .expect("the fixture seeds the cache");

        let outcome = handle_fetch(app_state(&pool), FIRST_ATTEMPT, job_args(row.id)).await;

        assert_eq!(outcome, Ok(()), "a cache hit is a completed job");
        assert_eq!(
            hits.load(Ordering::SeqCst),
            0,
            "the fresh seeded row was never fetched"
        );
        assert_eq!(
            cached(&pool, &url)
                .await
                .and_then(|entry| entry.card)
                .expect("the seeded card"),
            json!({"title": "Seeded"}),
            "and the hit's card is the answer the status shows"
        );
        assert_eq!(
            linked_url(&pool, row.id).await.as_deref(),
            Some(url.as_str()),
            "the hit still links the status"
        );
    }

    #[tokio::test]
    async fn posts_below_the_gate_store_nothing() {
        let db = crate::test_db::test_db().await.expect("test database slot");
        let pool = db.pool().clone();
        let alice = user(&pool, "alice").await;
        let (url, hits) = mock_page().await;
        let state = app_state(&pool);

        for visibility in [Visibility::Private, Visibility::Direct, Visibility::Local] {
            let row = linking(&pool, &alice, visibility, &url).await;
            let outcome = handle_fetch(state.clone(), FIRST_ATTEMPT, job_args(row.id)).await;
            assert_eq!(outcome, Ok(()), "a refused visibility completes silently");
        }

        assert_eq!(cache_count(&pool).await, 0, "nothing was cached");
        assert_eq!(hits.load(Ordering::SeqCst), 0, "nothing was fetched");
    }

    #[tokio::test]
    async fn a_remote_post_waits_for_rich_media_remote() {
        let db = crate::test_db::test_db().await.expect("test database slot");
        let pool = db.pool().clone();
        let bob = remote_user(&pool, "bob").await;
        let (url, hits) = mock_page().await;
        let row = mirrored_linking(&pool, &bob, &url).await;

        let gated = handle_fetch(app_state(&pool), FIRST_ATTEMPT, job_args(row.id)).await;
        assert_eq!(gated, Ok(()), "the closed gate completes silently");
        assert_eq!(cache_count(&pool).await, 0, "and stores nothing");
        assert_eq!(hits.load(Ordering::SeqCst), 0, "without fetching");

        let opened =
            handle_fetch(remote_enabled_state(&pool), FIRST_ATTEMPT, job_args(row.id)).await;
        assert_eq!(opened, Ok(()), "with the flag set the same job runs");
        assert_eq!(
            cached(&pool, &url).await.map(|entry| entry.status),
            Some(STATUS_OK.to_owned()),
            "and the remote post's card is stored"
        );
        assert_eq!(
            linked_url(&pool, row.id).await.as_deref(),
            Some(url.as_str()),
            "with its status link"
        );
    }

    #[tokio::test]
    async fn a_post_without_a_fetchable_link_stores_nothing() {
        let db = crate::test_db::test_db().await.expect("test database slot");
        let pool = db.pool().clone();
        let alice = user(&pool, "alice").await;
        let new = NewStatus::local(&alice, "plain words, no links at all");
        let row = status::insert(&pool, TEST_BASE_URL, &new)
            .await
            .expect("the status fixture inserts");

        let outcome = handle_fetch(app_state(&pool), FIRST_ATTEMPT, job_args(row.id)).await;

        assert_eq!(
            outcome,
            Ok(()),
            "there is nothing to fetch and nothing to retry"
        );
        assert_eq!(cache_count(&pool).await, 0, "and nothing is stored");
    }

    #[tokio::test]
    async fn a_link_on_an_ignored_host_stores_nothing() {
        let db = crate::test_db::test_db().await.expect("test database slot");
        let pool = db.pool().clone();
        let alice = user(&pool, "alice").await;
        let (url, hits) = mock_page().await;
        let row = linking(&pool, &alice, Visibility::Public, &url).await;
        let mut config = Config::for_tests();
        config.rich_media_ignore_hosts = vec!["127.0.0.1".to_owned()];

        let outcome =
            handle_fetch(state_with(&pool, config), FIRST_ATTEMPT, job_args(row.id)).await;

        assert_eq!(outcome, Ok(()), "an ignored host completes silently");
        assert_eq!(cache_count(&pool).await, 0, "and stores nothing");
        assert_eq!(hits.load(Ordering::SeqCst), 0, "without fetching");
    }

    #[tokio::test]
    async fn a_failed_fetch_stores_a_failed_row_and_is_never_retried() {
        let db = crate::test_db::test_db().await.expect("test database slot");
        let pool = db.pool().clone();
        let alice = user(&pool, "alice").await;
        let url = mock_missing_page().await;
        let row = linking(&pool, &alice, Visibility::Public, &url).await;

        let outcome = handle_fetch(app_state(&pool), FIRST_ATTEMPT, job_args(row.id)).await;

        assert_eq!(outcome, Ok(()), "a dead URL is cached, not a queue retry");
        let entry = cached(&pool, &url)
            .await
            .expect("the failure is remembered");
        assert_eq!(entry.status, STATUS_FAILED, "as a failed row");
        assert_eq!(entry.card, None, "with no card behind it");
        assert_eq!(
            linked_url(&pool, row.id).await.as_deref(),
            Some(url.as_str()),
            "and the status still links it"
        );
    }

    #[tokio::test]
    async fn a_missing_status_discards_the_job() {
        let db = crate::test_db::test_db().await.expect("test database slot");
        let pool = db.pool().clone();

        let outcome = handle_fetch(app_state(&pool), FIRST_ATTEMPT, job_args(Uuid::now_v7())).await;

        match outcome {
            Err(JobError::Discard(detail)) => assert!(
                detail.contains("which is not stored"),
                "the discard states why: {detail}"
            ),
            Ok(()) => panic!("an id that names nothing must not complete the job"),
            Err(other) => panic!("an unschedulable row is permanent, not {other:?}"),
        }
    }

    #[tokio::test]
    async fn unusable_arguments_discard_the_job() {
        let db = crate::test_db::test_db().await.expect("test database slot");
        let pool = db.pool().clone();

        let outcome = handle_fetch(app_state(&pool), FIRST_ATTEMPT, json!({})).await;

        match outcome {
            Err(JobError::Discard(detail)) => assert!(
                detail.starts_with("rich media fetch job arguments are unusable"),
                "the discard states why: {detail}"
            ),
            Ok(()) => panic!("bad arguments must not complete the job"),
            Err(other) => panic!("bad arguments are permanent, not {other:?}"),
        }
    }

    #[tokio::test]
    async fn an_unreachable_database_retries_the_pass() {
        let unusable = sqlx::postgres::PgPoolOptions::new()
            .connect_lazy("postgres://localhost/unused")
            .expect("a lazy pool needs no database");
        unusable.close().await;

        let outcome = handle_fetch(
            app_state(&unusable),
            FIRST_ATTEMPT,
            job_args(Uuid::now_v7()),
        )
        .await;

        match outcome {
            Err(JobError::Retry(detail)) => assert!(
                detail.starts_with("rich media card fetch pass failed"),
                "the retry states what failed: {detail}"
            ),
            other => panic!("a closed pool is retryable, not {other:?}"),
        }
    }

    #[tokio::test]
    async fn scheduling_queues_one_deduped_job_on_the_background_queue() {
        let db = crate::test_db::test_db().await.expect("test database slot");
        let pool = db.pool().clone();
        let alice = user(&pool, "alice").await;
        let row = linking(&pool, &alice, Visibility::Public, "https://example.test").await;
        let state = app_state(&pool);

        schedule_fetch(&state, &row).await;
        // The dedupe key swallows the second claim.
        schedule_fetch(&state, &row).await;

        let jobs = sqlx::query_as::<_, Job>("SELECT * FROM jobs WHERE kind = $1 ORDER BY id")
            .bind(KIND_FETCH)
            .fetch_all(&pool)
            .await
            .expect("the job rows read");
        assert_eq!(jobs.len(), 1, "one job per status");
        assert_eq!(jobs[0].queue, QUEUE_BACKGROUND, "on the background queue");
        assert_eq!(jobs[0].args, job_args(row.id), "naming the status id");
        assert_eq!(
            jobs[0].unique_key.as_deref(),
            Some(unique_key(row.id).as_str()),
            "the ticket's dedupe key"
        );
    }

    /// The kind is wired into the registry the worker runs.
    #[test]
    fn the_kind_is_registered() {
        let registry = crate::job_registry();
        assert!(
            registry.get(KIND_FETCH).is_some(),
            "the worker knows `{KIND_FETCH}`"
        );
    }
}
