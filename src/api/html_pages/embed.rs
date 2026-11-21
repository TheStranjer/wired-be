//! The embeddable status card: `GET /embed/{id}` (Pleroma's
//! `EmbedController.show`) renders a public or unlisted status as a small
//! framable document — the author line, the sanitized content, the media
//! attachments and the date. The 404 [`not_found_page`], the
//! [`embeddable_status`] gate, the [`attachments_of`] read and the
//! [`MediaKind`] element renderers are the pieces the player page in
//! [`super::player`] shares; the shared page helpers and constants ride
//! on [`super`].

use axum::{
    extract::{Path, State},
    http::StatusCode,
    response::Response,
};
use chrono::{DateTime, Utc};
use uuid::Uuid;

use crate::{
    error::AppResult,
    models::{
        media_attachment::{self, MediaAttachment},
        status::{self, Status, Visibility},
        user::{self, User},
    },
    state::AppState,
    util::{id::parse_id, sanitize::escape_html, time::mastodon_time},
};

use super::{EMBED_TITLE, NOT_FOUND_MESSAGE, embed_page, message_body};

/// `GET /embed/{id}` (Pleroma's `EmbedController.show`, widened per this
/// ticket to remote statuses and to unlisted): the embeddable status card,
/// framable by construction (`embed_page` never puts up
/// `X-Frame-Options`). A malformed id, an unknown or deleted status, a
/// private or direct one, and a status whose author is gone all answer
/// the same 404 [`NOT_FOUND_MESSAGE`] page.
///
/// # Errors
///
/// [`AppError::Database`](crate::error::AppError::Database) when a lookup fails.
pub(super) async fn embed(
    State(state): State<AppState>,
    Path(id): Path<String>,
) -> AppResult<Response> {
    let Some(rows) = load_card(&state, &id).await? else {
        return Ok(not_found_page());
    };
    let card = Card::from(&rows);
    Ok(embed_page(StatusCode::OK, EMBED_TITLE, &embed_card(&card)))
}

/// What one embed page reads: the live status, its author and its
/// attachments, as the lookup loaded them for [`Card`].
#[derive(Debug)]
struct CardRows {
    /// The embeddable status.
    status: Status,
    /// Its author.
    author: User,
    /// The status's attachments, in the loaded `position` order.
    attachments: Vec<MediaAttachment>,
}

/// Loads the rows behind the card for path segment `id`, or `None` for
/// every refusal the route answers with one 404 page: the refusals of
/// [`embeddable_status`] plus an author with no row.
///
/// # Errors
///
/// [`AppError::Database`](crate::error::AppError::Database) when a lookup fails.
async fn load_card(state: &AppState, id: &str) -> AppResult<Option<CardRows>> {
    let Some(status) = embeddable_status(state, id).await? else {
        return Ok(None);
    };
    let Some(author) = user::get_by_id(&state.db, status.user_id).await? else {
        return Ok(None);
    };
    let attachments = attachments_of(state, status.id).await?;
    Ok(Some(CardRows {
        status,
        author,
        attachments,
    }))
}

/// The status path segment `id` names when it is embeddable: `id` is a
/// [`uuid::Uuid`], a live status carries it (an unknown id and a
/// tombstone read the same) and its visibility is [`Visibility::Public`]
/// or [`Visibility::Unlisted`]. `None` is the one refusal both the embed
/// card and the player answer with a 404 page for.
///
/// # Errors
///
/// [`AppError::Database`](crate::error::AppError::Database) when the lookup fails.
pub(super) async fn embeddable_status(state: &AppState, id: &str) -> AppResult<Option<Status>> {
    let Some(uuid) = parse_id(id) else {
        return Ok(None);
    };
    let Some(status) = status::get_by_id(&state.db, uuid).await? else {
        return Ok(None);
    };
    if !matches!(status.visibility, Visibility::Public | Visibility::Unlisted) {
        return Ok(None);
    }
    Ok(Some(status))
}

/// The attachments of status `status_id` in the loaded `position` order:
/// the single-status batch lookup, empty when it carries none.
///
/// # Errors
///
/// [`AppError::Database`](crate::error::AppError::Database) when the lookup fails.
pub(super) async fn attachments_of(
    state: &AppState,
    status_id: Uuid,
) -> AppResult<Vec<MediaAttachment>> {
    Ok(media_attachment::for_statuses(&state.db, &[status_id])
        .await?
        .remove(&status_id)
        .unwrap_or_default())
}

/// The borrowed bundle [`embed_card`] renders: everything the card shows,
/// cut out of [`CardRows`] (or a test fixture) so the renderer is a pure
/// function with no database in sight.
#[derive(Debug)]
struct Card<'a> {
    /// The author's display name, as stored.
    display_name: &'a str,
    /// The author's handle without its `@`: the full nickname, remote
    /// hosts and all, rendered as stored.
    handle: &'a str,
    /// The author's profile url: the stored `profile_url` else the actor
    /// id, [`crate::entities::account`]'s rule.
    profile_url: &'a str,
    /// The status content: sanitized HTML already, inserted verbatim.
    content: &'a str,
    /// The attachments to render.
    attachments: &'a [MediaAttachment],
    /// When the status was published.
    published: DateTime<Utc>,
}

impl<'a> From<&'a CardRows> for Card<'a> {
    fn from(rows: &'a CardRows) -> Self {
        Self {
            display_name: &rows.author.name,
            handle: &rows.author.nickname,
            profile_url: rows
                .author
                .profile_url
                .as_deref()
                .unwrap_or(&rows.author.ap_id),
            content: &rows.status.content,
            attachments: &rows.attachments,
            published: rows.status.inserted_at,
        }
    }
}

/// The body of an embed card: the author line (display name and `@`handle
/// linked to the profile, every field escaped), the status `content`
/// inserted verbatim — it was sanitized on write — the attachments block,
/// and the date as [`mastodon_time`] spells it.
fn embed_card(card: &Card<'_>) -> String {
    let mut out = String::from("<div class=\"status-embed\">\n");
    push_author(&mut out, card);
    out.push_str("<div class=\"status-content\">");
    out.push_str(card.content);
    out.push_str("</div>\n");
    push_attachments(&mut out, card.attachments);
    let published = mastodon_time(&card.published);
    out.push_str("<p class=\"status-date\"><time datetime=\"");
    out.push_str(&published);
    out.push_str("\">");
    out.push_str(&published);
    out.push_str("</time></p>\n</div>\n");
    out
}

/// Appends the card's author line: display name and `@handle`, each
/// escaped, inside a link to the (escaped) profile url.
fn push_author(out: &mut String, card: &Card<'_>) {
    out.push_str("<a class=\"author\" href=\"");
    out.push_str(&escape_html(card.profile_url));
    out.push_str("\"><span class=\"display-name\">");
    out.push_str(&escape_html(card.display_name));
    out.push_str("</span><span class=\"handle\">@");
    out.push_str(&escape_html(card.handle));
    out.push_str("</span></a>\n");
}

/// The media an embed card (and the player page) can show: the three
/// kinds an element exists for. A MIME type outside them is no media and
/// renders nothing.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum MediaKind {
    /// Rendered as an `<img>`.
    Image,
    /// Rendered as a `<video controls>`.
    Video,
    /// Rendered as an `<audio controls>`.
    Audio,
}

impl MediaKind {
    /// Classifies a `content_type` by its top-level prefix: the text
    /// before the `/`, lowercased and trimmed, must name one of the three
    /// kinds — anything else (an empty prefix, `application/pdf`, no type
    /// at all) is no media.
    pub(super) fn of(content_type: &str) -> Option<Self> {
        let head = content_type
            .split_once('/')
            .map_or(content_type, |(head, _)| head);
        match head.trim().to_ascii_lowercase().as_str() {
            "image" => Some(Self::Image),
            "video" => Some(Self::Video),
            "audio" => Some(Self::Audio),
            _ => None,
        }
    }

    /// The HTML element name this kind renders as.
    const fn tag(self) -> &'static str {
        match self {
            Self::Image => "img",
            Self::Video => "video",
            Self::Audio => "audio",
        }
    }
}

/// One attachment's element: an `<img>` with the escaped `url` and the
/// escaped `description` as its alt text, or a `controls`-bearing
/// `<video>` / `<audio>` carrying the same pair as `src` and `title`.
pub(super) fn media_element(kind: MediaKind, url: &str, description: &str) -> String {
    let src = escape_html(url);
    let text = escape_html(description);
    if matches!(kind, MediaKind::Image) {
        return format!("<img src=\"{src}\" alt=\"{text}\">");
    }
    let tag = kind.tag();
    format!("<{tag} controls src=\"{src}\" title=\"{text}\"></{tag}>")
}

/// Appends the attachments block: one `<div class="attachment">` per
/// media attachment in [`MediaAttachment::position`] order (a stable
/// sort, so equal positions keep their loaded order); attachments of no
/// [`MediaKind`] render nothing at all.
fn push_attachments(out: &mut String, attachments: &[MediaAttachment]) {
    let mut ordered: Vec<&MediaAttachment> = attachments.iter().collect();
    ordered.sort_by_key(|attachment| attachment.position);
    for attachment in ordered {
        let Some(kind) = MediaKind::of(&attachment.content_type) else {
            continue;
        };
        out.push_str("<div class=\"attachment\">");
        out.push_str(&media_element(
            kind,
            &attachment.url,
            attachment.description.as_deref().unwrap_or_default(),
        ));
        out.push_str("</div>\n");
    }
}

/// The one 404 answer of the embed route: [`NOT_FOUND_MESSAGE`] as an
/// embed page — framable, HTML and CSP'd exactly like the cards, so a
/// refusal tells nothing about what the id was.
pub(super) fn not_found_page() -> Response {
    embed_page(
        StatusCode::NOT_FOUND,
        NOT_FOUND_MESSAGE,
        &message_body(NOT_FOUND_MESSAGE),
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::api::html_pages::{
        CSP_HEADER, FRAME_OPTIONS_HEADER, PAGE_CONTENT_TYPE, PAGE_CSP,
        test_support::{attachment, headers_of, published},
    };
    use axum::http::header;

    /// A card named `display_name` / `handle`, at `profile_url`, carrying
    /// `content` and `attachments`, published at the fixed instant.
    fn card<'a>(
        display_name: &'a str,
        handle: &'a str,
        profile_url: &'a str,
        content: &'a str,
        attachments: &'a [MediaAttachment],
    ) -> Card<'a> {
        Card {
            display_name,
            handle,
            profile_url,
            content,
            attachments,
            published: published(),
        }
    }

    #[test]
    fn embed_card_draws_author_content_attachments_and_date() {
        let attachments = [attachment(
            "image/png",
            "/media/a.png",
            0,
            Some("a picture"),
        )];
        let html = embed_card(&card(
            "Alice",
            "alice",
            "https://test.example/@alice",
            "<p>Hello <b>world</b></p>",
            &attachments,
        ));
        assert!(
            html.contains("href=\"https://test.example/@alice\""),
            "the author line links to the profile: {html}"
        );
        assert!(html.contains(">Alice</span>"), "the display name");
        assert!(html.contains("@alice</span>"), "the handle gains its @");
        assert!(
            html.contains("<div class=\"status-content\"><p>Hello <b>world</b></p></div>"),
            "the content is inserted verbatim: {html}"
        );
        assert!(
            html.contains("<img src=\"/media/a.png\" alt=\"a picture\">"),
            "the image renders: {html}"
        );
        assert!(
            html.contains(
                "<time datetime=\"2024-01-31T12:34:56.000Z\">2024-01-31T12:34:56.000Z</time>"
            ),
            "the date is the mastodon_time spelling: {html}"
        );
    }

    #[test]
    fn a_hostile_author_field_cannot_escape_the_card() {
        let html = embed_card(&card(
            r#"<script>alert("xss")</script>"#,
            r#"eve" onload="pwn()""#,
            r#"https://x.example/" onmouseover="alert(1)"#,
            "<p>safe</p>",
            &[],
        ));
        assert!(!html.contains("<script"), "no tag opens anywhere: {html}");
        assert!(
            html.contains("&lt;script&gt;alert(&quot;xss&quot;)&lt;/script&gt;"),
            "the display name arrives escaped: {html}"
        );
        assert!(
            html.contains("href=\"https://x.example/&quot; onmouseover=&quot;alert(1)\""),
            "the profile url cannot close its attribute: {html}"
        );
        assert!(
            html.contains("@eve&quot; onload=&quot;pwn()&quot;"),
            "the handle stays text: {html}"
        );
        assert!(
            html.contains("<p>safe</p>"),
            "the sanitized content still passes through: {html}"
        );
    }

    #[test]
    fn a_remote_handle_renders_as_stored() {
        let html = embed_card(&card(
            "Eve",
            "eve@remote.example",
            "https://remote.example/users/eve",
            "",
            &[],
        ));
        assert!(
            html.contains("@eve@remote.example</span>"),
            "the stored nickname renders whole, host included: {html}"
        );
    }

    #[test]
    fn media_kind_reads_the_lowercased_trimmed_top_level_prefix() {
        assert_eq!(MediaKind::of("image/png"), Some(MediaKind::Image));
        assert_eq!(MediaKind::of("IMAGE/JPEG"), Some(MediaKind::Image));
        assert_eq!(MediaKind::of(" video/mp4 "), Some(MediaKind::Video));
        assert_eq!(MediaKind::of("Audio/MPEG"), Some(MediaKind::Audio));
        assert_eq!(MediaKind::of("image"), Some(MediaKind::Image), "no slash");
        assert_eq!(MediaKind::of("application/pdf"), None);
        assert_eq!(MediaKind::of("text/html"), None);
        assert_eq!(MediaKind::of(""), None, "no type at all");
        assert_eq!(MediaKind::of("/png"), None, "an empty prefix");
    }

    #[test]
    fn media_kind_tags_name_its_element() {
        assert_eq!(MediaKind::Image.tag(), "img");
        assert_eq!(MediaKind::Video.tag(), "video");
        assert_eq!(MediaKind::Audio.tag(), "audio");
    }

    #[test]
    fn media_elements_escape_the_url_and_the_description() {
        assert_eq!(
            media_element(
                MediaKind::Image,
                r#"/media/a.png" onerror="alert(1)""#,
                r#"a "quoted" & <b>pic</b>"#,
            ),
            "<img src=\"/media/a.png&quot; onerror=&quot;alert(1)&quot;\" \
             alt=\"a &quot;quoted&quot; &amp; &lt;b&gt;pic&lt;/b&gt;\">",
            "the image carries url and alt escaped"
        );
        assert_eq!(
            media_element(MediaKind::Video, "/m/v.mp4", ""),
            "<video controls src=\"/m/v.mp4\" title=\"\"></video>"
        );
        assert_eq!(
            media_element(MediaKind::Audio, "/m/a.mp3", "a song"),
            "<audio controls src=\"/m/a.mp3\" title=\"a song\"></audio>"
        );
    }

    #[test]
    fn push_attachments_orders_by_position_and_skips_non_media() {
        let attachments = [
            attachment("application/pdf", "/f/doc.pdf", 0, None),
            attachment("video/mp4", "/f/v.mp4", 5, None),
            attachment("image/png", "/f/a.png", 1, None),
            attachment("audio/mpeg", "/f/s.mp3", 3, None),
        ];
        let mut out = String::new();
        push_attachments(&mut out, &attachments);
        assert!(
            !out.contains("doc.pdf"),
            "a non-media attachment renders nothing: {out}"
        );
        assert_eq!(
            out.matches("<div class=\"attachment\">").count(),
            3,
            "one div per media attachment"
        );
        let image = out.find("<img").expect("the image renders");
        let audio = out.find("<audio").expect("the audio renders");
        let video = out.find("<video").expect("the video renders");
        assert!(image < audio && audio < video, "position order: {out}");
    }

    #[test]
    fn the_embed_not_found_page_is_an_embed_page() {
        let response = not_found_page();
        assert_eq!(response.status(), StatusCode::NOT_FOUND, "404");
        let headers = headers_of(&response);
        assert_eq!(
            headers[header::CONTENT_TYPE],
            PAGE_CONTENT_TYPE,
            "the refusal is an HTML page"
        );
        assert_eq!(headers[CSP_HEADER.clone()], PAGE_CSP, "with the CSP");
        assert!(
            !headers.contains_key(FRAME_OPTIONS_HEADER.clone()),
            "and framable like a card, so it tells nothing away"
        );
    }
}
