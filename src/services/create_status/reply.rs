//! The reply arms of `prepared_new_status`.
//!
//! `reply_parent` and `reply_participation` read the parent and the
//! `direct` conversation the body names (a malformed, unknown or unseen
//! one is the shared 404), `apply_conversation_reply` forces the
//! conversation decisions onto the built row, and the pure arms —
//! `inherited_mentions`, `merged_mentions`, `inherited_visibility` with
//! `narrower_visibility` and `visibility_width` — decide what a reply
//! auto-mentions and which audience it lands in.
//!
//! [`check_body`]: crate::services::create_status::check_body

use uuid::Uuid;

use super::{CreateStatusParams, MAX_INHERITED_MENTIONS};
use crate::{
    error::{AppError, AppResult},
    models::{
        conversation, mention,
        status::{self, NewStatus, Visibility},
        status_visibility,
        user::User,
    },
    state::AppState,
    util::id::parse_id,
};

/// The participation the body's `in_reply_to_conversation_id` names: read
/// with the poster as owner, so a malformed id, an id that names no
/// participation and a participation of somebody else's are all one
/// indistinguishable [`AppError::NotFound`] — the route never confirms
/// that a foreign conversation id exists. Absent, null or empty — the
/// same "nothing named" spelling as [`in_reply_to_id`] — is no
/// conversation at all.
///
/// [`in_reply_to_id`]: CreateStatusParams::in_reply_to_id
pub(super) async fn reply_participation(
    state: &AppState,
    poster: &User,
    params: &CreateStatusParams,
) -> AppResult<Option<conversation::Participation>> {
    let Some(text) = params
        .in_reply_to_conversation_id
        .as_deref()
        .filter(|text| !text.trim().is_empty())
    else {
        return Ok(None);
    };
    let Some(id) = parse_id(text) else {
        return Err(AppError::NotFound);
    };
    let found = conversation::get_participation(&state.db, poster.id, id).await?;
    found.map(Some).ok_or(AppError::NotFound)
}

/// The whole `in_reply_to_conversation_id` arm on a built [`NewStatus`]:
/// the post becomes a `direct` answer into `participation`'s thread —
/// visibility forced, the row linked to the conversation for the insert
/// (Pleroma's `make_context(in_reply_to, participation)`, which routes a
/// conversation reply's context through the participation), and the
/// participation's recipients merged into the mentions after whatever the
/// text and the parent arm already carried (Pleroma's addressed-users
/// union — each account linked once, in first-mention order).
pub(super) async fn apply_conversation_reply(
    state: &AppState,
    participation: &conversation::Participation,
    new: &mut NewStatus,
) -> AppResult<()> {
    new.visibility = Visibility::Direct;
    new.direct_conversation_id = Some(participation.conversation_id);
    let addressed = conversation::recipients(&state.db, participation.id).await?;
    new.mentioned_user_ids = merged_mentions(&new.mentioned_user_ids, &addressed);
    Ok(())
}

/// The status the body's `in_reply_to_id` names, read with exactly the
/// [`crate::api::helpers::status_for_viewer_or_404`] rules run against the
/// poster: a malformed, unknown, deleted or poster-invisible parent is the
/// shared [`AppError::NotFound`], so a probe through a reply never learns
/// that somebody's private post exists. A reblog row folds to the original
/// it boosts — Pleroma's `in_reply_to` arm accepts an `Announce` id and the
/// reply lands on the object under it — and the fold needs no second
/// visibility pass because the walk behind [`status_visibility::can_view`]
/// admits a reblog only when its original is admitted too. `None` when the
/// body named no parent at all, or named the empty string: Pleroma's own
/// "not a reply" spelling.
pub(super) async fn reply_parent(
    state: &AppState,
    poster: &User,
    params: &CreateStatusParams,
) -> AppResult<Option<status::Status>> {
    let Some(text) = params
        .in_reply_to_id
        .as_deref()
        .filter(|text| !text.trim().is_empty())
    else {
        return Ok(None);
    };
    let Some(id) = parse_id(text) else {
        return Err(AppError::NotFound);
    };
    let Some(row) = status::get_by_id(&state.db, id).await? else {
        return Err(AppError::NotFound);
    };
    if !status_visibility::can_view(&state.db, &row, Some(poster)).await? {
        return Err(AppError::NotFound);
    }
    let Some(original_id) = row.reblog_of_id else {
        return Ok(Some(row));
    };
    let original = status::get_by_id(&state.db, original_id).await?;
    original.map(Some).ok_or(AppError::NotFound)
}

/// The accounts the parent itself @-mentions, in mention order: the set
/// Pleroma's `get_to_and_cc` carries into a reply's addressing, which here
/// becomes the extra `status_mentions` links the reply stores.
pub(super) async fn parent_mentioned_ids(
    state: &AppState,
    parent_id: Uuid,
) -> AppResult<Vec<Uuid>> {
    let links = mention::for_statuses(&state.db, &[parent_id]).await?;
    Ok(links.get(&parent_id).map_or_else(Vec::new, |users| {
        users.iter().map(|account| account.id).collect()
    }))
}

/// The accounts a reply auto-mentions off its parent: its author first, then
/// the accounts the parent mentioned — the replier never (nobody mentions
/// themselves into their own reply), every account once, at most
/// [`MAX_INHERITED_MENTIONS`] in all. These are link rows only: the status
/// text is not modified and no mention span is inserted.
pub(super) fn inherited_mentions(
    parent_author: Uuid,
    parent_mentions: &[Uuid],
    replier: Uuid,
) -> Vec<Uuid> {
    let mut inherited: Vec<Uuid> = Vec::new();
    for id in std::iter::once(parent_author).chain(parent_mentions.iter().copied()) {
        if id != replier && !inherited.contains(&id) {
            inherited.push(id);
        }
    }
    inherited.truncate(MAX_INHERITED_MENTIONS);
    inherited
}

/// The body's own mentions in text order with the auto-mentions appended:
/// an account the text already mentioned keeps its place and is not linked
/// a second time.
pub(super) fn merged_mentions(own: &[Uuid], inherited: &[Uuid]) -> Vec<Uuid> {
    let mut merged: Vec<Uuid> = own.to_vec();
    for &id in inherited {
        if !merged.contains(&id) {
            merged.push(id);
        }
    }
    merged
}

/// The audience a reply lands in, from the parent's visibility, whether the
/// body *submitted* a `visibility` of its own, and what [`check_body`]
/// decided (the submitted name, or the author's default scope):
///
/// - a `direct` parent forces `direct`, whoever the body asked for;
/// - under a `private` parent, a submitted `public` or `unlisted` comes down
///   to `private`; every other submitted scope stands as asked — Pleroma's
///   own rules keep a submitted `public` under an `unlisted` parent, and
///   `check_body` has already refused any submitted name outside the five;
/// - a body that submitted nothing posts to its author's default scope, but
///   never wider than the parent it answers.
pub(super) fn inherited_visibility(
    parent: Visibility,
    submitted: bool,
    chosen: Visibility,
) -> Visibility {
    if parent == Visibility::Direct {
        return Visibility::Direct;
    }
    if !submitted {
        return narrower_visibility(chosen, parent);
    }
    if parent == Visibility::Private && matches!(chosen, Visibility::Public | Visibility::Unlisted)
    {
        return Visibility::Private;
    }
    chosen
}

/// The narrower of two visibilities, by [`visibility_width`].
const fn narrower_visibility(chosen: Visibility, cap: Visibility) -> Visibility {
    if visibility_width(chosen) <= visibility_width(cap) {
        chosen
    } else {
        cap
    }
}

/// How wide a visibility's audience is, for the reply cap: `direct` reaches
/// the fewest eyes and `public` the most. `local` sits between the federated
/// scopes and `private` — everybody on this instance, nobody off it.
const fn visibility_width(visibility: Visibility) -> u8 {
    match visibility {
        Visibility::Direct => 0,
        Visibility::Private => 1,
        Visibility::Local => 2,
        Visibility::Unlisted => 3,
        Visibility::Public => 4,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    /// The whole visibility matrix of the reply inheritance rule: a
    /// `direct` parent forces `direct`; under a `private` parent a
    /// submitted `public` or `unlisted` comes down while every other
    /// submitted scope stands; a submitted scope under a wider parent
    /// stands untouched; and an unsubmitted one is the author's default
    /// capped at the parent's width, never wider.
    #[test]
    fn the_inherited_visibility_matrix_is_exact() {
        for parent in Visibility::ALL {
            assert_eq!(
                inherited_visibility(Visibility::Direct, true, parent),
                Visibility::Direct,
                "a direct parent forces direct whatever was asked for"
            );
            assert_eq!(
                inherited_visibility(Visibility::Direct, false, parent),
                Visibility::Direct,
                "and whatever the author defaults to"
            );
        }
        for chosen in [Visibility::Public, Visibility::Unlisted] {
            assert_eq!(
                inherited_visibility(Visibility::Private, true, chosen),
                Visibility::Private,
                "a submitted `{chosen}` under a private parent comes down"
            );
        }
        for (chosen, kept) in [
            (Visibility::Private, Visibility::Private),
            (Visibility::Direct, Visibility::Direct),
            (Visibility::Local, Visibility::Local),
        ] {
            assert_eq!(
                inherited_visibility(Visibility::Private, true, chosen),
                kept,
                "every other submitted scope under a private parent stands"
            );
        }
        assert_eq!(
            inherited_visibility(Visibility::Unlisted, true, Visibility::Public),
            Visibility::Public,
            "Pleroma keeps a submitted public under an unlisted parent"
        );
        assert_eq!(
            inherited_visibility(Visibility::Public, true, Visibility::Private),
            Visibility::Private,
            "a narrower submitted scope is never widened by a public parent"
        );
        assert_eq!(
            inherited_visibility(Visibility::Local, true, Visibility::Unlisted),
            Visibility::Unlisted,
            "a submitted scope under a local parent stands as asked"
        );
        // Unsubmitted: the default `chosen` capped at the parent's width.
        for (parent, chosen, expected) in [
            (Visibility::Public, Visibility::Local, Visibility::Local),
            (Visibility::Public, Visibility::Public, Visibility::Public),
            (Visibility::Public, Visibility::Private, Visibility::Private),
            (
                Visibility::Unlisted,
                Visibility::Public,
                Visibility::Unlisted,
            ),
            (Visibility::Private, Visibility::Public, Visibility::Private),
            (Visibility::Private, Visibility::Local, Visibility::Private),
            (Visibility::Local, Visibility::Public, Visibility::Local),
            (Visibility::Local, Visibility::Private, Visibility::Private),
            (
                Visibility::Unlisted,
                Visibility::Private,
                Visibility::Private,
            ),
        ] {
            assert_eq!(
                inherited_visibility(parent, false, chosen),
                expected,
                "a default `{chosen}` replying to `{parent}` is capped to \
                 `{expected}`, never wider than the parent"
            );
        }
    }

    /// The pure half of the auto-mention rule: the parent's author leads,
    /// duplicates fold to their first place, the replier never appears, and
    /// the inherited set is capped.
    #[test]
    fn inherited_mentions_lead_with_the_author_and_drop_the_rest() {
        let author = Uuid::now_v7();
        let first = Uuid::now_v7();
        let second = Uuid::now_v7();
        let replier = Uuid::now_v7();
        assert_eq!(
            inherited_mentions(author, &[first, second], replier),
            vec![author, first, second],
            "the parent's author leads, then its mentions in order"
        );
        assert_eq!(
            inherited_mentions(author, &[author, first, first, second], second),
            vec![author, first],
            "duplicates fold to the first place and the replier drops out"
        );
        assert_eq!(
            inherited_mentions(author, &[first, author], author),
            vec![first],
            "an author replying to their own post is never auto-mentioned, \
             even where the parent mentioned them"
        );
        let crowd: Vec<Uuid> = (0..25).map(|_| Uuid::now_v7()).collect();
        let inherited = inherited_mentions(author, &crowd, replier);
        assert_eq!(
            inherited.len(),
            MAX_INHERITED_MENTIONS,
            "author plus 25 mentions is capped at {MAX_INHERITED_MENTIONS}"
        );
        assert_eq!(inherited[0], author, "and the author leads the cap");
        assert_eq!(
            &inherited[1..],
            &crowd[..MAX_INHERITED_MENTIONS - 1],
            "the first accounts in order win the rest"
        );
    }

    /// The body's own mentions lead and keep their places; the inherited
    /// ones join behind them, each account linked once however both sides
    /// name it.
    #[test]
    fn the_bodies_own_mentions_lead_the_merge() {
        let (a, b, c) = (Uuid::now_v7(), Uuid::now_v7(), Uuid::now_v7());
        assert_eq!(merged_mentions(&[b], &[a, b, c]), vec![b, a, c]);
        assert_eq!(merged_mentions(&[], &[a]), vec![a]);
        assert_eq!(
            merged_mentions(&[a, b], &[]),
            vec![a, b],
            "no parent add nothing"
        );
    }
}
