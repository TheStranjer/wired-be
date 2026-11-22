//! The `LibreTranslate` adapter: its translate call, its language list and
//! the answer shapes it parses.

use serde::Deserialize;
use serde_json::json;

use super::{
    AUTO_SOURCE, Job, LIBRETRANSLATE_NAME, LanguagePair, TranslateError, Translation,
    transport::{endpoint, get_json, matrix, post_json},
};
use crate::{http_client::HttpClient, util::sanitize::sanitize_html};

/// One `LibreTranslate` `POST /translate` round trip.
pub(super) async fn libretranslate(
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

/// `LibreTranslate`'s language list: every code its `GET /languages` answers
/// with, usable as a source and as a target (Pleroma reads the same document
/// twice).
pub(super) async fn libretranslate_languages(
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

#[cfg(test)]
mod tests {
    use std::sync::Arc;

    use serde_json::Value;

    use super::super::{
        JSON_MIME,
        test_support::{Mock, TEST_KEY, libre_body, provider_translator, spawn},
    };
    use super::*;

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
}
