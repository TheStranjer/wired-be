//! The inline `RE: <link>` fallback a quote post carries.
//!
//! A server that cannot authorize and show a quote (Mastodon without a
//! `QuoteAuthorization`) still renders the post's `content`, so the content
//! of a quote ends with `<p class="quote-inline">RE: <a …>…</a></p>`, the
//! marker Mastodon, Misskey and Pleroma use for the same purpose and the one
//! quote-aware clients hide. The fallback is presentation, not user text:
//! the stored `raw_text` never carries it, so an edit renders it once more
//! from the unchanged source instead of stacking a second one.

use crate::{
    error::AppResult,
    models::status::{self, Status},
    state::AppState,
    util::format::{text_to_inline_html, urls::scheme_len},
};

/// The class that marks the fallback paragraph.
const QUOTE_INLINE_CLASS: &str = "quote-inline";

/// The URL a fallback links to: the quoted post's human `url`, its `uri`
/// when the origin named none.
fn fallback_url(quoted: &Status) -> &str {
    quoted
        .url
        .as_deref()
        .filter(|url| !url.is_empty())
        .unwrap_or(&quoted.uri)
}

/// The `RE:` paragraph for `quoted`, `None` when the post needs none.
///
/// None is for a `raw_text` — what the author wrote — that already contains
/// the quoted post's URL, and for a URL that is not an `http(s)` one that
/// could be linked. The link's visible text is the one a bare URL gets
/// everywhere else: scheme hidden, cut at 30 characters.
pub(super) fn fallback_paragraph(raw_text: &str, quoted: &Status) -> Option<String> {
    let url = fallback_url(quoted);
    if scheme_len(url, 0).is_none() || raw_text.contains(url) || raw_text.contains(&quoted.uri) {
        return None;
    }
    let inline = text_to_inline_html(&format!("RE: {url}"));
    Some(format!("<p class=\"{QUOTE_INLINE_CLASS}\">{inline}</p>"))
}

/// The paragraph an edit of the post `row` appends to its content: the
/// fallback of the post `row` quotes, `None` for a post quoting nothing or
/// a quoted post that no longer exists.
///
/// # Errors
///
/// Any database failure of the quoted post's read, as
/// [`crate::error::AppError::Database`].
pub(super) async fn edit_fallback(
    state: &AppState,
    row: &Status,
    raw_text: &str,
) -> AppResult<Option<String>> {
    let Some(quoted_id) = row.quote_of_id else {
        return Ok(None);
    };
    let quoted = status::get_by_id(&state.db, quoted_id).await?;
    Ok(quoted.and_then(|quoted| fallback_paragraph(raw_text, &quoted)))
}

#[cfg(test)]
mod tests {
    use serde_json::json;
    use uuid::Uuid;

    use super::*;
    use crate::{
        federation::note::{load_note_context, note_json},
        services::create_status::{
            CreateStatusParams, EditStatusParams, edit_status,
            test_support::{BASE, created, posted, state, stored, user},
        },
    };

    /// A body quoting the post `quoted_id`.
    fn quoting(text: &str, quoted_id: &str) -> CreateStatusParams {
        CreateStatusParams {
            quote_id: Some(quoted_id.to_owned()),
            ..posted(text)
        }
    }

    /// The paragraph a quote of the post `row` owes, spelled out: the link's
    /// text is the scheme-less URL cut to 30 characters plus an ellipsis.
    fn expected_paragraph(row: &Status) -> String {
        let url = row.url.as_deref().expect("a local post has a URL");
        let naked = url.trim_start_matches("https://");
        let shown: String = naked.chars().take(30).collect();
        let ellipsis = if naked.chars().count() > 30 {
            "…"
        } else {
            ""
        };
        format!(
            "<p class=\"quote-inline\">RE: <a href=\"{url}\" \
             rel=\"ugc nofollow noopener noreferrer\">{shown}{ellipsis}</a></p>"
        )
    }

    #[tokio::test]
    async fn a_quote_ends_with_the_fallback_and_keeps_its_source_text() {
        let db = crate::test_db::test_db().await.expect("test database slot");
        let pool = db.pool();
        let app_state = state(pool);
        let alice = user(pool, "quotefallbackalice").await;
        let bob = user(pool, "quotefallbackbob").await;
        let original = created(&app_state, &bob, posted("the quoted word")).await;
        let original_row = stored(pool, &original).await;

        let quote = created(&app_state, &alice, quoting("walt quoting", &original.id)).await;
        let row = stored(pool, &quote).await;

        assert_eq!(
            row.content,
            format!("<p>walt quoting</p>{}", expected_paragraph(&original_row)),
            "the content links the quoted post after the text"
        );
        assert!(
            row.content.contains(&format!("{BASE}/notice/")),
            "the link is the quoted post's human URL"
        );
        assert_eq!(row.raw_text, "walt quoting", "the source text is untouched");
        assert_eq!(row.quote_of_id, Some(original_row.id));
    }

    #[tokio::test]
    async fn a_quote_whose_text_has_the_url_gets_no_second_link() {
        let db = crate::test_db::test_db().await.expect("test database slot");
        let pool = db.pool();
        let app_state = state(pool);
        let alice = user(pool, "quotefallbackcarol").await;
        let bob = user(pool, "quotefallbackdave").await;
        let original = created(&app_state, &bob, posted("the quoted word")).await;
        let url = stored(pool, &original).await.url.expect("a URL");

        let text = format!("look at {url}");
        let quote = created(&app_state, &alice, quoting(&text, &original.id)).await;
        let row = stored(pool, &quote).await;

        assert!(
            !row.content.contains("quote-inline"),
            "the author's own link already points at the quoted post: {}",
            row.content
        );
        assert_eq!(row.raw_text, text, "the text is stored as written");
    }

    #[tokio::test]
    async fn a_post_quoting_nothing_gets_no_fallback() {
        let db = crate::test_db::test_db().await.expect("test database slot");
        let pool = db.pool();
        let app_state = state(pool);
        let alice = user(pool, "quotefallbacknone").await;

        let plain = created(&app_state, &alice, posted("just words")).await;

        assert_eq!(stored(pool, &plain).await.content, "<p>just words</p>");
    }

    #[tokio::test]
    async fn an_edit_renders_the_fallback_once() {
        let db = crate::test_db::test_db().await.expect("test database slot");
        let pool = db.pool();
        let app_state = state(pool);
        let alice = user(pool, "quotefallbackeve").await;
        let bob = user(pool, "quotefallbackfred").await;
        let original = created(&app_state, &bob, posted("the quoted word")).await;
        let original_row = stored(pool, &original).await;
        let quote = created(&app_state, &alice, quoting("first take", &original.id)).await;
        let mut row = stored(pool, &quote).await;

        for text in ["second take", "third take"] {
            let params = crate::extract::into_params::<EditStatusParams>(json!({"status": text}))
                .expect("a text-only edit body");
            let _edited = edit_status(&app_state, &alice, &row, params)
                .await
                .expect("the edit writes")
                .expect("the status is live");
            row = status::get_by_id(pool, row.id)
                .await
                .unwrap()
                .expect("the edited status is stored");
            assert_eq!(
                row.content,
                format!("<p>{text}</p>{}", expected_paragraph(&original_row)),
                "one fallback after each edit, never stacked"
            );
            assert_eq!(row.raw_text, text, "the source text stays the author's");
        }
    }

    #[tokio::test]
    async fn the_federated_note_carries_the_fallback() {
        let db = crate::test_db::test_db().await.expect("test database slot");
        let pool = db.pool();
        let app_state = state(pool);
        let alice = user(pool, "quotefallbackgina").await;
        let bob = user(pool, "quotefallbackhank").await;
        let original = created(&app_state, &bob, posted("the quoted word")).await;
        let original_row = stored(pool, &original).await;
        let quote = created(&app_state, &alice, quoting("walt quoting", &original.id)).await;
        let id = Uuid::parse_str(&quote.id).expect("the entity carries its id");

        let ctx = load_note_context(pool, &app_state.config, id)
            .await
            .expect("the quote's context loads");
        let note = note_json(&ctx).expect("a public post federates");

        assert_eq!(
            note["content"],
            json!(format!(
                "<p>walt quoting</p>{}",
                expected_paragraph(&original_row)
            )),
            "the federated content includes the fallback"
        );
        assert_eq!(
            note["quoteUri"],
            json!(original_row.uri),
            "beside the quote"
        );
        assert_eq!(
            note["source"]["content"],
            json!("walt quoting"),
            "while the source stays the author's text"
        );
    }
}
