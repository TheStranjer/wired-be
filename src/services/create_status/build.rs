//! The write's row builder and the shared pre-write half of both create
//! arms.
//!
//! [`render_status_fields`] and its pure half `rendered_fields` turn a
//! checked text into the HTML, mention links and hashtags the status
//! stores, and `prepared_new_status` runs the checks, the reply's parent
//! and conversation participation, the quote target and the render into
//! the [`NewStatus`] the schedule service parks and the creates store.
//!
//! [`AppError::NotFound`]: crate::error::AppError::NotFound
//! [`check_body`]: crate::services::create_status::check_body

use std::collections::HashMap;

use uuid::Uuid;

use super::{
    CreateStatusParams, RenderedFields,
    create::quote_target,
    params::CheckedPoll,
    reply::{
        apply_conversation_reply, inherited_mentions, inherited_visibility, merged_mentions,
        parent_mentioned_ids, reply_parent, reply_participation,
    },
    validate::{Checked, check_body},
};
use crate::{
    error::AppResult,
    models::{oauth_app::OauthApp, status::NewStatus, user::User},
    services::mentions::resolve_mentions,
    state::AppState,
    util::entities::{MentionRef, extract_hashtags, linkify_entities},
    util::format::render_content,
};

/// Renders one status text the way this instance stores it.
///
/// The shared render half of both status writes — [`crate::services::create_status::create_status`]
/// runs it on the checked body's text, [`crate::services::create_status::edit_status`]
/// on the merged one — so an
/// edit linkifies, mention-resolves and tags a post exactly the way posting
/// the same text would. The answer is the HTML with every known
/// `@-mention` and every hashtag linked, the mentioned accounts in mention
/// order, and the hashtags as [`extract_hashtags`] folded them.
///
/// The lookup is [`resolve_mentions`], so a bare `@nick`, a
/// `@nick@our-domain` and a mirrored `@nick@their.domain` are linked from
/// the stored rows, an unknown `@nick@their.domain` is fetched over the
/// network (under that function's shared budget and per-status cap, see
/// [`crate::services::mentions`]) and linked when it answers, while what
/// stays unknown remains the plain text it was written as. Both sides of
/// the map are keyed case-folded, because the text spells the account
/// however its author typed it and the link still carries that spelling.
///
/// # Errors
///
/// Any database failure behind the mention lookups arrives as
/// [`crate::error::AppError::Database`]. A remote lookup's own failures are not errors:
/// those mentions just stay plain text.
pub async fn render_status_fields(
    state: &AppState,
    text: &str,
    content_type: &str,
) -> AppResult<RenderedFields> {
    let found = resolve_mentions(state, text).await?;
    Ok(rendered_fields(state, text, content_type, &found))
}

/// The pure half of [`render_status_fields`]: the same HTML, the same
/// mentioned ids and the same tags, for mentions a caller has already
/// resolved. [`preview_status`] keeps the resolved rows beside the fields
/// (its entity renders the mentioned accounts themselves), so the split is
/// the preview's, not the create's — for `create_status` and `edit_status`
/// this is the step [`render_status_fields`] runs after its lookup.
pub(super) fn rendered_fields(
    state: &AppState,
    text: &str,
    content_type: &str,
    found: &[(MentionRef, User)],
) -> RenderedFields {
    let urls: HashMap<String, String> = found
        .iter()
        .map(|(mention, account)| {
            (
                mention_key(mention),
                account.profile_url_or_ap_id().to_owned(),
            )
        })
        .collect();
    let link = |mention: &MentionRef| urls.get(&mention_key(mention)).cloned();
    let html = render_content(text, content_type);
    RenderedFields {
        content: linkify_entities(&html, &link, &state.config.base_url),
        mentioned_user_ids: found.iter().map(|(_, account)| account.id).collect(),
        hashtags: extract_hashtags(text),
    }
}

/// The lookup key of one mention: the account it names, case-folded — `nick`
/// for a bare reference, `nick@domain` when the text spelled the domain out.
fn mention_key(mention: &MentionRef) -> String {
    mention.acct().to_lowercase()
}

/// [`NewStatus`] of one checked body: a `local` post by `user` with its
/// rendered content and source text, its warning and flags, the two entity
/// lists [`status::insert`] links in its transaction, the attachments
/// [`status::insert`] claims on the same transaction with, the client `app`
/// recorded as the post's application — Pleroma's `put_application`, which
/// is what makes the entity's `application` block come back — and the
/// original [`quote_target`] resolved the body's `quote_id` to, `None` for a
/// post quoting nothing.
fn new_status(
    user: &User,
    app: Option<&OauthApp>,
    checked: &Checked<'_>,
    entities: &RenderedFields,
    quote_of_id: Option<Uuid>,
) -> NewStatus {
    NewStatus {
        user_id: user.id,
        content: entities.content.clone(),
        raw_text: checked.text.to_owned(),
        content_type: checked.content_type.to_owned(),
        spoiler_text: checked.spoiler_text.to_owned(),
        sensitive: checked.sensitive,
        visibility: checked.visibility,
        language: checked.language.map(str::to_owned),
        expires_at: checked.expires_at,
        mentioned_user_ids: entities.mentioned_user_ids.clone(),
        hashtags: entities.hashtags.clone(),
        media_ids: checked.media_ids.clone(),
        local: true,
        quote_of_id,
        application_name: app.map(|found| found.client_name.clone()),
        application_website: app.and_then(|found| found.website.clone()),
        ..NewStatus::default()
    }
}

/// What [`prepared_new_status`] answers: the [`NewStatus`] both create
/// arms store and the checked body's poll beside it — the question names
/// no `NewStatus` column, so it rides out separately for `store_poll`.
/// Factored out because a mixed tuple of row and poll reads better by
/// name (and a bare `Result` of it trips `type_complexity`).
type Prepared = (NewStatus, Option<CheckedPoll>);

/// The shared pre-write half of both create arms and of the schedule
/// service: [`check_body`] over the body, the reply's parent when the body
/// names one, the
/// `in_reply_to_conversation_id` participation when the body names one,
/// the quote target when the body names one,
/// the render through [`render_status_fields`], and the [`NewStatus`] to
/// store with every reply decision already applied — the parent's id,
/// author and thread, the [`inherited_visibility`] audience, the parent's
/// language where the body named none, and the parent's author and
/// mentions auto-linked after the accounts the text itself mentioned
/// (`status_mentions` only; the text is not modified and no mention span
/// is inserted) — and, for a conversation reply, the `direct` visibility
/// forced, the participation's recipients merged into the mentions, and
/// its thread recorded for the insert ([`apply_conversation_reply`]).
/// The checked body's poll comes back beside the row: it belongs to no
/// `NewStatus` column, and both create arms hand it to `store_poll` on
/// the insert's own transaction.
///
/// The parent read happens before the render, like Pleroma's `ActivityDraft`
/// pipeline, where the `in_reply_to` arm runs before `content`: a body with
/// a bad parent and a long text is answered for its parent, but only after
/// [`check_body`] has owed the body its own 422s.
///
/// # Errors
///
/// Those of [`check_body`] and [`render_status_fields`], plus
/// [`AppError::NotFound`] through [`reply_parent`] for an `in_reply_to_id`
/// naming no status the poster may see, plus [`AppError::NotFound`]
/// through [`reply_participation`] for an `in_reply_to_conversation_id`
/// naming no participation the poster owns, plus those of
/// [`quote_target`] for the body's `quote_id`, plus any database failure
/// behind the parent's mention read as [`AppError::Database`].
pub(crate) async fn prepared_new_status(
    state: &AppState,
    user: &User,
    app: Option<&OauthApp>,
    params: &CreateStatusParams,
) -> AppResult<Prepared> {
    let checked = check_body(
        &state.config,
        &user.default_scope,
        user.sensitive_default.get(),
        params,
    )?;
    let parent = reply_parent(state, user, params).await?;
    let participation = reply_participation(state, user, params).await?;
    let quote_of_id = quote_target(state, user, params.quote_id.as_deref()).await?;
    let entities = render_status_fields(state, checked.text, checked.content_type).await?;
    let mut new = new_status(user, app, &checked, &entities, quote_of_id);
    if let Some(parent) = parent {
        let inherited = inherited_mentions(
            parent.user_id,
            &parent_mentioned_ids(state, parent.id).await?,
            user.id,
        );
        new.visibility = inherited_visibility(
            parent.visibility,
            params.visibility.is_some(),
            new.visibility,
        );
        if new.language.is_none() {
            new.language = parent.language.clone();
        }
        new.in_reply_to_id = Some(parent.id);
        new.in_reply_to_account_id = Some(parent.user_id);
        new.conversation_id = Some(parent.conversation_id);
        new.mentioned_user_ids = merged_mentions(&new.mentioned_user_ids, &inherited);
    }
    if let Some(participation) = participation {
        apply_conversation_reply(state, &participation, &mut new).await?;
    }
    Ok((new, checked.poll))
}
