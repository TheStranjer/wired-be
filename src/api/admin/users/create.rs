//! The batch create: the body shapes, the per-item creation and the
//! answer and refusal renderers.

use std::{collections::BTreeMap, fmt};

use axum::{Json, extract::State};
use serde::Deserialize;
use serde_json::{Value, json};
use tracing::debug;

use super::{AREA_ACCOUNTS, CLASS_WRITE, MAX_CREATE_USERS};
use crate::{
    auth::admin::AdminUser,
    error::{AppError, AppResult},
    extract::ApiBody,
    models::{
        moderation_log,
        user::{User, admin, register},
    },
    state::AppState,
};

/// The body of `POST /api/v1/pleroma/admin/users`: the batch of accounts
/// the admin creates, at most [`MAX_CREATE_USERS`] of them.
#[derive(Debug, Deserialize)]
pub(super) struct CreateUsersBody {
    /// The accounts to create, in the order the request states them.
    users: Vec<CreateUserItem>,
}

/// One `users` entry: the three keys the ticket pins, all required —
/// Pleroma's passwordless variant (a random password behind a minted
/// reset link) is not part of this endpoint's pinned shape.
#[derive(Deserialize)]
pub(super) struct CreateUserItem {
    /// The handle to register, validated by
    /// [`create_local_user`](crate::models::user::register::create_local_user)'s
    /// registration rules.
    nickname: String,
    /// The address to store, format- and uniqueness-checked by the same.
    email: String,
    /// The password in plain text — only its hash ever reaches a row, and
    /// it never lives past this request.
    password: String,
}

/// The password is secret material and renders as `<redacted>`; everything
/// else shows.
impl fmt::Debug for CreateUserItem {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("CreateUserItem")
            .field("nickname", &self.nickname)
            .field("email", &self.email)
            .field("password", &"<redacted>")
            .finish()
    }
}

/// What one create item came to: the stored account, or the refusal the
/// registration changeset reported for it. The row rides boxed so the
/// refusal's text does not pay for the [`User`]'s size in every variant.
#[derive(Debug)]
pub(super) enum Creation {
    /// The account the creation stored, active, confirmed and approved.
    Created(Box<User>),
    /// The refusal's message: the field map as JSON text, the encoding the
    /// user-facing registration's own 422 body carries.
    Refused(String),
}

/// `POST /api/v1/pleroma/admin/users` — Pleroma's `create`: a batch of
/// local accounts, each answered for itself. Where Pleroma wraps the batch
/// in one `Multi` transaction that rolls every account back on the first
/// conflict and answers 409, the ticket pins an always-200 per-item answer:
/// each entry is the stored account's `"created.json"` map or the 400
/// `"create_error.json"` one, and the accounts that passed stay created.
/// The created ones log one `create` entry, whatever the batch also
/// refused.
///
/// # Errors
///
/// [`AppError::Unauthorized`] / [`AppError::Forbidden`] from [`AdminUser`]
/// and its `admin:write` gate; [`AppError::Unprocessable`] for a batch over
/// [`MAX_CREATE_USERS`]; [`AppError::Internal`] or [`AppError::Database`]
/// from the shared registration path's server-side failures (a hashing,
/// key-generation or database fault is nobody's validation error and
/// aborts the request) and from the log.
pub(super) async fn create_users(
    State(state): State<AppState>,
    admin: AdminUser,
    body: ApiBody<CreateUsersBody>,
) -> AppResult<Json<Vec<Value>>> {
    admin.require(CLASS_WRITE, AREA_ACCOUNTS)?;
    if body.0.users.len() > MAX_CREATE_USERS {
        return Err(AppError::Unprocessable(format!(
            "Cannot create more than {MAX_CREATE_USERS} users in one request"
        )));
    }
    let mut results = Vec::with_capacity(body.0.users.len());
    let mut created = Vec::new();
    let mut refused = 0;
    for item in &body.0.users {
        match create_one(&state, item).await? {
            Creation::Created(user) => {
                let user = *user;
                results.push(created_item(&user));
                created.push(user);
            }
            Creation::Refused(message) => {
                results.push(refused_item(item, &message));
                refused += 1;
            }
        }
    }
    let logged = moderation_log::create_users(&state.db, &admin.0, &created).await?;
    debug!(
        log = %logged.id,
        created = created.len(),
        refused, "admin created users"
    );
    Ok(Json(results))
}

/// One create item through the shared registration path, with the three
/// gates the admin creation bypasses: the display name and the profile
/// note stay the registration's own defaults (the handle, an empty note —
/// Pleroma's admin create likewise puts the nickname in `name`), and
/// [`provider_email_verified`](crate::models::user::register::RegisterParams::provider_email_verified)
/// stands for the confirmation bypass — the admin who set the password
/// vouches for the address, so `ACCOUNT_ACTIVATION_REQUIRED` leaves the
/// row confirmed and queues no letter (Pleroma's
/// `register_changeset(…, confirmed: true)`).
/// The approval flag, which the registration stores from the instance
/// mode whatever the caller says, is lifted afterwards by
/// [`creation_bypasses_approval`].
///
/// # Errors
///
/// As [`create_users`]'s for a server-side [`register::RegisterError`];
/// validation refusals come back as [`Creation::Refused`].
pub(super) async fn create_one(state: &AppState, item: &CreateUserItem) -> AppResult<Creation> {
    let params = register::RegisterParams {
        nickname: item.nickname.clone(),
        email: Some(item.email.clone()),
        password: item.password.clone(),
        name: None,
        bio: None,
        reason: None,
        provider_email_verified: true,
        identifiers: BTreeMap::new(),
    };
    match register::create_local_user(state, params, None).await {
        Ok(stored) => Ok(Creation::Created(Box::new(
            creation_bypasses_approval(state, stored).await?,
        ))),
        Err(register::RegisterError::Invalid(errors)) => {
            Ok(Creation::Refused(field_errors_message(&errors)))
        }
        Err(register::RegisterError::Failed(err)) => Err(err),
    }
}

/// The approval half of the bypass: with `REGISTRATIONS_APPROVAL_REQUIRED`
/// the freshly stored row awaits staff, and an admin-created account must
/// not — [`set_approved`] lifts the flag and hands the finished row back
/// for the answer. A row that went away mid-request keeps the copy the
/// insert returned.
///
/// [`set_approved`]: crate::models::user::admin::set_approved
pub(super) async fn creation_bypasses_approval(state: &AppState, stored: User) -> AppResult<User> {
    if !stored.approval_pending.get() {
        return Ok(stored);
    }
    Ok(admin::set_approved(&state.db, stored.id)
        .await?
        .unwrap_or(stored))
}

/// The per-item `error` message: the refused field map as JSON text, the
/// same encoding [`RegisterError`] carries into the user-facing
/// registration's 422 body. The fallback fold is for a map serde could
/// never encode anyway; it reads like [`RegisterError`]'s own `Display`.
///
/// [`RegisterError`]: crate::models::user::register::RegisterError
pub(super) fn field_errors_message(errors: &register::FieldErrors) -> String {
    serde_json::to_string(errors).unwrap_or_else(|_| {
        errors
            .iter()
            .map(|(field, messages)| format!("{field}: {}", messages.join(", ")))
            .collect::<Vec<String>>()
            .join("; ")
    })
}

/// The per-item success entry: Pleroma's `"created.json"` — code 200, type
/// `"success"`, the stored address and handle as `data` (the
/// `password_reset_link` Pleroma adds to its passwordless items has no
/// place in this endpoint's pinned shape).
pub(super) fn created_item(user: &User) -> Value {
    json!({
        "code": 200,
        "type": "success",
        "data": { "email": user.email, "nickname": user.nickname },
    })
}

/// The per-item failure entry: Pleroma's `"create_error.json"` with the
/// ticket's 400 — code, type `"error"`, the refusal's message, and the
/// handle and address as the request submitted them.
pub(super) fn refused_item(item: &CreateUserItem, message: &str) -> Value {
    json!({
        "code": 400,
        "type": "error",
        "error": message,
        "data": { "nickname": item.nickname, "email": item.email },
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The create body reads its list of three-key items and nothing
    /// looser: every key of every item is required, and an item that
    /// states two of them is the parse error the extractor turns into
    /// its 422 — the same for a body with no list at all.
    #[test]
    fn the_create_body_requires_every_key_of_every_item() {
        let batch: CreateUsersBody = serde_json::from_value(json!({
            "users": [
                {"nickname": "alice", "email": "alice@example.org", "password": "long enough"},
                {"nickname": "bob", "email": "bob@example.org", "password": "another pass"},
            ],
        }))
        .expect("the create body");
        assert_eq!(batch.users.len(), 2, "both items");
        assert_eq!(batch.users[0].nickname, "alice", "each with");
        assert_eq!(batch.users[1].password, "another pass", "all three keys");
        for broken in [
            json!({"users": [{"nickname": "a", "email": "a@b.cx"}]}),
            json!({"users": [{"nickname": "a", "password": "p"}]}),
            json!({"users": [{"email": "a@b.cx", "password": "p"}]}),
            json!({}),
        ] {
            assert!(
                serde_json::from_value::<CreateUsersBody>(broken.clone()).is_err(),
                "an incomplete request does not parse: {broken}"
            );
        }
    }

    /// The submitted password never renders, and the per-item answers keep
    /// Pleroma's two shapes: the success entry the ticket pins its data
    /// keys to, and the refusal carrying the message and the submitted
    /// handle and address.
    #[test]
    fn the_create_items_redact_and_render_the_pinned_shapes() {
        let item: CreateUserItem = serde_json::from_value(json!({
            "nickname": "alice",
            "email": "alice@example.org",
            "password": "hunter2 password",
        }))
        .expect("one item");
        let shown = format!("{item:?}");
        assert!(
            !shown.contains("hunter2"),
            "the plaintext never reaches a log line: {shown}"
        );
        assert_eq!(
            refused_item(&item, "nope"),
            json!({
                "code": 400,
                "type": "error",
                "error": "nope",
                "data": {"nickname": "alice", "email": "alice@example.org"},
            }),
            "the refusal, with what the request submitted"
        );
    }

    /// The per-item `error` text is the field map as JSON, every refusal
    /// of every field inside it — the encoding the user-facing
    /// registration's own 422 body carries.
    #[test]
    fn the_create_refusal_encodes_the_whole_field_map() {
        let mut errors = register::FieldErrors::new();
        let first: Option<Vec<String>> =
            errors.insert("nickname", vec!["has already been taken".to_owned()]);
        let second: Option<Vec<String>> =
            errors.insert("email", vec!["has invalid format".to_owned()]);
        drop((first, second));
        assert_eq!(
            field_errors_message(&errors),
            "{\"email\":[\"has invalid format\"],\"nickname\":[\"has already been taken\"]}",
            "the map's own JSON text, keys sorted"
        );
        assert_eq!(
            field_errors_message(&register::FieldErrors::new()),
            "{}",
            "and an empty map is no special case"
        );
    }
}
