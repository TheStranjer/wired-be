//! The tag pair and the helpers the tag and group writes share: the
//! lowercase merge and subtract, the local-only filter and the group name
//! reader.

use axum::{Json, extract::State, http::StatusCode};
use serde_json::{Value, json};
use tracing::debug;

use super::maintenance::TagsBody;
use super::{AREA_ACCOUNTS, CLASS_WRITE, NO_SUCH_GROUP};
use crate::{
    auth::admin::AdminUser,
    entities::admin_account::AdminAccounts,
    error::{AppError, AppResult},
    extract::ApiParams,
    models::{
        moderation_log,
        user::{User, admin},
    },
    state::AppState,
};

/// The normalized spelling of one submitted tag list: Pleroma's
/// `normalize_tags` (`lib/pleroma/user.ex`), which folds every tag the
/// admin submits to lowercase. The fold is what makes the `mrf_tag:`
/// switches forgiving — [`crate::federation::mrf::tag::Tag`] matches the
/// stored spelling exactly, and its six keys are spelled in lowercase.
pub(super) fn normalized(tags: &[String]) -> Vec<String> {
    tags.iter().map(|tag| tag.to_lowercase()).collect()
}

/// Pleroma's `Enum.uniq((user.tags || []) ++ normalize_tags(tags))`, the
/// `User.tag` write: the stored tags keep their order and their spelling,
/// and every submitted tag the account does not already carry is appended
/// once, in the order it was stated. Adding a tag twice is therefore one
/// entry, which is the pair's idempotence.
pub(super) fn merged_tags(stored: &[String], submitted: &[String]) -> Vec<String> {
    let mut merged = stored.to_vec();
    for tag in normalized(submitted) {
        if !merged.contains(&tag) {
            merged.push(tag);
        }
    }
    merged
}

/// Pleroma's `(user.tags || []) -- normalize_tags(tags)`, the `User.untag`
/// write: the stored tags that the submission does not name, order kept.
/// The comparison is exact, so a tag stored in some other spelling by a
/// hand-written row survives a lowercase removal — and no tag that is not
/// named goes anywhere, which is the pair's other idempotence.
pub(super) fn kept_tags(stored: &[String], submitted: &[String]) -> Vec<String> {
    let gone = normalized(submitted);
    stored
        .iter()
        .filter(|tag| !gone.contains(*tag))
        .cloned()
        .collect()
}

/// The accounts a staff write may move: [`local_only`]'s filter over what
/// one request named, applied to the nickname-list routes (see
/// [`grant_role`]'s refusal of the same account in the singular).
pub(super) fn local_only(found: Vec<User>) -> Vec<User> {
    found
        .into_iter()
        .filter(|account| account.local.get())
        .collect()
}

/// The staff group one `permission_group` segment names, and Pleroma's
/// `No such permission_group` 404 for any other text (its `right_add`,
/// `right_delete` and bulk twins all end in that fallback clause).
///
/// # Errors
///
/// [`AppError::NotFoundMessage`] with [`NO_SUCH_GROUP`].
pub(super) fn group_of(text: &str) -> AppResult<admin::Role> {
    admin::Role::parse(text).ok_or_else(|| AppError::NotFoundMessage(NO_SUCH_GROUP.to_owned()))
}

/// The account one `permission_group` route names: the show's
/// nickname-or-id lookup, remote mirrors included. The read answers a
/// mirror's two flags (both false, and Pleroma's `right_get` looks no
/// further than the row), and the write looks one up precisely so that it
/// can refuse it with its own 400.
///
/// # Errors
///
/// [`AppError::RouteNotFound`] for text that names neither;
/// [`AppError::Database`] from the read.
pub(super) async fn named_account(state: &AppState, raw: &str) -> AppResult<User> {
    admin::get_by_nickname_or_id(&state.db, raw)
        .await?
        .ok_or(AppError::RouteNotFound)
}

/// The role write the four permission-group routes share: the flag lands on
/// every account in `subjects`, one `grant` or `revoke` entry names them,
/// and the answer is Pleroma's `json(conn, update)` — the group's own
/// column, carrying the state it was written to.
///
/// # Errors
///
/// [`AppError::Database`] from the writes and the log.
pub(super) async fn write_role(
    state: &AppState,
    actor: &User,
    subjects: &[User],
    role: admin::Role,
    granted: bool,
) -> AppResult<Json<Value>> {
    for found in subjects {
        admin::set_role(&state.db, found.id, role, granted).await?;
    }
    let logged = if granted {
        moderation_log::grant(&state.db, actor, subjects, role.as_str()).await?
    } else {
        moderation_log::revoke(&state.db, actor, subjects, role.as_str()).await?
    };
    debug!(
        log = %logged.id,
        group = role.as_str(),
        moved = subjects.len(),
        granted,
        "admin changed a staff group"
    );
    Ok(Json(json!({ role.column(): granted })))
}

/// The pool write both suggest routes share: `is_suggested` lands on every
/// named account that answers, one entry names the moved subjects, and the
/// answer is the moved accounts as admin entities, in the order the request
/// stated them.
///
/// # Errors
///
/// [`AppError::Database`] from the lookups, the writes and the log.
pub(super) async fn write_suggestions(
    state: &AppState,
    actor: &User,
    nicknames: &[String],
    suggested: bool,
) -> AppResult<Json<AdminAccounts>> {
    let found = admin::resolve_nicknames(&state.db, nicknames).await?;
    let mut moved = Vec::with_capacity(found.len());
    for account in &found {
        if let Some(updated) = admin::set_suggested(&state.db, account.id, suggested).await? {
            moved.push(updated);
        }
    }
    let logged = if suggested {
        moderation_log::add_suggestion(&state.db, actor, &moved).await?
    } else {
        moderation_log::remove_suggestion(&state.db, actor, &moved).await?
    };
    debug!(
        log = %logged.id,
        moved = moved.len(),
        suggested, "admin changed the suggestion pool"
    );
    Ok(Json(AdminAccounts::from_users(&moved, &state.config)))
}

/// `PUT /api/v1/pleroma/admin/users/tag` — Pleroma's `tag_users`: every
/// account the request names gets the submitted tags appended, deduped and
/// lowercased by [`merged_tags`], and the write logs a `tag` entry naming
/// the accounts it found and the tags as they were spelled.
///
/// A remote mirror is taggable — the `mrf_tag:` switches exist to be put on
/// mirrors — and a handle nobody answers to is skipped as the other list
/// actions skip it, inside the same 204. A tag list that adds nothing still
/// leaves the row alone (no empty write, no `updated_at` churn) and still
/// logs.
///
/// # Errors
///
/// [`AppError::Unauthorized`] / [`AppError::Forbidden`] from [`AdminUser`]
/// and its `admin:write` gate; [`AppError::Unprocessable`] from the
/// extractor for a request that states no `nicknames` or no `tags`;
/// [`AppError::Database`] from the reads, the writes and the log.
pub(super) async fn tag_users(
    State(state): State<AppState>,
    admin: AdminUser,
    params: ApiParams<TagsBody>,
) -> AppResult<StatusCode> {
    admin.require(CLASS_WRITE, AREA_ACCOUNTS)?;
    let subjects = admin::resolve_nicknames(&state.db, &params.0.nicknames).await?;
    for found in &subjects {
        let merged = merged_tags(&found.tags, &params.0.tags);
        if merged != found.tags {
            admin::set_tags(&state.db, found.id, &merged).await?;
        }
    }
    let logged = moderation_log::tag(&state.db, &admin.0, &subjects, &params.0.tags).await?;
    debug!(
        log = %logged.id,
        tagged = subjects.len(),
        "admin tagged users"
    );
    Ok(StatusCode::NO_CONTENT)
}

/// `DELETE /api/v1/pleroma/admin/users/tag` — Pleroma's `untag_users`:
/// [`tag_users`]'s twin, with [`kept_tags`] doing the subtracting and the
/// entry logged as an `untag`.
///
/// # Errors
///
/// As [`tag_users`].
pub(super) async fn untag_users(
    State(state): State<AppState>,
    admin: AdminUser,
    params: ApiParams<TagsBody>,
) -> AppResult<StatusCode> {
    admin.require(CLASS_WRITE, AREA_ACCOUNTS)?;
    let subjects = admin::resolve_nicknames(&state.db, &params.0.nicknames).await?;
    for found in &subjects {
        let kept = kept_tags(&found.tags, &params.0.tags);
        if kept != found.tags {
            admin::set_tags(&state.db, found.id, &kept).await?;
        }
    }
    let logged = moderation_log::untag(&state.db, &admin.0, &subjects, &params.0.tags).await?;
    debug!(
        log = %logged.id,
        untagged = subjects.len(),
        "admin untagged users"
    );
    Ok(StatusCode::NO_CONTENT)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The tag pair's body reads both lists, and a request that states only
    /// one of them is the extractor's 422 (Pleroma's clause requires both).
    #[test]
    fn the_tag_body_requires_both_lists() {
        let both: TagsBody = serde_json::from_value(json!({
            "nicknames": ["alice", "bob"],
            "tags": ["staff", "mrf_tag:sandbox"],
        }))
        .expect("the tag body");
        assert_eq!(both.nicknames, vec!["alice", "bob"], "both handles");
        assert_eq!(
            both.tags,
            vec!["staff", "mrf_tag:sandbox"],
            "an MRF tag is an ordinary string here, and is allowed"
        );
        for missing in [json!({"nicknames": ["alice"]}), json!({"tags": ["x"]})] {
            assert!(
                serde_json::from_value::<TagsBody>(missing.clone()).is_err(),
                "half a request does not parse: {missing}"
            );
        }
    }

    /// One `Vec<String>` from bare text, for the tag tests.
    fn texts(items: &[&str]) -> Vec<String> {
        items.iter().map(|text| (*text).to_owned()).collect()
    }

    /// The tag fold, order and all: the append is Pleroma's
    /// `Enum.uniq(stored ++ normalize_tags(tags))` and the removal its
    /// `stored -- normalize_tags(tags)`, each over the LOWERCASED
    /// submission — which is what makes a tag the `mrf_tag:` switches
    /// actually match, and what makes both writes idempotent.
    #[test]
    fn the_tag_lists_merge_and_subtract_over_the_lowercase_submission() {
        let stored = texts(&["bots", "staff", "MANTAINED"]);
        assert_eq!(
            merged_tags(&stored, &texts(&["vip", "staff"])),
            texts(&["bots", "staff", "MANTAINED", "vip"]),
            "the stored list leads in its own spelling, the new tag follows, the held one is dropped"
        );
        assert_eq!(
            merged_tags(&stored, &texts(&["STAFF", "Staff"])),
            stored,
            "tagging a held tag — twice, in any case — changes nothing"
        );
        assert_eq!(
            merged_tags(&[], &texts(&["MRF_Tag:Sandbox"])),
            texts(&["mrf_tag:sandbox"]),
            "the switch's own spelling, whatever case the admin typed"
        );
        assert_eq!(
            kept_tags(&stored, &texts(&["maintained"])),
            stored,
            "the removal compares the stored spelling exactly, so a tag kept in caps since before this
             writer existed survives a lowercase untag — Pleroma's subtraction, quirk included"
        );
        assert_eq!(
            kept_tags(&stored, &texts(&["bots", "STAFF"])),
            texts(&["MANTAINED"]),
            "every named tag goes, the rest stays in order"
        );
        assert_eq!(
            kept_tags(&stored, &texts(&["nobody"])),
            stored,
            "an unheld tag is subtracted in peace"
        );
        assert_eq!(
            merged_tags(&stored, &[]).len() + kept_tags(&stored, &[]).len(),
            6,
            "an empty tag list writes nothing either way"
        );
    }
}
