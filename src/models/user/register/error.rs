//! Every way a registration fails, and what the failure becomes on the way
//! out: [`RegisterError`] carries either the field-keyed validation map or
//! the server-side error behind it, [`Display`](fmt::Display) and
//! [`Error`](std::error::Error) spell that out for the logs, and the
//! [`From`] impls turn a validation failure into the 422 Pleroma's
//! account creation answers with or pass the inner [`AppError`] through.
//!
//! [`add_error`] is how the rest of the module writes one message under one
//! field, and [`map_insert_error`] is how a lost race against a `users`
//! unique constraint arrives as the same field error the up-front
//! availability check would have produced.

use std::{error::Error, fmt};

use sqlx::Error as SqlxError;

use super::{
    FieldErrors,
    validate::{HAS_ALREADY_BEEN_TAKEN, SQLSTATE_UNIQUE_VIOLATION},
};
use crate::error::AppError;

/// Every way registration can fail.
#[derive(Debug)]
pub enum RegisterError {
    /// The submitted parameters did not pass validation; the map is keyed
    /// by field name, exactly like Pleroma's changeset errors.
    Invalid(FieldErrors),
    /// A server-side failure that is not the registrant's fault: password
    /// hashing, key generation or the database.
    Failed(AppError),
}

impl fmt::Display for RegisterError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Invalid(errors) => {
                let rendered: Vec<String> = errors
                    .iter()
                    .map(|(field, messages)| format!("{field}: {}", messages.join(", ")))
                    .collect();
                write!(
                    f,
                    "invalid registration parameters: {}",
                    rendered.join("; ")
                )
            }
            Self::Failed(err) => write!(f, "could not create the account: {err}"),
        }
    }
}

impl Error for RegisterError {
    fn source(&self) -> Option<&(dyn Error + 'static)> {
        match self {
            Self::Invalid(_) => None,
            Self::Failed(err) => Some(err),
        }
    }
}

/// Any server-side failure is [`RegisterError::Failed`], so `?` works on
/// hashing, key generation and database calls inside the registration.
impl From<AppError> for RegisterError {
    fn from(err: AppError) -> Self {
        Self::Failed(err)
    }
}

/// Validation failures become the 422 Pleroma's Mastodon-API account
/// creation sends: `{"error": "<json-encoded errors map as string>"}`, the
/// `error` value being the JSON string of the field map (decision 8's body
/// shape carrying [`FieldErrors`] encoded by `serde_json`).
impl From<RegisterError> for AppError {
    fn from(err: RegisterError) -> Self {
        match err {
            RegisterError::Invalid(errors) => match serde_json::to_string(&errors) {
                Ok(encoded) => Self::Unprocessable(encoded),
                Err(encode_err) => Self::Internal(format!(
                    "could not encode the registration errors: {encode_err}"
                )),
            },
            RegisterError::Failed(err) => err,
        }
    }
}

/// Turns a database failure from the insert into a [`RegisterError`]: a
/// Postgres unique violation (SQLSTATE `23505`) on one of `users`' unique
/// constraints lost the race against a concurrent registration and
/// arrives as the same field error the availability check would have
/// produced; anything else stays a [`RegisterError::Failed`].
pub(super) fn map_insert_error(err: AppError) -> RegisterError {
    unique_violation_field(&err).map_or_else(
        || RegisterError::Failed(err),
        |field| {
            let mut errors = FieldErrors::new();
            add_error(&mut errors, field, HAS_ALREADY_BEEN_TAKEN);
            RegisterError::Invalid(errors)
        },
    )
}

/// The `users` field whose unique constraint `err` violated, if `err` is a
/// Postgres `23505` on one of those constraints at all.
fn unique_violation_field(err: &AppError) -> Option<&'static str> {
    let AppError::Database(SqlxError::Database(db_err)) = err else {
        return None;
    };
    if db_err.code().as_deref() != Some(SQLSTATE_UNIQUE_VIOLATION) {
        return None;
    }
    db_err.constraint().and_then(constraint_field)
}

/// The field protected by the `users` unique constraint `constraint`
/// (Postgres' default names for the table's three column constraints).
fn constraint_field(constraint: &str) -> Option<&'static str> {
    match constraint {
        "users_nickname_key" => Some("nickname"),
        "users_email_key" => Some("email"),
        "users_ap_id_key" => Some("ap_id"),
        _ => None,
    }
}

/// Appends one validation `message` under `field`, creating its list.
pub(super) fn add_error(errors: &mut FieldErrors, field: &'static str, message: &str) {
    errors.entry(field).or_default().push(message.to_owned());
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{
        models::user::register::validate::{CANNOT_BE_BLANK, HAS_INVALID_FORMAT},
        util::id::new_id,
    };

    #[test]
    fn constraint_field_maps_the_three_unique_constraints() {
        assert_eq!(
            constraint_field("users_nickname_key"),
            Some("nickname"),
            "the citext nickname constraint"
        );
        assert_eq!(
            constraint_field("users_email_key"),
            Some("email"),
            "the citext email constraint"
        );
        assert_eq!(
            constraint_field("users_ap_id_key"),
            Some("ap_id"),
            "the actor id constraint"
        );
        assert_eq!(
            constraint_field("jobs_pkey"),
            None,
            "an unrelated constraint maps to no field"
        );
    }

    #[test]
    fn unique_violation_field_only_answers_database_errors() {
        assert_eq!(
            unique_violation_field(&AppError::NotFound),
            None,
            "a plain AppError carries no constraint"
        );
        assert_eq!(
            unique_violation_field(&AppError::Database(SqlxError::PoolClosed)),
            None,
            "a driver-less sqlx failure carries no constraint"
        );
    }

    #[test]
    fn register_error_display_and_source() {
        let mut errors = FieldErrors::new();
        add_error(&mut errors, "nickname", HAS_ALREADY_BEEN_TAKEN);
        add_error(&mut errors, "email", CANNOT_BE_BLANK);
        add_error(&mut errors, "email", HAS_INVALID_FORMAT);
        let invalid = RegisterError::Invalid(errors);
        assert_eq!(
            invalid.to_string(),
            "invalid registration parameters: email: can't be blank, has invalid format; nickname: has already been taken",
            "every field and message, ordered by field"
        );
        assert!(invalid.source().is_none(), "validation has no cause");

        let failed = RegisterError::Failed(AppError::Internal("os rng died".to_owned()));
        assert_eq!(
            failed.to_string(),
            "could not create the account: os rng died",
            "the inner message shows through Display"
        );
        assert_eq!(
            failed
                .source()
                .expect("the cause survives")
                .to_string()
                .as_str(),
            "os rng died",
            "Failed keeps its AppError as source"
        );
    }

    #[test]
    fn invalid_becomes_a_422_wrapping_the_json_field_map() {
        let mut errors = FieldErrors::new();
        add_error(&mut errors, "nickname", HAS_ALREADY_BEEN_TAKEN);
        let app_err = AppError::from(RegisterError::Invalid(errors));
        assert_eq!(
            app_err.to_string(),
            r#"{"nickname":["has already been taken"]}"#,
            "the message is the JSON string of the field map, exactly the \
             value Pleroma's account creation puts in `error`"
        );
        let not_found = AppError::from(RegisterError::Failed(AppError::NotFound));
        assert!(
            matches!(not_found, AppError::NotFound),
            "Failed forwards the wrapped AppError unchanged"
        );
    }

    #[test]
    fn an_app_error_lifts_into_a_failed_registration() {
        let lifted: RegisterError = AppError::RateLimited.into();
        assert!(
            matches!(lifted, RegisterError::Failed(AppError::RateLimited)),
            "From<AppError> wraps for the ? operator"
        );
    }

    #[tokio::test]
    async fn a_other_database_error_stays_a_failure() {
        let db = crate::test_db::test_db().await.expect("test database slot");
        let pool = db.pool().clone();
        // A constraint failure that is *not* a unique violation must not be
        // dressed up as field errors: run a NOT NULL violation through the
        // mapper and expect the database failure to come out intact.
        let raw = sqlx::query("INSERT INTO users (id, ap_id, local) VALUES ($1, $2, true)")
            .bind(new_id())
            .bind("https://test.example/users/nonull")
            .execute(&pool)
            .await
            .expect_err("the nickname is NOT NULL");
        let mapped = map_insert_error(AppError::Database(raw));
        assert!(
            matches!(mapped, RegisterError::Failed(AppError::Database(_))),
            "a non-23505 violation stays a Failed database error, got {mapped:?}"
        );
    }
}
