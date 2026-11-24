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
use chrono::TimeDelta;
use tracing::warn;

mod deepl;
mod error;
mod libretranslate;
#[cfg(test)]
mod test_support;
mod transport;

pub use error::TranslateError;
pub use transport::languages_matrix;

use self::{
    deepl::{deepl, deepl_languages},
    libretranslate::{libretranslate, libretranslate_languages},
};
use crate::{
    config::{Config, TranslationProvider},
    http_client::HttpClient,
    util::cache::TimedCache,
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
/// must not be buffered. The `POST` calls cap their answers at
/// [`DEFAULT_RESPONSE_BYTES`](crate::http_client::DEFAULT_RESPONSE_BYTES),
/// the same size.
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

#[cfg(test)]
mod tests {
    use std::sync::Arc;

    use axum::http::StatusCode;

    use super::test_support::{
        Mock, TEST_KEY, TEST_UA, libre_body, provider_config, provider_translator, spawn,
    };
    use super::*;

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
}
