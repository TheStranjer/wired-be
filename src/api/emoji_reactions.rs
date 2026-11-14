//! Emoji reactions over HTTP: reacting to a status, withdrawing a reaction,
//! and reading a status's reactions back (`docs/ARCHITECTURE.md` decision 1).
//!
//! The reference is Pleroma's `EmojiReactionController`
//! (`lib/pleroma/web/pleroma_api/controllers/emoji_reaction_controller.ex`),
//! routed in `lib/pleroma/web/router.ex` as
//!
//! ```text
//! put    "/api/v1/pleroma/statuses/:id/reactions/:emoji"  -> create
//! delete "/api/v1/pleroma/statuses/:id/reactions/:emoji"  -> delete
//! get    "/api/v1/pleroma/statuses/:id/reactions"         -> index
//! get    "/api/v1/pleroma/statuses/:id/reactions/:emoji"  -> index
//! ```
//!
//! over `CommonAPI.react_with_emoji` / `unreact_with_emoji`
//! (`lib/pleroma/web/common_api.ex`). Those two write an `EmojiReact` activity
//! per reaction and an `Undo` per withdrawal and read the fact back out of the
//! object's `reactions` JSONB; here the fact is the `emoji_reactions` table
//! ([`crate::models::emoji_reaction`]) and these four routes are its direct
//! interface. What clients see is Pleroma's: the writes answer the reacted-to
//! [`Status`] entity as the caller sees it, the reads answer Pleroma's reaction
//! array, and the same roll-up rides in every status render as
//! `pleroma.emoji_reactions` (see [`crate::services::render_status`]).
//!
//! # Naming an emoji
//!
//! The `{emoji}` path segment arrives percent-decoded, so a client sends
//! `…/reactions/%F0%9F%91%8D` for a thumbs-up and `…/reactions/:cat:` for a
//! custom one. [`crate::util::emoji`] decides what the *shape* may be — one
//! unicode emoji grapheme, or `:shortcode:` — and a shortcode must name a
//! local, enabled [`CustomEmoji`] row: that is `accepted_emoji`, whose
//! refusal is the 400 `{"error":"Invalid emoji"}` Pleroma's `create` answers
//! for the same case. A unicode reaction is stored as sent and carries no image
//! URL; a custom one is stored with the shortcode text *and* the emoji's image
//! URL, which is what reaches clients as `url` (Pleroma reads it back off the
//! activity's `tag` list and runs it through its media proxy; this instance
//! serves the URL as stored).
//!
//! # Reading reactions
//!
//! Both `GET` routes are Pleroma's `index`: authentication is optional
//! ([`MaybeAuthUser`], with `read:statuses` enforced on a *presented* user
//! token — its `fallback: :proceed_unauthenticated`) and the answer is one
//! object per emoji — `{name, count, me, url, accounts}` — ordered by the
//! emoji's first reaction and then by the emoji text, the same order the status
//! field uses. `…/reactions/{emoji}` narrows that to the one named emoji; an
//! emoji nobody reacted with answers the empty array, as does a status with no
//! reactions at all.
//!
//! Pleroma's `filter_allowed_users` is the other half of the read: reactions of
//! accounts the viewer has blocked are hidden, and so are those of accounts the
//! viewer has muted unless they asked for them with `with_muted`. An emoji
//! whose every reactor is hidden leaves the array rather than appearing with a
//! hole in it, and an anonymous viewer hides nobody — there is nobody to hide
//! from. `count` follows the same rule as Pleroma's, counting the reactors this
//! viewer is actually shown, so `count` and the length of `accounts` always
//! agree.
//!
//! The exclusion is this endpoint's, not the renderer's: the same roll-up
//! reaches `pleroma.emoji_reactions` through
//! [`crate::services::render_status`] unfiltered, because
//! [`emoji_reaction::for_statuses`] is one batched read for a whole page and
//! knows only the viewer's id (for `me`), not their relationships. Pleroma
//! filters its status view with cached block and mute lists; when a ticket
//! preloads those for the renderer, the same narrowing moves there and these
//! reads keep theirs.
//!
//! The status's own visibility is the shared [`status_for_viewer_or_404`] gate
//! on all four routes — a malformed id, a deleted post and a post the viewer may
//! not see are one indistinguishable 404 — and both writes are idempotent on
//! `(account, status, emoji)`: repeating a `PUT` is one row and one answer, and
//! a `DELETE` of a reaction that is not there changes nothing and is answered
//! the unchanged [`Status`] with 200, where Pleroma's `unreact_with_emoji`
//! falls into its error arm.
//!
//! The reaction's telling of the status's author is landed: both writes
//! run [`on_reaction_changed`](crate::services::notify::on_reaction_changed)
//! through this module's `after_reaction_changed` hook, creating the
//! `pleroma:emoji_reaction` notification on the `PUT` and taking it back on
//! the `DELETE`. Neither write federates anything yet: Pleroma's
//! `react_with_emoji` runs the `EmojiReact` activity through its pipeline,
//! which delivers it and broadcasts the `pleroma:emoji_reaction` socket
//! event; both land at the same hook when those tickets arrive.
//!
//! [`status_for_viewer_or_404`]: crate::api::helpers::status_for_viewer_or_404
//! [`CustomEmoji`]: crate::models::custom_emoji::CustomEmoji
//! [`Status`]: crate::entities::status::Status

use std::collections::{HashMap, HashSet};

use axum::{
    Json, Router,
    extract::{Path, State},
    routing::get,
};
use serde::Deserialize;
use sqlx::PgPool;
use tracing::debug;
use uuid::Uuid;

use crate::{
    api::helpers::status_for_viewer_or_404,
    auth::{AuthUser, MaybeAuthUser},
    entities::{
        account::{Account, AccountViewer},
        emoji_reaction::EmojiReaction,
        status::Status,
    },
    error::{AppError, AppResult},
    extract::{ApiParams, flex},
    models::{
        block,
        custom_emoji::{self, CustomEmoji},
        emoji_reaction::{self, EmojiReaction as ReactionRow, NewReaction, ReactorGroup},
        mute,
        status::Status as StatusRow,
        user::{self, User},
    },
    oauth::scopes::{READ_STATUSES, WRITE_STATUSES},
    services::{notify, render_status::render_status},
    state::AppState,
    util::emoji::Emoji,
};

/// 400 body for an `{emoji}` segment that is neither one unicode emoji
/// grapheme nor a shortcode this instance has (Pleroma's `create` refuses the
/// same way).
const INVALID_EMOJI: &str = "Invalid emoji";

/// The `/api/v1/pleroma/statuses` reaction router, merged into
/// [`crate::api::v1`].
///
/// `react` and `unreact` share their path with the single-emoji read, and
/// `list_reactions` answers the same status id without the emoji segment.
pub fn router() -> Router<AppState> {
    Router::new()
        .route("/{id}/reactions", get(list_reactions))
        .route(
            "/{id}/reactions/{emoji}",
            get(list_one_reaction).put(react).delete(unreact),
        )
}

/// The `with_muted` query parameter of both reaction reads (Pleroma's `index`
/// `with_muted`).
#[derive(Debug, Deserialize)]
struct ReactionParams {
    /// Whether reactions from muted accounts are listed; absent means `false`,
    /// which is what [`hidden_reactors`] acts on. Reactions from blocked
    /// accounts are hidden whatever this says.
    #[serde(default, deserialize_with = "flex::opt_bool")]
    with_muted: Option<bool>,
}

impl ReactionParams {
    /// Whether muted reactors stay in the answer: Pleroma's `Map.get(params,
    /// :with_muted, false)`.
    fn muted_visible(self) -> bool {
        self.with_muted.unwrap_or_default()
    }
}

/// A reaction emoji the endpoint accepted, in the form it stores.
#[derive(Debug)]
struct Accepted {
    /// The emoji text as it goes into `emoji_reactions.emoji`: the unicode
    /// grapheme as sent, or the `:shortcode:` text.
    text: String,
    /// The custom emoji's image URL, `None` for a unicode emoji.
    url: Option<String>,
}

impl Accepted {
    /// The model's write of `user_id` reacting to `status_id` with this emoji.
    fn reaction(&self, user_id: Uuid, status_id: Uuid) -> NewReaction<'_> {
        NewReaction {
            user_id,
            status_id,
            emoji: &self.text,
            url: self.url.as_deref(),
        }
    }
}

/// `PUT /api/v1/pleroma/statuses/{id}/reactions/{emoji}` — react to the status
/// with `emoji` and answer the status as the caller sees it, `me` true on their
/// own emoji.
///
/// The write ([`emoji_reaction::react`]) is idempotent on
/// `(account, status, emoji)`, so a repeat reaction is one row that keeps the
/// original's id, activity id and ordering time; only a custom emoji's image URL
/// is refreshed. Pleroma checks the post is visible to the actor
/// (`activity_visible_to_actor`) before it builds the activity; the same check
/// here is [`status_for_viewer_or_404`], which is also what lets an author
/// react to their own post and refuses a reaction to one the caller may not see
/// with the 404.
///
/// # Errors
///
/// [`AppError::Unauthorized`] / [`AppError::Forbidden`] from [`AuthUser`] and
/// the `write:statuses` check; [`AppError::BadRequest`] with `"Invalid emoji"`
/// for an unusable `{emoji}`; [`AppError::NotFound`] for a malformed, unknown,
/// deleted or invisible status, and for the one row the render cannot answer;
/// any database failure arrives through `?` as [`AppError::Database`].
async fn react(
    State(state): State<AppState>,
    auth: AuthUser,
    Path((id, emoji)): Path<(String, String)>,
) -> AppResult<Json<Status>> {
    auth.require(WRITE_STATUSES)?;
    let row = status_for_viewer_or_404(&state, Some(&auth.0), &id).await?;
    let accepted = accepted_emoji(&state, &emoji).await?;
    let stored = emoji_reaction::react(
        &state.db,
        &state.config.base_url,
        &accepted.reaction(auth.0.id, row.id),
    )
    .await?;
    debug!(
        status = %row.id,
        user = %auth.0.id,
        emoji = %stored.emoji,
        "status reacted to"
    );
    after_reaction_changed(&state, &stored, true).await;
    rendered_status(&state, &auth.0, &row).await
}

/// `DELETE /api/v1/pleroma/statuses/{id}/reactions/{emoji}` — withdraw the
/// caller's reaction with `emoji` and answer the status as they see it.
///
/// [`emoji_reaction::unreact`] deletes whatever pair it is given and is content
/// with none, so withdrawing a reaction that was never made (or already
/// withdrawn) changes nothing and is the unchanged [`Status`] with 200. The
/// emoji still has to be one this endpoint can name, so an unusable `{emoji}`
/// is the same 400 [`react`] sends.
///
/// # Errors
///
/// Those of [`react`]. Any database failure arrives through `?` as
/// [`AppError::Database`].
async fn unreact(
    State(state): State<AppState>,
    auth: AuthUser,
    Path((id, emoji)): Path<(String, String)>,
) -> AppResult<Json<Status>> {
    auth.require(WRITE_STATUSES)?;
    let row = status_for_viewer_or_404(&state, Some(&auth.0), &id).await?;
    let accepted = accepted_emoji(&state, &emoji).await?;
    if let Some(removed) =
        emoji_reaction::unreact(&state.db, auth.0.id, row.id, &accepted.text).await?
    {
        debug!(
            status = %row.id,
            user = %auth.0.id,
            emoji = %removed.emoji,
            "reaction withdrawn"
        );
        after_reaction_changed(&state, &removed, false).await;
    }
    rendered_status(&state, &auth.0, &row).await
}

/// `GET /api/v1/pleroma/statuses/{id}/reactions` — every emoji reacted to the
/// status `id`, with the accounts behind each of them.
///
/// # Errors
///
/// [`AppError::Forbidden`] when a presented user token lacks `read:statuses`;
/// [`AppError::Unauthorized`] for a token that does not resolve;
/// [`AppError::NotFound`] for a status this route may not answer;
/// [`AppError::BadRequest`] / [`AppError::Unprocessable`] from [`ApiParams`]
/// for a `with_muted` that is not a boolean. Any database failure arrives
/// through `?` as [`AppError::Database`].
async fn list_reactions(
    State(state): State<AppState>,
    auth: MaybeAuthUser,
    Path(id): Path<String>,
    ApiParams(params): ApiParams<ReactionParams>,
) -> AppResult<Json<Vec<EmojiReaction>>> {
    auth.require_user_scope(READ_STATUSES)?;
    read_reactions(&state, auth.0.as_ref(), &id, None, params.muted_visible()).await
}

/// `GET /api/v1/pleroma/statuses/{id}/reactions/{emoji}` — the same array
/// narrowed to the one emoji the path names.
///
/// The filter is on the emoji text as sent (Pleroma's `filter/2` compares the
/// decoded parameter with the stored name), so an emoji nobody reacted with —
/// usable shape or not — answers `[]`; nothing has to validate the segment to
/// say that, and nothing does.
///
/// # Errors
///
/// Those of [`list_reactions`]. Any database failure arrives through `?` as
/// [`AppError::Database`].
async fn list_one_reaction(
    State(state): State<AppState>,
    auth: MaybeAuthUser,
    Path((id, emoji)): Path<(String, String)>,
    ApiParams(params): ApiParams<ReactionParams>,
) -> AppResult<Json<Vec<EmojiReaction>>> {
    auth.require_user_scope(READ_STATUSES)?;
    read_reactions(
        &state,
        auth.0.as_ref(),
        &id,
        Some(&emoji),
        params.muted_visible(),
    )
    .await
}

/// The body of both reads: the reactions of the status `id` as `viewer` may see
/// them, narrowed to `emoji` (`None`: every emoji), with `muted_visible`
/// deciding whether the viewer's muted accounts stay in.
///
/// # Errors
///
/// [`AppError::NotFound`] from [`status_for_viewer_or_404`]; any database
/// failure arrives through `?` as [`AppError::Database`].
async fn read_reactions(
    state: &AppState,
    viewer: Option<&User>,
    id: &str,
    emoji: Option<&str>,
    muted_visible: bool,
) -> AppResult<Json<Vec<EmojiReaction>>> {
    let row = status_for_viewer_or_404(state, viewer, id).await?;
    let groups = emoji_reaction::reactors(&state.db, row.id, emoji).await?;
    let hidden = hidden_reactors(&state.db, viewer, muted_visible).await?;
    let shown: Vec<ReactorGroup> = groups
        .into_iter()
        .filter_map(|group| visible(group, &hidden))
        .collect();
    let reactors = reactor_rows(&state.db, &shown).await?;
    let listed = entries(state, viewer, shown, &reactors);
    debug!(
        status = %row.id,
        reactions = listed.len(),
        "emoji reactions listed"
    );
    Ok(Json(listed))
}

/// The ids of the reactors `viewer` may not see: the accounts they blocked,
/// plus — unless `muted_visible` — the ones they muted. Pleroma's
/// `filter_allowed_users` excludes exactly these two lists (the viewer's own
/// *outgoing* edges: someone who blocked the viewer still shows up), and an
/// anonymous request has nothing to exclude.
async fn hidden_reactors(
    pool: &PgPool,
    viewer: Option<&User>,
    muted_visible: bool,
) -> AppResult<HashSet<Uuid>> {
    let Some(who) = viewer else {
        return Ok(HashSet::new());
    };
    let blocked = block::blocked_ids(pool, who.id).await?;
    let mut hidden: HashSet<Uuid> = blocked.into_iter().collect();
    if !muted_visible {
        hidden.extend(mute::muted_ids(pool, who.id).await?);
    }
    Ok(hidden)
}

/// `group` with the reactors `hidden` names dropped, and `None` when that leaves
/// nobody: Pleroma's `filter_emoji` answering `nil` for an emoji whose whole
/// reactor list was excluded, which is how such an emoji leaves the array rather
/// than appearing empty.
fn visible(group: ReactorGroup, hidden: &HashSet<Uuid>) -> Option<ReactorGroup> {
    let reactor_ids: Vec<Uuid> = group
        .reactor_ids
        .iter()
        .copied()
        .filter(|id| !hidden.contains(id))
        .collect();
    let shown = !reactor_ids.is_empty();
    // The emoji and its image ride along from the group the model returned;
    // only the reactor list is rewritten.
    shown.then_some(ReactorGroup {
        reactor_ids,
        ..group
    })
}

/// Every account any group still names, keyed by id: one [`user::get_many`] for
/// the whole answer, whatever its shape. A reactor whose account is deactivated
/// is left out here — Pleroma's `fetch_users` keeps only the active ones — which
/// drops it from its emoji's `accounts` and so from its `count` too.
async fn reactor_rows(pool: &PgPool, groups: &[ReactorGroup]) -> AppResult<HashMap<Uuid, User>> {
    let ids: Vec<Uuid> = groups
        .iter()
        .flat_map(|group| group.reactor_ids.iter().copied())
        .collect();
    if ids.is_empty() {
        return Ok(HashMap::new());
    }
    let found = user::get_many(pool, &ids).await?;
    Ok(found
        .into_iter()
        .filter(|account| account.is_active.get())
        .map(|account| (account.id, account))
        .collect())
}

/// The reaction array: one [`EmojiReaction`] per group in the order the model
/// returned them, each group's reactors rendered as [`Account`] entities and
/// `me` answered from `viewer` (an anonymous viewer is never `me`).
///
/// A reactor with no row in `reactors` — deactivated, or gone — is not listed
/// and not counted: the count is of the accounts on the wire.
fn entries(
    state: &AppState,
    viewer: AccountViewer<'_>,
    groups: Vec<ReactorGroup>,
    reactors: &HashMap<Uuid, User>,
) -> Vec<EmojiReaction> {
    let mut listed = Vec::with_capacity(groups.len());
    for group in groups {
        let accounts: Vec<Account> = group
            .reactor_ids
            .iter()
            .filter_map(|id| reactors.get(id))
            .map(|account| Account::from_user_for(viewer, account, &state.config, None))
            .collect();
        let me = viewer.is_some_and(|who| group.reactor_ids.contains(&who.id));
        listed.push(EmojiReaction::new(group.name, group.url, me, accounts));
    }
    listed
}

/// The one status a write answers: the row `viewer` just changed, rendered for
/// them, so a `PUT` reports its own effect through the same
/// `pleroma.emoji_reactions` field any other read of the status uses.
///
/// # Errors
///
/// [`AppError::NotFound`] for the one row the renderer answers `None` for (a
/// reblog whose original went away between the visibility read and the batch
/// load — [`crate::api::statuses`] answers it the same way); any database
/// failure arrives through `?` as [`AppError::Database`].
async fn rendered_status(
    state: &AppState,
    viewer: &User,
    row: &StatusRow,
) -> AppResult<Json<Status>> {
    let entity = render_status(state, Some(viewer), row)
        .await?
        .ok_or(AppError::NotFound)?;
    Ok(Json(entity))
}

/// The `{emoji}` segment as a stored reaction: [`crate::util::emoji`] for the
/// shape, and — for a shortcode — one read of `custom_emojis` to say whether
/// this instance has that emoji and what its image is.
///
/// A disabled emoji is refused: [`custom_emoji`] documents a disabled row as
/// served to nobody, and a reaction naming it would show an image the instance
/// has retired. The stored text is the client's own `:shortcode:`, so the same
/// segment reads the reaction back on `GET …/reactions/{emoji}`.
///
/// # Errors
///
/// [`AppError::BadRequest`] with `"Invalid emoji"` for text that is neither one
/// unicode emoji grapheme nor a shortcode naming a local, enabled emoji; any
/// database failure arrives through `?` as [`AppError::Database`].
async fn accepted_emoji(state: &AppState, text: &str) -> AppResult<Accepted> {
    let Some(classified) = Emoji::classify(text) else {
        return Err(invalid_emoji());
    };
    match classified {
        Emoji::Unicode(_) => Ok(Accepted {
            text: text.to_owned(),
            url: None,
        }),
        Emoji::Custom(shortcode) => local_emoji(&state.db, shortcode)
            .await?
            .map(|emoji| Accepted {
                text: text.to_owned(),
                url: Some(emoji.image_url),
            })
            .ok_or_else(invalid_emoji),
    }
}

/// The 400 both writes send for an `{emoji}` segment they cannot name.
fn invalid_emoji() -> AppError {
    AppError::BadRequest(INVALID_EMOJI.to_owned())
}

/// The local, enabled custom emoji `shortcode` names: [`custom_emoji`]'s batch
/// read asked for exactly one shortcode with a `NULL` domain, so a remote
/// instance's emoji of the same name never answers.
///
/// # Errors
///
/// Any database failure arrives through `?` as [`AppError::Database`].
async fn local_emoji(pool: &PgPool, shortcode: &str) -> AppResult<Option<CustomEmoji>> {
    let wanted = [shortcode.to_owned()];
    let found = custom_emoji::get_by_shortcodes(pool, &wanted, None).await?;
    Ok(found.into_iter().find(|emoji| !emoji.disabled))
}

/// Post-write hook of both reaction routes: the telling of the status's
/// author of a new reaction — and its withdrawal, when the reaction is
/// taken back — runs through here, landed from
/// [`notify::on_reaction_changed`] (Pleroma's `EmojiReact` notification
/// arm, whose potential receiver is the object's actor alone). The inbound
/// `EmojiReact` handler runs it too
/// ([`crate::federation::handlers::interactions`]): a mirrored remote
/// reaction tells the local author exactly as a local one does, and the
/// reaction's `Undo` takes that telling back. The federation ticket that
/// sends an `EmojiReact` activity (and its `Undo`, when a reaction is
/// withdrawn) wires the delivery in here too — the activity to send is
/// [`ReactionRow::ap_id`], the triple it names is [`ReactionRow::user_id`],
/// [`ReactionRow::status_id`] and [`ReactionRow::emoji`], and the queueing
/// happens through `state` (decision 11). The same hook is where Pleroma's
/// `pleroma:emoji_reaction` socket broadcast will land.
///
/// `reacting` states which way the write moved the fact: `true` for the reaction
/// a `PUT` left behind — including a repeat, which restates the same fact
/// because the write is idempotent (and which the telling deduplicates) —
/// and `false` for the `Undo` of the reaction a `DELETE` removed. A
/// withdrawal that found no row to remove never reaches this hook.
///
/// [`notify::on_reaction_changed`]: crate::services::notify::on_reaction_changed
pub(crate) async fn after_reaction_changed(
    state: &AppState,
    reaction: &ReactionRow,
    reacting: bool,
) {
    notify::on_reaction_changed(state, reaction, reacting).await;
}

#[cfg(test)]
mod tests {
    use serde_json::{Value, json};

    use super::*;

    /// The `with_muted` spelling `query` as the endpoint's extractor reads it.
    fn muted_visible(value: Value) -> bool {
        crate::extract::into_params::<ReactionParams>(value)
            .expect("the fixture is a parameters object")
            .muted_visible()
    }

    #[test]
    fn with_muted_defaults_to_hiding_muted_reactors() {
        assert!(
            !muted_visible(json!({})),
            "absent is Pleroma's `false` default"
        );
        assert!(
            !muted_visible(json!({"with_muted": null})),
            "an explicit null is still absent"
        );
        assert!(
            !muted_visible(json!({"with_muted": false})),
            "and a plain false stays false"
        );
    }

    #[test]
    fn with_muted_is_read_in_its_client_spellings() {
        assert!(muted_visible(json!({"with_muted": true})), "a JSON boolean");
        assert!(
            muted_visible(json!({"with_muted": "true"})),
            "the form spelling"
        );
        assert!(
            muted_visible(json!({"with_muted": "1"})),
            "and the numeric one"
        );
    }

    #[test]
    fn an_accepted_emoji_builds_the_write_it_names() {
        let accepted = Accepted {
            text: ":cat:".to_owned(),
            url: Some("https://test.example/emoji/cats/cat.png".to_owned()),
        };
        let (author_id, status_id) = (Uuid::now_v7(), Uuid::now_v7());
        let reaction = accepted.reaction(author_id, status_id);
        assert_eq!(
            (reaction.user_id, reaction.status_id, reaction.emoji),
            (author_id, status_id, ":cat:"),
            "the triple the table keys on"
        );
        assert_eq!(
            reaction.url,
            Some("https://test.example/emoji/cats/cat.png"),
            "and the image the custom emoji brought"
        );
        let thumb = Accepted {
            text: "👍".to_owned(),
            url: None,
        };
        let unicode = thumb.reaction(author_id, status_id);
        assert_eq!(unicode.url, None, "a unicode reaction names no image");
    }

    /// A [`ReactorGroup`] of `ids`, for [`visible`]'s assertions.
    fn group_with(ids: &[Uuid]) -> ReactorGroup {
        ReactorGroup {
            name: "👍".to_owned(),
            url: Some("https://test.example/emoji/cats/cat.png".to_owned()),
            reactor_ids: ids.to_vec(),
        }
    }

    #[test]
    fn hidden_reactors_drop_out_and_an_emptied_emoji_leaves() {
        let (a, b, gone) = (Uuid::now_v7(), Uuid::now_v7(), Uuid::now_v7());
        let hidden: HashSet<Uuid> = std::iter::once(gone).collect();
        assert_eq!(
            visible(group_with(&[a, b, gone]), &hidden).map(|group| group.reactor_ids),
            Some(vec![a, b]),
            "the hidden reactor leaves the list and the order stays"
        );
        let all_hidden: HashSet<Uuid> = [a, b].into_iter().collect();
        assert_eq!(
            visible(group_with(&[a, b]), &all_hidden),
            None,
            "an emoji whose every reactor is hidden is not shown at all"
        );
        let kept = visible(group_with(&[a]), &HashSet::new()).expect("nothing was hidden");
        assert_eq!(
            kept.reactor_ids,
            vec![a],
            "and nothing hidden keeps the list"
        );
        assert_eq!(
            kept.url.as_deref(),
            Some("https://test.example/emoji/cats/cat.png"),
            "the group carries its custom emoji image through the rewrite"
        );
    }

    #[test]
    fn the_endpoint_names_the_util_shapes_and_adds_the_database_half() {
        // `accepted_emoji` is `util::emoji` plus one lookup, so the shapes are
        // the util's: usable text, and everything else the 400 wording.
        assert!(
            matches!(Emoji::classify("👍"), Some(Emoji::Unicode(_))),
            "a unicode emoji needs no lookup"
        );
        assert!(
            matches!(Emoji::classify(":cat:"), Some(Emoji::Custom("cat"))),
            "a shortcode is the half the lookup decides"
        );
        for text in ["", "cat", ":cat", "🙂🙂", "hello", ":unknown*"] {
            assert!(
                Emoji::classify(text).is_none(),
                "{text} is no emoji, so no lookup is even asked"
            );
        }
        assert_eq!(
            INVALID_EMOJI, "Invalid emoji",
            "the message the ticket pins"
        );
    }

    #[test]
    fn an_unusable_emoji_is_a_four_hundred_with_that_message() {
        let err = AppError::BadRequest(INVALID_EMOJI.to_owned());
        assert_eq!(err.to_string(), INVALID_EMOJI, "the body's text");
    }
}
