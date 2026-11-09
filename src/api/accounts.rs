//! The account endpoints: the single-account reads and listings, the
//! relationship batch, the notes and follow writes, and the profile write.
//!
//! `GET /api/v1/accounts/verify_credentials`, `GET /api/v1/accounts/{id}`,
//! `…/lookup`, `…/relationships` and the relationship listings
//! `…/{id}/followers` and `…/{id}/following`, with the account's status
//! listing `…/{id}/statuses`, are the reads;
//! `PATCH /api/v1/accounts/update_credentials`,
//! `POST /api/v1/accounts/{id}/note`, `…/{id}/remove_from_followers`,
//! `…/{id}/follow`, `…/{id}/unfollow` and `POST /api/v1/follows` are the
//! writes.
//!
//! Pleroma's `AccountController.verify_credentials`, `.show`, `.lookup`,
//! `.relationships`, `.followers`, `.following` and `.statuses`
//! (`lib/pleroma/web/mastodon_api/controllers/account_controller.ex`)
//! are the reference.
//!
//! All of these answer the core [`Account`] entity
//! (decision 3: rendered from the row by [`Account::from_user_for`]); they
//! serve local and remote mirrors alike (decision 7) and treat an
//! account that is unknown, unresolvable or deactivated as a 404 "Record
//! not found" — the answer Pleroma's `visible_for` check renders through
//! its fallback controller.
//!
//! `verify_credentials` is the account's own view of itself: [`AuthUser`]
//! plus the `read:accounts` scope, and the entity rendered with the
//! caller as its [`AccountViewer`], so the owner-only keys of the
//! `pleroma` block are in and nothing is hidden. Its additions over `show`
//! are the [`source`](crate::entities::account::AccountSource) object — the
//! raw note and fields a client edits, the posting defaults, and the size
//! of the pending follow-request queue, the one value here that is not on
//! the row and so arrives from
//! [`pending_count`](crate::models::follow::pending_count) — and the
//! top-level `also_known_as` alias list the account manages through
//! `/api/pleroma/aliases` (see
//! [`crate::api::pleroma_account_settings`]). Pleroma gates the `source`
//! block on the viewer being the account itself
//! (`AccountView.maybe_put_settings`), which is why no other route on this
//! module carries it.
//!
//! `PATCH /api/v1/accounts/update_credentials` (and the `POST` alias older
//! clients send) is Pleroma's `AccountController.update_credentials`
//! (`lib/pleroma/web/mastodon_api/controllers/account_controller.ex`):
//! [`AuthUser`] with the `write:accounts` scope, and only the fields the
//! body actually carries are written — [`MultipartForm`] parses JSON,
//! urlencoded and multipart bodies alike (decision 10) and every parameter
//! here is optional, a JSON `null` counting as absent as in Pleroma's
//! `Enum.filter(fn {_, value} -> not is_nil(value) end)`. The three
//! profile-image parameters are the exception to that rule: Pleroma's
//! `user_image_value` reads their empty string as "reset", so an empty
//! string or a JSON `null` *clears* `avatar`, `header` or
//! `pleroma_background_image` while an absent one keeps the stored image.
//! `display_name`
//! is length-checked against
//! [`max_display_name_chars`](Config::max_display_name_chars) (422 with
//! Mastodon's wording on overflow) and stored through
//! [`sanitize_display_name`]; an empty one is stored empty, not reset to
//! the nickname. `note` is length-checked against
//! [`max_bio_chars`](Config::max_bio_chars), stored verbatim as the row's
//! `raw_bio` and rendered to `bio` by [`text_to_html`] plus
//! [`linkify_entities`] — bare-URL links, hashtag links, and mention
//! links only for mentions naming a local user of this instance (unknown
//! and remote mentions stay plain text). `locked`, `bot` and
//! `discoverable` are [`flex::opt_bool`] booleans; an unrecognized
//! boolean string is the extractor's 422. `fields_attributes` is the
//! profile metadata: a JSON array of `{name, value}` objects, an index
//! map (`{"0": {…}}`) or the form spelling
//! `fields_attributes[0][name]=…` alike — [`index_map_to_vec`] folds all
//! three shapes into the submitted list (see `stored_fields`). An
//! entry whose name and value are both empty is dropped; more than
//! [`max_profile_fields`](Config::max_profile_fields) remaining entries
//! are the 422 "Too many fields", and a name over
//! `MAX_PROFILE_FIELD_NAME_CHARS` or a value over
//! `MAX_PROFILE_FIELD_VALUE_CHARS` characters is a length 422. Two
//! jsonb columns store the pair: `raw_fields` keeps the entries as
//! typed, `fields` the names reduced to plain text and the values
//! rendered by [`text_to_html`] — so a bare URL becomes a link with the
//! standard `rel`. An empty submission clears the metadata. The three
//! profile images — the `avatar`, `header` and `pleroma_background_image`
//! file parts, `users.avatar_url`, `banner_url` and `background_image_url`
//! on this side — are stored through [`crate::uploads`] under a
//! [`media_key`](uploads::media_key) of the caller's id, and their columns
//! hold the [`public_url`](uploads::UploaderBackend::public_url) of what
//! was stored, the address [`crate::api::media_files`] serves the bytes
//! from. What a part's bytes name decides everything: only `png`, `jpeg`,
//! `gif` and `webp` are profile images (any other bytes, and any other
//! type the client's part header claimed, are the 422 "Unsupported file
//! type"), and a part over [`avatar_limit_bytes`](Config::avatar_limit_bytes)
//! for the avatar or [`banner_limit_bytes`](Config::banner_limit_bytes) for
//! the banner and the background is the 422 "File is too large". Every slot
//! is checked before any file is stored, so a body with one good image and
//! one bad one changes nothing; the file a replacement or a clear left
//! behind is removed best-effort once the row stopped naming it.
//! `avatar_description` and `header_description` are accepted and ignored.
//!
//! The posting defaults arrive in Mastodon's `source` block —
//! `source[privacy]`, `source[sensitive]` and `source[language]` as form keys
//! or as the nested JSON object of the same shape — and Pleroma's flat
//! `default_scope` is the other spelling of the privacy one, losing to
//! `source[privacy]` when a body carries both, as in
//! `AccountController.update_credentials`. `privacy` must be one of the four
//! visibilities in `SCOPES` (any other text is the 422 `PRIVACY_INVALID`
//! wording), `sensitive` is a [`flex`] boolean, and `language` is a tag of
//! at most `MAX_LANGUAGE_CHARS` characters whose empty string clears the
//! column.
//! The eleven Pleroma preference flags — `hide_followers`, `hide_follows`,
//! `hide_followers_count`, `hide_follows_count`, `hide_favorites`,
//! `show_role`, `skip_thread_containment`, `allow_following_move`,
//! `accepts_chat_messages`, `no_rich_text` and `show_birthday` — are
//! [`flex::opt_bool`] booleans like the three Mastodon ones. `birthday` is
//! `YYYY-MM-DD` text (the empty string clears it, anything else that is not a
//! date is 422, and a date after today is 422 as Pleroma's age check refuses
//! it). `pleroma_settings_store` is a JSON object — the JSON *text* of one
//! from a form body — merged key by key into the stored `settings_store` and
//! refused with the 422 of
//! [`STORE_TOO_LARGE`](crate::api::pleroma_settings::STORE_TOO_LARGE) when
//! the merged blob passes 1 MiB, the cap
//! `PATCH /api/v1/pleroma/settings/{app}` measures with (see
//! [`check_store_size`]). `actor_type` is `Person` or `Service` (anything
//! else, `Group` included, is 422); it picks the actor kind and the `bot`
//! flag follows it, while `bot` alone picks the kind Pleroma's controller
//! does — `true` a `Service`, `false` a `Person` — so the two columns never
//! disagree.
//!
//! The answer
//! is a 200 [`Account::for_owner`] render — the owner view with its
//! `source` block, the same one `verify_credentials` answers — and every
//! accepted request runs the `after_profile_updated` hook, which the
//! later ticket that federates the actor's `Update` fills in.
//!
//! Authentication on `show` and `lookup` is optional, as in Pleroma's
//! `OAuthScopesPlug … fallback: :proceed_unauthenticated`: the routes
//! answer without a token. A token is not ignored, though: it names the
//! [`AccountViewer`] the entity is
//! rendered for, so the account's own user sees what the row hides from
//! strangers (hidden follow counts, hidden role flags, and the owner-only
//! keys of the `pleroma` block) while anyone else sees the restricted view.
//! An authenticated viewer looking at *another* account also gets their
//! [`Relationship`] embedded in `pleroma.relationship`, preloaded per
//! request; anonymous viewers and self-views keep Pleroma's empty object.
//! The [`MaybeAuthUser`] extractor is attached to both routes, so a request
//! presenting an invalid, expired or deactivated token is answered 401 "The
//! access token is invalid" instead of being silently treated as anonymous.
//!
//! `followers` and `following` are the account's relationship listings,
//! Pleroma's `.followers` and `.following`: [`MaybeAuthUser`] with the
//! `read:accounts` scope enforced on a presented user token
//! ([`require_user_scope`](MaybeAuthUser::require_user_scope)) and
//! [`PageParams`] keyset pagination (default limit 20, at most
//! [`MAX_LIMIT`](crate::pagination::MAX_LIMIT)). They answer [`Account`]
//! entities — the accounts of the *accepted* follows, newest follow first,
//! local and remote alike — and paginate on the `follows` row: the
//! [`Link`](crate::pagination::link_header) header's `max_id`/`min_id`
//! carry [`Follow::id`](crate::models::follow::Follow::id), which is why
//! [`followers_page`](crate::models::follow::followers_page) returns
//! [`FollowPageEntry`](crate::models::follow::FollowPageEntry) rather than
//! bare rows. An authenticated viewer also gets their relationship to
//! every listed account embedded, one batched
//! [`Relationship::load_many`] per request. Privacy per Pleroma's
//! `cond`: an account with `hide_followers` (resp. `hide_follows`) answers
//! the empty array to everyone but itself — the count-hiding flags alone
//! never hide the list — while a locked account's lists stay visible,
//! pending requests excluded, to any viewer.
//!
//! `statuses` is the account's posting history, Pleroma's `.statuses` over
//! `ActivityPub.fetch_user_activities/3`: the same [`MaybeAuthUser`] gate
//! with `read:statuses` instead, the same [`PageParams`] page, and the rows
//! of [`TimelineSource::Account`] read through the shared
//! [`crate::services::timeline`] builder and rendered by the same helper the
//! `/api/v1/timelines/*` routes answer with (see
//! [`timeline_response`]), so the
//! page is the account's own posts *and its reblogs* — each row still judged
//! by the viewer's visibility, and the viewer's own exclusions unless
//! `with_muted`. `pinned`, `only_media`, `exclude_replies`,
//! `exclude_reblogs`, `tagged` and `exclude_visibilities[]` narrow it as
//! Pleroma's `statuses_operation` reads them, `pinned` also taking the page
//! off the pagination keys and dropping the `Link` header with it. A
//! malformed, unknown or deactivated id is the shared 404; a viewer the
//! account blocked sees the empty list; and while `RESTRICT_UNAUTHENTICATED`
//! names the account's profile half (`profiles_local` for a local account,
//! `profiles_remote` for a mirror) an unauthenticated request is the 403
//! `{"error":"This API requires an authenticated user"}` — see
//! [`Config::restricts`].
//!
//! `lookup` resolves its `acct` parameter through
//! [`get_by_acct`](crate::models::user::get_by_acct): a stored nickname,
//! case-insensitively, with this instance's own domain stripped for local
//! accounts. Resolving a handle this instance has never seen is left to
//! the `WebFinger` federation ticket.
//!
//! `relationships` requires auth: [`AuthUser`] plus the `read:follows`
//! scope, exactly Pleroma's plug pipeline for that action
//! (`%{scopes: ["read:follows"]}`). It reads Mastodon's `id[]` array
//! parameter — a single `id=` is accepted too, Pleroma's `List.wrap(id)` —
//! and answers one [`Relationship`] per *known* id in the order requested:
//! values that are not `UUID` text and ids that name no stored user are
//! skipped (as Pleroma's `User.get_all_by_ids` skips them), and an absent,
//! empty or wholly-unknown `id` is the empty array, never an error
//! (Pleroma returns `[]` when the parameter is missing). The flags come
//! from [`Relationship::load_many`], one batched query set per request.
//!
//! `POST /api/v1/accounts/{id}/note` is Pleroma's `AccountController.note`
//! (`lib/pleroma/web/mastodon_api/controllers/account_controller.ex`) over
//! `Pleroma.UserNote` (`lib/pleroma/user_note.ex`): [`AuthUser`] plus the
//! `write:accounts` scope, and the whole answer is the caller's
//! [`Relationship`] to the account with [`note`](Relationship::note)
//! carrying what the write left stored. The `comment` parameter is
//! required, at most 2000 characters (Mastodon's `AccountNote` limit; longer is 422 with Mastodon's changeset wording)
//! and stored verbatim — but an empty or all-whitespace one *deletes* the
//! note (the answered `note` is then `""`), which is how Mastodon clears
//! it too. An unknown, malformed or deactivated account id is the same
//! 404 as every other route here; noting *yourself* is allowed, unlike
//! Pleroma's follow-side checks.
//!
//! `POST /api/v1/accounts/{id}/remove_from_followers` is Pleroma's
//! `AccountController.remove_from_followers` over `User.remove_follower`
//! (`lib/pleroma/web/mastodon_api/controllers/account_controller.ex`,
//! `lib/pleroma/user.ex`): [`AuthUser`] plus the `follow` / `write:follows`
//! scope, and the write takes the account's accepted follow of the caller
//! out of the table through [`remove_follower`](crate::models::follow::remove_follower),
//! counters adjusted in the same transaction. The answer is the caller's
//! [`Relationship`] to the account, so `followed_by` reads `false`
//! afterwards. An account that is not a follower — no row, a `pending`
//! request of its own (those wait on the `/api/v1/follow_requests` answers,
//! this route never touches them), or one the caller follows only — is no
//! error: the unchanged [`Relationship`] answers it with 200. The malformed,
//! unknown or deactivated id is the usual 404, resolved ahead of the guard
//! that answers the caller's *own* id with 422 `{"error":"Can not remove
//! yourself from followers"}`. A follower actually removed is the moment
//! federation will owe that account a `Reject` for its `Follow`; the later
//! federation ticket wires the delivery into this module's
//! `after_follower_removed` hook.
//!
//! `POST /api/v1/accounts/{id}/follow` and `…/unfollow` are Pleroma's
//! `AccountController.follow` and `.unfollow` over `MastodonAPI.follow` /
//! `CommonAPI.unfollow` (`lib/pleroma/web/mastodon_api/controllers/
//! account_controller.ex`, `lib/pleroma/web/mastodon_api/mastodon_api.ex`,
//! `lib/pleroma/user.ex`): [`AuthUser`] plus the `write:follows` / `follow`
//! scope, both running in the limiter's `relations_actions` bucket. Follow
//! writes through [`follow::follow`] and unfollow through
//! [`follow::unfollow`]; both answer the
//! caller's [`Relationship`] to the account, read back off the row so the
//! answer states the stored fact rather than trusting the request. Follow
//! takes the two per-follow toggles through [`ApiParams`] — `reblogs`
//! (default `true`) and `notify` (default `false`), the JSON, form and query
//! spellings alike — and stores the ones the request named with
//! [`set_toggles`](crate::models::follow::set_toggles), which is how a repeat
//! follow of an existing pair updates its flags instead of being a plain
//! no-op; a parameter the body left out keeps what the row holds. Errors:
//! the malformed, unknown or deactivated id is the usual 404, following
//! yourself is the model's 422 `{"error":"Can not follow yourself"}`, and
//! unfollowing an account the caller does not follow is no error at all
//! (where Pleroma's controller answers "Not subscribed!", this ticket is
//! idempotent) — the unchanged [`Relationship`] answers it with 200. Creating
//! or withdrawing a follow is the moment federation will owe the pair a
//! `Follow` or its `Undo`; the later ticket wires that into this module's
//! `after_follow_changed` hook.
//!
//! `POST /api/v1/follows` is Pleroma's `AccountController.follow_by_uri`
//! (`lib/pleroma/web/mastodon_api/controllers/account_controller.ex`): the
//! legacy Mastodon spelling of the follow write, keyed by handle instead of
//! by id. Same gates as `…/{id}/follow` — [`AuthUser`] plus the
//! `write:follows` / `follow` scope, the limiter's `relations_actions`
//! bucket — and the same two optional toggles, arriving beside the required
//! `uri` through [`ApiParams`]. The `uri` names the target three ways: an
//! `acct` handle (`user@domain`, with or without the leading `@`), resolved
//! by [`get_or_fetch_by_acct`], or an actor URL, resolved by
//! [`get_or_fetch_by_ap_id`]; both answer from the stored rows first and
//! reach for `WebFinger` and the
//! actor fetch only for a handle this instance has never seen. What the
//! fetch then does is exactly what `…/{id}/follow` does with its resolved
//! account — the same [`follow::follow`] write, the same toggles, the same
//! `after_follow_changed` hook and the same refusals (a target that resolves
//! to no live account is the shared 404, following yourself the model's
//! 422). The answer is the target's [`Account`] entity rendered for the
//! caller — not the [`Relationship`] the id-keyed route answers — because
//! this legacy endpoint returns the account it followed, with the caller's
//! relationship embedded in `pleroma.relationship`. An absent, blank or
//! `null` `uri` is Mastodon's 400 "Missing parameter: uri".
//!
//! `POST /api/v1/accounts` is open-mode registration: Pleroma's
//! `AccountController.create` (`lib/pleroma/web/mastodon_api/controllers/
//! account_controller.ex`) over `Registration.register_user`
//! (`lib/pleroma/web/registration.ex`). Like Pleroma's `OAuthPlug` the
//! route authenticates with [`AppAuth`] — any valid bearer token,
//! app-only (`client_credentials`) or user-bound — plus the
//! `write:accounts` scope, and a caller who already holds a user token
//! may register another account (Pleroma permits it). The gate behind
//! that: while `REGISTRATIONS_OPEN` is true the route registers, and the
//! body's invite `token` — if it carries one — is optional and ignored,
//! never checked and never spent. While it is false and `INVITES_ENABLED`
//! is true a valid invite `token` is REQUIRED: the unknown, spent,
//! expired or exhausted token (and the absent one) is the 400
//! `{"error":"Invalid token"}`, and the accepted token is consumed in
//! the same transaction that creates the user (see
//! [`crate::models::invite_token`]). An invited user skips approval —
//! moot until the approval-flow ticket, which must keep the invited row
//! active — but not email confirmation. While registrations are closed
//! and invites are disabled the route answers the 403 `{"error":
//! "Registration is disabled"}`; Pleroma's 2.10 `create` action carries
//! no such string (closed registrations there mean "return with an invite
//! token"), so this endpoint's ticket pins the wording. The body is
//! `username`, `email`, `password`, `agreement` (which must be `true`,
//! else 422), `locale`, `reason`, `fullname` (the display name), `bio`
//! and the invite `token`; `locale` and `confirm` are accepted and
//! ignored, as Pleroma reads neither on this path. The user lands through
//! [`create_local_user`]
//! — every validation failure arrives as its 422 whose `error` value is
//! the JSON string of the field map — and `reason` is stored in
//! `users.registration_reason` for the later approval flow. Success mints
//! a fresh user token for the issuing app carrying the presenting
//! token's scopes (Pleroma's `OAuthController.login(user, app, …)`) and
//! answers the token JSON: `token_type`, `access_token`, `scope`,
//! `created_at` (unix seconds), the token row's `id`, `expires_in` and
//! `refresh_token`.

use std::{
    collections::{HashMap, HashSet},
    time::Duration,
};

use axum::{
    Json, Router,
    extract::{OriginalUri, Path, Query, State},
    http::{HeaderMap, Uri},
    routing::{get, patch, post},
};
use bytes::Bytes;
use chrono::{NaiveDate, Utc};
use serde::{Deserialize, Deserializer, Serialize};
use serde_json::{Map, Value, json};
use sqlx::PgPool;
use tracing::{debug, warn};
use url::form_urlencoded;
use uuid::Uuid;

use crate::{
    api::{
        helpers::{
            account_or_404, is_true, normalize_tag, push_visibility, requested_ids,
            timeline_response,
        },
        pleroma_account_settings::looks_like_ap_id,
        pleroma_settings::{check_store_size, shallow_merge},
    },
    auth::{AppAuth, AuthUser, MaybeAuthUser, require_scope},
    config::Config,
    entities::{
        account::{Account, AccountViewer, local_url},
        relationship::{Relationship, RelationshipMap},
        status::Status as StatusEntity,
    },
    error::{AppError, AppResult},
    extract::{
        ApiBody, ApiParams, flex, index_map_to_vec,
        multipart::{MultipartForm, UploadedFile, UploadedFiles as _, upload_body_limit},
    },
    federation::{
        fetch_actor::{get_or_fetch_by_acct, get_or_fetch_by_ap_id},
        webfinger_client::strip_finger_prefixes,
    },
    models::{
        account_note,
        follow::{self, Follow, FollowState, FollowToggles},
        invite_token::{self, InviteToken},
        oauth_app::OauthApp,
        oauth_token::{self, NewToken, OauthToken},
        pinned_status,
        status::{Status as StatusRow, Visibility},
        user::{
            self, DEFAULT_ACTOR_TYPE, Flag, PleromaPrefs, ProfileFields, User,
            register::{RegisterParams, create_local_user},
        },
    },
    oauth::scopes::{
        FOLLOW, READ_ACCOUNTS, READ_FOLLOWS, READ_STATUSES, WRITE_ACCOUNTS, WRITE_FOLLOWS,
    },
    pagination::{PageParams, link_header, set_link_header},
    services::{
        render_status::render_statuses,
        timeline::{TimelineFilters, TimelineQuery, TimelineSource},
    },
    state::AppState,
    uploads,
    util::{
        entities::{MentionRef, extract_mentions, linkify_entities},
        format::text_to_html,
        sanitize::{sanitize_display_name, strip_html},
    },
};

/// The 400 body message when the `acct` query parameter is absent or blank
/// (Mastodon's `ParameterMissing` wording).
const MISSING_ACCT: &str = "Missing parameter: acct";

/// The 400 body message when the `uri` parameter of `POST /api/v1/follows`
/// is absent, blank or `null` (Mastodon's `ParameterMissing` wording).
const MISSING_URI: &str = "Missing parameter: uri";

/// The longest `comment` `POST /api/v1/accounts/{id}/note` accepts
/// (Mastodon's `AccountNote` limit).
const MAX_COMMENT_CHARS: usize = 2000;

/// The longest `fields_attributes` name accepted, in characters
/// (Pleroma's `[:instance, :account_field_name_length]` default, the
/// same cap Mastodon stores); longer is the 422 in [`check_field_len`]'s
/// wording.
const MAX_PROFILE_FIELD_NAME_CHARS: usize = 255;

/// The longest `fields_attributes` value accepted, in characters
/// (Mastodon's `account_field_value_length`); longer is the 422 in
/// [`check_field_len`]'s wording.
const MAX_PROFILE_FIELD_VALUE_CHARS: usize = 2047;

/// The 422 body `PATCH /api/v1/accounts/update_credentials` answers when
/// `fields_attributes` carries more entries than
/// [`max_profile_fields`](Config::max_profile_fields); this endpoint's
/// ticket pins the wording.
const TOO_MANY_FIELDS: &str = "Too many fields";

/// The four `default_scope` values `source[privacy]` accepts: the
/// visibilities new statuses may default to (the same list a scrobble's
/// `visibility` picks from).
const SCOPES: [&str; 4] = ["public", "unlisted", "private", "direct"];

/// The 422 body for a `source[privacy]` (or the flat `default_scope` of the
/// same parameter) outside [`SCOPES`], in the wording the scrobble endpoint
/// uses for its visibility.
const PRIVACY_INVALID: &str = "privacy: is not included in the list";

/// The longest `source[language]` tag accepted, in characters; longer is the
/// 422 in [`merged_language`]'s wording.
const MAX_LANGUAGE_CHARS: usize = 10;

/// The two `actor_type` values `update_credentials` accepts: a human actor
/// and an automated one (`Service` is what Pleroma stores for `bot=true`).
const ACTOR_TYPES: [&str; 2] = [PERSON_ACTOR_TYPE, SERVICE_ACTOR_TYPE];

/// The [`ACTOR_TYPES`] entry of a human actor: the column default, which is
/// what `bot=false` stores.
const PERSON_ACTOR_TYPE: &str = DEFAULT_ACTOR_TYPE;

/// The [`ACTOR_TYPES`] entry of an automated account.
const SERVICE_ACTOR_TYPE: &str = "Service";

/// The 422 body for an `actor_type` outside [`ACTOR_TYPES`].
const ACTOR_TYPE_INVALID: &str = "actor_type: is not included in the list";

/// The 422 body for a `pleroma_settings_store` that is neither a JSON object
/// nor the JSON text of one.
const STORE_NOT_OBJECT: &str = "pleroma_settings_store: expected a JSON object";

/// The 422 body for a `birthday` that is not `YYYY-MM-DD` text.
const BIRTHDAY_INVALID: &str = "birthday: is not a valid date";

/// The 422 body for a `birthday` after today: Pleroma's `validate_min_age`
/// reduced to the one rule this ticket states.
const BIRTHDAY_FUTURE: &str = "birthday: must not be in the future";

/// The format of a `birthday` parameter: an ISO 8601 calendar date, the
/// `YYYY-MM-DD` text this ticket fixes.
const BIRTHDAY_FORMAT: &str = "%Y-%m-%d";

/// The 422 message `POST /api/v1/accounts/{id}/remove_from_followers`
/// answers when the path names the caller: this endpoint's ticket pins the
/// wording for the action.
const SELF_REMOVE: &str = "Can not remove yourself from followers";

/// The form field `PATCH /api/v1/accounts/update_credentials` reads the
/// avatar image from (Mastodon's and Pleroma's `avatar`).
const AVATAR_FIELD: &str = "avatar";

/// The field the profile banner arrives under: Mastodon calls it `header`,
/// Pleroma stores it in the `banner` column.
const HEADER_FIELD: &str = "header";

/// The field Pleroma's profile background image arrives under.
const BACKGROUND_FIELD: &str = "pleroma_background_image";

/// The 422 body for a profile-image part whose bytes are none of
/// [`ACCEPTED_IMAGE_TYPES`] — the same wording the media upload uses for
/// bytes it cannot name an accepted type for.
const UNSUPPORTED_FILE_TYPE: &str = "Unsupported file type";

/// The 422 body for a profile-image part over its configured byte limit
/// ([`avatar_limit_bytes`](Config::avatar_limit_bytes) for the avatar,
/// [`banner_limit_bytes`](Config::banner_limit_bytes) for the banner and the
/// background); Pleroma's `update_credentials` wording, which this
/// endpoint's ticket pins to 422 rather than its 413.
const FILE_TOO_LARGE: &str = "File is too large";

/// The media types a profile image may be, as the MIME types [`infer`]
/// reads from the bytes: the four the ticket names (Pleroma accepts these
/// and nothing else for `avatar`, `header` and `pleroma_background_image`).
const ACCEPTED_IMAGE_TYPES: [&str; 4] = ["image/png", "image/jpeg", "image/gif", "image/webp"];

/// The `Link` header's query parameters carry nothing but the cursor: the
/// relationship listings have no filter to repeat on either link (the
/// scrobbles listing's constant, same rule).
const NO_EXTRA_QUERY: [(String, String); 0] = [];

/// The 403 body `GET /api/v1/accounts/{id}/statuses` answers an anonymous
/// request while the instance keeps the account's profile area restricted
/// (Pleroma's `EnsureAuthenticatedPlug` wording, the one [`crate::auth`]
/// renders for an app-only token and [`crate::api::feeds`] for its feeds).
const USER_REQUIRED: &str = "This API requires an authenticated user";

/// The `RESTRICT_UNAUTHENTICATED` area of this instance's own accounts'
/// profile reads (Pleroma's `profiles: %{local: …}`).
const PROFILES_LOCAL: &str = "profiles_local";

/// The [`PROFILES_LOCAL`] twin for a mirrored account of another instance
/// (Pleroma's `profiles: %{remote: …}`).
const PROFILES_REMOTE: &str = "profiles_remote";

/// `token_type` of the registration answer: issued tokens are bearer
/// credentials, as on `/oauth/token` (see [`crate::api::oauth`]).
const TOKEN_TYPE_BEARER: &str = "Bearer";

/// The 403 body message `POST /api/v1/accounts` answers while
/// `REGISTRATIONS_OPEN` is false and `INVITES_ENABLED` is false. Pleroma's
/// 2.10 `create` action has no such string (closed registrations there route
/// to invite tokens), so this endpoint's ticket pins this wording.
const REGISTRATIONS_CLOSED: &str = "Registration is disabled";
/// The 400 body message `POST /api/v1/accounts` answers while
/// `REGISTRATIONS_OPEN` is false and `INVITES_ENABLED` is true and the
/// submitted invite `token` is absent, unknown, spent, expired or
/// exhausted (Pleroma's `Registration.create_user_with_invite` wording;
/// this endpoint's ticket pins the one message for every refusal).
const INVALID_TOKEN: &str = "Invalid token";

/// The 422 body message `POST /api/v1/accounts` answers when `agreement`
/// is present but not true (Mastodon's signup terms wording; Pleroma's
/// schema requires only that the field is there).
const AGREEMENT_REQUIRED: &str = "You have to accept the terms to sign up";

/// The `/api/v1/accounts` router, merged into [`crate::api::v1`].
///
/// [`state.config`](AppState::config) is what [`upload_body_limit`] sizes
/// the routes' body limit from: `update_credentials` takes the three
/// profile images as multipart file parts, and axum's multipart extractor
/// refuses a body over its 2 MiB default unless the route raises that —
/// which for an avatar capped at [`DEFAULT_AVATAR_LIMIT_BYTES`] it must,
/// or a legal upload would never reach the handler.
///
/// `POST /accounts` runs in the limiter's `app_account_creation` bucket
/// (Pleroma throttles registration there; see [`crate::rate_limit`]), so
/// the whole `state` rides along for the middleware layer. The follow
/// routes (`POST /accounts/{id}/follow`, `…/unfollow` and `POST /follows`)
/// run in `relations_actions`, the bucket Pleroma throttles every
/// relationship write through.
///
/// [`DEFAULT_AVATAR_LIMIT_BYTES`]: crate::config::DEFAULT_AVATAR_LIMIT_BYTES
pub fn router(state: &AppState) -> Router<AppState> {
    Router::new()
        .route(
            "/accounts",
            post(create_account)
                .layer(crate::rate_limit::rate_limit(state, "app_account_creation")),
        )
        .route(
            "/follows",
            post(follow_by_uri).layer(crate::rate_limit::rate_limit(state, "relations_actions")),
        )
        .route("/accounts/verify_credentials", get(verify_credentials))
        .route(
            "/accounts/update_credentials",
            patch(update_credentials).post(update_credentials),
        )
        .route("/accounts/relationships", get(relationships))
        .route("/accounts/lookup", get(lookup_account))
        .route("/accounts/{id}/followers", get(list_followers))
        .route("/accounts/{id}/following", get(list_following))
        .route("/accounts/{id}/statuses", get(list_statuses))
        .route("/accounts/{id}/note", post(set_note))
        .route(
            "/accounts/{id}/follow",
            post(follow_account).layer(crate::rate_limit::rate_limit(state, "relations_actions")),
        )
        .route(
            "/accounts/{id}/unfollow",
            post(unfollow_account).layer(crate::rate_limit::rate_limit(state, "relations_actions")),
        )
        .route(
            "/accounts/{id}/remove_from_followers",
            post(remove_from_followers),
        )
        .route("/accounts/{id}", get(show_account))
        .layer(upload_body_limit(&state.config))
}

/// The request parameters of `POST /api/v1/accounts`, Pleroma's
/// `AccountCreateRequest` (`lib/pleroma/web/api_spec/operations/
/// account_operation.ex`) minus the captcha fields: `username`, `email`,
/// `password` and `agreement` are read, the first three required and
/// `agreement` required too (an absent one is the extractor's 422).
/// `agreement` deserializes through [`flex::bool`] so the form spelling
/// `true` arrives. The invite `token` is read as well — required and
/// checked while registrations are closed and invites are enabled,
/// optional and ignored otherwise (see [`load_invite`]). Every other key
/// the body carries — `locale`, `confirm`, the captcha pair, Pleroma's
/// own `birthday` — is accepted and ignored, as
/// [`Registration.register_user`] reads none of them on this path;
/// `reason`, `fullname` and `bio` are the optional registration facts
/// that do reach the row.
///
/// [`Registration.register_user`]: https://hexdocs.pm/pleroma/Pleroma.Web.Registration.html
#[derive(Debug, Deserialize)]
struct CreateAccountParams {
    /// The desired handle, validated by [`create_local_user`] as
    /// `RegisterParams::nickname`.
    username: String,
    /// Contact email; required unless
    /// [`accounts_email_optional`](crate::config::Config::accounts_email_optional).
    #[serde(default)]
    email: Option<String>,
    /// Plaintext password, stored only as a hash.
    password: String,
    /// Whether the applicant accepts the instance's terms; must be true.
    #[serde(deserialize_with = "flex::bool")]
    agreement: bool,
    /// The registration invite Pleroma's `token` parameter carries: read
    /// only through [`load_invite`]'s gate, and consumed by
    /// [`create_local_user`] in the user's own insert transaction.
    #[serde(default)]
    token: Option<String>,
    /// Free-text reason for the later approval flow, stored in
    /// `users.registration_reason`.
    #[serde(default)]
    reason: Option<String>,
    /// Display name (Pleroma's `fullname`); the nickname stands in when
    /// absent or blank.
    #[serde(default)]
    fullname: Option<String>,
    /// Profile note as source text, stored as `raw_bio` and rendered for
    /// `bio`.
    #[serde(default)]
    bio: Option<String>,
}

impl CreateAccountParams {
    /// The registration [`RegisterParams`] these body parameters claim:
    /// `fullname` is Pleroma's display `name` fallback to the username,
    /// and `reason` rides along for storage.
    fn into_register_params(self) -> RegisterParams {
        RegisterParams {
            nickname: self.username,
            email: self.email,
            password: self.password,
            name: self.fullname,
            bio: self.bio,
            reason: self.reason,
        }
    }
}

/// The 200 body of a successful registration: the freshly minted user
/// token in the shape Pleroma's "token.json" view renders on this
/// endpoint (its `me` key belongs to `/oauth/token`; the registration
/// ticket's response does not carry it).
#[derive(Debug, Serialize)]
struct RegistrationToken {
    /// Token type, always `Bearer`.
    token_type: &'static str,
    /// The access token, to present as `Authorization: Bearer`.
    access_token: String,
    /// The granted scopes, space-separated — the presenting token's.
    scope: String,
    /// Issue time, in unix seconds (Pleroma's `format_created_at`).
    created_at: i64,
    /// The token row's `UUIDv7` id, hyphenated (decision 4).
    id: String,
    /// Seconds of validity left at render time.
    expires_in: i64,
    /// The refresh token, to exchange for a fresh pair.
    refresh_token: String,
}

impl RegistrationToken {
    /// Renders the freshly minted `token` (always user-bound here, so
    /// `refresh_token` is always in).
    fn new(token: OauthToken) -> Self {
        Self {
            token_type: TOKEN_TYPE_BEARER,
            access_token: token.token,
            scope: token.scopes,
            created_at: token.inserted_at.timestamp(),
            id: token.id.to_string(),
            expires_in: token
                .valid_until
                .signed_duration_since(Utc::now())
                .num_seconds(),
            refresh_token: token.refresh_token,
        }
    }
}

/// `POST /api/v1/accounts` — register a local account and receive a
/// bearer token for it (Pleroma's `.create`, which ends in
/// `OAuthController.login(user, app, …)` + the token view).
///
/// The gates run in Pleroma's plug order: [`AppAuth`] first rejects a
/// missing or invalid credential (401) — an app-only `client_credentials`
/// token satisfies it exactly as a user token does — then the
/// `write:accounts` scope check (403), then the registration/invite gate
/// (403 [`REGISTRATIONS_CLOSED`] or the invite check, [`load_invite`]),
/// then the agreement (422). The account itself is built by
/// [`create_local_user`], whose errors this endpoint's ticket maps as
/// implemented there: a 422 whose `error` value is the JSON string of the
/// field map; an invited registration spends its token inside that same
/// write's transaction. Success mints the new user's token for the
/// presenting token's app with the presenting token's scopes, valid for
/// `OAUTH_TOKEN_TTL_SECS`.
///
/// # Errors
///
/// [`AppError::Unauthorized`] from [`AppAuth`] without a valid token;
/// [`AppError::Forbidden`] from [`require_scope`] without `write:accounts`
/// or while registrations are closed without invites;
/// [`AppError::BadRequest`] from [`load_invite`] for a refused invite;
/// [`AppError::Unprocessable`] when `agreement` is absent (the
/// extractor's) or not true, and [`create_local_user`]'s field-map 422s
/// for every invalid parameter. Any database or key-generation failure
/// arrives through `?` as [`AppError::Database`] or [`AppError::Internal`].
async fn create_account(
    State(state): State<AppState>,
    AppAuth(app, token): AppAuth,
    ApiBody(params): ApiBody<CreateAccountParams>,
) -> AppResult<Json<RegistrationToken>> {
    require_scope(&token, WRITE_ACCOUNTS)?;
    let invite = load_invite(&state, params.token.as_deref()).await?;
    if !params.agreement {
        return Err(AppError::Unprocessable(AGREEMENT_REQUIRED.to_owned()));
    }
    let user = create_local_user(&state, params.into_register_params(), invite.as_ref()).await?;
    let issued = issue_registration_token(&state, &app, &token.scopes, user.id).await?;
    debug!(
        user = %user.id,
        app = %app.id,
        invited = invite.is_some(),
        "registered a new local account"
    );
    Ok(Json(RegistrationToken::new(issued)))
}

/// The invite the registration gate hands the user-creation write, or the
/// refusal it owes: with `REGISTRATIONS_OPEN` the invite path is off and
/// the submitted `token` is optional and ignored — `Ok(None)`, whatever
/// it holds, and it is never checked or spent; with registrations closed
/// and `INVITES_ENABLED` off nobody registers — 403
/// [`REGISTRATIONS_CLOSED`]; with registrations closed and invites on a
/// valid invite is REQUIRED — the absent or blank text, an unknown token,
/// and a row [`is_valid`] denies on today's date (spent, expired,
/// exhausted) all answer the 400 [`INVALID_TOKEN`], this endpoint's
/// ticket pinning one message for every refusal.
///
/// [`is_valid`]: InviteToken::is_valid
///
/// # Errors
///
/// [`AppError::Forbidden`] while closed without invites;
/// [`AppError::BadRequest`] for every refused invite; any database
/// failure arrives through `?` as [`AppError::Database`].
async fn load_invite(state: &AppState, submitted: Option<&str>) -> AppResult<Option<InviteToken>> {
    if state.config.registrations_open.get() {
        return Ok(None);
    }
    if !state.config.invites_enabled.get() {
        return Err(AppError::Forbidden(REGISTRATIONS_CLOSED.to_owned()));
    }
    let invalid = || AppError::BadRequest(INVALID_TOKEN.to_owned());
    let Some(text) = submitted.map(str::trim).filter(|text| !text.is_empty()) else {
        return Err(invalid());
    };
    let Some(invite) = invite_token::get_by_token(&state.db, text).await? else {
        return Err(invalid());
    };
    if !invite.is_valid(Utc::now().date_naive()) {
        return Err(invalid());
    }
    Ok(Some(invite))
}

/// Mints the new user's bearer token: the presenting token's scopes on
/// the issuing application, valid for the configured
/// `OAUTH_TOKEN_TTL_SECS` (the `/oauth/token` grant's
/// `issue_token`, with the scopes fixed by the presenting credential
/// rather than requested).
///
/// # Errors
///
/// Any database failure arrives through `?` as [`AppError::Database`].
async fn issue_registration_token(
    state: &AppState,
    app: &OauthApp,
    scopes: &str,
    user_id: Uuid,
) -> AppResult<OauthToken> {
    // `OAUTH_TOKEN_TTL_SECS` is a `usize`; on any target where it could
    // exceed `u64::MAX` the absurd upper bound fails the insert instead
    // of silently shortening the lifetime.
    let ttl_secs = u64::try_from(state.config.oauth_token_ttl_secs).unwrap_or(u64::MAX);
    oauth_token::create_token(
        &state.db,
        &NewToken {
            user_id: Some(user_id),
            app_id: app.id,
            scopes: scopes.to_owned(),
            ttl: Some(Duration::from_secs(ttl_secs)),
        },
    )
    .await
}

/// `GET /api/v1/accounts/verify_credentials` — the caller's own account,
/// rendered as they see it: the owner view of [`Account::from_user_for`]
/// (hidden counters visible, the owner-only `pleroma` keys present) with
/// the `source` block [`Account::for_owner`] attaches.
///
/// The only thing that block needs beyond the token's row is the pending
/// follow-request queue, counted by
/// [`pending_count`](crate::models::follow::pending_count).
///
/// # Errors
///
/// [`AppError::Unauthorized`] from [`AuthUser`] without a valid user
/// token; [`AppError::Forbidden`] when the token is app-only or does not
/// grant `read:accounts`. Any database failure arrives through `?` as
/// [`AppError::Database`].
async fn verify_credentials(
    State(state): State<AppState>,
    auth: AuthUser,
) -> AppResult<Json<Account>> {
    auth.require(READ_ACCOUNTS)?;
    owner_view(&state, &auth.0).await
}

/// The owner render both credentials endpoints answer — the self view of
/// [`Account::for_owner`] with its [`source`](Account::source) block (the
/// pending queue counted by
/// [`pending_count`](crate::models::follow::pending_count) included) and a
/// stored move target attached as the nested `moved` entity (see
/// [`with_moved`]).
///
/// # Errors
///
/// Any database failure arrives through `?` as [`AppError::Database`].
async fn owner_view(state: &AppState, owner: &User) -> AppResult<Json<Account>> {
    let requests = follow::pending_count(&state.db, owner.id).await?;
    let account = Account::for_owner(owner, &state.config, requests);
    Ok(Json(with_moved(state, account, owner).await?))
}

/// Resolves [`moved_to_ap_id`](User::moved_to_ap_id) for one rendered
/// [`Account`]: when the row names a move target this instance has a row
/// for, [`Account::with_moved`] nests the target's plain entity. A move
/// to an actor id not mirrored here keeps `moved: null` (the alias and
/// move flows are what create that row; federation of a remote `Move`
/// arrives with a later ticket).
///
/// # Errors
///
/// Any database failure arrives through `?` as [`AppError::Database`].
async fn with_moved(state: &AppState, account: Account, user: &User) -> AppResult<Account> {
    let Some(ap_id) = user.moved_to_ap_id.as_deref() else {
        return Ok(account);
    };
    Ok(match user::get_by_ap_id(&state.db, ap_id).await? {
        Some(target) => account.with_moved(&target, &state.config),
        None => account,
    })
}

/// The request parameters of `PATCH /api/v1/accounts/update_credentials`.
///
/// Every field is optional and only the ones present in the body are
/// written; a JSON `null` reads as absent (Pleroma drops nil-valued
/// parameters before the changeset). The booleans arrive through
/// [`flex::opt_bool`], so a form's `"true"`/`"on"`/`"1"` spells the same
/// as JSON `true`, and an unknown string is the extractor's 422. That
/// covers the eleven Pleroma preference flags and `source[sensitive]` as
/// well as the three Mastodon booleans.
///
/// The `source` block is [`SourceParams`], read from the form's
/// `source[privacy]=…` keys or the JSON object of the same shape;
/// `default_scope` is Pleroma's flat spelling of `source[privacy]` and
/// loses to it when a body carries both. `birthday` is `YYYY-MM-DD` text
/// ([`merged_birthday`]), `pleroma_settings_store` the JSON object — or,
/// from a form, the JSON *text* of one — that merges into the stored
/// settings blob ([`merged_store`]), and `actor_type` the actor kind
/// [`merged_actor`] pairs with `bot`.
///
/// The three profile images are the exception to the `null` rule, and so are
/// read through [`kept_value`] as raw [`Value`]s: Pleroma's
/// `user_image_value` treats their empty string as the reset, so an
/// *absent* parameter keeps the stored image while `null` or `""` clears it
/// (see [`resolve_images`]). `avatar_description` and `header_description`
/// are not fields of this shape on purpose: serde drops unknown keys, so
/// both are accepted and ignored, as this endpoint's ticket asks.
#[derive(Debug, Deserialize)]
struct ProfileParams {
    /// The display name to store: at most
    /// [`max_display_name_chars`](Config::max_display_name_chars)
    /// characters, then reduced to one line of plain text (see
    /// [`sanitized_name`]).
    #[serde(default)]
    display_name: Option<String>,
    /// The profile note: source text for `raw_bio`, at most
    /// [`max_bio_chars`](Config::max_bio_chars) characters, rendered to
    /// `bio` (see [`render_note`]).
    #[serde(default)]
    note: Option<String>,
    /// Whether follow requests need manual approval.
    #[serde(default, deserialize_with = "flex::opt_bool")]
    locked: Option<bool>,
    /// Whether the account is flagged as automated; the other half of the
    /// pair [`merged_actor`] writes.
    #[serde(default, deserialize_with = "flex::opt_bool")]
    bot: Option<bool>,
    /// Whether the account opts into discovery.
    #[serde(default, deserialize_with = "flex::opt_bool")]
    discoverable: Option<bool>,
    /// The `source` block of Mastodon's account entity, sent as
    /// `source[privacy]`/`source[sensitive]`/`source[language]` form keys or
    /// as the nested JSON object of the same shape (see [`SourceParams`]).
    #[serde(default)]
    source: Option<SourceParams>,
    /// Pleroma's flat spelling of `source[privacy]`: the visibility new
    /// statuses default to, checked against [`SCOPES`].
    #[serde(default)]
    default_scope: Option<String>,
    /// Pleroma's `pleroma_settings_store`: a JSON object (or its JSON text,
    /// from a form) merged key by key into the stored settings blob.
    #[serde(default)]
    pleroma_settings_store: Option<Value>,
    /// The `ActivityPub` actor kind, one of [`ACTOR_TYPES`]; wins over
    /// [`bot`](Self::bot) and decides it (see [`merged_actor`]).
    #[serde(default)]
    actor_type: Option<String>,
    /// The account's birthday as `YYYY-MM-DD` text, or the empty string to
    /// clear it (see [`merged_birthday`]).
    #[serde(default)]
    birthday: Option<String>,
    /// Whether the follower collection is hidden from other accounts.
    #[serde(default, deserialize_with = "flex::opt_bool")]
    hide_followers: Option<bool>,
    /// Whether the follow collection is hidden from other accounts.
    #[serde(default, deserialize_with = "flex::opt_bool")]
    hide_follows: Option<bool>,
    /// Whether the follower count renders as zero for others.
    #[serde(default, deserialize_with = "flex::opt_bool")]
    hide_followers_count: Option<bool>,
    /// Whether the follow count renders as zero for others.
    #[serde(default, deserialize_with = "flex::opt_bool")]
    hide_follows_count: Option<bool>,
    /// Whether the favourites list is private to the account.
    #[serde(default, deserialize_with = "flex::opt_bool")]
    hide_favorites: Option<bool>,
    /// Whether the admin/moderator flags may be shown to others.
    #[serde(default, deserialize_with = "flex::opt_bool")]
    show_role: Option<bool>,
    /// Whether this account's statuses skip thread containment.
    #[serde(default, deserialize_with = "flex::opt_bool")]
    skip_thread_containment: Option<bool>,
    /// Whether a `Move` of the account may take the following list with it.
    #[serde(default, deserialize_with = "flex::opt_bool")]
    allow_following_move: Option<bool>,
    /// Whether the account accepts chat messages.
    #[serde(default, deserialize_with = "flex::opt_bool")]
    accepts_chat_messages: Option<bool>,
    /// Whether clients should render this account's content as plain text.
    #[serde(default, deserialize_with = "flex::opt_bool")]
    no_rich_text: Option<bool>,
    /// Whether strangers may see the stored birthday.
    #[serde(default, deserialize_with = "flex::opt_bool")]
    show_birthday: Option<bool>,
    /// The profile metadata: Mastodon's `fields_attributes`, a JSON array
    /// of `{name, value}` objects, an object keyed by index or the
    /// `fields_attributes[0][name]=…` spelling of form and multipart
    /// bodies alike — read as `Value` so every shape survives the
    /// extractor and [`stored_fields`] folds them into one list.
    #[serde(default)]
    fields_attributes: Option<Value>,
    /// The avatar: a `multipart/form-data` file part named `avatar`, or the
    /// text/JSON spelling that clears the stored one.
    #[serde(default, deserialize_with = "kept_value")]
    avatar: Option<Value>,
    /// The profile banner, Mastodon's name for what Pleroma stores in
    /// `banner_url`.
    #[serde(default, deserialize_with = "kept_value")]
    header: Option<Value>,
    /// The profile background image, Pleroma's own extension.
    #[serde(default, deserialize_with = "kept_value")]
    pleroma_background_image: Option<Value>,
}

/// The `source` object of an `update_credentials` body: Mastodon's editing
/// half of the account entity, which Pleroma reads back into the posting
/// defaults. A form carries its keys as `source[privacy]=…`, which the
/// body parser nests into exactly this shape; a JSON body carries the object
/// itself.
#[derive(Debug, Deserialize)]
struct SourceParams {
    /// The visibility new statuses default to, checked against [`SCOPES`]
    /// (the `users.default_scope` column).
    #[serde(default)]
    privacy: Option<String>,
    /// Whether new statuses are flagged sensitive by default (the
    /// `users.sensitive_default` column).
    #[serde(default, deserialize_with = "flex::opt_bool")]
    sensitive: Option<bool>,
    /// The BCP 47 tag new statuses default to, at most
    /// [`MAX_LANGUAGE_CHARS`] characters and cleared by the empty string
    /// (the `users.language` column).
    #[serde(default)]
    language: Option<String>,
}

/// Serde field helper for a parameter that must tell an absent key from a
/// JSON `null`: `deserialize_with` runs only when the key is there, and
/// [`Value`] keeps `null` as [`Value::Null`] instead of folding it into
/// absence the way `Option<T>` does. Paired with `#[serde(default)]` the
/// field is `None` for a key the body never carried and
/// `Some(Value::Null)` for one it carried as `null` — the difference the
/// profile-image clears depend on.
///
/// # Errors
///
/// Whatever the underlying [`Value`] deserializer reports (which, for the
/// JSON and form bodies the extractors hand over, is nothing: any value
/// becomes a [`Value`]).
fn kept_value<'de, D>(deserializer: D) -> Result<Option<Value>, D::Error>
where
    D: Deserializer<'de>,
{
    Value::deserialize(deserializer).map(Some)
}

/// `PATCH /api/v1/accounts/update_credentials` (and the `POST` alias of
/// the older clients) — change the caller's own profile, the fields the
/// body carries and nothing else, answering the updated account as
/// [`verify_credentials`] answers it.
///
/// [`MultipartForm`] takes JSON, urlencoded and multipart bodies alike
/// (decision 10), its file parts arriving beside the text parameters: the
/// three profile images are the file parts this endpoint reads
/// ([`resolve_images`]), every other file part is ignored.
/// [`profile_fields`] validates what the body carries and merges it over the
/// token's row, the image writes land in the same merged struct after
/// [`ImageWrites::apply`] stored their files, and
/// [`update_profile`](crate::models::user::update_profile) writes every
/// profile column in one statement. The fresh row answers through
/// [`owner_view`] —
/// the write-behind render that states the stored fact rather than trusting
/// the request — and only after it does [`delete_replaced_files`] remove the
/// files the write replaced. A body that carries none of these fields is the
/// accepted no-op: 200 with the unchanged entity.
///
/// # Errors
///
/// [`AppError::Unauthorized`] / [`AppError::Forbidden`] from [`AuthUser`]
/// and the `write:accounts` check; [`AppError::Unprocessable`] when
/// `display_name` or `note` is too long, when `fields_attributes` breaks
/// one of its limits, when `source[privacy]` (or the flat `default_scope`)
/// names no visibility, when `source[language]` is over
/// [`MAX_LANGUAGE_CHARS`] characters, when `actor_type` is neither `Person`
/// nor `Service`, when `birthday` is no `YYYY-MM-DD` date or a future one,
/// when `pleroma_settings_store` is no JSON object or the merged store passes
/// the 1 MiB cap, when a profile image is not a `png`, `jpeg`, `gif` or
/// `webp` file or when one passes its configured byte limit, and from the
/// extractor for an invalid boolean string or a file over
/// `UPLOAD_LIMIT_BYTES` (413). Storage failures arrive as
/// [`AppError::Internal`], any database failure through `?` as
/// [`AppError::Database`], and the [`AppError::Internal`] arm of the write
/// only documents that the row behind a just-authenticated token is gone.
async fn update_credentials(
    State(state): State<AppState>,
    auth: AuthUser,
    form: MultipartForm<ProfileParams>,
) -> AppResult<Json<Account>> {
    auth.require(WRITE_ACCOUNTS)?;
    let MultipartForm { fields, mut files } = form;
    let images = resolve_images(&state.config, &mut files, &fields)?;
    let mut profile = profile_fields(&state, &auth.0, fields).await?;
    let applied = images.apply(&state, auth.0.id, &auth.0).await?;
    profile.avatar_url = applied.avatar_url;
    profile.banner_url = applied.banner_url;
    profile.background_image_url = applied.background_image_url;
    let updated = user::update_profile(&state.db, auth.0.id, &profile)
        .await?
        .ok_or_else(|| AppError::Internal("profile update matched no row".to_owned()))?;
    delete_replaced_files(&state, applied.replaced).await;
    after_profile_updated(&state, &updated);
    owner_view(&state, &updated).await
}

/// Validates the present profile parameters and merges them over the
/// stored row: every column
/// [`update_profile`](crate::models::user::update_profile) will write, with
/// fields absent from the request carrying the value they already hold and
/// fields present-but-blank carrying the blank (an empty `display_name` is
/// stored as `""`, not reset to the nickname — this endpoint's ticket pins
/// that against Pleroma's registration-side fallback). A `note` over
/// [`max_bio_chars`](Config::max_bio_chars) characters (counted in
/// characters, not bytes) is the 422 mirroring the name message; a submitted
/// `fields_attributes` is validated and rendered by [`stored_fields`]. The
/// Pleroma side of the body merges through the `merged_*` helpers below:
/// [`merged_scope`] and [`merged_language`] for two of the three posting
/// defaults (`source[sensitive]` is a plain [`flex`] boolean),
/// [`merged_prefs`] for the eleven preference flags, [`merged_birthday`],
/// [`merged_store`] and [`merged_actor`] for the date, the client settings
/// blob and the actor kind. The three image columns start from the stored
/// row here and are overwritten by [`ImageWrites::apply`], which is the only
/// writer that can store a file.
///
/// # Errors
///
/// [`AppError::Unprocessable`] from [`sanitized_name`], the note length
/// check, [`stored_fields`], [`merged_scope`], [`merged_language`],
/// [`merged_actor`], [`merged_birthday`] or [`merged_store`];
/// [`render_note`] adds any database failure as [`AppError::Database`].
async fn profile_fields(
    state: &AppState,
    current: &User,
    params: ProfileParams,
) -> AppResult<ProfileFields> {
    let name = params
        .display_name
        .as_deref()
        .map(|raw| sanitized_name(raw, state.config.max_display_name_chars))
        .transpose()?;
    if params
        .note
        .as_deref()
        .is_some_and(|text| text.chars().count() > state.config.max_bio_chars)
    {
        return Err(AppError::Unprocessable(format!(
            "Bio is too long (maximum is {} characters)",
            state.config.max_bio_chars
        )));
    }
    let (bio, raw_bio) = match params.note.as_deref() {
        Some(raw) => (render_note(state, raw).await?, raw.to_owned()),
        None => (current.bio.clone(), current.raw_bio.clone()),
    };
    let (fields, raw_fields) = match params.fields_attributes.as_ref() {
        Some(submitted) => stored_fields(submitted.clone(), state.config.max_profile_fields)?,
        None => (current.fields.clone(), current.raw_fields.clone()),
    };
    let privacy = params
        .source
        .as_ref()
        .and_then(|nested| nested.privacy.as_deref())
        .or(params.default_scope.as_deref());
    let language = params
        .source
        .as_ref()
        .and_then(|nested| nested.language.as_deref());
    let sensitive = params.source.as_ref().and_then(|nested| nested.sensitive);
    let (actor_type, bot) = merged_actor(&params, current)?;
    Ok(ProfileFields {
        name: name.unwrap_or_else(|| current.name.clone()),
        bio,
        raw_bio,
        locked: params.locked.map_or(current.locked, Flag::from),
        bot,
        discoverable: params.discoverable.map_or(current.discoverable, Flag::from),
        fields,
        raw_fields,
        avatar_url: current.avatar_url.clone(),
        banner_url: current.banner_url.clone(),
        background_image_url: current.background_image_url.clone(),
        default_scope: merged_scope(privacy, &current.default_scope)?,
        sensitive_default: sensitive.map_or(current.sensitive_default, Flag::from),
        language: merged_language(language, current.language.as_deref())?,
        pleroma: merged_prefs(&params, current),
        birthday: merged_birthday(
            params.birthday.as_deref(),
            current.birthday,
            Utc::now().date_naive(),
        )?,
        settings_store: merged_store(
            params.pleroma_settings_store.as_ref(),
            &current.settings_store,
        )?,
        actor_type,
    })
}

/// The `default_scope` to store: `submitted` — the body's
/// `source[privacy]`, which wins over Pleroma's flat `default_scope` because
/// that is the order `update_credentials` applies them — checked against
/// [`SCOPES`]; `None` (neither parameter, or a JSON `null`) keeps the stored
/// visibility.
///
/// # Errors
///
/// [`AppError::Unprocessable`] with [`PRIVACY_INVALID`] for any other text.
fn merged_scope(submitted: Option<&str>, stored: &str) -> AppResult<String> {
    match submitted {
        Some(text) if SCOPES.contains(&text) => Ok(text.to_owned()),
        Some(_) => Err(AppError::Unprocessable(PRIVACY_INVALID.to_owned())),
        None => Ok(stored.to_owned()),
    }
}

/// The `language` to store: `submitted` — the body's `source[language]`, at
/// most [`MAX_LANGUAGE_CHARS`] characters counted in characters, not bytes —
/// with the empty string clearing the preference to `None`; `None` (no
/// parameter) keeps the stored tag.
///
/// # Errors
///
/// [`AppError::Unprocessable`] in [`sanitized_name`]'s wording for an
/// over-long tag.
fn merged_language(submitted: Option<&str>, stored: Option<&str>) -> AppResult<Option<String>> {
    let Some(raw) = submitted else {
        return Ok(stored.map(str::to_owned));
    };
    if raw.chars().count() > MAX_LANGUAGE_CHARS {
        return Err(AppError::Unprocessable(format!(
            "Language is too long (maximum is {MAX_LANGUAGE_CHARS} characters)"
        )));
    }
    Ok((!raw.is_empty()).then(|| raw.to_owned()))
}

/// The eleven `pleroma` preference flags after this body: every one of them
/// the submitted [`flex`] boolean or, absent, the flag the row holds. No
/// validation beyond the coercion itself: these are plain booleans.
fn merged_prefs(params: &ProfileParams, current: &User) -> PleromaPrefs {
    PleromaPrefs {
        hide_followers: params
            .hide_followers
            .map_or(current.hide_followers, Flag::from),
        hide_follows: params.hide_follows.map_or(current.hide_follows, Flag::from),
        hide_followers_count: params
            .hide_followers_count
            .map_or(current.hide_followers_count, Flag::from),
        hide_follows_count: params
            .hide_follows_count
            .map_or(current.hide_follows_count, Flag::from),
        hide_favorites: params
            .hide_favorites
            .map_or(current.hide_favorites, Flag::from),
        show_role: params.show_role.map_or(current.show_role, Flag::from),
        skip_thread_containment: params
            .skip_thread_containment
            .map_or(current.skip_thread_containment, Flag::from),
        allow_following_move: params
            .allow_following_move
            .map_or(current.allow_following_move, Flag::from),
        accepts_chat_messages: params
            .accepts_chat_messages
            .map_or(current.accepts_chat_messages, Flag::from),
        no_rich_text: params.no_rich_text.map_or(current.no_rich_text, Flag::from),
        show_birthday: params
            .show_birthday
            .map_or(current.show_birthday, Flag::from),
    }
}

/// The `birthday` to store: the parsed `submitted` date, the empty string
/// clearing the column and `None` (no parameter) keeping the stored one.
/// Text that is not the `YYYY-MM-DD` spelling is [`BIRTHDAY_INVALID`] —
/// chrono's parser also takes `1990-1-3`, so the parsed date has to print
/// back exactly what was sent — and a date after `today` is
/// [`BIRTHDAY_FUTURE`], Pleroma's `validate_min_age` kept to the one rule
/// this ticket states.
///
/// # Errors
///
/// [`AppError::Unprocessable`] with those two messages.
fn merged_birthday(
    submitted: Option<&str>,
    stored: Option<NaiveDate>,
    today: NaiveDate,
) -> AppResult<Option<NaiveDate>> {
    let Some(raw) = submitted else {
        return Ok(stored);
    };
    if raw.is_empty() {
        return Ok(None);
    }
    let parsed = NaiveDate::parse_from_str(raw, BIRTHDAY_FORMAT)
        .map_err(|_err| AppError::Unprocessable(BIRTHDAY_INVALID.to_owned()))?;
    if parsed.format(BIRTHDAY_FORMAT).to_string() != raw {
        return Err(AppError::Unprocessable(BIRTHDAY_INVALID.to_owned()));
    }
    if parsed > today {
        return Err(AppError::Unprocessable(BIRTHDAY_FUTURE.to_owned()));
    }
    Ok(Some(parsed))
}

/// The `settings_store` to write: the stored blob with the submitted
/// `pleroma_settings_store` merged into it key by key ([`shallow_merge`],
/// Pleroma's `Map.merge` in `User.update_changeset` — a submitted key wins,
/// every other stored entry stays) and then measured by
/// [`check_store_size`]. No parameter keeps the stored blob; a merged blob
/// over the cap is refused here, before the write, so nothing changes.
///
/// # Errors
///
/// [`AppError::Unprocessable`] with [`STORE_NOT_OBJECT`] for a value that is
/// neither a JSON object nor the JSON text of one, or with
/// [`STORE_TOO_LARGE`](crate::api::pleroma_settings::STORE_TOO_LARGE) for a
/// store over the limit.
fn merged_store(submitted: Option<&Value>, stored: &Value) -> AppResult<Value> {
    let Some(patch) = submitted else {
        return Ok(stored.clone());
    };
    let merged = shallow_merge(stored, &store_object(patch)?);
    check_store_size(&merged)?;
    Ok(merged)
}

/// The object a submitted `pleroma_settings_store` names: a JSON body
/// carries the object itself, a form or multipart body carries its JSON
/// *text*, which is parsed here; anything else — an array, a scalar, text
/// that is no JSON — has no keys to merge and is the 422.
///
/// # Errors
///
/// [`AppError::Unprocessable`] with [`STORE_NOT_OBJECT`].
fn store_object(submitted: &Value) -> AppResult<Map<String, Value>> {
    let decoded = match submitted {
        Value::String(text) => serde_json::from_str::<Value>(text)
            .map_err(|_err| AppError::Unprocessable(STORE_NOT_OBJECT.to_owned())),
        Value::Object(map) => Ok(Value::Object(map.clone())),
        Value::Null | Value::Bool(_) | Value::Number(_) | Value::Array(_) => {
            Err(AppError::Unprocessable(STORE_NOT_OBJECT.to_owned()))
        }
    }?;
    let Value::Object(map) = decoded else {
        return Err(AppError::Unprocessable(STORE_NOT_OBJECT.to_owned()));
    };
    Ok(map)
}

/// The `actor_type` and `bot` pair after this body, in Pleroma's order: the
/// `bot` parameter picks the actor kind ([`actor_type_for_bot`]) and an
/// explicit `actor_type` wins over it and re-decides `bot` from the kind, so
/// the two columns never disagree. A body carrying neither leaves both
/// stored values as they are.
///
/// # Errors
///
/// [`AppError::Unprocessable`] with [`ACTOR_TYPE_INVALID`] for a kind
/// outside [`ACTOR_TYPES`] (`Group` and every other actor type is not
/// settable through this endpoint).
fn merged_actor(params: &ProfileParams, current: &User) -> AppResult<(String, Flag)> {
    match params.actor_type.as_deref() {
        Some(kind) if ACTOR_TYPES.contains(&kind) => {
            Ok((kind.to_owned(), Flag::from(kind == SERVICE_ACTOR_TYPE)))
        }
        Some(_) => Err(AppError::Unprocessable(ACTOR_TYPE_INVALID.to_owned())),
        None => Ok(params.bot.map_or_else(
            || (current.actor_type.clone(), current.bot),
            |automated| {
                (
                    actor_type_for_bot(automated).to_owned(),
                    Flag::new(automated),
                )
            },
        )),
    }
}

/// The `actor_type` Pleroma stores for an account flagged as a bot (or not):
/// an automated account is a `Service`, a human one a [`Person`].
///
/// [`Person`]: crate::models::user::DEFAULT_ACTOR_TYPE
#[must_use]
const fn actor_type_for_bot(automated: bool) -> &'static str {
    if automated {
        SERVICE_ACTOR_TYPE
    } else {
        PERSON_ACTOR_TYPE
    }
}

/// What one profile-image parameter of an `update_credentials` body asks
/// for, after its bytes passed the checks and before anything was stored.
#[derive(Debug)]
enum ImageWrite {
    /// The parameter is absent: the column keeps its stored value.
    Unchanged,
    /// The empty string (the form and multipart spelling) or JSON `null`:
    /// the column is cleared, Pleroma's `user_image_value` reset.
    Clear,
    /// A file part whose bytes named an accepted image type and stayed
    /// within the column's byte limit.
    Store {
        /// The image's bytes.
        bytes: Bytes,
        /// The client's file name, used only for the key's extension
        /// fallback (the type comes from the bytes).
        filename: Option<String>,
        /// The media type the magic bytes named.
        content_type: &'static str,
    },
}

/// The three profile-image writes of one body, resolved together so that a
/// body with one good image and one bad one stores nothing at all.
#[derive(Debug)]
struct ImageWrites {
    /// The write for `users.avatar_url`, from the `avatar` parameter.
    avatar: ImageWrite,
    /// The write for `users.banner_url`, from the `header` parameter.
    banner: ImageWrite,
    /// The write for `users.background_image_url`, from Pleroma's
    /// `pleroma_background_image` parameter.
    background: ImageWrite,
}

/// One profile-image column after its write was applied: the value the row
/// will hold, and the URL the write replaced.
type MergedImage = (Option<String>, Option<String>);

impl ImageWrites {
    /// Stores every upload of `writes` through the uploader and answers the
    /// three merged column values beside the list of URLs the writes
    /// replaced: [`ImageWrite::Unchanged`] leaves the `current` row's value
    /// in place, [`ImageWrite::Clear`] replaces it with `None` and
    /// [`ImageWrite::Store`] with the new file's public URL. Whatever the
    /// column held before and no longer holds is the file
    /// [`delete_replaced_files`] removes, once the row stopped naming it.
    ///
    /// # Errors
    ///
    /// Storage failures arrive from [`store_image`] as
    /// [`AppError::Internal`] / [`AppError::BadRequest`].
    async fn apply(self, state: &AppState, user_id: Uuid, current: &User) -> AppResult<Applied> {
        let (avatar_url, avatar_replaced) = self
            .avatar
            .merged(state, user_id, current.avatar_url.as_deref())
            .await?;
        let (banner_url, banner_replaced) = self
            .banner
            .merged(state, user_id, current.banner_url.as_deref())
            .await?;
        let (background_image_url, background_replaced) = self
            .background
            .merged(state, user_id, current.background_image_url.as_deref())
            .await?;
        Ok(Applied {
            avatar_url,
            banner_url,
            background_image_url,
            replaced: [avatar_replaced, banner_replaced, background_replaced]
                .into_iter()
                .flatten()
                .collect(),
        })
    }
}

/// The three merged profile-image columns and the files that replaced them
/// (see [`ImageWrites::apply`]).
#[derive(Debug)]
struct Applied {
    /// The value for `users.avatar_url`.
    avatar_url: Option<String>,
    /// The value for `users.banner_url`.
    banner_url: Option<String>,
    /// The value for `users.background_image_url`.
    background_image_url: Option<String>,
    /// The URLs the writes replaced, in column order: empty for a slot that
    /// was untouched, or cleared while already empty.
    replaced: Vec<String>,
}

impl ImageWrite {
    /// The value this write leaves in its column and the URL it replaced:
    /// see [`ImageWrites::apply`] for the three cases. `current` is what the
    /// row holds now, so an untouched slot round-trips it.
    ///
    /// # Errors
    ///
    /// As [`store_image`]'s for a [`ImageWrite::Store`].
    async fn merged(
        self,
        state: &AppState,
        user_id: Uuid,
        current: Option<&str>,
    ) -> AppResult<MergedImage> {
        let value = match self {
            Self::Unchanged => current.map(str::to_owned),
            Self::Clear => None,
            Self::Store {
                bytes,
                filename,
                content_type,
            } => Some(store_image(state, user_id, bytes, filename.as_deref(), content_type).await?),
        };
        Ok((value.clone(), replaced_url(current, value.as_deref())))
    }
}

/// The URL a write leaves behind: `Some` only when the column held one and
/// the write stores a different value — a replacement, or a clear. Re-sending
/// the URL already stored replaces nothing (and a body cannot send one
/// anyway, since this endpoint only ever writes URLs it minted itself).
fn replaced_url(before: Option<&str>, after: Option<&str>) -> Option<String> {
    before
        .filter(|held| Some(*held) != after)
        .map(str::to_owned)
}

/// Resolves the three profile-image parameters of one body against the file
/// parts that rode along, each against its own configured cap: the avatar
/// against [`avatar_limit_bytes`](Config::avatar_limit_bytes), the banner and
/// the background against [`banner_limit_bytes`](Config::banner_limit_bytes),
/// Pleroma's one number for both. A file part wins over a text or JSON
/// spelling of the same parameter — that is what a browser's `FormData`
/// sends — and every slot is resolved before any file is stored, so a body
/// with one good image and one bad one writes nothing.
///
/// # Errors
///
/// [`AppError::Unprocessable`] with [`FILE_TOO_LARGE`] or
/// [`UNSUPPORTED_FILE_TYPE`] from [`resolve_image`].
fn resolve_images(
    config: &Config,
    files: &mut Vec<UploadedFile>,
    params: &ProfileParams,
) -> AppResult<ImageWrites> {
    Ok(ImageWrites {
        avatar: resolve_image(
            files,
            AVATAR_FIELD,
            params.avatar.as_ref(),
            config.avatar_limit_bytes,
        )?,
        banner: resolve_image(
            files,
            HEADER_FIELD,
            params.header.as_ref(),
            config.banner_limit_bytes,
        )?,
        background: resolve_image(
            files,
            BACKGROUND_FIELD,
            params.pleroma_background_image.as_ref(),
            config.banner_limit_bytes,
        )?,
    })
}

/// Resolves one profile-image slot: the file part uploaded under `field`
/// when the body carried one, else the parameter's text or JSON `value` —
/// absent keeps the column, `null` and the empty string clear it, and
/// anything else is a value this endpoint has no way to store, answered with
/// the same 422 as bytes it cannot name an image for.
///
/// # Errors
///
/// [`AppError::Unprocessable`] with [`FILE_TOO_LARGE`] or
/// [`UNSUPPORTED_FILE_TYPE`].
fn resolve_image(
    files: &mut Vec<UploadedFile>,
    field: &str,
    value: Option<&Value>,
    max_bytes: usize,
) -> AppResult<ImageWrite> {
    if let Some(file) = files.take(field) {
        return validated_write(file, max_bytes);
    }
    match value {
        None => Ok(ImageWrite::Unchanged),
        Some(Value::Null) => Ok(ImageWrite::Clear),
        Some(Value::String(text)) if text.is_empty() => Ok(ImageWrite::Clear),
        Some(_) => Err(AppError::Unprocessable(UNSUPPORTED_FILE_TYPE.to_owned())),
    }
}

/// The write for one image file part: its size checked against `max_bytes`
/// and its bytes against [`ACCEPTED_IMAGE_TYPES`], before anything reached
/// storage.
///
/// # Errors
///
/// [`AppError::Unprocessable`] with [`FILE_TOO_LARGE`] over the cap or
/// [`UNSUPPORTED_FILE_TYPE`] from [`detect_image_type`].
fn validated_write(file: UploadedFile, max_bytes: usize) -> AppResult<ImageWrite> {
    if file.bytes.len() > max_bytes {
        return Err(AppError::Unprocessable(FILE_TOO_LARGE.to_owned()));
    }
    let content_type = detect_image_type(&file.bytes)?;
    Ok(ImageWrite::Store {
        bytes: file.bytes,
        filename: file.filename,
        content_type,
    })
}

/// The media type of profile-image bytes, from the bytes themselves and not
/// from the `content-type` the part was sent with: the `infer` match when its
/// type is one of [`ACCEPTED_IMAGE_TYPES`].
///
/// # Errors
///
/// [`AppError::Unprocessable`] with [`UNSUPPORTED_FILE_TYPE`] for bytes no
/// matcher names, and for the media types a profile image may not be.
fn detect_image_type(bytes: &[u8]) -> AppResult<&'static str> {
    let Some(mime) = infer::get(bytes)
        .map(|kind| kind.mime_type())
        .filter(|mime| ACCEPTED_IMAGE_TYPES.contains(mime))
    else {
        return Err(AppError::Unprocessable(UNSUPPORTED_FILE_TYPE.to_owned()));
    };
    Ok(mime)
}

/// Stores one profile image: a fresh [`media_key`](uploads::media_key) under
/// the uploader's id, the bytes at that key, and the key's public URL as the
/// value the column holds — the address [`crate::api::media_files`] serves
/// the file from.
///
/// # Errors
///
/// [`AppError::Internal`] when the filesystem rejects the write and
/// [`AppError::BadRequest`] from the key guard (unreachable for a generated
/// key), both from [`UploaderBackend::put`](uploads::UploaderBackend::put).
async fn store_image(
    state: &AppState,
    user_id: Uuid,
    bytes: Bytes,
    filename: Option<&str>,
    content_type: &str,
) -> AppResult<String> {
    let key = uploads::media_key(user_id, filename, content_type);
    state.uploader.put(&key, bytes, content_type).await?;
    debug!(
        user = %user_id,
        %key,
        %content_type,
        "stored profile image"
    );
    Ok(state.uploader.public_url(&key))
}

/// Removes the files the profile-image writes replaced, run after the row
/// stopped naming them: see [`delete_replaced_file`] for one URL.
async fn delete_replaced_files(state: &AppState, replaced: Vec<String>) {
    for stored_url in replaced {
        delete_replaced_file(state, &stored_url).await;
    }
}

/// Best-effort removal of one replaced profile image. Only a URL under this
/// instance's own media prefix names a file we hold; a foreign or hand-set
/// URL is left alone, and a removal that fails is logged, not an error — the
/// profile update it follows already succeeded.
async fn delete_replaced_file(state: &AppState, stored_url: &str) {
    let Some(key) = own_media_key(&state.config, stored_url) else {
        debug!(
            stored_url,
            "keeping a replaced profile image we do not store"
        );
        return;
    };
    if let Err(err) = state.uploader.delete(&key).await {
        warn!(%err, %key, "could not delete the replaced profile image");
    }
}

/// The uploads key `stored_url` names when it is one of this instance's own
/// media addresses — `{MEDIA_BASE_URL}/{key}` with a `key`
/// [`check_key`](uploads::check_key) accepts — and `None` for no URL, for a
/// file on another host, and for a remainder that is not a usable key.
#[must_use]
fn own_media_key(config: &Config, stored_url: &str) -> Option<String> {
    let prefix = format!("{}/", config.media_base_url);
    let key = stored_url.strip_prefix(prefix.as_str())?;
    uploads::check_key(key).is_ok().then(|| key.to_owned())
}

/// The `fields` and `raw_fields` jsonb a submitted `fields_attributes`
/// stores: [`index_map_to_vec`] folds the JSON array, index-object and
/// form-spelled bodies into one list of `{name, value}` entries (a key
/// neither holds reads as the empty string, so entries that are not
/// objects are empty ones), and an entry with both name and value empty
/// is dropped — Pleroma's `put_fields` filtering, widened to the whole
/// entry. What survives must be at most `max_fields` entries (the 422
/// [`TOO_MANY_FIELDS`]) with each name within
/// [`MAX_PROFILE_FIELD_NAME_CHARS`] and each value within
/// [`MAX_PROFILE_FIELD_VALUE_CHARS`] characters, counted on the raw text
/// like Pleroma's `valid_field?`. The stored pair is the raw entry in
/// `raw_fields` and, in `fields`, the name reduced to plain text by
/// [`strip_html`] and the value rendered by [`text_to_html`] — bare URLs
/// become links carrying the standard `rel`. An empty submission is an
/// empty pair: the metadata, cleared.
///
/// # Errors
///
/// [`AppError::Unprocessable`] for the count and both length limits.
fn stored_fields(submitted: Value, max_fields: usize) -> AppResult<(Value, Value)> {
    let mut entries = Vec::new();
    for entry in index_map_to_vec(submitted) {
        let name = field_text(&entry, "name");
        let value = field_text(&entry, "value");
        if name.is_empty() && value.is_empty() {
            continue;
        }
        entries.push((name, value));
    }
    if entries.len() > max_fields {
        return Err(AppError::Unprocessable(TOO_MANY_FIELDS.to_owned()));
    }
    let mut rendered = Vec::with_capacity(entries.len());
    let mut raw = Vec::with_capacity(entries.len());
    for (name, value) in entries {
        check_field_len("Field name", &name, MAX_PROFILE_FIELD_NAME_CHARS)?;
        check_field_len("Field value", &value, MAX_PROFILE_FIELD_VALUE_CHARS)?;
        rendered.push(json!({
            "name": strip_html(&name),
            "value": text_to_html(&value),
        }));
        raw.push(json!({"name": name, "value": value}));
    }
    Ok((Value::Array(rendered), Value::Array(raw)))
}

/// The string one submitted `fields_attributes` entry holds under `key`:
/// a missing key or a non-string value is the empty string, which is how
/// a form that spelled only `fields_attributes[1][name]` leaves the
/// value.
fn field_text(entry: &Value, key: &str) -> String {
    entry
        .get(key)
        .and_then(Value::as_str)
        .unwrap_or_default()
        .to_owned()
}

/// The 422 mirroring the name and bio messages when one profile field's
/// `text` is longer than `max_chars` characters (counted in characters,
/// not bytes).
fn check_field_len(what: &str, text: &str, max_chars: usize) -> AppResult<()> {
    if text.chars().count() > max_chars {
        return Err(AppError::Unprocessable(format!(
            "{what} is too long (maximum is {max_chars} characters)"
        )));
    }
    Ok(())
}

/// The display name to store: at most `max_chars` characters (in
/// characters, not bytes) of the submitted text — the limit is checked
/// before sanitizing, like Pleroma's changeset checks the parameter —
/// and then [`sanitize_display_name`]'s one line of plain text. An empty
/// string survives as empty.
fn sanitized_name(raw: &str, max_chars: usize) -> AppResult<String> {
    if raw.chars().count() > max_chars {
        return Err(AppError::Unprocessable(format!(
            "Name is too long (maximum is {max_chars} characters)"
        )));
    }
    Ok(sanitize_display_name(raw))
}

/// Renders one submitted note into the `bio` column: [`text_to_html`]
/// (escape, bare-URL links, paragraphs) and then [`linkify_entities`]
/// over its output, so hashtags link to `{base_url}/tags/{tag}` and
/// mentions naming a local user of this instance become their `h-card`
/// anchors — unknown and remote mentions stay the plain text they were
/// written as.
async fn render_note(state: &AppState, raw: &str) -> AppResult<String> {
    let html = text_to_html(raw);
    let urls = local_mention_urls(&state.db, &state.config, &html).await?;
    let resolver = |mention: &MentionRef| urls.get(&mention_key(mention)).cloned();
    Ok(linkify_entities(&html, &resolver, &state.config.base_url))
}

/// The profile URLs of every mention [`extract_mentions`] finds in
/// `text` that names a local user, keyed by [`mention_key`]. Lookups are
/// citext, so `@ALICE` finds `alice`; the [`local`](User::local) flag is
/// the guard that keeps a remote mirror out of the table whatever its
/// stored nickname looks like.
async fn local_mention_urls(
    db: &PgPool,
    config: &Config,
    text: &str,
) -> AppResult<HashMap<String, String>> {
    let mut urls = HashMap::new();
    for mention in extract_mentions(text) {
        if !names_own_domain(config, &mention) {
            continue;
        }
        if let Some(found) = user::get_by_nickname(db, &mention.nickname)
            .await?
            .filter(|found| found.local.get())
        {
            let _seen = urls.insert(mention_key(&mention), local_url(config, &found.nickname));
        }
    }
    Ok(urls)
}

/// The lookup key of one mention: the account it names, lowercased —
/// `nick` for a bare one, `nick@domain` when the reference spelled the
/// domain out (the case-fold
/// [`mention_html`](crate::util::entities::mention_html) never needs
/// because it renders the written spelling).
fn mention_key(mention: &MentionRef) -> String {
    mention
        .domain
        .as_ref()
        .map_or_else(
            || mention.nickname.clone(),
            |domain| format!("{}@{}", mention.nickname, domain),
        )
        .to_lowercase()
}

/// Whether a mention can name a local user at all: a bare `@nick`, or a
/// `@nick@domain` whose domain is this instance's own
/// ([`Config::domain`], compared case-insensitively). Any other domain
/// names a remote account, which this render never links.
fn names_own_domain(config: &Config, mention: &MentionRef) -> bool {
    mention
        .domain
        .as_deref()
        .is_none_or(|domain| domain.eq_ignore_ascii_case(config.domain()))
}

/// Post-update hook of [`update_credentials`], run once per accepted
/// profile write with the updated row: the federation ticket that answers
/// a profile change with an `Update` of the actor wires the delivery in
/// here — the actor is [`User::ap_id`], and the queueing happens through
/// `state` (decision 11).
///
/// Intentionally empty until then, like [`after_follower_removed`]:
/// Pleroma's `update_credentials` federates through the pipeline inline,
/// and this server's delivery queue is a later ticket.
const fn after_profile_updated(_state: &AppState, _user: &User) {}

/// `GET /api/v1/accounts/{id}` — the account whose id is `id`, local or
/// remote. An id that is not `UUID` text is a 404, not a 400, like every
/// other miss (see [`account_or_404`]).
///
/// The viewer behind an optional token is the account the entity is
/// rendered for; without one the anonymous view is answered. A viewer who
/// is looking at someone else gets their relationship to the account
/// embedded (see [`render_account`]).
///
/// # Errors
///
/// [`AppError::NotFound`] for a malformed, unknown or deactivated id;
/// [`AppError::Unauthorized`] from the extractor for an invalid token.
/// Any database failure arrives through `?` as [`AppError::Database`].
async fn show_account(
    State(state): State<AppState>,
    MaybeAuthUser(viewer, _): MaybeAuthUser,
    Path(id): Path<String>,
) -> AppResult<Json<Account>> {
    let found = account_or_404(&state, &id).await?;
    Ok(Json(render_account(&state, viewer.as_ref(), &found).await?))
}

/// The query parameters of `GET /api/v1/accounts/lookup`. The absent and
/// blank cases are the documented 400 and 404, which the type cannot
/// express, so the field is optional here (the `webfinger` handler's
/// pattern).
#[derive(Debug, Deserialize)]
struct LookupQuery {
    /// The handle to resolve: `nick` or `nick@domain`.
    acct: Option<String>,
}

/// `GET /api/v1/accounts/lookup?acct=…` — the account a handle names,
/// resolved by [`get_by_acct`](crate::models::user::get_by_acct). The
/// viewer behind an optional token is the account the entity is rendered
/// for, as in `show`.
///
/// # Errors
///
/// [`AppError::BadRequest`] when `acct` is absent or blank;
/// [`AppError::NotFound`] when it names no live account (unknown, on a
/// foreign domain not mirrored here, or deactivated);
/// [`AppError::Unauthorized`] from the extractor for an invalid token.
/// Any database failure arrives through `?` as [`AppError::Database`].
async fn lookup_account(
    State(state): State<AppState>,
    MaybeAuthUser(viewer, _): MaybeAuthUser,
    Query(query): Query<LookupQuery>,
) -> AppResult<Json<Account>> {
    let Some(acct) = query
        .acct
        .as_deref()
        .map(str::trim)
        .filter(|acct| !acct.is_empty())
    else {
        return Err(AppError::BadRequest(MISSING_ACCT.to_owned()));
    };
    let found = user::get_by_acct(&state.db, &state.config, acct)
        .await?
        .filter(|found| found.is_active.get())
        .ok_or(AppError::NotFound)?;
    Ok(Json(render_account(&state, viewer.as_ref(), &found).await?))
}

/// `GET /api/v1/accounts/relationships?id[]=a&id[]=b` — the caller's
/// relationship to each requested account, as
/// [`Relationship::load_many`] computes it, in the order requested.
///
/// The `id` values are read by [`requested_ids`]; ones that are not `UUID`
/// text and ones that name no stored user are dropped (Pleroma's
/// `User.get_all_by_ids` simply finds nothing for them), so the response
/// holds only the known ids in requested order. No usable `id` at all is
/// the empty array, Pleroma's explicit `json(conn, [])` for a missing
/// parameter, not a `Missing parameter` error.
///
/// # Errors
///
/// [`AppError::Unauthorized`] from [`AuthUser`] without a valid user
/// token; [`AppError::Forbidden`] when the token does not grant
/// `read:follows`. Any database failure arrives through `?` as
/// [`AppError::Database`].
async fn relationships(
    State(state): State<AppState>,
    auth: AuthUser,
    uri: Uri,
) -> AppResult<Json<Vec<Relationship>>> {
    auth.require(READ_FOLLOWS)?;
    let requested = requested_ids(uri.query());
    if requested.is_empty() {
        return Ok(Json(Vec::new()));
    }
    let targets = known_ids(&state, &requested).await?;
    Ok(Json(
        Relationship::load_many(&state.db, auth.0.id, &targets).await?,
    ))
}

/// The subset of `requested` that names a stored user, kept in the
/// requested order ([`user::get_many`] answers in id order, so membership
/// is decided from its rows). One query per request whatever the batch
/// size.
async fn known_ids(state: &AppState, requested: &[Uuid]) -> AppResult<Vec<Uuid>> {
    let known: HashSet<Uuid> = user::get_many(&state.db, requested)
        .await?
        .into_iter()
        .map(|found| found.id)
        .collect();
    Ok(requested
        .iter()
        .copied()
        .filter(|id| known.contains(id))
        .collect())
}

/// The request parameters of `POST /api/v1/accounts/{id}/note`: Pleroma's
/// `%{comment: comment}` pattern match — the field is required (an absent
/// one is the extractor's 422), and the empty string is the documented
/// delete, so the type carries no default.
#[derive(Debug, Deserialize)]
struct NoteParams {
    /// The note text, verbatim up to [`MAX_COMMENT_CHARS`] characters;
    /// empty or all whitespace clears the note.
    comment: String,
}

/// `POST /api/v1/accounts/{id}/note` — write the caller's private note on
/// the account and answer their [`Relationship`] to it, with
/// [`note`](Relationship::note) carrying what the write left stored.
///
/// [`account_note::set`] is the write: a non-blank `comment` replaces the
/// caller's note on the target, a blank one deletes it (Pleroma's
/// `UserNote.create` stores blanks, this endpoint's ticket deletes on
/// them; both read back as the same relationship). The account is resolved
/// by [`account_or_404`], and noting yourself is allowed — unlike
/// Pleroma's follow-side checks, `note` has no self guard. The
/// [`Relationship::load_many`] after the write is the render of Pleroma's
/// `relationship.json`: it reads the row back, so the answer states the
/// stored fact rather than trusting the request.
///
/// # Errors
///
/// [`AppError::Unauthorized`] / [`AppError::Forbidden`] from [`AuthUser`]
/// and the `write:accounts` check; [`AppError::Unprocessable`] when
/// `comment` is absent or longer than [`MAX_COMMENT_CHARS`] characters;
/// [`AppError::NotFound`] for a malformed, unknown or deactivated account
/// id. Any database failure arrives through `?` as [`AppError::Database`].
async fn set_note(
    State(state): State<AppState>,
    auth: AuthUser,
    Path(id): Path<String>,
    ApiBody(params): ApiBody<NoteParams>,
) -> AppResult<Json<Relationship>> {
    auth.require(WRITE_ACCOUNTS)?;
    // Validation before the lookup, as the list endpoints do: a
    // too-long comment is the 422 whatever the id names.
    if params.comment.chars().count() > MAX_COMMENT_CHARS {
        return Err(AppError::Unprocessable(format!(
            "Validation failed: Comment is too long (maximum is {MAX_COMMENT_CHARS} characters)"
        )));
    }
    let target = account_or_404(&state, &id).await?;
    account_note::set(&state.db, auth.0.id, target.id, &params.comment).await?;
    one_relationship(&state, auth.0.id, target.id).await
}

/// `POST /api/v1/accounts/{id}/remove_from_followers` — drop the account
/// from the caller's followers: the accepted follow of the caller it holds
/// is removed ([`remove_follower`](crate::models::follow::remove_follower),
/// counters adjusted in the same transaction) and the caller's
/// [`Relationship`] to it answered, `followed_by: false` afterwards.
///
/// Not being a follower is not an error: an account with no row, one whose
/// follow of the caller is still `pending` (requests are answered through
/// `/api/v1/follow_requests`, never removed here) or one the caller follows
/// without being followed back changes nothing, and the unchanged
/// [`Relationship`] answers the call with 200. The account is resolved
/// before the self guard, as Pleroma's plug order does: a malformed,
/// unknown or deactivated id is the shared 404 whatever it would have
/// meant, and only a live account that turns out to be the caller is the
/// 422.
///
/// # Errors
///
/// [`AppError::Unauthorized`] / [`AppError::Forbidden`] from [`AuthUser`]
/// and the `write:follows | follow` check; [`AppError::NotFound`] for a
/// malformed, unknown or deactivated account id; [`AppError::Unprocessable`]
/// with [`SELF_REMOVE`] when the id names the caller. Any database failure
/// arrives through `?` as [`AppError::Database`].
async fn remove_from_followers(
    State(state): State<AppState>,
    auth: AuthUser,
    Path(id): Path<String>,
) -> AppResult<Json<Relationship>> {
    auth.require_any(&[WRITE_FOLLOWS, FOLLOW])?;
    let follower = account_or_404(&state, &id).await?;
    if follower.id == auth.0.id {
        return Err(AppError::Unprocessable(SELF_REMOVE.to_owned()));
    }
    let removed = follow::remove_follower(&state.db, auth.0.id, follower.id).await?;
    if let Some(row) = removed.as_ref() {
        debug!(
            follow = %row.id,
            user = %auth.0.id,
            "follower removed"
        );
        after_follower_removed(&state, &auth.0, row);
    }
    one_relationship(&state, auth.0.id, follower.id).await
}

/// The caller's [`Relationship`] to one account, rendered after a write:
/// [`Relationship::load_many`] over the single target, so the answer states
/// the stored fact rather than trusting the request (the write-behind read
/// of Pleroma's `relationship.json`). Shared by [`set_note`] and
/// [`remove_from_followers`].
///
/// # Errors
///
/// Any database failure arrives through `?` as [`AppError::Database`]; the
/// [`AppError::Internal`] arm only documents that the loader answers one row
/// per requested id, which a single-target batch always does.
async fn one_relationship(
    state: &AppState,
    viewer_id: Uuid,
    target_id: Uuid,
) -> AppResult<Json<Relationship>> {
    let loaded = Relationship::load_many(&state.db, viewer_id, &[target_id]).await?;
    let relationship = loaded.into_iter().next().ok_or_else(|| {
        AppError::Internal("the relationship loader skipped its only target".to_owned())
    })?;
    Ok(Json(relationship))
}

/// Post-removal hook of [`remove_from_followers`], run once per actually
/// removed follower: the federation ticket that answers a dropped follower
/// with a `Reject` for its `Follow` wires the delivery in here — the
/// activity to reject is [`Follow::ap_id`], its sender [`Follow::follower_id`],
/// and the queueing happens through `state` (decision 11).
///
/// Intentionally empty until then, like the follow-request answers'
/// [`after_follow_request_resolved`](crate::api::follow_requests): Pleroma's
/// `User.remove_follower` delivers the `Reject` inline, and this server's
/// delivery queue is a later ticket.
const fn after_follower_removed(_state: &AppState, _followee: &User, _removed: &Follow) {}

/// The request parameters of `POST /api/v1/accounts/{id}/follow`: Mastodon's
/// two per-follow toggles, read from the query string, a form body or a JSON
/// body alike ([`ApiParams`], decision 10 — clients use all three, and a
/// request may carry no body at all). Both are optional, and a form's
/// `"true"` / `"1"` spells the same thing through [`flex::opt_bool`]; anything
/// that is neither is the extractor's 422.
#[derive(Debug, Deserialize)]
struct FollowParams {
    /// Mastodon's `reblogs`, the row's [`Follow::show_reblogs`] column.
    #[serde(default, deserialize_with = "flex::opt_bool")]
    reblogs: Option<bool>,
    /// Mastodon's `notify`, the row's [`Follow::notify`] column.
    #[serde(default, deserialize_with = "flex::opt_bool")]
    notify: Option<bool>,
}

impl FollowParams {
    /// The toggles to store, exactly the parameters the request named: an
    /// absent one keeps the flag the row holds, which needs no default here
    /// because the documented ones (`reblogs` true, `notify` false) *are*
    /// the `follows` column defaults a fresh row is inserted with (see
    /// [`set_toggles`](crate::models::follow::set_toggles)).
    const fn into_toggles(self) -> FollowToggles {
        FollowToggles {
            show_reblogs: self.reblogs,
            notify: self.notify,
        }
    }
}

/// `POST /api/v1/accounts/{id}/follow` — follow the account and answer the
/// caller's [`Relationship`] to it: `following: true` when this instance
/// grants the follow on the spot (its own unlocked account), `requested: true`
/// when the target is locked and the request waits on
/// `/api/v1/follow_requests`. The two toggles the request names are stored on
/// the relationship the write created *or* found already there, so a repeat
/// follow of a pair is how a client rewrites its flags (Pleroma's `follow`
/// does the same: the second call changes nothing but the toggles).
///
/// The account is resolved by [`account_or_404`] before the write, so a
/// malformed, unknown or deactivated id is the shared 404 whatever the body
/// asked for; following the caller's own account is the model's 422 (its
/// wording, Pleroma's `Can not follow yourself`). A pair blocked either way is
/// the model's own 403 refusal, which stands between this route and the write.
///
/// # Errors
///
/// [`AppError::Unauthorized`] / [`AppError::Forbidden`] from [`AuthUser`]
/// and the `write:follows | follow` check; [`AppError::NotFound`] for a
/// malformed, unknown or deactivated account id; [`AppError::Unprocessable`]
/// with `"Can not follow yourself"`, with the model's block refusal, or with
/// the extractor's message when a toggle is not a boolean. Any database
/// failure arrives through `?` as [`AppError::Database`].
async fn follow_account(
    State(state): State<AppState>,
    auth: AuthUser,
    Path(id): Path<String>,
    ApiParams(params): ApiParams<FollowParams>,
) -> AppResult<Json<Relationship>> {
    auth.require_any(&[WRITE_FOLLOWS, FOLLOW])?;
    let target = account_or_404(&state, &id).await?;
    let toggles = params.into_toggles();
    let created = follow::follow(&state.db, &state.config.base_url, &auth.0, &target).await?;
    let stored = store_toggles(&state, created, toggles).await?;
    debug!(
        follow = %stored.id,
        user = %auth.0.id,
        target = %target.id,
        granted = stored.state == FollowState::Accepted,
        reblogs = stored.show_reblogs,
        notifying = stored.notify,
        "account followed"
    );
    after_follow_changed(&state, &auth.0, &target, Some(&stored), true);
    one_relationship(&state, auth.0.id, target.id).await
}

/// Writes the toggles a follow named onto the row [`follow::follow`] returned
/// and hands that row back as it now stands, so the log and the federation
/// hook state the stored fact rather than the request. A call that named
/// neither toggle writes nothing, and a relationship the pair lost to a
/// concurrent unfollow between the two statements keeps the row the follow
/// returned.
///
/// # Errors
///
/// Any database failure arrives through `?` as [`AppError::Database`].
async fn store_toggles(
    state: &AppState,
    created: Follow,
    toggles: FollowToggles,
) -> AppResult<Follow> {
    if toggles.is_empty() {
        return Ok(created);
    }
    let updated =
        follow::set_toggles(&state.db, created.follower_id, created.followee_id, toggles).await?;
    Ok(updated.unwrap_or(created))
}

/// The request parameters of `POST /api/v1/follows`: the required `uri`
/// beside the same two per-follow [`toggles`](FollowParams) the id-keyed
/// follow route reads, from the query string, a form body or a JSON body
/// alike ([`ApiParams`], decision 10). `uri` is optional in the type only
/// because its absence is the documented 400 the type cannot express (the
/// [`LookupQuery`] pattern); the toggles are the documented optionals,
/// each a [`flex::opt_bool`] so a form's `"1"` spells the same as JSON
/// `true` and anything else is the extractor's 422.
#[derive(Debug, Deserialize)]
struct FollowByUriParams {
    /// The account to follow: `user@domain`, `@user@domain` or an actor
    /// URL, resolved by [`resolve_follow_target`].
    #[serde(default)]
    uri: Option<String>,
    /// Mastodon's `reblogs`, the row's [`Follow::show_reblogs`] column.
    #[serde(default, deserialize_with = "flex::opt_bool")]
    reblogs: Option<bool>,
    /// Mastodon's `notify`, the row's [`Follow::notify`] column.
    #[serde(default, deserialize_with = "flex::opt_bool")]
    notify: Option<bool>,
}

impl FollowByUriParams {
    /// The toggles exactly as [`FollowParams::into_toggles`] reads them for
    /// the id-keyed route: an absent one keeps what the row holds. Not a
    /// `const fn` like that one, because this type owns the `uri` text.
    fn into_toggles(self) -> FollowToggles {
        FollowToggles {
            show_reblogs: self.reblogs,
            notify: self.notify,
        }
    }
}

/// `POST /api/v1/follows` — follow the account a `uri` names (Pleroma's
/// `.follow_by_uri`) and answer its [`Account`] entity rendered for the
/// caller, their [`Relationship`] embedded in `pleroma.relationship`.
///
/// The gates and the write are [`follow_account`]'s, applied to a target
/// resolved from the parameter instead of the path: the scope check first,
/// then the `uri` ([`MISSING_URI`] for an absent, blank or `null` one),
/// then [`resolve_follow_target`], and the very same
/// [`follow::follow`] + [`store_toggles`] sequence, the same log and the
/// same [`after_follow_changed`] hook — so the self refusal, the block
/// refusal and the locked-target request are the model's own answers.
/// Where Pleroma's controller hands off to the relationship-rendering
/// `follow` action, this legacy Mastodon endpoint answers the followed
/// account itself (decision: the ticket pins the [`Account`] entity).
///
/// # Errors
///
/// [`AppError::Unauthorized`] / [`AppError::Forbidden`] from [`AuthUser`]
/// and the `write:follows | follow` check; [`AppError::BadRequest`] for a
/// missing `uri`; [`AppError::NotFound`] when the `uri` resolves to no live
/// account; [`AppError::Unprocessable`] with `"Can not follow yourself"`
/// or the extractor's message when a toggle is not a boolean; the model's
/// [`AppError::Forbidden`] block refusal. Any database failure arrives
/// through `?` as [`AppError::Database`].
async fn follow_by_uri(
    State(state): State<AppState>,
    auth: AuthUser,
    ApiParams(params): ApiParams<FollowByUriParams>,
) -> AppResult<Json<Account>> {
    auth.require_any(&[WRITE_FOLLOWS, FOLLOW])?;
    let Some(uri) = params
        .uri
        .as_deref()
        .map(str::trim)
        .filter(|uri| !uri.is_empty())
    else {
        return Err(AppError::BadRequest(MISSING_URI.to_owned()));
    };
    let target = resolve_follow_target(&state, uri).await?;
    let toggles = params.into_toggles();
    let created = follow::follow(&state.db, &state.config.base_url, &auth.0, &target).await?;
    let stored = store_toggles(&state, created, toggles).await?;
    debug!(
        follow = %stored.id,
        user = %auth.0.id,
        target = %target.id,
        granted = stored.state == FollowState::Accepted,
        reblogs = stored.show_reblogs,
        notifying = stored.notify,
        "account followed by uri"
    );
    after_follow_changed(&state, &auth.0, &target, Some(&stored), true);
    Ok(Json(render_account(&state, Some(&auth.0), &target).await?))
}

/// Resolves the `uri` parameter of [`follow_by_uri`] to the target row:
/// an `http(s)` URL goes through [`get_or_fetch_by_ap_id`], anything else
/// is an `acct` handle — with a leading `@` or `acct:` prefix stripped —
/// through [`get_or_fetch_by_acct`]. Both answer from the stored rows
/// first, so a local account or a mirror resolves without the network,
/// and only an unseen handle reaches for `WebFinger` and the actor fetch.
/// Whatever the fetchers refuse — an unresolvable handle, an unreachable
/// or invalid actor, an unknown local URL — and a row that resolves but
/// is deactivated is the same shared [`AppError::NotFound`] the id-keyed
/// routes give through [`account_or_404`]; the fetcher's own reason is
/// logged, never sent (decision 8).
async fn resolve_follow_target(state: &AppState, uri: &str) -> AppResult<User> {
    let fetched = if looks_like_ap_id(uri) {
        get_or_fetch_by_ap_id(state, uri).await
    } else {
        get_or_fetch_by_acct(state, strip_finger_prefixes(uri)).await
    };
    let resolved = fetched.map_err(|err| {
        debug!(%err, uri, "follow target could not be resolved");
        AppError::NotFound
    })?;
    if resolved.is_active.get() {
        Ok(resolved)
    } else {
        Err(AppError::NotFound)
    }
}

/// `POST /api/v1/accounts/{id}/unfollow` — withdraw the caller's follow of the
/// account and answer their [`Relationship`] to it, `following: false` and
/// `requested: false` afterwards.
///
/// [`follow::unfollow`] takes back an accepted follow or a request of the
/// caller's that is still waiting, and is content with neither: unfollowing an
/// account the caller never followed (or one that already stopped following
/// them, or a `rejected` mirror) changes nothing and is the unchanged
/// [`Relationship`] with 200 — this ticket's idempotence, where Pleroma's
/// controller would answer "Not subscribed!". The usual 404 resolves the
/// account first, and unfollowing *yourself* is one of those no-op withdrawals
/// rather than an error, because no follow row of the pair can exist.
///
/// # Errors
///
/// Those of [`follow_account`] minus the parameter and self-follow refusals.
/// Any database failure arrives through `?` as [`AppError::Database`].
async fn unfollow_account(
    State(state): State<AppState>,
    auth: AuthUser,
    Path(id): Path<String>,
) -> AppResult<Json<Relationship>> {
    auth.require_any(&[WRITE_FOLLOWS, FOLLOW])?;
    let target = account_or_404(&state, &id).await?;
    // The row the delete removes is read while it still exists: the `Undo` the
    // federation hook will one day send names the `Follow` activity the
    // relationship holds, and `unfollow` reports only whether a row was there.
    let previous = follow::get(&state.db, auth.0.id, target.id).await?;
    let removed = follow::unfollow(&state.db, &auth.0, &target).await?;
    debug!(
        user = %auth.0.id,
        target = %target.id,
        removed,
        "follow withdrawn"
    );
    after_follow_changed(
        &state,
        &auth.0,
        &target,
        previous.as_ref().filter(|_| removed),
        false,
    );
    one_relationship(&state, auth.0.id, target.id).await
}

/// Post-write hook of [`follow_account`] and [`unfollow_account`], the landing
/// point of the `Follow` / `Undo` federation: the ticket that delivers them
/// wires the queueing in here (decision 11) — the activity to federate is
/// `follow`'s [`Follow::ap_id`], its sender [`User::ap_id`] and its target
/// [`Follow::followee_id`], and `following` states which way the write moved
/// the fact: `true` for the `Follow` a `…/follow` created or reopened, `false`
/// for the `Undo` of the `Follow` a `…/unfollow` withdrew.
///
/// `follow` is the relationship the write left behind (a `…/follow` always has
/// one; a `…/unfollow` names the row it deleted and is `None` for a pair with
/// nothing to take back, which is nothing to federate either).
///
/// Intentionally empty until then: like [`after_follower_removed`] and the
/// blocks' [`after_block_changed`](crate::api::blocks), it waits for the
/// delivery queue — Pleroma's `User.follow` / `User.unfollow` build and send
/// their activities inline, and this server's federation is a later ticket.
/// It is a plain `fn` rather than an `async` one for the reason the others
/// are: the gate denies the `unused_async` an empty `async fn` would be and
/// forbids silencing it, so the ticket that queues the `Follow` / `Undo`
/// delivery grows the body and the `async` together and leaves these two
/// call sites alone.
pub(crate) const fn after_follow_changed(
    _state: &AppState,
    _follower: &User,
    _target: &User,
    _follow: Option<&Follow>,
    _following: bool,
) {
}

/// `GET /api/v1/accounts/{id}/followers` — the accounts that follow `id`
/// with an accepted relationship, newest follow first, one [`PageParams`]
/// page at a time (limit 20 by default, at most
/// [`MAX_LIMIT`](crate::pagination::MAX_LIMIT)). Auth is optional; a
/// presented user token must grant `read:accounts`, as in Pleroma's plug
/// for this action, and the viewer it names decides whose [`Relationship`]
/// the entries embed.
///
/// Pleroma's `followers` `cond`: an account that hides its followers is
/// answered the empty array — 200, no `Link` header — unless the viewer is
/// that account itself; `hide_followers_count` alone does not hide the
/// list, only the counter the entities carry. A locked account's follower
/// list is listed as any other's, its unanswered requests left out by the
/// model's `accepted` filter.
///
/// The `Link` header's path is the request's own [`OriginalUri`], not the
/// [`Uri`] the router hands a nested handler: axum strips the `/api/v1`
/// prefix off the latter, and Pleroma links to `conn.request_path`.
///
/// # Errors
///
/// [`AppError::Forbidden`] from [`MaybeAuthUser::require_user_scope`] when
/// the authenticated token lacks `read:accounts`;
/// [`AppError::Unauthorized`] from the extractor for an invalid token;
/// [`AppError::NotFound`] for a malformed, unknown or deactivated account
/// id; [`AppError::BadRequest`] from [`PageParams`] for a malformed
/// cursor. Any database failure arrives through `?` as
/// [`AppError::Database`], and [`set_link_header`] reports a `Link` value
/// that is not header-safe as [`AppError::Internal`].
async fn list_followers(
    State(state): State<AppState>,
    auth: MaybeAuthUser,
    Path(id): Path<String>,
    page: PageParams,
    OriginalUri(uri): OriginalUri,
) -> AppResult<(HeaderMap, Json<Vec<Account>>)> {
    auth.require_user_scope(READ_ACCOUNTS)?;
    let target = account_or_404(&state, &id).await?;
    let viewer = auth.0.as_ref();
    if target.hide_followers.get() && !viewer_is(viewer, &target) {
        return Ok(empty_page());
    }
    let entries = follow::followers_page(&state.db, target.id, page).await?;
    page_response(&state, viewer, &entries, uri.path()).await
}

/// `GET /api/v1/accounts/{id}/following` — the accounts `id` follows with
/// an accepted relationship, newest follow first. The exact counterpart
/// of [`list_followers`]: the same optional auth and `read:accounts`
/// gate, the same [`PageParams`] keyset pagination on the `follows` row,
/// and Pleroma's `following` `cond` hiding the list behind
/// [`hide_follows`](User::hide_follows) from everyone but the account
/// itself.
///
/// # Errors
///
/// Exactly those of [`list_followers`].
async fn list_following(
    State(state): State<AppState>,
    auth: MaybeAuthUser,
    Path(id): Path<String>,
    page: PageParams,
    OriginalUri(uri): OriginalUri,
) -> AppResult<(HeaderMap, Json<Vec<Account>>)> {
    auth.require_user_scope(READ_ACCOUNTS)?;
    let target = account_or_404(&state, &id).await?;
    let viewer = auth.0.as_ref();
    if target.hide_follows.get() && !viewer_is(viewer, &target) {
        return Ok(empty_page());
    }
    let entries = follow::following_page(&state.db, target.id, page).await?;
    page_response(&state, viewer, &entries, uri.path()).await
}

/// `GET /api/v1/accounts/{id}/statuses` — one account's posting history as
/// a page of [`StatusEntity`] rows, newest first: the statuses it authored
/// and its own reblogs — Pleroma's `statuses` action over
/// `ActivityPub.fetch_user_activities/3`, which reads `Create` and
/// `Announce` alike, so `exclude_reblogs` (not Mastodon's default-off
/// `with_reblogs`) is what takes the boosts out. Auth is optional
/// ([`MaybeAuthUser`] with `read:statuses` enforced on a presented user
/// token, Pleroma's scopes plug with `fallback: :proceed_unauthenticated`),
/// and the page every viewer reaches is judged row by row by
/// [`push_visibility_clause`]: a stranger (or an anonymous request) sees
/// `public` and `unlisted`, a follower adds `private`, a mentioned party
/// adds `direct`, and the account behind the token sees everything of its
/// own. An account that blocked the viewer answers the empty list — the
/// visibility clause's rule, not a check of its own — and a malformed,
/// unknown or deactivated id is the shared 404.
///
/// The read is [`TimelineSource::Account`] over the shared
/// [`crate::services::timeline`] builder, narrowed by the request's filters
/// ([`StatusesParams`]) and answered by [`timeline_response`], so its
/// `Link` header repeats the filters and drops the cursors exactly as the
/// `/api/v1/timelines/*` routes do. `pinned=true` is the one exception: it
/// ignores the pagination keys, answers the account's whole pinned set in
/// the order its profile features it (newest pin first) and sends no `Link`
/// header at all.
///
/// While `RESTRICT_UNAUTHENTICATED` names the account's profile area —
/// `profiles_local` for this instance's own, `profiles_remote` for a mirror
/// — an unauthenticated request is the 403 [`USER_REQUIRED`]; a presented
/// user token is never refused for it (see [`require_profile_read`]).
///
/// The `Link` header's path is the request's own [`OriginalUri`], not the
/// [`Uri`] the router hands a nested handler: axum strips the `/api/v1`
/// prefix off the latter, and Pleroma links to `conn.request_path`.
///
/// # Errors
///
/// [`AppError::Forbidden`] from [`MaybeAuthUser::require_user_scope`] when
/// the authenticated token lacks `read:statuses`, and from
/// [`require_profile_read`] for a restricted area behind an anonymous
/// request; [`AppError::Unauthorized`] from the extractor for an invalid
/// token; [`AppError::NotFound`] for a malformed, unknown or deactivated
/// account id; [`AppError::BadRequest`] from [`PageParams`] for a malformed
/// cursor. Any database failure arrives through `?` as
/// [`AppError::Database`], and [`set_link_header`] reports a `Link` value
/// that is not header-safe as [`AppError::Internal`].
///
/// [`push_visibility_clause`]: crate::models::status_visibility::push_visibility_clause
/// [`set_link_header`]: crate::pagination::set_link_header
async fn list_statuses(
    State(state): State<AppState>,
    auth: MaybeAuthUser,
    Path(id): Path<String>,
    page: PageParams,
    OriginalUri(uri): OriginalUri,
) -> AppResult<(HeaderMap, Json<Vec<StatusEntity>>)> {
    auth.require_user_scope(READ_STATUSES)?;
    let target = account_or_404(&state, &id).await?;
    let viewer = auth.0;
    require_profile_read(&state.config, viewer.as_ref(), &target)?;
    let query = StatusesParams::parse(uri.query()).into_timeline(target.id, viewer, page);
    if query.filters.pinned_only.get() {
        pinned_response(&state, &query, target.id).await
    } else {
        timeline_response(&state, &query, &uri).await
    }
}

/// The `RESTRICT_UNAUTHENTICATED` gate of the account's status listing: an
/// authenticated request always passes — the restriction is about the
/// anonymous reader, as Pleroma's `visible_for` is — while an anonymous one
/// is the 403 [`USER_REQUIRED`] when this instance keeps the account's
/// profile area closed to it (see [`profile_area`] and
/// [`Config::restricts`]).
///
/// # Errors
///
/// [`AppError::Forbidden`] as described.
fn require_profile_read(config: &Config, viewer: Option<&User>, target: &User) -> AppResult<()> {
    if viewer.is_some() || !config.restricts(profile_area(target)) {
        return Ok(());
    }
    Err(AppError::Forbidden(USER_REQUIRED.to_owned()))
}

/// The `RESTRICT_UNAUTHENTICATED` profile half `target` belongs to:
/// [`PROFILES_LOCAL`] when this instance authored the account,
/// [`PROFILES_REMOTE`] when it only mirrors it — decision 7's `local` column
/// is the whole test.
const fn profile_area(target: &User) -> &'static str {
    if target.local.get() {
        PROFILES_LOCAL
    } else {
        PROFILES_REMOTE
    }
}

/// The non-cursor query parameters of `GET /api/v1/accounts/{id}/statuses`:
/// Pleroma's `statuses_operation` — the five booleans, the one tag name and
/// the repeated visibility exclusion — every one a no-op in its [`Default`]
/// spelling, the cursors and `limit` belonging to [`PageParams`]. The
/// booleans are [`Flag`]s because that is what this repository models every
/// boolean with (see [`crate::models::user`]). `only_reblogs`, which
/// Pleroma's own spec lists beside these, is not a parameter of this
/// endpoint's ticket and is never read.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
struct StatusesParams {
    /// `pinned`: only the account's pinned statuses, in pin order, off the
    /// pagination keys.
    pinned: Flag,
    /// `only_media`: keep only statuses carrying an attachment.
    only_media: Flag,
    /// `exclude_replies`: drop rows that answer another status.
    exclude_replies: Flag,
    /// `exclude_reblogs`: drop the account's reblog rows.
    exclude_reblogs: Flag,
    /// `with_muted`: skip the viewer's block/mute exclusions (Pleroma's
    /// `with_muted`, default `false`).
    with_muted: Flag,
    /// `tagged`: keep only statuses carrying this tag, in the spelling the
    /// `hashtags` column stores; see [`tagged_name`] for the value a request
    /// that names nothing usable gets. `None` when the parameter is absent.
    tagged: Option<String>,
    /// `exclude_visibilities[]`: drop rows whose visibility is one of these;
    /// names that are not one of the five stored spellings are ignored.
    exclude_visibilities: Vec<Visibility>,
}

impl StatusesParams {
    /// Reads the parameters out of a raw query string (what `Uri::query`
    /// returns, percent-decoding and all), the same reading the timeline
    /// routes give: booleans through [`is_true`], `exclude_visibilities` in
    /// both the bracketed and the bare spelling through [`push_visibility`],
    /// `tagged` through [`tagged_name`]. The last occurrence of a key wins;
    /// an unrelated key is never looked at.
    fn parse(query: Option<&str>) -> Self {
        let mut found = Self::default();
        for (key, value) in form_urlencoded::parse(query.unwrap_or_default().as_bytes()) {
            match key.as_ref() {
                "pinned" => found.pinned = Flag::new(is_true(&value)),
                "only_media" => found.only_media = Flag::new(is_true(&value)),
                "exclude_replies" => found.exclude_replies = Flag::new(is_true(&value)),
                "exclude_reblogs" => found.exclude_reblogs = Flag::new(is_true(&value)),
                "with_muted" => found.with_muted = Flag::new(is_true(&value)),
                "tagged" => found.tagged = Some(tagged_name(&value)),
                "exclude_visibilities" | "exclude_visibilities[]" => {
                    push_visibility(&mut found.exclude_visibilities, &value);
                }
                _ => {}
            }
        }
        found
    }

    /// Assembles the [`TimelineQuery`] the request asks for:
    /// [`TimelineSource::Account`] of `user_id` as `viewer` (`None` for an
    /// anonymous read), narrowed by these parameters and paged by `page` —
    /// except under `pinned`, which reads the whole pinned set on a page of
    /// its own (see [`every_pin`]).
    fn into_timeline(self, user_id: Uuid, viewer: Option<User>, page: PageParams) -> TimelineQuery {
        let pinned = self.pinned;
        TimelineQuery {
            viewer,
            source: TimelineSource::Account { user_id },
            filters: TimelineFilters {
                only_media: self.only_media,
                exclude_replies: self.exclude_replies,
                exclude_reblogs: self.exclude_reblogs,
                with_muted: self.with_muted,
                tagged: self.tagged,
                exclude_visibilities: self.exclude_visibilities,
                pinned_only: pinned,
                ..TimelineFilters::default()
            },
            page: if pinned.get() { every_pin() } else { page },
        }
    }
}

/// The `tagged` parameter in the spelling the `hashtags` column stores (see
/// [`normalize_tag`]). A value that leaves nothing usable — `tagged=`,
/// `tagged=%23` — becomes the empty name, which no stored tag can carry:
/// such a request is answered the empty page, not the unfiltered one.
fn tagged_name(raw: &str) -> String {
    normalize_tag(raw).unwrap_or_default()
}

/// The page `pinned` reads: every pin of the account, with no cursor and no
/// limit. The pagination keys are ignored because the set they would page
/// through is already bounded by [`max_pinned_statuses`] — the whole
/// profile's featured list is one page (see
/// [`list_for_user`](crate::models::pinned_status::list_for_user), the
/// model read this mirrors).
///
/// [`max_pinned_statuses`]: Config::max_pinned_statuses
const fn every_pin() -> PageParams {
    PageParams {
        max_id: None,
        min_id: None,
        since_id: None,
        limit: i64::MAX,
        offset: None,
    }
}

/// The `pinned` answer: the account's pins in the order its profile
/// features them — newest pin first, no `Link` header, because there is no
/// older page of a list that never pages (Pleroma's `pinned` branch adds
/// its restriction to the same fetch, so the request's other filters still
/// apply).
///
/// # Errors
///
/// Any database failure arrives through `?` as [`AppError::Database`].
async fn pinned_response(
    state: &AppState,
    query: &TimelineQuery,
    user_id: Uuid,
) -> AppResult<(HeaderMap, Json<Vec<StatusEntity>>)> {
    let rows = query.fetch(&state.db).await?;
    let rows = ordered_by_pin(state, user_id, rows).await?;
    let statuses = render_statuses(state, query.viewer.as_ref(), &rows).await?;
    Ok((HeaderMap::new(), Json(statuses)))
}

/// `rows` in the account's pin order:
/// [`pinned_ids`](crate::models::pinned_status::pinned_ids) reads each row's
/// pin time and the sort applies the order
/// [`crate::models::pinned_status`]'s `LIST_SQL` states — newest pin first,
/// the status id breaking a same-instant tie. A row with no pin (the query
/// cannot answer one, but the comparator does not assume it) sorts by that
/// same id order, so the sort stays total.
///
/// # Errors
///
/// Any database failure arrives through `?` as [`AppError::Database`].
async fn ordered_by_pin(
    state: &AppState,
    user_id: Uuid,
    mut rows: Vec<StatusRow>,
) -> AppResult<Vec<StatusRow>> {
    let ids: Vec<Uuid> = rows.iter().map(|row| row.id).collect();
    let pins = pinned_status::pinned_ids(&state.db, user_id, &ids).await?;
    rows.sort_by(|a, b| match (pins.get(&a.id), pins.get(&b.id)) {
        (Some(a_at), Some(b_at)) if a_at != b_at => b_at.cmp(a_at),
        _ => b.id.cmp(&a.id),
    });
    Ok(rows)
}

/// Renders `found` for `viewer` (who may be `None`), preloading the
/// viewer's relationship to the account when there is one to embed: a
/// viewer who is not the account itself, answered by a single-target
/// [`Relationship::load_many`]. Anyone else — anonymous, self — renders
/// with no preload, which keeps `pleroma.relationship` the empty object.
/// A stored move target is attached as the nested `moved` entity (see
/// [`with_moved`]).
async fn render_account(
    state: &AppState,
    viewer: AccountViewer<'_>,
    found: &User,
) -> AppResult<Account> {
    let relationships = preload_relationship(state, viewer, found).await?;
    let account = Account::from_user_for(viewer, found, &state.config, relationships.as_ref());
    with_moved(state, account, found).await
}

/// The single-account [`RelationshipMap`] for the `show`/`lookup` view:
/// [`Some`] only for a viewer looking at someone else, `None` when there
/// is nothing to embed (and so nothing to query).
async fn preload_relationship(
    state: &AppState,
    viewer: AccountViewer<'_>,
    found: &User,
) -> AppResult<Option<RelationshipMap>> {
    let Some(who) = viewer.filter(|who| who.id != found.id) else {
        return Ok(None);
    };
    let loaded =
        Relationship::load_many(&state.db, who.id, std::slice::from_ref(&found.id)).await?;
    let relationship = loaded.into_iter().next();
    Ok(relationship.map(|rel| RelationshipMap::from([(found.id, rel)])))
}

/// Whether `viewer` is looking at the account `target` itself — Pleroma's
/// `user.id == for_user.id` check in the `followers`/`following` `cond`,
/// by primary key. An anonymous request is never the target.
fn viewer_is(viewer: AccountViewer<'_>, target: &User) -> bool {
    viewer.is_some_and(|who| who.id == target.id)
}

/// The answer to a hidden relationship listing: the empty array as a plain
/// 200 with no `Link` header at all — Pleroma's `[]` branch, where the
/// page never got a cursor to link from.
fn empty_page() -> (HeaderMap, Json<Vec<Account>>) {
    (HeaderMap::new(), Json(Vec::new()))
}

/// Renders one fetched relationship page for `viewer`: every entry's
/// [`User`] as an [`Account`] with the viewer's relationship embedded
/// where one is preloaded (see [`preload_page_relationships`]), under the
/// `Link` header [`page_link`] derives from the relationship ids.
async fn page_response(
    state: &AppState,
    viewer: AccountViewer<'_>,
    entries: &[follow::FollowPageEntry],
    path: &str,
) -> AppResult<(HeaderMap, Json<Vec<Account>>)> {
    let relationships = preload_page_relationships(state, viewer, entries).await?;
    let link = page_link(&state.config.base_url, path, entries);
    let mut headers = HeaderMap::new();
    set_link_header(&mut headers, link.as_deref())?;
    let accounts = entries
        .iter()
        .map(|entry| {
            Account::from_user_for(viewer, &entry.user, &state.config, relationships.as_ref())
        })
        .collect();
    Ok((headers, Json(accounts)))
}

/// The [`RelationshipMap`] for one relationship page: the viewer's
/// relationship to every listed account in a single batched
/// [`Relationship::load_many`], keyed by target id (which is how
/// [`Account::from_user_for`] reads its preload). [`None`] only for an
/// anonymous viewer, whose entries keep Pleroma's empty
/// `pleroma.relationship` object and so need no query at all.
async fn preload_page_relationships(
    state: &AppState,
    viewer: AccountViewer<'_>,
    entries: &[follow::FollowPageEntry],
) -> AppResult<Option<RelationshipMap>> {
    let Some(who) = viewer else {
        return Ok(None);
    };
    let targets: Vec<Uuid> = entries.iter().map(|entry| entry.user.id).collect();
    let loaded = Relationship::load_many(&state.db, who.id, &targets).await?;
    let map = loaded
        .into_iter()
        .zip(targets)
        .map(|(relationship, target)| (target, relationship))
        .collect();
    Ok(Some(map))
}

/// The `Link` header value for a fetched relationship page: Pleroma's
/// `add_link_headers` over the first (newest) and last (oldest) entry's
/// [`follow::FollowPageEntry::follow_id`] — the cursors key on the
/// `follows` row, not on the users — and `None` for an empty page.
fn page_link(base_url: &str, path: &str, entries: &[follow::FollowPageEntry]) -> Option<String> {
    let newest = entries.first()?.follow_id.to_string();
    let oldest = entries.last()?.follow_id.to_string();
    link_header(base_url, path, &NO_EXTRA_QUERY, &newest, &oldest)
}

#[cfg(test)]
mod tests {
    use serde_json::json;

    use super::*;

    /// Builds a `MentionRef` for the key and domain helpers.
    fn mention(nickname: &str, domain: Option<&str>) -> MentionRef {
        MentionRef {
            nickname: nickname.to_owned(),
            domain: domain.map(str::to_owned),
        }
    }

    #[test]
    fn display_name_limit_is_counted_in_characters() {
        assert_eq!(
            sanitized_name("abcdé", 5).unwrap(),
            "abcdé",
            "five characters fit, bytes are not counted"
        );
        let err = sanitized_name("abcdeé", 5).unwrap_err();
        assert_eq!(
            err.to_string(),
            "Name is too long (maximum is 5 characters)",
            "the pinned wording carries the configured limit"
        );
    }

    #[test]
    fn display_name_is_reduced_to_one_line_of_text() {
        assert_eq!(
            sanitized_name("  <b>Bob</b><br>the Builder ", 50).unwrap(),
            "Bob the Builder",
            "markup and control characters collapse to spaces"
        );
        assert_eq!(
            sanitized_name("", 50).unwrap(),
            "",
            "the empty string is stored empty, not reset to the nickname"
        );
    }

    #[test]
    fn mention_keys_fold_case_across_domains() {
        assert_eq!(mention_key(&mention("Alice", None)), "alice", "a bare nick");
        assert_eq!(
            mention_key(&mention("Bob", Some("Remote.Example"))),
            "bob@remote.example",
            "the spelled-out domain joins the key, lowercased"
        );
    }

    #[test]
    fn only_bare_or_own_domain_mentions_can_be_local() {
        let config = Config::for_tests();
        assert!(
            names_own_domain(&config, &mention("alice", None)),
            "a bare nick may name a local user"
        );
        assert!(
            names_own_domain(&config, &mention("alice", Some("TEST.example"))),
            "and so may one spelled with this instance's own domain"
        );
        assert!(
            !names_own_domain(&config, &mention("bob", Some("elsewhere.example"))),
            "another domain never names a local user"
        );
    }

    /// Magic-byte fixtures for the image slots: a header is all [`infer`]
    /// reads, and the endpoint never decodes a profile image.
    const PNG: &[u8] = b"\x89PNG\r\n\x1a\n\x00\x00\x00\rIHDR\x00\x00\x00\x01";
    /// A `JPEG`: the `SOI` marker `infer` matches on.
    const JPEG: &[u8] = b"\xFF\xD8\xFF\xE0\x00\x10JFIF\x00\x01\x01\x00\x00\x01";
    /// A `GIF`: the versioned header, `GIF89a`.
    const GIF: &[u8] = b"GIF89a\x01\x00\x01\x00\x80\x00\x00";
    /// A `WebP`: the `RIFF` container with the `WEBP` and `VP8` tags.
    const WEBP: &[u8] = b"RIFF\x24\x00\x00\x00WEBPVP8 ";
    /// A `BMP`: an image, just not one of the four the endpoint accepts.
    const BMP: &[u8] = b"BM\x36\x00\x00\x00\x00\x00\x36\x00\x00\x00";
    /// A `PDF`: bytes with a type that is not an image at all.
    const PDF: &[u8] = b"%PDF-1.4\n1 0 obj\n<<>>\nendobj\ntrailer\n<<>>\n%%EOF\n";
    /// Text: bytes no matcher can name.
    const TEXT: &[u8] = b"a plain text file, definitely not an image\n";

    /// A file part named `field` holding `bytes`, as the multipart extractor
    /// hands one to the endpoint. The client's claimed `content_type` is
    /// `None` where the test does not care: what is stored never comes from
    /// it.
    fn part(field: &str, bytes: &[u8]) -> UploadedFile {
        UploadedFile {
            field: field.to_owned(),
            filename: Some("picture.bmp".to_owned()),
            content_type: Some("image/png".to_owned()),
            bytes: Bytes::copy_from_slice(bytes),
        }
    }

    /// The parameters of a body, parsed the way the extractor hands them
    /// over: `json!` text for the JSON spellings, a map of strings for the
    /// form and multipart ones.
    fn params(text: &str) -> ProfileParams {
        serde_json::from_str(text).expect("the fixture body parses")
    }

    /// The empty body, for the tests that only pass a file part.
    fn no_params() -> ProfileParams {
        params("{}")
    }

    /// Resolves `value` as the `avatar` slot with no file part, `unwrap`ing
    /// the success or naming the rejection the test asserts on.
    fn avatar_slot(value: Option<&str>) -> Result<ImageWrite, AppError> {
        let parsed: Option<Value> =
            value.map(|text| serde_json::from_str(text).expect("the fixture value parses"));
        let mut files = Vec::new();
        resolve_image(
            &mut files,
            AVATAR_FIELD,
            parsed.as_ref(),
            Config::for_tests().avatar_limit_bytes,
        )
    }

    #[test]
    fn an_absent_image_parameter_keeps_the_stored_column() {
        assert!(
            matches!(avatar_slot(None).unwrap(), ImageWrite::Unchanged),
            "no key in the body is no write"
        );
    }

    #[test]
    fn null_and_empty_image_parameters_clear_the_column() {
        // The two spellings of Pleroma's reset: JSON `null` and the empty
        // string a form or multipart text part carries.
        for value in [Some("null"), Some(r#""""#)] {
            assert!(
                matches!(avatar_slot(value).unwrap(), ImageWrite::Clear),
                "`{value:?}` clears"
            );
        }
        let body = params(r#"{"avatar": "", "header": null}"#);
        let mut files = Vec::new();
        let writes = resolve_images(&Config::for_tests(), &mut files, &body).unwrap();
        assert!(
            matches!(writes.avatar, ImageWrite::Clear)
                && matches!(writes.banner, ImageWrite::Clear)
                && matches!(writes.background, ImageWrite::Unchanged),
            "the two sent slots clear, the third stays"
        );
    }

    #[test]
    fn a_url_string_is_not_an_image_the_endpoint_can_store() {
        for value in [Some(r#""https://elsewhere.example/a.png""#), Some("[1,2]")] {
            let err = avatar_slot(value).expect_err("a string is no upload");
            assert_eq!(
                err.to_string(),
                UNSUPPORTED_FILE_TYPE,
                "`{value:?}` is refused with the type message"
            );
        }
    }

    #[test]
    fn kept_value_tells_absent_from_null() {
        let absent = no_params();
        assert_eq!(absent.avatar, None, "an absent key is no parameter");
        let nulled = params(r#"{"avatar": null}"#);
        assert_eq!(nulled.avatar, Some(Value::Null), "a JSON null is one");
        let blank = params(r#"{"avatar": ""}"#);
        assert_eq!(
            blank.avatar,
            Some(Value::String(String::new())),
            "and so is the form spelling"
        );
    }

    #[test]
    fn a_file_part_wins_over_the_text_spelling_of_its_field() {
        let mut files = vec![part(AVATAR_FIELD, PNG)];
        let writes = resolve_images(
            &Config::for_tests(),
            &mut files,
            &params(r#"{"avatar": ""}"#),
        )
        .unwrap();
        assert!(
            matches!(writes.avatar, ImageWrite::Store { .. }),
            "the uploaded file is what the slot reads"
        );
        assert!(
            files.is_empty(),
            "and the part is taken out of the body's files"
        );
    }

    #[test]
    fn the_four_accepted_image_types_are_read_from_the_bytes() {
        for (bytes, mime) in [
            (PNG, "image/png"),
            (JPEG, "image/jpeg"),
            (GIF, "image/gif"),
            (WEBP, "image/webp"),
        ] {
            assert_eq!(
                detect_image_type(bytes).unwrap(),
                mime,
                "the magic bytes name {mime}"
            );
        }
    }

    #[test]
    fn anything_else_is_the_unsupported_file_type() {
        // `BMP` proves the allowlist, not merely the "is it an image"
        // question: `infer` names it, and the endpoint still refuses it.
        for bytes in [BMP, PDF, TEXT, b""] {
            let err = detect_image_type(bytes).expect_err("no profile image");
            assert_eq!(
                err.to_string(),
                UNSUPPORTED_FILE_TYPE,
                "bytes {bytes:?} are refused with the type message"
            );
        }
    }

    #[test]
    fn the_part_content_type_claim_is_ignored() {
        // The extractor's `content_type` says `image/png` for every fixture
        // part; what decides is the bytes, so a text body under an image
        // claim is still the 422.
        let mut files = vec![part(AVATAR_FIELD, TEXT)];
        let err = resolve_images(&Config::for_tests(), &mut files, &no_params())
            .expect_err("a text file is no avatar");
        assert_eq!(err.to_string(), UNSUPPORTED_FILE_TYPE);
    }

    #[test]
    fn each_slot_is_measured_against_its_own_limit() {
        // [`PNG`] is 20 bytes: over the 8-byte avatar cap the fixture sets,
        // under the 32-byte banner one — which is also what measures the
        // background image.
        let config = Config {
            avatar_limit_bytes: 8,
            banner_limit_bytes: 32,
            ..Config::for_tests()
        };
        let err = resolve_one(&config, AVATAR_FIELD).expect_err("the avatar cap is 8 bytes");
        assert_eq!(
            err.to_string(),
            FILE_TOO_LARGE,
            "the avatar is over its own cap"
        );
        for field in [HEADER_FIELD, BACKGROUND_FIELD] {
            assert!(
                matches!(
                    resolve_one(&config, field).unwrap(),
                    ImageWrite::Store { .. }
                ),
                "{field} fits the banner cap"
            );
        }
    }

    /// Resolves `field` as the whole of a body: one [`PNG`] part under that
    /// name, no text parameters, measured by `config`'s limits.
    fn resolve_one(config: &Config, field: &str) -> AppResult<ImageWrite> {
        let mut files = vec![part(field, PNG)];
        resolve_images(config, &mut files, &no_params()).map(|writes| write_for(writes, field))
    }

    /// The slot of `writes` that arrived under `field`, for the tests that
    /// resolve one field at a time.
    fn write_for(writes: ImageWrites, field: &str) -> ImageWrite {
        if field == AVATAR_FIELD {
            writes.avatar
        } else if field == HEADER_FIELD {
            writes.banner
        } else {
            writes.background
        }
    }

    #[test]
    fn an_unrelated_file_part_is_ignored_and_no_file_is_stored() {
        let mut files = vec![part("file", PNG)];
        let writes = resolve_images(&Config::for_tests(), &mut files, &no_params()).unwrap();
        assert!(
            matches!(
                (writes.avatar, writes.banner, writes.background),
                (
                    ImageWrite::Unchanged,
                    ImageWrite::Unchanged,
                    ImageWrite::Unchanged
                )
            ),
            "a file under another field name writes no column"
        );
        assert_eq!(files.len(), 1, "and the part is left where it was");
    }

    #[test]
    fn replaced_urls_are_only_named_by_writes_that_overwrote_one() {
        let old = "https://test.example/media/alice/a.png";
        let new = "https://test.example/media/alice/b.png";
        assert_eq!(
            replaced_url(None, None),
            None,
            "nothing stored, nothing gone"
        );
        assert_eq!(
            replaced_url(Some(old), Some(old)),
            None,
            "the same URL still uses its file"
        );
        let replaced = replaced_url(Some(old), Some(new));
        assert_eq!(
            replaced.as_deref(),
            Some(old),
            "a replacement names the old"
        );
        let cleared = replaced_url(Some(old), None);
        assert_eq!(
            cleared.as_deref(),
            Some(old),
            "a clear names the stored one"
        );
    }

    #[test]
    fn only_this_instances_own_media_urls_name_a_file_we_hold() {
        let config = Config::for_tests();
        assert_eq!(
            own_media_key(&config, "https://test.example/media/alice/a.png").as_deref(),
            Some("alice/a.png"),
            "our own media URL is a key"
        );
        for stored_url in [
            "",
            "https://elsewhere.example/media/alice/a.png",
            "https://test.example/media",
            "https://test.example/mediazonk/a.png",
            "https://test.example/media/../escape.png",
            "https://test.example/media/alice//a.png",
        ] {
            assert_eq!(
                own_media_key(&config, stored_url),
                None,
                "`{stored_url}` names no file of ours",
            );
        }
        assert_eq!(
            own_media_key(&config, "https://test.example/media/"),
            None,
            "an empty remainder is no key either"
        );
    }

    /// The same day every `merged_birthday` test measures "the future"
    /// against, so the assertions do not drift with the clock.
    const TODAY: &str = "2026-02-28";

    /// `TODAY` as the date [`merged_birthday`] compares against.
    fn today() -> NaiveDate {
        NaiveDate::parse_from_str(TODAY, BIRTHDAY_FORMAT).expect("the fixture date is valid")
    }

    /// The stored row the merge helpers are tested against: the column
    /// defaults, with a few values set apart — `private`, a tag, a
    /// two-key settings blob, one date, and two on-by-default flags — so a
    /// merge that ignored the stored state cannot pass by accident.
    fn stored_user() -> User {
        User {
            id: Uuid::now_v7(),
            nickname: "alice".to_owned(),
            ap_id: "https://test.example/users/alice".to_owned(),
            local: Flag::new(true),
            domain: None,
            actor_type: DEFAULT_ACTOR_TYPE.to_owned(),
            email: None,
            password_hash: None,
            name: String::new(),
            bio: String::new(),
            raw_bio: String::new(),
            avatar_url: None,
            banner_url: None,
            profile_url: None,
            locked: Flag::new(false),
            bot: Flag::new(false),
            discoverable: Flag::new(false),
            fields: json!([]),
            raw_fields: json!([]),
            inbox: None,
            shared_inbox: None,
            outbox: None,
            followers_url: None,
            following_url: None,
            featured_url: None,
            public_key: None,
            private_key: None,
            is_admin: Flag::new(false),
            is_moderator: Flag::new(false),
            is_active: Flag::new(true),
            hide_followers: Flag::new(true),
            hide_follows: Flag::new(false),
            hide_followers_count: Flag::new(false),
            hide_follows_count: Flag::new(false),
            hide_favorites: Flag::new(true),
            allow_following_move: Flag::new(true),
            skip_thread_containment: Flag::new(false),
            accepts_chat_messages: Flag::new(true),
            show_role: Flag::new(true),
            no_rich_text: Flag::new(false),
            default_scope: "private".to_owned(),
            sensitive_default: Flag::new(false),
            language: Some("pt-BR".to_owned()),
            tags: Vec::new(),
            settings_store: json!({"theme": "dark", "locale": "pt"}),
            notification_settings: json!({}),
            background_image_url: None,
            birthday: Some(NaiveDate::from_ymd_opt(1990, 1, 31).expect("a real day")),
            show_birthday: Flag::new(false),
            also_known_as: Vec::new(),
            moved_to_ap_id: None,
            last_move_at: None,
            email_confirmed: Flag::new(true),
            approval_pending: Flag::new(false),
            registration_reason: None,
            last_active_at: None,
            followers_count: 0,
            following_count: 0,
            statuses_count: 0,
            last_status_at: None,
            last_fetched_at: None,
            inserted_at: Utc::now(),
            updated_at: Utc::now(),
            mfa_settings: json!({}),
            preferred_frontend: None,
            is_suggested: Flag::new(false),
            password_reset_pending: Flag::new(false),
            deleted_at: None,
        }
    }

    /// The `source` block of a body, `unwrap`ing the parse.
    fn source_params(text: &str) -> ProfileParams {
        params(&format!(r#"{{"source": {text}}}"#))
    }

    #[test]
    fn the_source_block_reads_both_spellings() {
        let json_body = source_params(r#"{"privacy": "unlisted", "sensitive": "1"}"#);
        let nested = json_body
            .source
            .as_ref()
            .expect("the JSON object is the block");
        assert_eq!(nested.privacy.as_deref(), Some("unlisted"), "privacy");
        assert_eq!(nested.sensitive, Some(true), "the string spelling coerces");
        assert_eq!(nested.language, None, "an absent key is no parameter");

        let form = crate::extract::parse_pairs([
            ("source[privacy]", "direct"),
            ("source[sensitive]", "off"),
            ("source[language]", "de"),
        ]);
        let from_form: ProfileParams =
            serde_json::from_value(form).expect("the form nests into the same shape");
        let block = from_form
            .source
            .as_ref()
            .expect("`source[privacy]` is the block");
        assert_eq!(block.privacy.as_deref(), Some("direct"), "the form privacy");
        assert_eq!(block.sensitive, Some(false), "the form sensitive");
        assert_eq!(block.language.as_deref(), Some("de"), "the form language");
    }

    #[test]
    fn privacy_is_a_scope_or_the_422() {
        assert_eq!(
            merged_scope(Some("direct"), "public").unwrap(),
            "direct",
            "a stated scope wins over the stored one"
        );
        assert_eq!(
            merged_scope(None, "private").unwrap(),
            "private",
            "no parameter keeps the stored scope"
        );
        for text in ["internal", "", "PUBLIC", "unlisted "] {
            let err = merged_scope(Some(text), "public").expect_err("a scope outside the list");
            assert_eq!(err.to_string(), PRIVACY_INVALID, "the pinned wording");
        }
    }

    #[test]
    fn a_language_tag_is_capped_and_the_empty_one_clears() {
        assert_eq!(
            merged_language(Some("pt-BR"), Some("de"))
                .unwrap()
                .as_deref(),
            Some("pt-BR"),
            "a submitted tag replaces the stored one"
        );
        assert_eq!(
            merged_language(Some(""), Some("pt-BR")).unwrap(),
            None,
            "the empty string clears the preference"
        );
        assert_eq!(
            merged_language(None, Some("pt-BR")).unwrap().as_deref(),
            Some("pt-BR"),
            "and no parameter keeps it"
        );
        assert_eq!(
            merged_language(None, None).unwrap(),
            None,
            "nothing stated is nothing stored"
        );
        let at_cap = "é".repeat(MAX_LANGUAGE_CHARS);
        assert_eq!(
            merged_language(Some(at_cap.as_str()), None)
                .unwrap()
                .as_deref(),
            Some(at_cap.as_str()),
            "exactly the cap fits, counted in characters, not bytes"
        );
        let err = merged_language(Some("0123456789a"), None).expect_err("eleven is over the cap");
        assert_eq!(
            err.to_string(),
            "Language is too long (maximum is 10 characters)",
            "the length message names the cap"
        );
    }

    #[test]
    fn a_birthday_is_a_past_date_or_the_clear_or_the_422() {
        let stored = today().pred_opt().expect("yesterday exists");
        assert_eq!(
            merged_birthday(Some(TODAY), Some(stored), today()).unwrap(),
            Some(today()),
            "today is not in the future"
        );
        assert_eq!(
            merged_birthday(Some("1990-01-31"), Some(stored), today()).unwrap(),
            NaiveDate::from_ymd_opt(1990, 1, 31),
            "a submitted date replaces the stored one"
        );
        assert_eq!(
            merged_birthday(None, Some(stored), today()).unwrap(),
            Some(stored),
            "no parameter keeps the stored date"
        );
        assert_eq!(
            merged_birthday(Some(""), Some(stored), today()).unwrap(),
            None,
            "the empty string clears it"
        );
        for text in ["1990-1-31", "31-01-1990", "1990-13-01", "yesterday"] {
            let err = merged_birthday(Some(text), None, today()).expect_err("not a birthday");
            assert_eq!(err.to_string(), BIRTHDAY_INVALID, "the parse message");
        }
        for text in ["2026-03-01", "2099-01-01"] {
            let err = merged_birthday(Some(text), None, today()).expect_err("a future birthday");
            assert_eq!(err.to_string(), BIRTHDAY_FUTURE, "the future message");
        }
    }

    #[test]
    fn the_settings_store_merges_key_by_key() {
        let stored = stored_user();
        let merged = merged_store(
            Some(&json!({"locale": "de", "theme": null})),
            &stored.settings_store,
        )
        .expect("a small object fits");
        assert_eq!(
            merged,
            json!({"locale": "de", "theme": Value::Null}),
            "submitted keys replace, a null is stored as null, and only the merged keys change"
        );
        assert_eq!(
            merged_store(None, &stored.settings_store).unwrap(),
            stored.settings_store,
            "no parameter keeps the stored blob"
        );
        let from_text = merged_store(
            Some(&json!(r#"{"custom": {"nested": 1}}"#)),
            &stored.settings_store,
        )
        .expect("a form carries the JSON text of the object");
        assert_eq!(
            from_text["custom"],
            json!({"nested": 1}),
            "the text parses to the object it names"
        );
    }

    #[test]
    fn a_settings_store_submission_is_an_object_within_the_cap() {
        use crate::api::pleroma_settings::{MAX_SETTINGS_STORE_BYTES, STORE_TOO_LARGE};

        let stored = stored_user().settings_store;
        for patch in [json!(5), json!("plain text"), json!([1, 2]), json!(true)] {
            let err = merged_store(Some(&patch), &stored).expect_err("a value with no keys");
            assert_eq!(err.to_string(), STORE_NOT_OBJECT, "the object message");
        }
        let blob = "x".repeat(MAX_SETTINGS_STORE_BYTES + 1);
        let err = merged_store(Some(&json!({"blob": blob})), &stored)
            .expect_err("over one mebibyte does not fit");
        assert_eq!(err.to_string(), STORE_TOO_LARGE, "the size message");
        assert!(
            matches!(err, AppError::Unprocessable(_)),
            "both store refusals are 422s"
        );
    }

    /// `merged_actor` over the stored fixture row, as the `(actor_type,
    /// bot)` pair the body leaves.
    fn actor_pair(body: &str) -> (String, bool) {
        let (kind, bot) =
            merged_actor(&params(body), &stored_user()).expect("the fixture body is accepted");
        (kind, bot.get())
    }

    #[test]
    fn actor_type_and_bot_are_written_as_one_pair() {
        assert_eq!(
            actor_pair(r#"{"actor_type": "Service"}"#),
            ("Service".to_owned(), true),
            "the stated kind is stored and `bot` follows it"
        );
        assert_eq!(
            actor_pair(r#"{"actor_type": "Person"}"#),
            ("Person".to_owned(), false),
            "and so the other way round"
        );
        assert_eq!(
            actor_pair(r#"{"bot": true}"#),
            ("Service".to_owned(), true),
            "`bot=true` picks the actor kind Pleroma stores for a bot"
        );
        assert_eq!(
            actor_pair(r#"{"bot": false}"#),
            ("Person".to_owned(), false),
            "and `bot=false` picks `Person`"
        );
        assert_eq!(
            actor_pair(r#"{"bot": false, "actor_type": "Service"}"#),
            ("Service".to_owned(), true),
            "an explicit `actor_type` wins, and `bot` reads back from the kind"
        );
        assert_eq!(
            actor_pair("{}"),
            (DEFAULT_ACTOR_TYPE.to_owned(), false),
            "a body with neither parameter keeps both stored columns"
        );
        for text in ["Group", "person", ""] {
            let body = params(&format!(r#"{{"actor_type": "{text}"}}"#));
            let err = merged_actor(&body, &stored_user()).expect_err("no such actor kind");
            assert_eq!(err.to_string(), ACTOR_TYPE_INVALID, "the kind message");
        }
    }

    #[test]
    fn each_pleroma_flag_merges_on_its_own() {
        let current = stored_user();
        let body = params(r#"{"hide_follows": "on", "no_rich_text": false}"#);
        let prefs = merged_prefs(&body, &current);
        assert!(prefs.hide_follows.get(), "the submitted flag is set");
        assert!(!prefs.no_rich_text.get(), "and so is the submitted false");
        assert!(
            prefs.hide_followers.get() && prefs.hide_favorites.get(),
            "flags the body never carried keep what the row held"
        );
        assert!(
            prefs.show_role.get()
                && prefs.allow_following_move.get()
                && prefs.accepts_chat_messages.get(),
            "the on-by-default flags survive an unrelated write"
        );
        assert!(
            !prefs.show_birthday.get()
                && !prefs.skip_thread_containment.get()
                && !prefs.hide_followers_count.get()
                && !prefs.hide_follows_count.get(),
            "and so do the off-by-default ones"
        );
    }

    /// Parses `value` as the registration endpoint's parameters, as
    /// `ApiBody` would.
    fn registration_params(value: Value) -> AppResult<CreateAccountParams> {
        crate::extract::into_params(value)
    }

    /// A stored-looking token row minted right now, for the
    /// [`RegistrationToken`] render.
    fn minted_token() -> OauthToken {
        let now = Utc::now();
        OauthToken {
            id: Uuid::now_v7(),
            token: "access-secret".to_owned(),
            refresh_token: "refresh-secret".to_owned(),
            user_id: Some(Uuid::now_v7()),
            app_id: Uuid::now_v7(),
            scopes: "read write follow push".to_owned(),
            valid_until: now + chrono::Duration::days(30),
            inserted_at: now,
            updated_at: now,
        }
    }

    #[test]
    fn registration_params_take_their_required_fields_and_default_the_rest() {
        let params = registration_params(json!({
            "username": "lain",
            "email": "lain@example.org",
            "password": "PlzDontHackLain",
            "agreement": true,
        }))
        .expect("the required four are there");
        assert_eq!(params.username, "lain", "username");
        assert!(params.agreement, "a real json boolean");
        assert!(
            params.reason.is_none() && params.fullname.is_none() && params.bio.is_none(),
            "every optional field defaults away when absent"
        );
        assert!(params.token.is_none(), "an absent invite token is None");
    }

    #[test]
    fn registration_agreement_accepts_the_form_spellings() {
        // The extractor turns `agreement=true` into this shape
        // (`src/extract.rs` covers the parsing itself).
        let params = registration_params(json!({
            "username": "lain",
            "password": "PlzDontHackLain",
            "agreement": "true",
            "reason": "I'm a cool dude, bro",
            "fullname": "Lain Iwakura",
            "bio": "☕️",
        }))
        .expect("a form-style body");
        assert!(params.agreement, "`\"true\"` is the form spelling");
        assert_eq!(params.reason.as_deref(), Some("I'm a cool dude, bro"));
    }

    #[test]
    fn ignored_fields_are_accepted_and_untyped() {
        // `locale` and `confirm` are on the wire but never read: the
        // endpoint takes them as sent, in any shape, and ignores them.
        let params = registration_params(json!({
            "username": "lain",
            "password": "PlzDontHackLain",
            "agreement": "on",
            "locale": "en",
            "confirm": "PlzDontHackLain",
            "captcha_response": 9001,
        }))
        .expect("unknown keys do not bind and do not reject");
        assert!(params.agreement, "`\"on\"` is the checkbox spelling");
    }

    #[test]
    fn the_invite_token_is_a_read_optional() {
        // Unlike the ignored extras, Pleroma's `token` binds: the gate in
        // `load_invite` weighs it, and a body without it still parses.
        let with = registration_params(json!({
            "username": "lain",
            "password": "PlzDontHackLain",
            "agreement": true,
            "token": "VjtxW_-a-some-random-text",
        }))
        .expect("a body carrying an invite");
        assert_eq!(
            with.token.as_deref(),
            Some("VjtxW_-a-some-random-text"),
            "the token text arrives verbatim"
        );
    }

    #[test]
    fn a_missing_or_unboolean_agreement_is_unprocessable() {
        for value in [
            json!({"username": "lain", "password": "x"}),
            json!({"username": "lain", "password": "x", "agreement": "maybe"}),
        ] {
            let err = registration_params(value).expect_err("`agreement` is required");
            assert!(
                matches!(err, AppError::Unprocessable(_)),
                "a bad agreement is a 422, got {err}"
            );
        }
    }

    #[test]
    fn registration_params_map_onto_the_register_model() {
        let params = registration_params(json!({
            "username": "lain",
            "email": "lain@example.org",
            "password": "PlzDontHackLain",
            "agreement": true,
            "fullname": "Lain Iwakura",
            "bio": "wired",
            "reason": "let me in",
            "locale": "ja",
            "confirm": "PlzDontHackLain",
        }))
        .expect("a full body");
        let register = params.into_register_params();
        assert_eq!(register.nickname, "lain", "username is the nickname");
        assert_eq!(register.email.as_deref(), Some("lain@example.org"), "email");
        assert_eq!(register.password, "PlzDontHackLain", "password");
        assert_eq!(register.name.as_deref(), Some("Lain Iwakura"), "fullname");
        assert_eq!(register.bio.as_deref(), Some("wired"), "bio");
        assert_eq!(register.reason.as_deref(), Some("let me in"), "reason");
    }

    #[test]
    fn the_registration_answer_is_the_token_json() {
        let stored = minted_token();
        let expected_id = stored.id;
        let expected_created = stored.inserted_at.timestamp();
        let body = serde_json::to_value(RegistrationToken::new(stored)).unwrap();
        assert_eq!(body["token_type"], "Bearer", "bearer credentials");
        assert_eq!(body["access_token"], "access-secret", "the access token");
        assert_eq!(body["refresh_token"], "refresh-secret", "the refresh token");
        assert_eq!(
            body["scope"], "read write follow push",
            "the presenting token's scopes"
        );
        assert_eq!(body["created_at"], expected_created, "unix seconds");
        assert_eq!(
            body["id"],
            expected_id.to_string().as_str(),
            "hyphenated `UUIDv7`"
        );
        let ttl = i64::try_from(crate::config::DEFAULT_OAUTH_TOKEN_TTL_SECS).unwrap();
        assert!(
            (ttl - 60..=ttl + 1).contains(&body["expires_in"].as_i64().unwrap()),
            "expires_in counts down the month-long lifetime"
        );
        assert_eq!(
            body.as_object().expect("an object").len(),
            7,
            "exactly the ticket's seven keys, no `me`"
        );
    }

    /// Deserializes a `POST /api/v1/follows` parameter map, the way the
    /// extractor hands form bodies over (every value a string).
    fn follow_by_uri_params(pairs: &[(&str, &str)]) -> FollowByUriParams {
        let map = pairs
            .iter()
            .map(|(key, value)| ((*key).to_owned(), json!(value)))
            .collect::<Map<String, Value>>();
        serde_json::from_value(Value::Object(map)).expect("the fixture params parse")
    }

    #[test]
    fn follow_by_uri_keeps_only_the_toggles_the_request_named() {
        let toggles = follow_by_uri_params(&[("uri", "@bob@test.example"), ("notify", "true")])
            .into_toggles();
        assert_eq!(
            (toggles.show_reblogs, toggles.notify),
            (None, Some(true)),
            "the named `notify` moves, the absent `reblogs` keeps the row"
        );
        let bare = follow_by_uri_params(&[("uri", "bob")]).into_toggles();
        assert!(bare.is_empty(), "a follow naming no toggle writes nothing");
    }

    #[test]
    fn follow_by_uri_reads_the_uri_wherever_it_was_sent() {
        let json: FollowByUriParams =
            serde_json::from_str(r#"{"uri": "https://remote.example/users/bob"}"#)
                .expect("a JSON body parses");
        assert_eq!(
            json.uri.as_deref(),
            Some("https://remote.example/users/bob"),
            "the JSON spelling"
        );
        let nulled = follow_by_uri_params(&[("uri", "")]);
        assert_eq!(
            nulled.uri.as_deref(),
            Some(""),
            "the form's empty string reads empty"
        );
        let absent: FollowByUriParams =
            serde_json::from_str(r#"{"uri": null}"#).expect("a null parses");
        assert_eq!(absent.uri, None, "and a JSON null is the absent one");
    }
    /// The parsed status-listing parameters, from a raw query string.
    fn statuses_parsed(query: &str) -> StatusesParams {
        StatusesParams::parse(Some(query))
    }

    /// A page cursor of `n`, the request's half of [`PageParams`] the
    /// listing hands its query.
    fn page_of(limit: i64) -> PageParams {
        PageParams {
            max_id: Some(Uuid::now_v7()),
            min_id: None,
            since_id: None,
            limit,
            offset: None,
        }
    }

    /// The local and the remote account the area and gate tests read.
    async fn local_and_remote() -> (crate::test_db::TestDb, User, User) {
        let db = crate::test_db::test_db().await.expect("test database slot");
        let local = user::insert(
            db.pool(),
            &user::NewUser::local(
                "alice",
                &user::local_ap_id(crate::config::TEST_BASE_URL, "alice"),
            ),
        )
        .await
        .expect("the local fixture inserts");
        let remote = user::insert(
            db.pool(),
            &user::NewUser {
                nickname: "bob@remote.example".to_owned(),
                ap_id: "https://remote.example/users/bob".to_owned(),
                domain: Some("remote.example".to_owned()),
                ..user::NewUser::default()
            },
        )
        .await
        .expect("the remote fixture inserts");
        (db, local, remote)
    }

    /// The config restricting exactly the areas in `areas`.
    fn config_restricting(areas: &[&str]) -> Config {
        let mut config = Config::for_tests();
        config.restrict_unauthenticated = areas.iter().map(|area| (*area).to_owned()).collect();
        config
    }

    #[test]
    fn the_status_params_read_their_flags_and_ignore_the_rest() {
        let on = statuses_parsed(
            "pinned=true&only_media=1&exclude_replies=TRUE&exclude_reblogs=yes\
             &with_muted=true&limit=5&max_id=abc&since_id=def",
        );
        assert!(on.pinned.get(), "`pinned=true`");
        assert!(on.only_media.get(), "`only_media=1`");
        assert!(
            on.exclude_replies.get(),
            "`exclude_replies` is case-insensitive"
        );
        assert!(!on.exclude_reblogs.get(), "`yes` is not true here");
        assert!(on.with_muted.get(), "`with_muted=true`");
        assert_eq!(on.tagged, None, "no `tagged` names no tag");
        assert_eq!(
            on.exclude_visibilities,
            Vec::new(),
            "the page keys are `PageParams`' and were not looked at"
        );
        assert_eq!(
            statuses_parsed(""),
            StatusesParams::default(),
            "an empty query narrows nothing"
        );
        let off = statuses_parsed("pinned=false&only_media=&with_muted=0");
        assert!(
            !off.pinned.get() && !off.only_media.get() && !off.with_muted.get(),
            "`false`, empty and `0` are all off"
        );
        let last = statuses_parsed("pinned=true&pinned=false");
        assert!(!last.pinned.get(), "the last occurrence wins");
    }

    #[test]
    fn the_status_params_read_the_tag_and_the_visibility_exclusions() {
        assert_eq!(
            statuses_parsed("tagged=%23Rust").tagged.as_deref(),
            Some("rust"),
            "`#` is stripped and the name lowercased"
        );
        assert_eq!(
            statuses_parsed("tagged=").tagged.as_deref(),
            Some(""),
            "an empty `tagged` names no stored tag, so it matches nothing"
        );
        assert_eq!(
            statuses_parsed("tagged=%23").tagged.as_deref(),
            Some(""),
            "and so does a lone `#`"
        );
        let excluded = statuses_parsed(
            "exclude_visibilities[]=public&exclude_visibilities=Private&exclude_visibilities[]=direct",
        );
        assert_eq!(
            excluded.exclude_visibilities,
            vec![Visibility::Public, Visibility::Direct],
            "both spellings fill the one list and a capitalization names nothing"
        );
    }

    /// The query a plain request builds: the account's own timeline with the
    /// request's filters and the request's page.
    #[tokio::test]
    async fn the_status_query_is_the_accounts_own_timeline() {
        let (_db, alice, _bob) = local_and_remote().await;
        let alice_id = alice.id;
        let page = page_of(7);
        let query =
            statuses_parsed("only_media=true&exclude_reblogs=1&tagged=rust&with_muted=true")
                .into_timeline(alice_id, Some(alice), page);
        assert_eq!(
            query.source,
            TimelineSource::Account { user_id: alice_id },
            "the read is the account's own timeline"
        );
        assert_eq!(query.page, page, "the request's page is threaded through");
        assert_eq!(
            query.filters.only_media,
            Flag::new(true),
            "`only_media` rides from the query string"
        );
        assert_eq!(
            query.filters.exclude_reblogs,
            Flag::new(true),
            "so does `exclude_reblogs`, which is what takes the boosts out"
        );
        assert_eq!(
            query.filters.with_muted,
            Flag::new(true),
            "and the viewer's own exclusions can be lifted"
        );
        assert_eq!(
            query.filters.tagged.as_deref(),
            Some("rust"),
            "and the `tagged` name narrows to that tag"
        );
        assert!(
            !query.filters.pinned_only.get() && !query.filters.exclude_replies.get(),
            "what the request never asked for stays off"
        );
        assert!(
            query
                .viewer
                .as_ref()
                .is_some_and(|viewer| viewer.id == alice_id),
            "the query owns the authenticated viewer"
        );
    }

    /// `pinned` pages the whole featured set instead of the request's page:
    /// the pagination keys are ignored, and the pin filter is what the
    /// fetch runs on.
    #[tokio::test]
    async fn the_pinned_query_reads_every_pin_off_the_request_page() {
        let (_db, alice, _bob) = local_and_remote().await;
        let page = page_of(2);
        let query = statuses_parsed("pinned=1&only_media=true").into_timeline(alice.id, None, page);
        assert!(
            query.filters.pinned_only.get(),
            "`pinned` is the author's-pin filter"
        );
        assert_eq!(
            query.filters.only_media,
            Flag::new(true),
            "the other filters still apply"
        );
        assert!(query.viewer.is_none(), "an anonymous request has no viewer");
        assert_eq!(
            query.page,
            every_pin(),
            "the request's cursors and limit are ignored"
        );
        assert_eq!(
            (query.page.limit, query.page.max_id, query.page.offset),
            (i64::MAX, None, None),
            "that page is the whole set, uncursored"
        );
    }

    #[test]
    fn every_pin_is_the_uncursored_whole_set() {
        let page = every_pin();
        assert_eq!(page.limit, i64::MAX, "no page of the featured list");
        assert_eq!(
            (page.max_id, page.min_id, page.since_id, page.offset),
            (None, None, None, None),
            "and no cursor either"
        );
    }

    #[tokio::test]
    async fn the_profile_area_follows_the_local_column() {
        let (_db, alice, bob) = local_and_remote().await;
        assert_eq!(profile_area(&alice), PROFILES_LOCAL, "a local account");
        assert_eq!(
            profile_area(&bob),
            PROFILES_REMOTE,
            "a mirror is the other half"
        );
    }

    /// The `RESTRICT_UNAUTHENTICATED` gate: only an anonymous request to a
    /// restricted half is refused, and the two halves are independent.
    #[tokio::test]
    async fn restricted_areas_refuse_anonymous_reads_of_their_own_half() {
        let (_db, alice, bob) = local_and_remote().await;
        let open = Config::for_tests();
        assert!(
            require_profile_read(&open, None, &alice).is_ok(),
            "a public instance answers the anonymous request"
        );
        let locals = config_restricting(&[PROFILES_LOCAL]);
        let refused =
            require_profile_read(&locals, None, &alice).expect_err("a closed local profile");
        assert_eq!(
            refused.to_string(),
            USER_REQUIRED,
            "the refusal is Pleroma's authenticated-user wording"
        );
        assert!(
            require_profile_read(&locals, None, &bob).is_ok(),
            "`profiles_local` says nothing about a mirror"
        );
        let remotes = config_restricting(&[PROFILES_REMOTE]);
        assert!(
            require_profile_read(&remotes, None, &bob).is_err(),
            "the other half closes on its own name"
        );
        assert!(
            require_profile_read(&remotes, Some(&alice), &bob).is_ok(),
            "a presented user token is never refused for the restriction"
        );
        assert!(
            require_profile_read(&locals, Some(&bob), &alice).is_ok(),
            "whatever account it belongs to"
        );
    }

    #[test]
    fn tagged_names_are_stored_spellings_or_nothing_at_all() {
        assert_eq!(
            tagged_name("#Rust"),
            "rust",
            "the written spelling normalizes"
        );
        assert_eq!(tagged_name(""), "", "an empty value names no tag");
        assert_eq!(
            tagged_name(" ## "),
            "",
            "and so does whitespace around a `#`"
        );
    }
}
