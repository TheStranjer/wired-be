//! The media player: `GET /notice/{id}/embed_player` (Pleroma's
//! `notice_player` action,
//! `lib/pleroma/web/o_status/o_status_controller.ex`) answers the bare
//! `<video>` / `<audio>` element for the one attachment of an embeddable
//! status whose url equals the `url` query parameter. The embeddability
//! gate, the attachment read, the element renderer and the shared 404
//! page are the ones [`super::embed`] exposes; the shared page helpers
//! and constants ride on [`super`].

use axum::{
    extract::{Path, Query, State},
    http::StatusCode,
    response::Response,
};
use serde::Deserialize;

use crate::{error::AppResult, models::media_attachment::MediaAttachment, state::AppState};

use super::{
    PLAYER_TITLE,
    embed::{MediaKind, attachments_of, embeddable_status, media_element, not_found_page},
    embed_page,
};

/// The query of `GET /notice/{id}/embed_player`: Pleroma's
/// `conn.params["url"]`, with the url optional so a request without it
/// deserializes and is refused as no match rather than as a parse error.
#[derive(Debug, Default, Deserialize)]
pub(super) struct PlayerQuery {
    /// The attachment url to play, matched verbatim against the status's
    /// attachments; `None` when the query carries no `url`.
    url: Option<String>,
}

/// `GET /notice/{id}/embed_player` (Pleroma's `notice_player` action,
/// `lib/pleroma/web/o_status/o_status_controller.ex`): the bare player
/// for the one attachment of the status whose `url` equals the `url`
/// query parameter exactly, under the same embeddability rule as the
/// card ([`embeddable_status`]). Video and audio only: an image match,
/// no match, a missing or empty `url`, a malformed or unknown id and a
/// non-media content type all answer the shared 404
/// [`NOT_FOUND_MESSAGE`](super::NOT_FOUND_MESSAGE) page. The 200 is
/// [`player_body`]'s document through [`embed_page`]: framable, HTML and
/// CSP'd like a card.
///
/// # Errors
///
/// [`AppError::Database`](crate::error::AppError::Database) when a lookup fails.
pub(super) async fn embed_player(
    State(state): State<AppState>,
    Path(id): Path<String>,
    Query(query): Query<PlayerQuery>,
) -> AppResult<Response> {
    let Some(url) = query.url.filter(|url| !url.is_empty()) else {
        return Ok(not_found_page());
    };
    let Some(status) = embeddable_status(&state, &id).await? else {
        return Ok(not_found_page());
    };
    let attachments = attachments_of(&state, status.id).await?;
    let Some(element) = player_element(&attachments, &url) else {
        return Ok(not_found_page());
    };
    Ok(embed_page(
        StatusCode::OK,
        PLAYER_TITLE,
        &player_body(&element),
    ))
}

/// The player element for `url` among `attachments`: the first
/// attachment in [`MediaAttachment::position`] order (a stable sort, so
/// equal positions keep their loaded order) whose `url` equals the
/// parameter verbatim, rendered through [`media_element`]. `None` when
/// nothing matches, when the match is an image, and when its content
/// type is no [`MediaKind`] at all — the player plays video and audio
/// only.
fn player_element(attachments: &[MediaAttachment], url: &str) -> Option<String> {
    let mut ordered: Vec<&MediaAttachment> = attachments.iter().collect();
    ordered.sort_by_key(|attachment| attachment.position);
    let attachment = ordered.into_iter().find(|found| found.url == url)?;
    let kind = MediaKind::of(&attachment.content_type)?;
    (!matches!(kind, MediaKind::Image)).then(|| {
        media_element(
            kind,
            &attachment.url,
            attachment.description.as_deref().unwrap_or_default(),
        )
    })
}

/// The body of the player page: just the player element inside
/// `<main class="player">`, the frame Pleroma's `embed_player` template
/// puts its media in.
fn player_body(element: &str) -> String {
    format!("<main class=\"player\">{element}</main>\n")
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::api::html_pages::test_support::attachment;

    #[test]
    fn the_player_query_reads_the_url_and_may_miss_it() {
        let picked: PlayerQuery = serde_json::from_value(serde_json::json!({ "url": "/m/v.mp4" }))
            .expect("a query with the url deserializes");
        assert_eq!(picked.url.as_deref(), Some("/m/v.mp4"), "the url");
        let empty: PlayerQuery =
            serde_json::from_value(serde_json::json!({})).expect("a query without it deserializes");
        assert_eq!(empty.url, None, "and reads as no url");
        let default = PlayerQuery::default();
        assert_eq!(default.url, None, "the default is no url");
    }

    #[test]
    fn player_element_wants_a_verbatim_video_or_audio_match() {
        let attachments = [
            attachment("image/png", "/m/a.png", 0, Some("a picture")),
            attachment("video/mp4", "/m/v.mp4", 1, Some("a film")),
            attachment("audio/mpeg", "/m/s.mp3", 2, None),
            attachment("application/pdf", "/m/doc.pdf", 3, None),
        ];
        assert_eq!(
            player_element(&attachments, "/m/v.mp4").as_deref(),
            Some("<video controls src=\"/m/v.mp4\" title=\"a film\"></video>"),
            "the video plays with its description as title"
        );
        assert_eq!(
            player_element(&attachments, "/m/s.mp3").as_deref(),
            Some("<audio controls src=\"/m/s.mp3\" title=\"\"></audio>"),
            "the audio plays with an empty title"
        );
        assert_eq!(
            player_element(&attachments, "/m/a.png"),
            None,
            "an image match is no player"
        );
        assert_eq!(
            player_element(&attachments, "/m/doc.pdf"),
            None,
            "a non-media content type is no player"
        );
        assert_eq!(
            player_element(&attachments, "/m/other.mp4"),
            None,
            "a url that no attachment carries is no player"
        );
        assert_eq!(
            player_element(&attachments, "/m/v.mp?4"),
            None,
            "the match is verbatim, not fuzzy"
        );
    }

    #[test]
    fn player_element_takes_the_first_position_match() {
        let attachments = [
            attachment("video/mp4", "/m/same.mp4", 5, None),
            attachment("image/png", "/m/same.mp4", 1, None),
        ];
        assert_eq!(
            player_element(&attachments, "/m/same.mp4"),
            None,
            "the position-first match is the image, so there is no player"
        );
    }

    #[test]
    fn player_body_frames_the_element_in_main_player() {
        assert_eq!(
            player_body("<video controls src=\"/m/v.mp4\" title=\"\"></video>"),
            "<main class=\"player\"><video controls src=\"/m/v.mp4\" title=\"\"></video></main>\n",
            "just the element inside the player main"
        );
    }
}
