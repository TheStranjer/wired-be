//! The `DeepL` adapter: its translate call, its language lists and the
//! answer shapes it parses.

use serde::Deserialize;
use serde_json::{Value, json};

use super::{
    DEEPL_NAME, Job, LanguagePair, TranslateError, Translation,
    transport::{endpoint, matrix, post_form, post_json},
};
use crate::{http_client::HttpClient, util::sanitize::sanitize_html};

/// One `DeepL` `POST /v2/translate` round trip.
pub(super) async fn deepl(
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

/// `DeepL`'s language list: its source list and its target list, from the two
/// `POST /v2/languages?type=…` calls Pleroma makes too, with the same empty
/// form-encoded body.
pub(super) async fn deepl_languages(
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

#[cfg(test)]
mod tests {
    use std::sync::Arc;

    use super::super::{
        FORM_MIME,
        test_support::{Mock, TEST_KEY, deepl_body, provider_translator, spawn},
    };
    use super::*;

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
}
