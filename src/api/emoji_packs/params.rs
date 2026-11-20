//! The query parameters the pack endpoints share: `name`, `url`, `page`
//! and `page_size`, and the pagination window over an ordered iterator.

use std::{
    convert::Infallible,
    future::{self, Future},
};

use axum::{extract::FromRequestParts, http::request::Parts};
use url::form_urlencoded;

use super::{DEFAULT_PAGE, DEFAULT_PAGE_SIZE, MAX_PAGE_SIZE, NAME_EMPTY};
use crate::error::{AppError, AppResult};

/// The `name`, `url`, `page` and `page_size` query parameters, parsed the
/// way [`crate::pagination::PageParams`] parses its keys: `name` and `url`
/// are kept raw for the endpoints to trim and validate (`url` is the
/// remote instance of the relay route), numbers that are absent or not
/// positive whole numbers fall back to the defaults, `page_size` is capped
/// at [`MAX_PAGE_SIZE`], the last occurrence of a repeated key wins and
/// unrelated keys are ignored.
#[derive(Debug, Clone)]
pub(super) struct PackParams {
    /// Raw `name` value, if present.
    pub(super) name: Option<String>,
    /// Raw `url` value, if present (the remote routes).
    pub(super) url: Option<String>,
    /// 1-based page number, defaulted.
    pub(super) page: usize,
    /// Page size, defaulted and capped at [`MAX_PAGE_SIZE`].
    pub(super) page_size: usize,
}

impl PackParams {
    /// The trimmed pack `name`; the endpoints' 400 when it is absent or
    /// blank (Pleroma's `validate_not_empty([name])`).
    pub(super) fn required_name(&self) -> AppResult<&str> {
        let name = self.name.as_deref().unwrap_or_default().trim();
        if name.is_empty() {
            return Err(AppError::BadRequest(NAME_EMPTY.to_owned()));
        }
        Ok(name)
    }
}

/// A query-string positive whole number: `None` for absent text, text
/// that is not a number, and zero (the endpoints fall back to their
/// defaults, as [`crate::pagination::PageParams`] does for `limit`).
fn number(text: &str) -> Option<usize> {
    text.parse().ok().filter(|value| *value > 0)
}

impl<S> FromRequestParts<S> for PackParams
where
    S: Send + Sync,
{
    type Rejection = Infallible;

    fn from_request_parts(
        parts: &mut Parts,
        _state: &S,
    ) -> impl Future<Output = Result<Self, Self::Rejection>> + Send {
        let mut name = None;
        let mut url = None;
        let mut page = None;
        let mut page_size = None;
        for (key, value) in form_urlencoded::parse(parts.uri.query().unwrap_or_default().as_bytes())
        {
            match key.as_ref() {
                "name" => name = Some(value.into_owned()),
                "url" => url = Some(value.into_owned()),
                "page" => page = number(&value),
                "page_size" => page_size = number(&value),
                _ => {}
            }
        }
        future::ready(Ok(Self {
            name,
            url,
            page: page.unwrap_or(DEFAULT_PAGE),
            page_size: page_size.unwrap_or(DEFAULT_PAGE_SIZE).min(MAX_PAGE_SIZE),
        }))
    }
}

/// The `page`-th 1-based window of `page_size` items of an ordered
/// iterator — Pleroma's `paginate/3` as iterators, empty page beyond the
/// end rather than an error.
pub(super) fn take_page<I>(items: I, page: usize, page_size: usize) -> Vec<I::Item>
where
    I: Iterator,
{
    let offset = page.saturating_sub(1).saturating_mul(page_size);
    items.skip(offset).take(page_size).collect()
}

#[cfg(test)]
mod tests {
    use axum::http::{Request, Uri};

    use super::*;

    /// Request parts carrying `uri`, the way the extractor receives them.
    fn parts_for(uri: &str) -> Parts {
        Request::get(uri.parse::<Uri>().unwrap())
            .body(())
            .unwrap()
            .into_parts()
            .0
    }

    /// The parameters the request line `uri` extracts.
    async fn params_for(uri: &str) -> PackParams {
        let mut parts = parts_for(uri);
        PackParams::from_request_parts(&mut parts, &())
            .await
            .unwrap()
    }

    #[tokio::test]
    async fn page_params_default_and_clamp_like_pleromas_paginate() {
        let plain = params_for("/api/v1/pleroma/emoji/packs").await;
        assert_eq!(
            (plain.page, plain.page_size),
            (DEFAULT_PAGE, DEFAULT_PAGE_SIZE),
            "absent parameters take the defaults"
        );
        assert_eq!(plain.name, None, "no name by default");
        assert_eq!(plain.url, None, "no url by default");

        let big = params_for("/p?page=0&page_size=500&name=x").await;
        assert_eq!(big.page, 1, "a page below 1 reads as the first");
        assert_eq!(
            (big.page_size, big.name.as_deref()),
            (100, Some("x")),
            "page_size caps at MAX_PAGE_SIZE, name is kept raw"
        );

        for query in ["/p?page=abc&page_size=1.5", "/p?page=-2&page_size=0"] {
            let junk = params_for(query).await;
            assert_eq!(
                (junk.page, junk.page_size),
                (DEFAULT_PAGE, DEFAULT_PAGE_SIZE),
                "{query}: numbers that are not whole fall back to the defaults"
            );
        }

        let repeated = params_for("/p?page=2&page=3&page_size=5&page_size=7").await;
        assert_eq!(
            (repeated.page, repeated.page_size),
            (3, 7),
            "the last occurrence wins"
        );
        let encoded = params_for("/p?name=a%20b%2Fc").await;
        assert_eq!(
            encoded.name.as_deref(),
            Some("a b/c"),
            "percent-encoding is decoded before validation rejects it"
        );
    }

    #[tokio::test]
    async fn a_missing_or_blank_name_is_pleromas_empty_value_error() {
        for uri in ["/p", "/p?name=", "/p?name=%20%20"] {
            let params = params_for(uri).await;
            let err = params.required_name().unwrap_err();
            assert!(
                matches!(err, AppError::BadRequest(_)),
                "{uri} is a 400, not {err}"
            );
            assert_eq!(err.to_string(), NAME_EMPTY, "message for {uri}");
        }
        let padded = params_for("/p?name=%20cats%20").await;
        assert_eq!(
            padded.required_name().unwrap(),
            "cats",
            "the name is trimmed like Pleroma's String.trim"
        );
    }

    #[test]
    fn take_pages_walks_ordered_items() {
        let items: Vec<usize> = (1..=7).collect();
        assert_eq!(
            take_page(items.iter().copied(), 1, 3),
            [1, 2, 3],
            "first page"
        );
        assert_eq!(
            take_page(items.iter().copied(), 3, 3),
            [7],
            "ragged last page"
        );
        assert!(
            take_page(items.iter().copied(), 4, 3).is_empty(),
            "past the end is an empty page"
        );
        assert_eq!(
            take_page(items.iter().copied(), usize::MAX, 30),
            Vec::<usize>::new(),
            "an absurd page cannot overflow"
        );
    }
}
