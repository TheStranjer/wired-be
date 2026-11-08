//! Status translation through the configured provider (`docs/ARCHITECTURE.md`
//! decision 3: a subsystem that is neither one table's rows nor HTTP
//! handling).
//!
//! Pleroma translates a status through whatever
//! `Pleroma.Language.Translation` provider is configured
//! (`lib/pleroma/language/translation.ex`), with
//! `Pleroma.Language.Translation.Libretranslate` and
//! `Pleroma.Language.Translation.Deepl` as its two remote implementations
//! (`lib/pleroma/language/translation/*.ex`). Here the same three states are
//! the variants of one enum, [`Translator`]: [`None`](Translator::None) (no
//! provider, the default), a `LibreTranslate` server, and the `DeepL` API.
//! There is no `trait` to box: the caller asks the value which of the three
//! it holds, and the compiler checks that every one is answered.
//!
//! # What each variant does
//!
//! [`Translator::translate`] posts the status's HTML `content` to the
//! provider and hands back the translated HTML, and [`Translator::languages`]
//! lists the language pairs the provider offers. Both go through the shared
//! [`HttpClient`], so the instance's outbound rules — timeouts, the redirect
//! cap and the `SSRF` guard — apply to a translation server exactly as they do
//! to a federation peer. A deployment whose provider lives on a private
//! network points `TRANSLATION_API_URL` at it and sets
//! `HTTP_ALLOW_PRIVATE_HOSTS` for it, the way the media proxy tests reach a
//! local upstream.
//!
//! - `LibreTranslate`: `POST {TRANSLATION_API_URL}/translate` with the JSON
//!   body `{q, source, target, format: "html", api_key}`, `source` falling
//!   back to `"auto"` when the status names no language of its own, and
//!   `GET {TRANSLATION_API_URL}/languages` for the list. The answer carries
//!   `translatedText` and, on servers new enough to report it,
//!   `detectedSourceLanguage`.
//! - `DeepL`: `POST {TRANSLATION_API_URL}/v2/translate` with the JSON body
//!   `{text: [content], source_lang, target_lang, tag_handling: "html"}` and
//!   the `Authorization: DeepL-Auth-Key <key>` header, and
//!   `POST {TRANSLATION_API_URL}/v2/languages?type=source` (and `=target`) for
//!   the list. The answer carries `translations: [{text,
//!   detected_source_language}]`. Unlike Pleroma, `source_lang` goes only when
//!   the status names a language, which is `DeepL`'s documented way of asking it
//!   to detect one.
//!
//! # Two rules the service owns
//!
//! - **Provider output is untrusted.** What comes back is HTML from some
//!   server on the network, so [`Translator::translate`] runs it through
//!   [`sanitize_html`] before anyone sees it — Pleroma's `scrub_html` step of
//!   `Translation.translate`, living in the one place no caller can skip.
//! - **A language list costs a round trip, so it is cached.**
//!   [`Translator::languages`] does not cache by itself; the instance endpoint
//!   keeps a list that arrived for [`LANGUAGES_TTL`] in
//!   [`TranslationLanguagesCache`], the [`TimedCache`] slot on
//!   [`AppState`](crate::state::AppState). A provider that will not answer its
//!   list is the empty list plus a logged warning, and an empty list is not
//!   cached: that document is a convenience, never a request that has to fail,
//!   and a refusal should not buy an hour of silence.
//!
//! [`HttpClient`]: crate::http_client::HttpClient
//! [`sanitize_html`]: crate::util::sanitize::sanitize_html
//! [`TimedCache`]: crate::util::cache::TimedCache

use std::{collections::BTreeSet, error::Error, fmt};

use chrono::TimeDelta;
use serde::{Deserialize, de::DeserializeOwned};
use serde_json::{Map, Value, json};
use tracing::warn;

use crate::{
    config::{Config, TranslationProvider},
    http_client::{HttpClient, HttpError, HttpResponse},
    util::{cache::TimedCache, sanitize::sanitize_html},
};

/// How long the provider's language pairs may be served from
/// [`TranslationLanguagesCache`]: one hour.
///
/// The list changes only when the operator swaps providers, so refetching it
/// per request would only cost a round trip to a third-party server.
pub const LANGUAGES_TTL: TimeDelta = TimeDelta::seconds(3600);

/// The [`crate::state::AppState`] slot caching the provider's language pairs;
/// `Default` builds it with [`LANGUAGES_TTL`].
pub type TranslationLanguagesCache = TimedCache<Vec<LanguagePair>>;

impl Default for TimedCache<Vec<LanguagePair>> {
    /// An empty language cache with the endpoint's [`LANGUAGES_TTL`] window,
    /// so every [`AppState`] constructor writes the same `Arc::default()`.
    ///
    /// [`AppState`]: crate::state::AppState
    fn default() -> Self {
        Self::new(LANGUAGES_TTL)
    }
}

/// The name the endpoints report as a `LibreTranslate` answer's `provider`
/// (Pleroma's `@name`).
const LIBRETRANSLATE_NAME: &str = "LibreTranslate";
/// The same, for the `DeepL` API.
const DEEPL_NAME: &str = "DeepL";
/// The `source` value that tells `LibreTranslate` to detect the language
/// itself, for a status that names none.
const AUTO_SOURCE: &str = "auto";
/// The `content-type` of the bodies both providers speak.
const JSON_MIME: &str = "application/json";
/// The `content-type` `DeepL`'s language endpoint is posted an empty body
/// under (Pleroma sends the same form label).
const FORM_MIME: &str = "application/x-www-form-urlencoded";
/// Bytes a `GET` provider answer may hold before the read gives up: a language
/// list is far smaller than this, and an endless body from a hostile server
/// must not be buffered. The `POST` calls — whose answers this service asks
/// for, from a URL the operator configured — go through the client's uncapped
/// read (see [`HttpClient::post_bytes`]).
///
/// [`HttpClient::post_bytes`]: crate::http_client::HttpClient::post_bytes
const MAX_RESPONSE_BYTES: usize = 1024 * 1024;

/// One source language and the languages it can be translated into: one row
/// of the matrix `GET /api/v1/instance/translation_languages` answers.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct LanguagePair {
    /// The source language's code, e.g. `en`.
    pub source: String,
    /// The codes it may be translated into: every known language but itself.
    pub targets: Vec<String>,
}

/// What a provider answered for one text.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Translation {
    /// The translated HTML, already run through the instance's sanitizer
    /// ([`crate::util::sanitize::sanitize_html`]).
    pub content: String,
    /// The language the provider recognised the source as, when it said:
    /// `DeepL` always, `LibreTranslate` only on servers that report it.
    pub detected_source_language: Option<String>,
    /// Which provider answered, spelled as the API reports it:
    /// `LibreTranslate` or `DeepL`.
    pub provider: &'static str,
}

/// How one translation attempt failed. Both kinds are the endpoints' 503;
/// what differs is only what the log says.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum TranslateError {
    /// The provider could not be reached, or refused the call. The payload is
    /// the transport failure or the status code it answered with.
    Unavailable(String),
    /// The provider answered success with something this service cannot read.
    /// The payload is the parse failure, or what was missing.
    InvalidResponse(String),
}

impl fmt::Display for TranslateError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Unavailable(detail) => write!(f, "translation provider unavailable: {detail}"),
            Self::InvalidResponse(detail) => {
                write!(f, "translation provider answered unreadably: {detail}")
            }
        }
    }
}

impl Error for TranslateError {}

/// The instance's translator: which provider status translation goes through,
/// built from the `TRANSLATION_*` settings (see [`Translator::from_config`]).
#[derive(Clone, Debug)]
pub enum Translator {
    /// No provider is configured, so nothing translates: every
    /// [`translate`](Self::translate) call fails and
    /// [`languages`](Self::languages) is empty.
    None,
    /// A `LibreTranslate` server at `api_url` (no trailing slash), keyed with
    /// `api_key` when that server asks for a key.
    LibreTranslate {
        /// The shared outbound client these calls go through.
        http: HttpClient,
        /// The provider's API base URL, from `TRANSLATION_API_URL`.
        api_url: String,
        /// The provider's API key, from `TRANSLATION_API_KEY`; a self-hosted
        /// server usually needs none.
        api_key: Option<String>,
    },
    /// The `DeepL` API at `api_url` (no trailing slash), keyed with `api_key`,
    /// which that API always needs.
    DeepL {
        /// The shared outbound client these calls go through.
        http: HttpClient,
        /// The provider's API base URL, from `TRANSLATION_API_URL`.
        api_url: String,
        /// The provider's API key, from `TRANSLATION_API_KEY`.
        api_key: String,
    },
}

impl Translator {
    /// The translator `config`'s `TRANSLATION_*` settings name, sending
    /// through `http`.
    ///
    /// A provider that cannot be talked to — none selected, no
    /// `TRANSLATION_API_URL` to reach it at, or `deepl` without its
    /// `TRANSLATION_API_KEY` — is [`Translator::None`]: the same rule
    /// [`TranslationConfig::is_configured`] states and the v2 instance
    /// document reports.
    ///
    /// [`TranslationConfig::is_configured`]: crate::config::TranslationConfig::is_configured
    #[must_use]
    pub fn from_config(config: &Config, http: &HttpClient) -> Self {
        let settings = &config.translation;
        let built = settings.api_url.as_deref().and_then(|api_url| {
            selected(
                settings.provider,
                http,
                api_url,
                settings.api_key.as_deref(),
            )
        });
        built.unwrap_or(Self::None)
    }

    /// The provider's name as the translate endpoint reports it, or `None`
    /// when this instance has no provider at all.
    #[must_use]
    pub const fn provider_name(&self) -> Option<&'static str> {
        match self {
            Self::None => None,
            Self::LibreTranslate { .. } => Some(LIBRETRANSLATE_NAME),
            Self::DeepL { .. } => Some(DEEPL_NAME),
        }
    }

    /// Translates `text` (the status's HTML `content`) into `target`, telling
    /// the provider it came from `source` — or asking it to detect the
    /// language when `source` is `None`.
    ///
    /// The translated HTML is sanitized before it is handed back, so a
    /// provider's answer cannot smuggle in markup this instance would not
    /// otherwise serve.
    ///
    /// # Errors
    ///
    /// [`TranslateError::Unavailable`] when no provider is configured or the
    /// call failed (transport, timeout, or a 4xx/5xx answer), and
    /// [`TranslateError::InvalidResponse`] when a successful answer is not the
    /// JSON the provider documents.
    pub async fn translate(
        &self,
        text: &str,
        source: Option<&str>,
        target: &str,
    ) -> Result<Translation, TranslateError> {
        let job = Job {
            text,
            source,
            target,
        };
        match self {
            Self::None => Err(TranslateError::Unavailable(
                "no translation provider is configured".to_owned(),
            )),
            Self::LibreTranslate {
                http,
                api_url,
                api_key,
            } => libretranslate(http, api_url, api_key.as_deref(), &job).await,
            Self::DeepL {
                http,
                api_url,
                api_key,
            } => deepl(http, api_url, api_key, &job).await,
        }
    }

    /// The language pairs this provider offers, as [`LanguagePair`] rows:
    /// every source it knows with every other language it can move it into.
    ///
    /// [`Translator::None`] answers the empty list without asking anyone. A
    /// provider whose list cannot be fetched — or cannot be read — answers it
    /// too, having logged why: the endpoint that serves these pairs has a
    /// plain empty document as its answer for "no list".
    #[must_use]
    pub async fn languages(&self) -> Vec<LanguagePair> {
        let fetched = match self {
            Self::None => return Vec::new(),
            Self::LibreTranslate { http, api_url, .. } => {
                libretranslate_languages(http, api_url).await
            }
            Self::DeepL {
                http,
                api_url,
                api_key,
            } => deepl_languages(http, api_url, api_key).await,
        };
        fetched.unwrap_or_else(|err| {
            warn!(%err, "translation provider listed no languages");
            Vec::new()
        })
    }
}

/// The translator one provider setting selects, `None` when a piece that
/// provider cannot run without is missing: `api_url` for both, and `api_key`
/// for `DeepL`, whose API has no unauthenticated mode.
fn selected(
    provider: TranslationProvider,
    http: &HttpClient,
    api_url: &str,
    api_key: Option<&str>,
) -> Option<Translator> {
    match provider {
        TranslationProvider::None => None,
        TranslationProvider::LibreTranslate => Some(Translator::LibreTranslate {
            http: http.clone(),
            api_url: api_url.to_owned(),
            api_key: api_key.map(str::to_owned),
        }),
        TranslationProvider::DeepL => Some(Translator::DeepL {
            http: http.clone(),
            api_url: api_url.to_owned(),
            api_key: api_key?.to_owned(),
        }),
    }
}

/// One translation job: what to move into which language, as
/// [`Translator::translate`] received it. Grouped because a provider call
/// needs these three beside its own endpoint and key, and AGENTS.md caps a
/// function at five parameters.
struct Job<'a> {
    /// The status's HTML `content`, the text to translate.
    text: &'a str,
    /// The language the status says it is written in, `None` when it names
    /// none and the provider is left to detect one.
    source: Option<&'a str>,
    /// The language to translate into.
    target: &'a str,
}

/// One `LibreTranslate` `POST /translate` round trip.
async fn libretranslate(
    http: &HttpClient,
    api_url: &str,
    api_key: Option<&str>,
    job: &Job<'_>,
) -> Result<Translation, TranslateError> {
    let body = json!({
        "q": job.text,
        "source": job.source.unwrap_or(AUTO_SOURCE),
        "target": job.target,
        "format": "html",
        "api_key": api_key.unwrap_or_default(),
    });
    let answer: LibreTranslateAnswer =
        post_json(http, &endpoint(api_url, "/translate"), None, &body).await?;
    Ok(Translation {
        content: sanitize_html(&answer.translated_text),
        detected_source_language: answer
            .detected_source_language
            .or_else(|| job.source.map(str::to_owned)),
        provider: LIBRETRANSLATE_NAME,
    })
}

/// One `DeepL` `POST /v2/translate` round trip.
async fn deepl(
    http: &HttpClient,
    api_url: &str,
    api_key: &str,
    job: &Job<'_>,
) -> Result<Translation, TranslateError> {
    let auth = format!("DeepL-Auth-Key {api_key}");
    // DeepL detects the source when `source_lang` is absent, so an unknown
    // source is left out rather than guessed at.
    let mut fields = vec![
        ("text", json!([job.text])),
        ("target_lang", json!(job.target)),
        ("tag_handling", json!("html")),
    ];
    if let Some(found) = job.source {
        fields.push(("source_lang", json!(found)));
    }
    let body = Value::Object(
        fields
            .into_iter()
            .map(|(name, value)| (name.to_owned(), value))
            .collect(),
    );
    let answer: DeepLAnswer = post_json(
        http,
        &endpoint(api_url, "/v2/translate"),
        Some(auth.as_str()),
        &body,
    )
    .await?;
    let first = answer.translations.into_iter().next().ok_or_else(|| {
        TranslateError::InvalidResponse("no translation in the answer".to_owned())
    })?;
    Ok(Translation {
        content: sanitize_html(&first.text),
        detected_source_language: first.detected_source_language,
        provider: DEEPL_NAME,
    })
}

/// `LibreTranslate`'s language list: every code its `GET /languages` answers
/// with, usable as a source and as a target (Pleroma reads the same document
/// twice).
async fn libretranslate_languages(
    http: &HttpClient,
    api_url: &str,
) -> Result<Vec<LanguagePair>, TranslateError> {
    let listed: Vec<LibreTranslateLanguage> =
        get_json(http, &endpoint(api_url, "/languages")).await?;
    let codes: Vec<String> = listed
        .into_iter()
        .map(|entry| entry.language_code.to_lowercase())
        .collect();
    Ok(matrix(&codes, &codes))
}

/// `DeepL`'s language list: its source list and its target list, from the two
/// `POST /v2/languages?type=…` calls Pleroma makes too, with the same empty
/// form-encoded body.
async fn deepl_languages(
    http: &HttpClient,
    api_url: &str,
    api_key: &str,
) -> Result<Vec<LanguagePair>, TranslateError> {
    let auth = format!("DeepL-Auth-Key {api_key}");
    let listed = endpoint(api_url, "/v2/languages");
    let sources: Vec<DeepLLanguage> =
        post_form(http, &format!("{listed}?type=source"), &auth).await?;
    let targets: Vec<DeepLLanguage> =
        post_form(http, &format!("{listed}?type=target"), &auth).await?;
    Ok(matrix(&deepl_codes(&sources), &deepl_codes(&targets)))
}

/// The codes `DeepL` names in a language list: each entry's own downcased code,
/// plus the base tag of a regional spelling (`pt-br` lists `pt-br` *and* `pt`),
/// which is how Pleroma flattens the same document so clients may ask for
/// either.
fn deepl_codes(listed: &[DeepLLanguage]) -> Vec<String> {
    listed
        .iter()
        .flat_map(|entry| {
            let code = entry.language_code.to_lowercase();
            match code.split_once('-') {
                Some((base, _region)) => vec![base.to_owned(), code],
                None => vec![code],
            }
        })
        .collect()
}

/// The pairs of `sources` into `targets`: one [`LanguagePair`] per source, each
/// carrying every target that is not that source itself, both sides
/// deduplicated and sorted so the document built from them is stable across
/// fetches.
fn matrix(sources: &[String], targets: &[String]) -> Vec<LanguagePair> {
    let known: BTreeSet<&str> = targets.iter().map(String::as_str).collect();
    let listable: BTreeSet<&str> = sources.iter().map(String::as_str).collect();
    listable
        .into_iter()
        .map(|source| LanguagePair {
            source: source.to_owned(),
            targets: known
                .iter()
                .filter(|candidate| **candidate != source)
                .map(|candidate| (*candidate).to_owned())
                .collect(),
        })
        .collect()
}

/// The URL of one provider path under a configured base (whose trailing slash
/// `Config` has already trimmed).
fn endpoint(base: &str, path: &str) -> String {
    format!("{base}{path}")
}

/// `GET`s `url` and reads its JSON answer as `T`.
///
/// # Errors
///
/// Those of [`checked`] and [`read_json`].
async fn get_json<T: DeserializeOwned>(http: &HttpClient, url: &str) -> Result<T, TranslateError> {
    let sent = http.get_limited(url, &[], MAX_RESPONSE_BYTES).await;
    read_json(&checked(sent)?)
}

/// `POST`s `body` to `url` as JSON, with `auth` as the `Authorization` header
/// when the provider needs one, and reads its JSON answer as `T`.
///
/// # Errors
///
/// Those of [`checked`] and [`read_json`].
async fn post_json<T: DeserializeOwned>(
    http: &HttpClient,
    url: &str,
    auth: Option<&str>,
    body: &Value,
) -> Result<T, TranslateError> {
    let text = body.to_string();
    post_raw(http, url, JSON_MIME, auth, &text).await
}

/// `POST`s the empty form body `DeepL`'s language endpoint asks for and reads
/// its JSON answer as `T`.
///
/// # Errors
///
/// Those of [`checked`] and [`read_json`].
async fn post_form<T: DeserializeOwned>(
    http: &HttpClient,
    url: &str,
    auth: &str,
) -> Result<T, TranslateError> {
    post_raw(http, url, FORM_MIME, Some(auth), "").await
}

/// The one `POST` both JSON and form calls go through.
///
/// # Errors
///
/// Those of [`checked`] and [`read_json`].
async fn post_raw<T: DeserializeOwned>(
    http: &HttpClient,
    url: &str,
    content_type: &str,
    auth: Option<&str>,
    body: &str,
) -> Result<T, TranslateError> {
    let mut headers = vec![("content-type", content_type)];
    if let Some(token) = auth {
        headers.push(("authorization", token));
    }
    let sent = http
        .post_bytes(url, &headers, body.as_bytes().to_vec())
        .await;
    read_json(&checked(sent)?)
}

/// A transport outcome, checked for what the provider answered: the failure is
/// [`TranslateError::Unavailable`], a 4xx/5xx status likewise, and anything
/// else passes through.
///
/// # Errors
///
/// [`TranslateError::Unavailable`] naming the transport failure or the status
/// code.
fn checked(sent: Result<HttpResponse, HttpError>) -> Result<HttpResponse, TranslateError> {
    let response = sent.map_err(|err| {
        TranslateError::Unavailable(format!("the provider could not be reached: {err}"))
    })?;
    response
        .error_for_status()
        .map_err(|err| TranslateError::Unavailable(format!("the provider refused: {err}")))
}

/// Parses a provider's buffered answer as `T`.
///
/// # Errors
///
/// [`TranslateError::InvalidResponse`] carrying what the parser said.
fn read_json<T: DeserializeOwned>(response: &HttpResponse) -> Result<T, TranslateError> {
    response.json().map_err(|err| {
        TranslateError::InvalidResponse(format!(
            "the provider answer is not its documented JSON: {err}"
        ))
    })
}

/// The `translatedText` half of a `LibreTranslate` answer.
#[derive(Debug, Deserialize)]
struct LibreTranslateAnswer {
    /// The translated text.
    #[serde(rename = "translatedText")]
    translated_text: String,
    /// The language the server says it translated from, on servers that report
    /// it; `None` on those that do not.
    #[serde(rename = "detectedSourceLanguage", default)]
    detected_source_language: Option<String>,
}

/// One entry of `LibreTranslate`'s `GET /languages` list; only its code is
/// this service's business.
#[derive(Debug, Deserialize)]
struct LibreTranslateLanguage {
    /// The provider's code for the language.
    #[serde(rename = "code")]
    language_code: String,
}

/// The `translations` array of a `DeepL` answer.
#[derive(Debug, Deserialize)]
struct DeepLAnswer {
    /// One entry per text sent; this service sends exactly one.
    translations: Vec<DeepLText>,
}

/// One entry of a `DeepL` `translations` array.
#[derive(Debug, Deserialize)]
struct DeepLText {
    /// The translated text.
    text: String,
    /// The language `DeepL` detected the source as, spelled as it spells it;
    /// `None` when the answer leaves it out.
    #[serde(default)]
    detected_source_language: Option<String>,
}

/// One entry of a `DeepL` `/v2/languages` list; only its code is this service's
/// business.
#[derive(Debug, Deserialize)]
struct DeepLLanguage {
    /// The provider's code for the language, uppercased (`DE`, `PT-BR`).
    #[serde(rename = "language")]
    language_code: String,
}

/// The language pairs as the instance endpoint answers them:
/// `{"<source>": ["<target>", …], …}`, the empty object for an empty list.
#[must_use]
pub fn languages_matrix(pairs: &[LanguagePair]) -> Map<String, Value> {
    pairs
        .iter()
        .map(|pair| {
            (
                pair.source.clone(),
                Value::Array(
                    pair.targets
                        .iter()
                        .map(|target| Value::String(target.clone()))
                        .collect(),
                ),
            )
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;

    use axum::{
        Router,
        body::Body,
        extract::State,
        http::{HeaderMap, Method, Request, StatusCode, header},
        response::Response,
        routing::{any, get},
    };
    use serde_json::json;
    use tokio::{net::TcpListener, sync::Mutex, task::JoinHandle};

    use super::*;
    use crate::config::{
        TRANSLATION_API_KEY_VAR, TRANSLATION_API_URL_VAR, TRANSLATION_PROVIDER_VAR,
    };

    /// The `User-Agent` these clients send.
    const TEST_UA: &str = "Wired (https://wired.test)";
    /// The key the tests configure a provider with.
    const TEST_KEY: &str = "test-provider-key";

    /// One provider request, as the mock recorded it.
    #[derive(Clone, Debug)]
    struct Seen {
        /// Method and path with query, e.g. `POST /v2/translate`.
        target: String,
        /// The `content-type` header, or the empty string.
        content_type: String,
        /// The `authorization` header, or the empty string.
        authorization: String,
        /// The request body as text.
        body: String,
    }

    /// The mock provider: what it recorded, and what it has been told to
    /// answer with.
    #[derive(Debug, Default)]
    struct Mock {
        /// Every request the mini-server has answered.
        seen: Mutex<Vec<Seen>>,
        /// Status to answer every call with.
        status: StatusCode,
        /// Body to answer a translate call with.
        translate_body: String,
        /// Body to answer `GET /languages` with.
        languages_body: String,
        /// Body to answer `POST /v2/languages` with.
        deepl_languages_body: String,
    }

    impl Mock {
        /// A mock answering success with `translate_body`, plus the fixed
        /// two-language lists.
        fn new(translate_body: String) -> Self {
            Self {
                status: StatusCode::OK,
                translate_body,
                languages_body: json!([{"code": "en"}, {"code": "de"}]).to_string(),
                deepl_languages_body: json!([{"language": "EN"}, {"language": "DE"}]).to_string(),
                ..Self::default()
            }
        }

        /// The requests this mock has answered so far.
        async fn requests(&self) -> Vec<Seen> {
            self.seen.lock().await.clone()
        }
    }

    /// Serves `mock` on a loopback port, returning its origin URL and the task
    /// that keeps it alive.
    async fn spawn(mock: Arc<Mock>) -> (String, JoinHandle<()>) {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let app = Router::new()
            .route("/languages", get(record))
            .route("/{*rest}", any(record))
            .with_state(mock);
        let server = tokio::spawn(async move {
            if let Err(err) = axum::serve(listener, app).await {
                tracing::debug!("mock provider stopped: {err}");
            }
        });
        (format!("http://{addr}"), server)
    }

    /// Records one request and answers it per the mock's script.
    async fn record(
        State(mock): State<Arc<Mock>>,
        method: Method,
        request: Request<Body>,
    ) -> Response {
        let (parts, body) = request.into_parts();
        let bytes = http_body_util::BodyExt::collect(body)
            .await
            .unwrap()
            .to_bytes();
        let path = parts.uri.path().to_owned();
        let seen = Seen {
            target: format!("{method} {}", parts.uri),
            content_type: header_text(&parts.headers, header::CONTENT_TYPE),
            authorization: header_text(&parts.headers, header::AUTHORIZATION),
            body: String::from_utf8_lossy(&bytes).into_owned(),
        };
        mock.seen.lock().await.push(seen);
        let text = if method == Method::GET {
            mock.languages_body.clone()
        } else if path.ends_with("/v2/languages") {
            mock.deepl_languages_body.clone()
        } else {
            mock.translate_body.clone()
        };
        Response::builder()
            .status(mock.status)
            .header(header::CONTENT_TYPE, JSON_MIME)
            .body(Body::from(text))
            .unwrap()
    }

    /// The visible text of header `name`, empty when absent or unreadable.
    fn header_text(headers: &HeaderMap, name: header::HeaderName) -> String {
        headers
            .get(name)
            .and_then(|value| value.to_str().ok())
            .unwrap_or_default()
            .to_owned()
    }

    /// The config of one provider, reaching it at `api_url`
    /// (`None` leaves that setting unset).
    fn provider_config(provider: &str, api_url: Option<&str>, api_key: Option<&str>) -> Config {
        Config::from_lookup(|key| match key {
            crate::config::DATABASE_URL_VAR => Some("postgres://localhost/wired".to_owned()),
            crate::config::BASE_URL_VAR => Some("https://test.example".to_owned()),
            TRANSLATION_PROVIDER_VAR => Some(provider.to_owned()),
            TRANSLATION_API_URL_VAR => api_url.map(str::to_owned),
            TRANSLATION_API_KEY_VAR => api_key.map(str::to_owned),
            _ => None,
        })
        .unwrap()
    }

    /// The translator of one provider configuration, talking to `api_url`.
    fn provider_translator(provider: &str, api_url: &str, api_key: Option<&str>) -> Translator {
        let config = provider_config(provider, Some(api_url), api_key);
        Translator::from_config(&config, &HttpClient::new_for_tests(TEST_UA))
    }

    /// The JSON body of a `LibreTranslate` answer.
    fn libre_body(text: &str) -> String {
        json!({ "translatedText": text }).to_string()
    }

    /// The JSON body of a `DeepL` answer.
    fn deepl_body(text: &str, detected: &str) -> String {
        json!({ "translations": [{ "text": text, "detected_source_language": detected }] })
            .to_string()
    }

    #[test]
    fn settings_that_name_no_usable_provider_build_none() {
        let client = HttpClient::new_for_tests(TEST_UA);
        for (provider, url, key) in [
            ("none", Some("http://host"), Some("k")),
            ("libretranslate", None, Some("k")),
            ("deepl", Some("http://host"), None),
            ("deepl", None, Some("k")),
        ] {
            let config = provider_config(provider, url, key);
            assert!(
                matches!(Translator::from_config(&config, &client), Translator::None),
                "`{provider}` with url {url:?} and key {key:?} is no translator"
            );
        }
    }

    #[test]
    fn a_named_provider_builds_its_translator_and_name() {
        let libre = provider_translator("libretranslate", "https://translate.example", None);
        let Translator::LibreTranslate {
            api_url, api_key, ..
        } = &libre
        else {
            panic!("`libretranslate` builds its own translator: {libre:?}");
        };
        assert_eq!(api_url, "https://translate.example", "the URL it reaches");
        assert!(api_key.is_none(), "a server that asks no key has none");
        assert_eq!(
            libre.provider_name(),
            Some("LibreTranslate"),
            "the name Pleroma's provider reports"
        );

        let deepl = provider_translator("deepl", "https://api.deepl.example", Some(TEST_KEY));
        let Translator::DeepL {
            api_url: sent_url,
            api_key: sent_key,
            ..
        } = &deepl
        else {
            panic!("`deepl` builds its own translator: {deepl:?}");
        };
        assert_eq!(sent_key, TEST_KEY, "DeepL carries its key");
        assert_eq!(sent_url, "https://api.deepl.example", "the URL it reaches");
        assert_eq!(deepl.provider_name(), Some("DeepL"), "its own name");
        assert_eq!(
            Translator::None.provider_name(),
            None,
            "no provider reports no name"
        );
    }

    #[tokio::test]
    async fn libretranslate_posts_the_documented_request() {
        let mock = Arc::new(Mock::new(libre_body("<p>Hallo</p>")));
        let (url, server) = spawn(Arc::clone(&mock)).await;
        let translator = provider_translator("libretranslate", &url, Some(TEST_KEY));

        let answer = translator
            .translate("<p>Hello</p>", Some("en"), "de")
            .await
            .unwrap();

        assert_eq!(answer.content, "<p>Hallo</p>", "the translated HTML");
        assert_eq!(
            answer.detected_source_language.as_deref(),
            Some("en"),
            "a server that says nothing reports back what it was told"
        );
        assert_eq!(answer.provider, "LibreTranslate", "which server answered");
        let seen = mock.requests().await;
        assert_eq!(seen.len(), 1, "one call per translate");
        assert_eq!(seen[0].target, "POST /translate", "the endpoint");
        assert_eq!(seen[0].content_type, JSON_MIME, "as JSON");
        let body: Value = serde_json::from_str(&seen[0].body).unwrap();
        assert_eq!(
            body,
            json!({
                "q": "<p>Hello</p>",
                "source": "en",
                "target": "de",
                "format": "html",
                "api_key": TEST_KEY,
            }),
            "the body Pleroma's provider sends"
        );
        server.abort();
    }

    #[tokio::test]
    async fn libretranslate_asks_for_detection_without_a_source() {
        let mock = Arc::new(Mock::new(
            json!({"translatedText": "<p>Hallo</p>", "detectedSourceLanguage": "nl"}).to_string(),
        ));
        let (url, server) = spawn(Arc::clone(&mock)).await;
        let translator = provider_translator("libretranslate", &url, None);

        let answer = translator
            .translate("<p>Hallo</p>", None, "de")
            .await
            .unwrap();

        assert_eq!(
            answer.detected_source_language.as_deref(),
            Some("nl"),
            "the language the server detected wins over the one it was told"
        );
        let body: Value = serde_json::from_str(&mock.requests().await[0].body).unwrap();
        assert_eq!(body["source"], "auto", "no source means auto");
        assert_eq!(
            body["api_key"], "",
            "an unkeyed server is sent an empty key"
        );
        server.abort();
    }

    #[tokio::test]
    async fn deepl_posts_the_documented_request_and_header() {
        let mock = Arc::new(Mock::new(deepl_body("<p>Hallo</p>", "EN")));
        let (url, server) = spawn(Arc::clone(&mock)).await;
        let translator = provider_translator("deepl", &url, Some(TEST_KEY));

        let answer = translator
            .translate("<p>Hello</p>", Some("en"), "de")
            .await
            .unwrap();

        assert_eq!(answer.content, "<p>Hallo</p>", "the translated HTML");
        assert_eq!(
            answer.detected_source_language.as_deref(),
            Some("EN"),
            "what DeepL detected"
        );
        assert_eq!(answer.provider, "DeepL", "which server answered");
        let seen = mock.requests().await;
        assert_eq!(seen[0].target, "POST /v2/translate", "the v2 endpoint");
        assert_eq!(
            seen[0].authorization,
            format!("DeepL-Auth-Key {TEST_KEY}"),
            "the key rides in the authorization header"
        );
        let body: Value = serde_json::from_str(&seen[0].body).unwrap();
        assert_eq!(
            body,
            json!({
                "text": ["<p>Hello</p>"],
                "target_lang": "de",
                "tag_handling": "html",
                "source_lang": "en",
            }),
            "the body Pleroma's provider sends"
        );
        server.abort();
    }

    #[tokio::test]
    async fn deepl_omits_the_source_it_does_not_know() {
        let mock = Arc::new(Mock::new(deepl_body("<p>Hallo</p>", "EN")));
        let (url, server) = spawn(Arc::clone(&mock)).await;
        let translator = provider_translator("deepl", &url, Some(TEST_KEY));

        let answer = translator
            .translate("<p>Hello</p>", None, "de")
            .await
            .unwrap();

        let body: Value = serde_json::from_str(&mock.requests().await[0].body).unwrap();
        assert!(
            body.get("source_lang").is_none(),
            "an unknown source is left for DeepL to detect: {body}"
        );
        assert_eq!(
            answer.detected_source_language.as_deref(),
            Some("EN"),
            "and it says what it found"
        );
        server.abort();
    }

    #[tokio::test]
    async fn a_provider_that_refuses_is_unavailable() {
        for refused in [
            StatusCode::TOO_MANY_REQUESTS,
            StatusCode::FORBIDDEN,
            StatusCode::INTERNAL_SERVER_ERROR,
        ] {
            let mock = Mock {
                status: refused,
                ..Mock::new(libre_body("<p>Hallo</p>"))
            };
            let (url, server) = spawn(Arc::new(mock)).await;
            let translator = provider_translator("libretranslate", &url, None);

            let err = translator
                .translate("<p>Hello</p>", Some("en"), "de")
                .await
                .unwrap_err();

            assert_eq!(
                err,
                TranslateError::Unavailable(format!(
                    "the provider refused: HTTP peer answered {}",
                    refused.as_u16()
                )),
                "the refused status is what failed the call"
            );
            server.abort();
        }
    }

    #[tokio::test]
    async fn an_unreadable_answer_is_an_invalid_response() {
        let mock = Arc::new(Mock::new("<html>not json</html>".to_owned()));
        let (url, server) = spawn(Arc::clone(&mock)).await;
        let translator = provider_translator("libretranslate", &url, None);

        let err = translator
            .translate("<p>Hello</p>", Some("en"), "de")
            .await
            .unwrap_err();

        assert!(
            matches!(err, TranslateError::InvalidResponse(_)),
            "a 200 that is not the documented JSON: {err}"
        );
        server.abort();
    }

    #[tokio::test]
    async fn an_empty_translations_array_is_an_invalid_response() {
        let mock = Arc::new(Mock::new(json!({"translations": []}).to_string()));
        let (url, server) = spawn(Arc::clone(&mock)).await;
        let translator = provider_translator("deepl", &url, Some(TEST_KEY));

        let err = translator
            .translate("<p>Hello</p>", Some("en"), "de")
            .await
            .unwrap_err();

        assert_eq!(
            err,
            TranslateError::InvalidResponse("no translation in the answer".to_owned()),
            "one text sent asks for exactly one translation"
        );
        server.abort();
    }

    #[tokio::test]
    async fn a_provider_that_cannot_be_reached_is_unavailable() {
        // Port 9 (discard) answers nothing on the loopback.
        let translator = provider_translator("deepl", "http://127.0.0.1:9", Some(TEST_KEY));

        let err = translator
            .translate("<p>Hello</p>", Some("en"), "de")
            .await
            .unwrap_err();

        assert!(
            matches!(err, TranslateError::Unavailable(_)),
            "a dead peer fails the call: {err}"
        );
    }

    #[tokio::test]
    async fn the_none_translator_refuses_everything_without_calling() {
        assert_eq!(
            Translator::None
                .translate("<p>Hello</p>", Some("en"), "de")
                .await
                .unwrap_err(),
            TranslateError::Unavailable("no translation provider is configured".to_owned()),
            "no provider, no translation"
        );
        assert!(
            Translator::None.languages().await.is_empty(),
            "and no languages either"
        );
    }

    #[tokio::test]
    async fn translated_html_is_sanitized_before_it_is_handed_back() {
        let mock = Arc::new(Mock::new(libre_body(
            "<p>Hallo</p><script>alert(1)</script><a href=\"javascript:x\">x</a>",
        )));
        let (url, server) = spawn(Arc::clone(&mock)).await;
        let translator = provider_translator("libretranslate", &url, None);

        let answer = translator
            .translate("<p>Hello</p>", Some("en"), "de")
            .await
            .unwrap();

        assert_eq!(
            answer.content, "<p>Hallo</p><a rel=\"ugc nofollow noopener noreferrer\">x</a>",
            "the script element goes with its contents, the bad link scheme \
             loses its href, and only the allowed markup remains"
        );
        server.abort();
    }

    #[tokio::test]
    async fn libretranslate_languages_is_their_own_matrix() {
        let mock = Arc::new(Mock {
            languages_body: json!([{"code": "EN"}, {"code": "de"}, {"code": "de"}]).to_string(),
            ..Mock::new(libre_body("<p>Hallo</p>"))
        });
        let (url, server) = spawn(Arc::clone(&mock)).await;
        let translator = provider_translator("libretranslate", &url, None);

        let pairs = translator.languages().await;

        assert_eq!(
            pairs,
            vec![
                LanguagePair {
                    source: "de".to_owned(),
                    targets: vec!["en".to_owned()],
                },
                LanguagePair {
                    source: "en".to_owned(),
                    targets: vec!["de".to_owned()],
                },
            ],
            "codes are downcased, deduplicated and crossed, never with a \
             language in its own target list"
        );
        assert_eq!(
            mock.requests().await[0].target,
            "GET /languages",
            "the list is one `GET`"
        );
        server.abort();
    }

    #[tokio::test]
    async fn deepl_languages_reads_both_of_its_lists() {
        let mock = Arc::new(Mock {
            deepl_languages_body: json!([{"language": "EN"}, {"language": "PT-BR"}]).to_string(),
            ..Mock::new(deepl_body("<p>Hallo</p>", "EN"))
        });
        let (url, server) = spawn(Arc::clone(&mock)).await;
        let translator = provider_translator("deepl", &url, Some(TEST_KEY));

        let pairs = translator.languages().await;

        assert_eq!(
            pairs,
            vec![
                LanguagePair {
                    source: "en".to_owned(),
                    targets: vec!["pt".to_owned(), "pt-br".to_owned()],
                },
                LanguagePair {
                    source: "pt".to_owned(),
                    targets: vec!["en".to_owned(), "pt-br".to_owned()],
                },
                LanguagePair {
                    source: "pt-br".to_owned(),
                    targets: vec!["en".to_owned(), "pt".to_owned()],
                },
            ],
            "a regional spelling is listed beside its base tag"
        );
        let seen = mock.requests().await;
        assert_eq!(
            seen.iter()
                .map(|entry| entry.target.as_str())
                .collect::<Vec<&str>>(),
            vec![
                "POST /v2/languages?type=source",
                "POST /v2/languages?type=target"
            ],
            "two form posts, sources first"
        );
        assert!(
            seen.iter()
                .all(|entry| entry.authorization == format!("DeepL-Auth-Key {TEST_KEY}")),
            "the language endpoint is authenticated too"
        );
        assert!(
            seen.iter().all(|entry| entry.content_type == FORM_MIME),
            "and asks under the form media type DeepL documents"
        );
        server.abort();
    }

    #[tokio::test]
    async fn a_failing_language_list_is_the_empty_list() {
        let mock = Mock {
            status: StatusCode::SERVICE_UNAVAILABLE,
            ..Mock::new(libre_body("<p>Hallo</p>"))
        };
        let (url, server) = spawn(Arc::new(mock)).await;
        let translator = provider_translator("libretranslate", &url, None);

        assert!(
            translator.languages().await.is_empty(),
            "an endpoint that cannot list languages answers none, not an error"
        );
        server.abort();
    }

    #[test]
    fn the_matrix_document_is_one_entry_per_source() {
        let pairs = vec![
            LanguagePair {
                source: "en".to_owned(),
                targets: vec!["de".to_owned(), "fr".to_owned()],
            },
            LanguagePair {
                source: "de".to_owned(),
                targets: Vec::new(),
            },
        ];
        assert_eq!(
            serde_json::to_value(languages_matrix(&pairs)).unwrap(),
            json!({"de": [], "en": ["de", "fr"]}),
            "the shape the instance endpoint answers"
        );
        assert!(
            languages_matrix(&[]).is_empty(),
            "no pairs is the empty object"
        );
    }

    #[test]
    fn the_language_cache_window_is_an_hour() {
        assert_eq!(LANGUAGES_TTL.num_seconds(), 3600, "one hour");
        let cache = TranslationLanguagesCache::default();
        let now = chrono::Utc::now();
        cache.insert(now, Vec::new());
        assert!(
            cache.get(now + TimeDelta::seconds(3599)).is_some(),
            "fresh inside the window"
        );
        assert!(
            cache.get(now + TimeDelta::seconds(3600)).is_none(),
            "stale once it lapses"
        );
    }

    #[test]
    fn translate_errors_state_why_the_provider_failed() {
        assert_eq!(
            TranslateError::Unavailable("timeout".to_owned()).to_string(),
            "translation provider unavailable: timeout",
            "the transport kind"
        );
        assert_eq!(
            TranslateError::InvalidResponse("not json".to_owned()).to_string(),
            "translation provider answered unreadably: not json",
            "the shape kind"
        );
        assert!(
            TranslateError::InvalidResponse(String::new())
                .source()
                .is_none(),
            "the reason is text, so nothing wraps it"
        );
    }
}
