//! The C2S outbox's relationship arms: the `Follow`, `Like`, `Announce`,
//! `Block` and `Undo` activities
//! [`post_activity`](super::outbox_create::post_activity) folds into the
//! models' own writes — the same ones the Mastodon client API's
//! follow, favourite, reblog, block and undo routes run.

use serde_json::Value;

use crate::{
    api::{
        blocks::after_block_changed,
        statuses::{CANNOT_REBLOG, action_target_id},
    },
    auth::AuthUser,
    error::{AppError, AppResult},
    federation::json as ap_json,
    models::{
        block::{self, Block},
        favourite,
        follow::{self, Follow},
        status::{self, Visibility},
    },
    services::notify,
    state::AppState,
};

use super::UNSUPPORTED_ACTIVITY_TYPE;
use super::c2s_misc::{
    activity_reference, interactable_status, new_activity_id, reblog_target, row_activity_id,
    status_by_reference, stored_actor, user_by_id,
};

/// `Follow` of an actor: the object names the followed account by actor
/// id, resolved among the stored rows (remote mirrors included — this
/// endpoint reads what is stored and does not fetch an unseen actor over
/// the network), and the write is the model's own [`follow::follow`], the
/// very one `POST /api/v1/accounts/{id}/follow` runs: a locked target
/// leaves a `pending` request, a repeat restates the relationship, and
/// the block refusals are the model's. `Location` carries the follow's
/// activity id — the `ap_id` the row keeps (decision 6), which is what
/// an `Undo` of it names.
pub(super) async fn post_follow(
    state: &AppState,
    auth: &AuthUser,
    activity: &Value,
) -> AppResult<String> {
    let reference = activity_reference(activity)?;
    let target = stored_actor(state, &reference).await?;
    let created = follow::follow(&state.db, &state.config.base_url, &auth.0, &target).await?;
    notify::on_follow(state, &auth.0, &target, Some(&created), true).await;
    Ok(row_activity_id(&state.config, created.ap_id.as_deref()))
}

/// `Like` of a status: the favourite, on the same gate and with the same
/// boost-folding as `POST /api/v1/statuses/{id}/favourite` — what the
/// caller may not see is the 404, and a reblog's id likes its original.
/// A repeat is the existing row's activity id with the same `201` (one
/// like, told once: the notification fires only on the write that made
/// the row).
pub(super) async fn post_like(
    state: &AppState,
    auth: &AuthUser,
    activity: &Value,
) -> AppResult<String> {
    let row = interactable_status(state, auth, activity).await?;
    let liked_id = action_target_id(&row);
    let created =
        favourite::favourite(&state.db, &state.config.base_url, auth.0.id, liked_id).await?;
    let stored = match created {
        Some(created) => {
            notify::on_favourite(state, &created, true).await;
            Some(created)
        }
        None => favourite::get(&state.db, auth.0.id, liked_id).await?,
    };
    Ok(row_activity_id(
        &state.config,
        stored.as_ref().and_then(|found| found.ap_id.as_deref()),
    ))
}

/// `Announce` of a status: the reblog at `public` visibility, under the
/// shared gate, the boost-folding ("nobody boosts a boost") and the
/// `private`/`direct` refusal of `POST /api/v1/statuses/{id}/reblog`. A
/// reblog row *is* its `Announce` (decision 6), so `Location` is that
/// row's own `uri` — which is what an `Undo` of the boost names.
pub(super) async fn post_announce(
    state: &AppState,
    auth: &AuthUser,
    activity: &Value,
) -> AppResult<String> {
    let row = interactable_status(state, auth, activity).await?;
    let original = reblog_target(state, &row).await?;
    if matches!(
        original.visibility,
        Visibility::Private | Visibility::Direct
    ) {
        return Err(AppError::Unprocessable(CANNOT_REBLOG.to_owned()));
    }
    let (reblog, created) = status::create_reblog(
        &state.db,
        &state.config.base_url,
        &auth.0,
        &original,
        Visibility::Public,
    )
    .await?;
    if created {
        notify::on_reblog(state, &reblog, true).await;
    }
    Ok(reblog.uri)
}

/// `Block` of an actor: [`block::block`], the same write
/// `POST /api/v1/accounts/{id}/block` runs — idempotent on the pair, and
/// the self-block refusal is the model's. `Location` carries the block's
/// activity id, the `ap_id` the row keeps.
pub(super) async fn post_block(
    state: &AppState,
    auth: &AuthUser,
    activity: &Value,
) -> AppResult<String> {
    let reference = activity_reference(activity)?;
    let target = stored_actor(state, &reference).await?;
    let created = block::block(&state.db, &state.config.base_url, &auth.0, &target).await?;
    after_block_changed(state, &created, true).await;
    Ok(row_activity_id(&state.config, created.ap_id.as_deref()))
}

/// `Undo` of one of those: the withdrawn activity named by id alone (the
/// `Location` the creation answered — the four relationship tables and
/// the reblog rows all key on it, decision 6) or as an embedded activity
/// whose inner type and object name the fact. The withdrawals are the
/// Mastodon routes' own writes, idempotent for the same peace: an `Undo`
/// of something already withdrawn is still accepted. `Location` names the
/// `Undo` itself, a fresh activity id.
pub(super) async fn post_undo(
    state: &AppState,
    auth: &AuthUser,
    activity: &Value,
) -> AppResult<String> {
    match activity.get("object") {
        Some(inner) if inner.is_object() => undo_embedded(state, auth, inner).await,
        Some(object) => {
            let reference = ap_json::value_or_id(object).ok_or(AppError::NotFound)?;
            undo_by_activity_id(state, auth, &reference).await
        }
        None => Err(AppError::NotFound),
    }
}

/// The `Undo` of an embedded activity: the inner `type` says what is
/// withdrawn and the inner `object` names the target — the four types the
/// outbox itself creates, and any other inner type is the 400.
async fn undo_embedded(state: &AppState, auth: &AuthUser, inner: &Value) -> AppResult<String> {
    let reference = inner
        .get("object")
        .and_then(ap_json::value_or_id)
        .ok_or(AppError::NotFound)?;
    match inner.get("type").and_then(ap_json::first_type).as_deref() {
        Some("Follow") => undo_follow_of(state, auth, &reference).await?,
        Some("Like") => undo_like_of(state, auth, &reference).await?,
        Some("Announce") => undo_announce_of(state, auth, &reference).await?,
        Some("Block") => undo_block_of(state, auth, &reference).await?,
        Some(_) | None => {
            return Err(AppError::BadRequest(UNSUPPORTED_ACTIVITY_TYPE.to_owned()));
        }
    }
    Ok(new_activity_id(&state.config))
}

/// An `Undo` naming the withdrawn activity by its id alone: whichever of
/// the caller's own rows carries the id (a `follows`, `favourite` or
/// `block` `ap_id`, or the `uri` of the caller's reblog row — an id
/// somebody else's activity bears names none of theirs, and a plain
/// status `uri` is no activity) is withdrawn, with the same hooks the
/// embedded form runs. Nothing carrying it is the shared 404.
async fn undo_by_activity_id(
    state: &AppState,
    auth: &AuthUser,
    reference: &str,
) -> AppResult<String> {
    if let Some(found) = follow::get_by_ap_id(&state.db, auth.0.id, reference).await? {
        return undo_named_follow(state, auth, &found).await;
    }
    if let Some(found) = favourite::get_by_ap_id(&state.db, auth.0.id, reference).await? {
        if let Some(removed) = favourite::unfavourite(&state.db, auth.0.id, found.status_id).await?
        {
            notify::on_favourite(state, &removed, false).await;
        }
        return Ok(new_activity_id(&state.config));
    }
    if let Some(found) = block::get_by_blocker_and_ap_id(&state.db, auth.0.id, reference).await? {
        return undo_named_block(state, auth, &found).await;
    }
    if let Some(row) = status::get_by_uri(&state.db, reference).await?
        && let Some(original_id) = row.reblog_of_id.filter(|_| row.user_id == auth.0.id)
    {
        if let Some(tombstone) = status::remove_reblog(&state.db, auth.0.id, original_id).await? {
            notify::on_reblog(state, &tombstone, false).await;
        }
        return Ok(new_activity_id(&state.config));
    }
    Err(AppError::NotFound)
}

/// The withdrawal of a follow found by its activity id: the followed
/// account is read off the row, and [`follow::unfollow`] takes it back —
/// the Mastodon route's unfollow ending, where a withdrawal reaches no
/// telling back, so the result needs only binding.
async fn undo_named_follow(state: &AppState, auth: &AuthUser, found: &Follow) -> AppResult<String> {
    let target = user_by_id(state, found.followee_id).await?;
    let _withdrawn = follow::unfollow(&state.db, &auth.0, &target).await?;
    Ok(new_activity_id(&state.config))
}

/// The withdrawal of a block found by its activity id: the blocked
/// account is read off the row, [`block::unblock`] takes the pair back,
/// and the hook fires for the row that went — the Mastodon route's own
/// ending.
async fn undo_named_block(state: &AppState, auth: &AuthUser, found: &Block) -> AppResult<String> {
    let target = user_by_id(state, found.blocked_id).await?;
    if let Some(removed) = block::unblock(&state.db, &auth.0, &target).await? {
        after_block_changed(state, &removed, false).await;
    }
    Ok(new_activity_id(&state.config))
}

/// The `Undo` of an embedded `Follow` naming its target actor.
async fn undo_follow_of(state: &AppState, auth: &AuthUser, reference: &str) -> AppResult<()> {
    let target = stored_actor(state, reference).await?;
    let previous = follow::get(&state.db, auth.0.id, target.id).await?;
    if follow::unfollow(&state.db, &auth.0, &target).await? {
        notify::on_follow(state, &auth.0, &target, previous.as_ref(), false).await;
    }
    Ok(())
}

/// The `Undo` of an embedded `Like` naming its status (or of a
/// withdrawn-by-id `Like`, whose row already names the status).
async fn undo_like_of(state: &AppState, auth: &AuthUser, reference: &str) -> AppResult<()> {
    let row = status_by_reference(state, reference)
        .await?
        .ok_or(AppError::NotFound)?;
    if let Some(removed) =
        favourite::unfavourite(&state.db, auth.0.id, action_target_id(&row)).await?
    {
        notify::on_favourite(state, &removed, false).await;
    }
    Ok(())
}

/// The `Undo` of an embedded `Announce` naming the boosted status: the
/// reblog-row-withdrawal of `POST …/unreblog`, tolerant of a pair with
/// nothing to take back.
async fn undo_announce_of(state: &AppState, auth: &AuthUser, reference: &str) -> AppResult<()> {
    let row = status_by_reference(state, reference)
        .await?
        .ok_or(AppError::NotFound)?;
    if let Some(tombstone) =
        status::remove_reblog(&state.db, auth.0.id, action_target_id(&row)).await?
    {
        notify::on_reblog(state, &tombstone, false).await;
    }
    Ok(())
}

/// The `Undo` of an embedded `Block` naming its target actor.
async fn undo_block_of(state: &AppState, auth: &AuthUser, reference: &str) -> AppResult<()> {
    let target = stored_actor(state, reference).await?;
    if let Some(removed) = block::unblock(&state.db, &auth.0, &target).await? {
        after_block_changed(state, &removed, false).await;
    }
    Ok(())
}
