//! The HTTP plumbing both provider adapters share: the calls, the status
//! check, the bounded JSON read and the language matrix document.

use std::collections::BTreeSet;

use serde::de::DeserializeOwned;
use serde_json::{Map, Value};

use super::{FORM_MIME, JSON_MIME, LanguagePair, MAX_RESPONSE_BYTES, TranslateError};
use crate::http_client::{HttpClient, HttpError, HttpResponse};

/// The pairs of `sources` into `targets`: one [`LanguagePair`] per source, each
/// carrying every target that is not that source itself, both sides
/// deduplicated and sorted so the document built from them is stable across
/// fetches.
pub(super) fn matrix(sources: &[String], targets: &[String]) -> Vec<LanguagePair> {
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
pub(super) fn endpoint(base: &str, path: &str) -> String {
    format!("{base}{path}")
}

/// `GET`s `url` and reads its JSON answer as `T`.
///
/// # Errors
///
/// Those of [`checked`] and [`read_json`].
pub(super) async fn get_json<T: DeserializeOwned>(
    http: &HttpClient,
    url: &str,
) -> Result<T, TranslateError> {
    let sent = http.get_limited(url, &[], MAX_RESPONSE_BYTES).await;
    read_json(&checked(sent)?)
}

/// `POST`s `body` to `url` as JSON, with `auth` as the `Authorization` header
/// when the provider needs one, and reads its JSON answer as `T`.
///
/// # Errors
///
/// Those of [`checked`] and [`read_json`].
pub(super) async fn post_json<T: DeserializeOwned>(
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
pub(super) async fn post_form<T: DeserializeOwned>(
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

    use axum::http::StatusCode;
    use serde_json::json;

    use super::super::test_support::{Mock, TEST_KEY, libre_body, provider_translator, spawn};
    use super::*;

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
}
