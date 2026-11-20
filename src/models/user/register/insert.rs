//! The write half of a registration: the up-front availability check, the
//! assembled [`NewUser`] and the insert that lands it — on the pool alone,
//! or, when the request came in on an invite, inside the one transaction
//! that also spends the token — plus the display-name fallback that
//! decides what `name` says.
//!
//! Everything here runs once the rules in [`super::validate`] have passed;
//! a database failure that names one of `users`' unique constraints still
//! arrives as the field error the availability check would have produced
//! (see [`map_insert_error`]).

use sqlx::PgPool;

use super::{
    FieldErrors, RegisterError, RegisterParams,
    error::{add_error, map_insert_error},
    modes::registration_waits_for_approval,
    validate::HAS_ALREADY_BEEN_TAKEN,
};
use crate::{
    error::AppResult,
    models::{
        invite_token::{self, InviteToken},
        user::{
            Flag, NewUser, User, featured, followers, following, get_by_email, get_by_nickname,
            inbox, insert, local_ap_id, outbox,
        },
    },
    state::AppState,
    util::{
        format::text_to_html, keys::generate_keypair_async, password::hash_password,
        sanitize::sanitize_display_name,
    },
};

/// The registration's second half, reached only once every rule passed:
/// hash the password, generate the keys, assemble the [`NewUser`] and
/// insert it — on the pool alone, or, when `invite` names one, inside a
/// transaction with the invite's consumption (see [`create_local_user`]).
///
/// The two registration modes decide the account's starting state here
/// (Pleroma's `register_changeset`, which reads the same two config keys):
/// `REGISTRATIONS_APPROVAL_REQUIRED` sets `approval_pending` unless the
/// registration came in on an invite, and `ACCOUNT_ACTIVATION_REQUIRED`
/// clears `email_confirmed` for an account with an address to confirm —
/// unless the provider already verified that address
/// ([`RegisterParams::provider_email_verified`], which starts such an
/// account confirmed). The
/// unconfirmed account's letter goes out afterwards, in [`create_local_user`].
///
/// [`create_local_user`]: super::create_local_user
pub(super) async fn build_and_insert(
    state: &AppState,
    params: &RegisterParams,
    nickname: &str,
    email: Option<String>,
    invite: Option<&InviteToken>,
) -> Result<User, RegisterError> {
    let password_hash = hash_password(&params.password)?;
    let keys = generate_keypair_async().await?;
    let ap_id = local_ap_id(&state.config.base_url, nickname);
    let shared_inbox = format!("{}/inbox", state.config.base_url);
    let raw_bio = params.bio.clone().unwrap_or_default();
    let bio = if raw_bio.is_empty() {
        String::new()
    } else {
        text_to_html(&raw_bio)
    };
    let activation_required = state.config.account_activation_required.get()
        && email.is_some()
        && !params.provider_email_verified;
    let new_user = NewUser {
        email,
        password_hash: Some(password_hash),
        name: display_name(params.name.as_deref(), nickname),
        bio,
        raw_bio,
        registration_reason: params.reason.clone(),
        approval_pending: registration_waits_for_approval(state, invite),
        email_confirmed: Flag::new(!activation_required),
        inbox: Some(inbox(&ap_id)),
        shared_inbox: Some(shared_inbox),
        outbox: Some(outbox(&ap_id)),
        followers_url: Some(followers(&ap_id)),
        following_url: Some(following(&ap_id)),
        featured_url: Some(featured(&ap_id)),
        public_key: Some(keys.public_pem),
        private_key: Some(keys.private_pem),
        ..NewUser::local(nickname, &ap_id)
    };
    let inserted = match invite {
        None => insert(&state.db, &new_user).await,
        Some(spent) => insert_with_invite(&state.db, &new_user, spent).await,
    };
    match inserted {
        Ok(user) => Ok(user),
        Err(err) => Err(map_insert_error(err)),
    }
}

/// The invited registration's write: user insert and invite consumption
/// in one transaction, so the two land together or not at all. A failed
/// insert aborts the transaction and the invite keeps its count and its
/// flag; the [`AppError`]s propagate as the caller's own (a
/// [`consume`] miss — the invite revoked under the registration's feet —
/// stays the internal arm it reports).
///
/// [`consume`]: invite_token::consume
/// [`AppError`]: crate::error::AppError
async fn insert_with_invite(
    pool: &PgPool,
    new_user: &NewUser,
    spent: &InviteToken,
) -> AppResult<User> {
    let mut tx = pool.begin().await?;
    let user = insert(&mut *tx, new_user).await?;
    invite_token::consume(&mut *tx, spent.id).await?;
    tx.commit().await?;
    Ok(user)
}

/// Whether the submitted nickname and email are free. The lookups are
/// citext comparisons, so "taken" means taken in any case.
pub(super) async fn check_taken(
    state: &AppState,
    nickname: &str,
    email: Option<&str>,
    errors: &mut FieldErrors,
) -> Result<(), RegisterError> {
    if get_by_nickname(&state.db, nickname).await?.is_some() {
        add_error(errors, "nickname", HAS_ALREADY_BEEN_TAKEN);
    }
    if let Some(email) = email
        && get_by_email(&state.db, email).await?.is_some()
    {
        add_error(errors, "email", HAS_ALREADY_BEEN_TAKEN);
    }
    Ok(())
}

/// The display name to store: the requested name reduced to one line of
/// plain text, falling back to the nickname when nothing usable was given
/// (Pleroma renders `name` through the same display-name sanitizer).
fn display_name(requested: Option<&str>, nickname: &str) -> String {
    requested
        .map(sanitize_display_name)
        .filter(|name| !name.is_empty())
        .unwrap_or_else(|| nickname.to_owned())
}

#[cfg(test)]
mod tests {
    use uuid::Uuid;

    use super::*;
    use crate::{
        config::{Config, TEST_BASE_URL},
        models::user::register::{
            create_local_user,
            test_support::*,
            validate::{CANNOT_BE_BLANK, HAS_INVALID_FORMAT, at_least},
        },
    };

    /// Reads the invite `id` back, panicking if it is gone.
    async fn stored_invite(pool: &PgPool, id: Uuid) -> InviteToken {
        invite_token::list(pool)
            .await
            .expect("the list runs")
            .into_iter()
            .find(|invite| invite.id == id)
            .expect("the invite exists")
    }

    #[test]
    fn display_names_default_to_the_nickname() {
        assert_eq!(display_name(None, "alice"), "alice", "absent");
        assert_eq!(
            display_name(Some("   "), "alice"),
            "alice",
            "blank after sanitizing"
        );
        assert_eq!(
            display_name(
                Some("<script>evil()</script>Bob <b>the</b> Builder"),
                "alice"
            ),
            "Bob the Builder",
            "markup reduces to one line of text"
        );
        assert_eq!(
            display_name(Some("  Real Name  "), "alice"),
            "Real Name",
            "ends are trimmed"
        );
    }

    #[tokio::test]
    async fn register_honours_email_optional_and_requested_name() {
        let db = crate::test_db::test_db().await.expect("test database slot");
        let pool = db.pool().clone();
        let config = Config {
            accounts_email_optional: true,
            ..Config::for_tests()
        };
        let state = test_state_with(&pool, config);
        let params = RegisterParams {
            nickname: "noemail".to_owned(),
            email: None,
            password: PASSWORD.to_owned(),
            name: Some("<b>Mr Nobody</b>".to_owned()),
            bio: None,
            reason: None,
            provider_email_verified: false,
        };
        let user = create_local_user(&state, params, None).await.unwrap();
        assert!(user.email.is_none(), "the account has no email");
        assert_eq!(user.name, "Mr Nobody", "the sanitized requested name");
        assert_eq!(user.bio, "", "absent bio is empty HTML");
        assert_eq!(user.raw_bio, "", "absent bio has no source text");
        assert!(
            user.registration_reason.is_none(),
            "an unstated reason stays NULL"
        );
    }

    #[tokio::test]
    async fn register_collects_every_failure_and_inserts_nothing() {
        let db = crate::test_db::test_db().await.expect("test database slot");
        let pool = db.pool().clone();
        let state = test_state(&pool);

        let bad_shape = RegisterParams {
            nickname: "bad nickname!".to_owned(),
            email: Some("not-an-email".to_owned()),
            password: SHORT_PASSWORD.to_owned(),
            name: None,
            bio: None,
            reason: None,
            provider_email_verified: false,
        };
        let shape_err = create_local_user(&state, bad_shape, None)
            .await
            .unwrap_err();
        let RegisterError::Invalid(shape_errors) = shape_err else {
            panic!("a validation failure must be Invalid, got {shape_err:?}")
        };
        assert_eq!(
            shape_errors.get("nickname").expect("nickname errors"),
            &vec![HAS_INVALID_FORMAT.to_owned()],
            "the nickname's format error"
        );
        assert_eq!(
            shape_errors.get("email").expect("email errors"),
            &vec![HAS_INVALID_FORMAT.to_owned()],
            "the email's format error"
        );
        assert_eq!(
            shape_errors.get("password").expect("password errors"),
            &vec![at_least(8)],
            "the password's length error"
        );

        let all_blank = RegisterParams {
            nickname: " ".to_owned(),
            email: None,
            password: String::new(),
            name: None,
            bio: None,
            reason: None,
            provider_email_verified: false,
        };
        let blank_err = create_local_user(&state, all_blank, None)
            .await
            .unwrap_err();
        let RegisterError::Invalid(blank_errors) = blank_err else {
            panic!("blank parameters must be Invalid, got {blank_err:?}")
        };
        let mut expected = FieldErrors::new();
        add_error(&mut expected, "nickname", CANNOT_BE_BLANK);
        add_error(&mut expected, "email", CANNOT_BE_BLANK);
        add_error(&mut expected, "password", CANNOT_BE_BLANK);
        assert_eq!(blank_errors, expected, "all three blanks land in one map");

        assert_eq!(user_count(&state.db).await, 0, "nothing was inserted");
    }

    #[tokio::test]
    async fn duplicates_are_taken_in_any_case() {
        let db = crate::test_db::test_db().await.expect("test database slot");
        let pool = db.pool().clone();
        let alice = insert_login(&pool, "Alice", "Alice@Example.ORG", None).await;
        let bob = insert_login(&pool, "bob", "bob@example.org", None).await;
        let state = test_state(&pool);
        let dup_params = RegisterParams {
            nickname: "aLiCe".to_owned(),
            email: Some("alice@example.org".to_owned()),
            ..params_for("aLiCe")
        };
        let dup_err = create_local_user(&state, dup_params, None)
            .await
            .unwrap_err();
        let RegisterError::Invalid(dup_errors) = dup_err else {
            panic!("a duplicate must be Invalid, got {dup_err:?}")
        };
        assert_eq!(
            dup_errors.get("nickname").expect("nickname errors"),
            &vec![HAS_ALREADY_BEEN_TAKEN.to_owned()],
            "the citext nickname collision is found"
        );
        assert_eq!(
            dup_errors.get("email").expect("email errors"),
            &vec![HAS_ALREADY_BEEN_TAKEN.to_owned()],
            "the citext email collision is found"
        );

        let carol_params = RegisterParams {
            email: Some("BOB@example.org".to_owned()),
            ..params_for("carol")
        };
        let carol_err = create_local_user(&state, carol_params, None)
            .await
            .unwrap_err();
        let RegisterError::Invalid(carol_errors) = carol_err else {
            panic!("a duplicate email must be Invalid, got {carol_err:?}")
        };
        assert_eq!(
            carol_errors.keys().copied().collect::<Vec<_>>(),
            vec!["email"],
            "only the email is taken for carol"
        );
        assert_eq!(
            carol_errors.get("email").expect("email errors"),
            &vec![HAS_ALREADY_BEEN_TAKEN.to_owned()],
            "bob's email blocks carol in any case"
        );

        assert_eq!(
            user_count(&pool).await,
            2,
            "only {alice:?} and {bob:?} are there"
        );
    }

    #[tokio::test]
    async fn a_lost_insert_race_maps_to_the_taken_field() {
        let db = crate::test_db::test_db().await.expect("test database slot");
        let pool = db.pool().clone();
        // Steal `alice`'s actor id from under the availability check: the
        // nickname looks free, so every check passes and only the INSERT
        // notices the `users_ap_id_key` conflict.
        let squatter = insert(
            &pool,
            &NewUser {
                ap_id: local_ap_id(TEST_BASE_URL, "alice"),
                ..local_new("mallory")
            },
        )
        .await
        .expect("the squatter inserts");
        assert_eq!(squatter.nickname, "mallory", "the squatter owns ap_id");
        let state = test_state(&pool);
        let race_err = create_local_user(&state, params_for("alice"), None)
            .await
            .unwrap_err();
        let RegisterError::Invalid(race_errors) = race_err else {
            panic!("the race must land as Invalid, got {race_err:?}")
        };
        assert_eq!(
            race_errors.get("ap_id").expect("ap_id errors"),
            &vec![HAS_ALREADY_BEEN_TAKEN.to_owned()],
            "the 23505 names its field just like the checks do"
        );
        assert_eq!(user_count(&pool).await, 1, "the race inserted nothing");
    }

    #[tokio::test]
    async fn an_invited_registration_consumes_the_invite_in_its_transaction() {
        let db = crate::test_db::test_db().await.expect("test database slot");
        let pool = db.pool().clone();
        let state = test_state(&pool);
        let invite = invite_token::create(&pool, &invite_token::InviteParams::one_time())
            .await
            .expect("the invite mints");
        let user = create_local_user(&state, params_for("invited"), Some(&invite))
            .await
            .expect("the invited registration lands");
        assert!(user.is_active.get(), "an invited user is active");
        assert!(
            !user.approval_pending.get(),
            "an invited user skips approval: it never awaits one"
        );
        let spent = stored_invite(&pool, invite.id).await;
        assert_eq!(spent.uses, 1, "the registration was counted");
        assert!(spent.used, "and the one-time invite is spent");
    }

    #[tokio::test]
    async fn a_rejected_invited_registration_spends_nothing() {
        let db = crate::test_db::test_db().await.expect("test database slot");
        let pool = db.pool().clone();
        let squatter = insert_login(&pool, "taken", "taken@example.org", None).await;
        assert_eq!(squatter.nickname, "taken", "the collision fixture exists");
        let state = test_state(&pool);
        let invite = invite_token::create(
            &pool,
            &invite_token::InviteParams {
                expires_at: None,
                max_use: Some(2),
            },
        )
        .await
        .expect("the invite mints");
        // The same nickname the insert race would lose on: validation
        // fails after the invite was picked up, and the shared
        // transaction must leave the invite exactly as it was.
        let dup_params = RegisterParams {
            nickname: "taken".to_owned(),
            ..params_for("taken")
        };
        let err = create_local_user(&state, dup_params, Some(&invite))
            .await
            .expect_err("the duplicate nickname is refused");
        assert!(
            matches!(err, RegisterError::Invalid(_)),
            "the field-map error survives, got {err:?}"
        );
        let untouched = stored_invite(&pool, invite.id).await;
        assert_eq!(untouched.uses, 0, "a refused registration counted nothing");
        assert!(!untouched.used, "and spent nothing");
    }
}
