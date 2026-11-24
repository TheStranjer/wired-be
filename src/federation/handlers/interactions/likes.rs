//! The `Like` and `EmojiReact` folds, their withdrawals and the reading of a
//! delivered reaction.

use serde_json::Value;
use tracing::debug;

use super::{NO_INNER_ID, NO_TARGET, UNDO_LOCAL_SIGNER, target::resolve_target};
use crate::{
    api::{emoji_reactions::after_reaction_changed, statuses::after_favourite_changed},
    federation::{
        dispatch::{ActivityView, FederationError, Outcome},
        json,
    },
    models::{
        emoji_reaction::{self, NewReaction},
        favourite,
        user::User,
    },
    state::AppState,
    util::{emoji::Emoji, sanitize::strip_html},
};

/// One delivered reaction, in the parts the fold stores.
#[derive(Debug)]
struct Reaction {
    /// The emoji in its stored spelling: the document's own text, outer
    /// whitespace gone.
    emoji: String,
    /// The `Emoji` tag's icon URL; `None` for a unicode reaction.
    url: Option<String>,
}

/// What a delivered `Like`'s `content` field states, read the way Pleroma
/// reads the Misskey/Fedibird favourite-with-emoji shape.
enum LikeContent {
    /// No content, or a blank one: an ordinary favourite.
    Plain,
    /// The content names one emoji: the activity is a reaction.
    Reaction(Reaction),
    /// The content says something no reaction can name: Pleroma's rewrite
    /// hands it to the reaction validator and the validator refuses, so
    /// this does too — nothing is folded.
    Unusable,
}

/// The body of [`handle_like`]: the content rewrite first — a
/// favourite-with-emoji is a reaction, not a like — then the favourite
/// fold behind the shared target step.
pub(super) async fn apply_like(
    state: AppState,
    view: ActivityView,
    signer: User,
) -> Result<Outcome, FederationError> {
    if signer.local.get() {
        return Ok(Outcome::Ignored(
            "a Like signed by an account this instance owns",
        ));
    }
    match like_content(&view) {
        LikeContent::Reaction(reaction) => fold_reaction(&state, &view, &signer, &reaction).await,
        LikeContent::Unusable => Ok(Outcome::Ignored(
            "a Like whose content names neither one emoji nor a tagged shortcode",
        )),
        LikeContent::Plain => fold_favourite(&state, &view, &signer).await,
    }
}

/// The body of [`handle_emoji_react`]: the document's own `content` as a
/// reaction, then the shared reaction fold.
pub(super) async fn apply_emoji_react(
    state: AppState,
    view: ActivityView,
    signer: User,
) -> Result<Outcome, FederationError> {
    if signer.local.get() {
        return Ok(Outcome::Ignored(
            "an EmojiReact signed by an account this instance owns",
        ));
    }
    let Some(content) = view.raw.get("content").and_then(Value::as_str) else {
        return Ok(Outcome::Ignored(
            "an EmojiReact whose content is not a string",
        ));
    };
    match reaction_of(&view.raw, content) {
        None => Ok(Outcome::Ignored(
            "the EmojiReact content is neither one emoji nor a tagged shortcode",
        )),
        Some(reaction) => fold_reaction(&state, &view, &signer, &reaction).await,
    }
}

/// The body of [`handle_undo_like`]: the signer's own like row under the
/// undone activity id, withdrawn through the endpoint's own write.
pub(super) async fn apply_undo_like(
    state: AppState,
    view: ActivityView,
    signer: User,
) -> Result<Outcome, FederationError> {
    if signer.local.get() {
        return Ok(Outcome::Ignored(UNDO_LOCAL_SIGNER));
    }
    let Some(like_id) = view.object.id() else {
        return Ok(Outcome::Ignored(NO_INNER_ID));
    };
    let row = favourite::get_by_ap_id(&state.db, signer.id, like_id)
        .await
        .map_err(|err| {
            FederationError::transient("could not fold the remote interaction's like lookup", &err)
        })?;
    let Some(row) = row else {
        debug!(like_id, "an Undo naming no Like of its actor");
        return Ok(Outcome::Ignored("an Undo of a Like its actor never made"));
    };
    let removed = favourite::unfavourite(&state.db, signer.id, row.status_id)
        .await
        .map_err(|err| {
            FederationError::transient(
                "could not fold the remote interaction's like withdrawal",
                &err,
            )
        })?;
    let Some(removed) = removed else {
        return Ok(Outcome::Ignored("that Like is already withdrawn"));
    };
    after_favourite_changed(&state, &removed, false).await;
    debug!(
        status = %removed.status_id,
        actor = %signer.nickname,
        "an inbound Undo withdrew a Like"
    );
    Ok(Outcome::Handled)
}

/// The body of [`handle_undo_emoji_react`]: the signer's own reaction under
/// the undone activity id, withdrawn through the endpoint's own write.
pub(super) async fn apply_undo_emoji_react(
    state: AppState,
    view: ActivityView,
    signer: User,
) -> Result<Outcome, FederationError> {
    if signer.local.get() {
        return Ok(Outcome::Ignored(UNDO_LOCAL_SIGNER));
    }
    let Some(react_id) = view.object.id() else {
        return Ok(Outcome::Ignored(NO_INNER_ID));
    };
    let row = emoji_reaction::get_by_ap_id(&state.db, signer.id, react_id)
        .await
        .map_err(|err| {
            FederationError::transient(
                "could not fold the remote interaction's reaction lookup",
                &err,
            )
        })?;
    let Some(row) = row else {
        debug!(react_id, "an Undo naming no EmojiReact of its actor");
        return Ok(Outcome::Ignored(
            "an Undo of an EmojiReact its actor never made",
        ));
    };
    let removed = emoji_reaction::unreact(&state.db, signer.id, row.status_id, &row.emoji)
        .await
        .map_err(|err| {
            FederationError::transient(
                "could not fold the remote interaction's reaction withdrawal",
                &err,
            )
        })?;
    let Some(removed) = removed else {
        return Ok(Outcome::Ignored("that reaction is already withdrawn"));
    };
    after_reaction_changed(&state, &removed, false).await;
    debug!(
        status = %removed.status_id,
        actor = %signer.nickname,
        emoji = %removed.emoji,
        "an inbound Undo withdrew a reaction"
    );
    Ok(Outcome::Handled)
}

/// The favourite arm of [`apply_like`]: the shared target step, the
/// pair-keyed write under the delivered id, and — only for the insert that
/// happened — the endpoint's post-write hook.
async fn fold_favourite(
    state: &AppState,
    view: &ActivityView,
    signer: &User,
) -> Result<Outcome, FederationError> {
    let Some(target) = resolve_target(state, view, signer).await? else {
        return Ok(Outcome::Ignored(NO_TARGET));
    };
    let created = favourite::favourite_like(&state.db, signer.id, target.id, &view.id)
        .await
        .map_err(|err| {
            FederationError::transient("could not fold the remote interaction's like", &err)
        })?;
    let Some(row) = created else {
        debug!(status = %target.id, "this actor's like of that status stands already");
        return Ok(Outcome::Ignored(
            "this actor's Like of that status stands already",
        ));
    };
    after_favourite_changed(state, &row, true).await;
    debug!(
        status = %target.id,
        actor = %signer.nickname,
        "an inbound Like favourited a status"
    );
    Ok(Outcome::Handled)
}

/// The reaction fold, shared by [`apply_like`]'s rewritten arm and
/// [`apply_emoji_react`]: the target step, the triple-keyed write under
/// the delivered id, and the reaction endpoint's post-write hook — whose
/// telling deduplicates a restated reaction exactly as the `PUT` route's
/// repeat does.
async fn fold_reaction(
    state: &AppState,
    view: &ActivityView,
    signer: &User,
    reaction: &Reaction,
) -> Result<Outcome, FederationError> {
    let Some(target) = resolve_target(state, view, signer).await? else {
        return Ok(Outcome::Ignored(NO_TARGET));
    };
    let new = NewReaction {
        user_id: signer.id,
        status_id: target.id,
        emoji: &reaction.emoji,
        url: reaction.url.as_deref(),
    };
    let row = emoji_reaction::react_with_ap_id(&state.db, &new, &view.id)
        .await
        .map_err(|err| {
            FederationError::transient("could not fold the remote interaction's reaction", &err)
        })?;
    after_reaction_changed(state, &row, true).await;
    debug!(
        status = %target.id,
        actor = %signer.nickname,
        emoji = %row.emoji,
        "an inbound reaction landed on a status"
    );
    Ok(Outcome::Handled)
}

/// What a `Like`'s `content` states: absent or blank (of any markup) is the
/// plain favourite; text that names one emoji (or a tagged shortcode) is
/// the reaction Pleroma's rewrite names; anything else is nothing this
/// instance folds.
fn like_content(view: &ActivityView) -> LikeContent {
    let Some(content) = view.raw.get("content").and_then(Value::as_str) else {
        return LikeContent::Plain;
    };
    if strip_html(content).trim().is_empty() {
        return LikeContent::Plain;
    }
    reaction_of(&view.raw, content).map_or(LikeContent::Unusable, LikeContent::Reaction)
}

/// The stored reaction one delivered document states: its emoji text, HTML
/// gone and outer whitespace trimmed, classified by the local reaction
/// rules — one unicode grapheme, or a `:shortcode:` whose image the
/// activity's `tag` list carries. `None` is the fold's refusal: text that
/// names no single emoji, or a shortcode no `Emoji` tag vouches for
/// (Pleroma's `maybe_validate_tag_presence`, restated).
fn reaction_of(document: &Value, content: &str) -> Option<Reaction> {
    let text = strip_html(content).trim().to_owned();
    let code = match Emoji::classify(&text) {
        Some(Emoji::Unicode(_)) => None,
        Some(Emoji::Custom(code)) => Some(code.to_owned()),
        None => return None,
    };
    let url = match code.as_deref() {
        None => None,
        Some(name) => Some(emoji_tag_url(document, name)?),
    };
    Some(Reaction { emoji: text, url })
}

/// The icon URL of the `Emoji` tag naming `shortcode`: a tag entry of type
/// `Emoji` whose `name` — outer colons dropped, the spellings both Misskey
/// and Pleroma send — equals the shortcode, carrying an `http(s)` URL under
/// `icon`. The first match wins; `None` refuses the shortcode.
fn emoji_tag_url(document: &Value, shortcode: &str) -> Option<String> {
    document
        .get("tag")
        .map_or_default(json::as_array)
        .iter()
        .find(|entry| {
            entry.get("type").and_then(Value::as_str) == Some("Emoji")
                && entry
                    .get("name")
                    .and_then(Value::as_str)
                    .is_some_and(|name| name.trim_matches(':') == shortcode)
        })?
        .get("icon")
        .and_then(json::first_string)
        .filter(|url| json::http_host(url).is_some())
}

#[cfg(test)]
mod tests {
    use serde_json::json;

    use super::super::tests::{BOB, view};
    use super::*;

    #[test]
    fn a_like_content_states_a_reaction_or_nothing() {
        let plain = view(
            "Like",
            "plain",
            BOB,
            json!("https://test.example/objects/x"),
            json!({}),
        );
        assert!(
            matches!(like_content(&plain), LikeContent::Plain),
            "no content is a plain favourite"
        );
        let blank = view(
            "Like",
            "blank",
            BOB,
            json!("x"),
            json!({"content": "  <p> </p>"}),
        );
        assert!(
            matches!(like_content(&blank), LikeContent::Plain),
            "a blank content is a plain favourite too"
        );
        let emoji = view(
            "Like",
            "emoji",
            BOB,
            json!("x"),
            json!({"content": "<p>🍺</p>"}),
        );
        let LikeContent::Reaction(reaction) = like_content(&emoji) else {
            panic!("an HTML-wrapped emoji is a reaction");
        };
        assert_eq!(reaction.emoji, "🍺", "the tags and whitespace go");
        assert_eq!(reaction.url, None, "a unicode reaction carries no image");
        let text = view(
            "Like",
            "text",
            BOB,
            json!("x"),
            json!({"content": "<p>thanks!</p>"}),
        );
        assert!(
            matches!(like_content(&text), LikeContent::Unusable),
            "text that names no emoji folds nothing"
        );
    }

    #[test]
    fn an_emoji_react_of_any_fully_qualified_emoji_folds_into_a_reaction() {
        for emoji in ["\u{2B50}", "\u{1F7E5}", "\u{1F7E2}", "\u{231B}", "\u{2764}"] {
            let react = view(
                "EmojiReact",
                "star",
                BOB,
                json!("https://test.example/objects/x"),
                json!({"content": emoji}),
            );
            let content = react.raw.get("content").and_then(Value::as_str);
            let reaction = content
                .and_then(|content| reaction_of(&react.raw, content))
                .expect("the content is one emoji, not an Ignored fold");
            assert_eq!(reaction.emoji, emoji, "the stored spelling is the sender's");
            assert_eq!(reaction.url, None, "a unicode reaction carries no image");
        }
        for text in ["a\u{1F3FB}", "ab", "\u{2B50}\u{2B50}"] {
            assert!(
                reaction_of(&json!({}), text).is_none(),
                "{text:?} is not one emoji"
            );
        }
    }

    #[test]
    fn a_shortcode_reaction_needs_its_emoji_tag() {
        let tagged = json!({
            "content": ":railroad_track:",
            "tag": [
                {"type": "Mention", "href": "https://remote.example/users/carol", "name": "@carol"},
                {"type": "Emoji", "name": ":railroad_track:",
                 "icon": {"type": "Image", "mediaType": "image/png",
                          "url": "https://remote.example/emoji/rail.png"}},
            ],
        });
        let reaction =
            reaction_of(&tagged, ":railroad_track:").expect("the tag vouches for the code");
        assert_eq!(
            reaction.emoji, ":railroad_track:",
            "the stored spelling keeps its colons"
        );
        assert_eq!(
            reaction.url.as_deref(),
            Some("https://remote.example/emoji/rail.png"),
            "the image is the tag's own"
        );

        let pleroma = json!({
            "content": ":blob-cat:",
            "tag": [{"type": "Emoji", "name": "blob-cat",
                     "icon": {"url": "https://remote.example/emoji/blob-cat.png"}}],
        });
        let dashed = reaction_of(&pleroma, ":blob-cat:").expect("a dashed shortcode folds");
        assert_eq!(
            dashed.emoji, ":blob-cat:",
            "the colons stay in the stored text"
        );
        assert_eq!(
            dashed.url.as_deref(),
            Some("https://remote.example/emoji/blob-cat.png"),
            "Pleroma's colon-less tag name still vouches for it"
        );

        let untagged = json!({"content": ":railroad_track:"});
        assert!(
            reaction_of(&untagged, ":railroad_track:").is_none(),
            "a shortcode no Emoji tag carries is refused"
        );
        let mismatched = json!({
            "tag": [{"type": "Emoji", "name": "other",
                     "icon": {"url": "https://remote.example/emoji/other.png"}}],
        });
        assert!(
            reaction_of(&mismatched, ":railroad_track:").is_none(),
            "and a tag naming another shortcode is no voucher"
        );
        for bad in ["", "word", "🙂🙂", "::", ":a b:"] {
            assert!(
                reaction_of(&json!({}), bad).is_none(),
                "{bad:?} is no reaction, tagged or not"
            );
        }
    }
}
