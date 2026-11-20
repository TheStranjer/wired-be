//! What the two registration-mode settings decide about a new account
//! before its row is written.
//!
//! [`registration_waits_for_approval`] answers the approval half:
//! `REGISTRATIONS_APPROVAL_REQUIRED` leaves the account awaiting staff,
//! except on an invite — an invite already is a staff decision, made in
//! advance. The activation half (`ACCOUNT_ACTIVATION_REQUIRED` and its
//! `email_confirmed` / confirmation-token consequences) is read where the
//! row is assembled, in [`super::insert`]; the flags, not the answers, are
//! what the endpoints act on afterwards.

use crate::{models::invite_token::InviteToken, models::user::Flag, state::AppState};

/// Whether the account this registration creates starts out awaiting staff
/// approval: `REGISTRATIONS_APPROVAL_REQUIRED` set and no invite token
/// spent on the registration (Pleroma's `Registration.create_user_with_invite`
/// passes `approved: true` for an invited user, so an invite is a staff
/// decision made in advance). The flag, not the answer, is what the login
/// gate and `POST /api/v1/accounts` read back off the row.
pub(super) fn registration_waits_for_approval(
    state: &AppState,
    invite: Option<&InviteToken>,
) -> Flag {
    Flag::new(state.config.registrations_approval_required.get() && invite.is_none())
}

#[cfg(test)]
mod tests {
    use sqlx::PgPool;
    use uuid::Uuid;

    use crate::{
        config::{Config, Toggle},
        jobs::QUEUE_MAILER,
        models::{
            invite_token,
            user::{
                get_by_id,
                register::{RegisterParams, create_local_user, test_support::*},
            },
        },
    };

    /// A [`Config`] in the two registration modes `approval` and
    /// `activation`, with `SMTP_HOST` set so a confirmation letter really
    /// queues (mail disabled would drop it before the table saw it).
    fn mode_config(approval: Toggle, activation: Toggle) -> Config {
        Config {
            registrations_approval_required: approval,
            account_activation_required: activation,
            smtp_host: Some("smtp.example.org".to_owned()),
            ..Config::for_tests()
        }
    }

    /// The live mailer jobs, oldest first: the letters a registration owed.
    async fn queued_letters(pool: &PgPool) -> Vec<serde_json::Value> {
        sqlx::query_as::<_, (serde_json::Value,)>(
            "SELECT args FROM jobs WHERE queue = $1 ORDER BY inserted_at",
        )
        .bind(QUEUE_MAILER)
        .fetch_all(pool)
        .await
        .expect("the jobs read")
        .into_iter()
        .map(|(args,)| args)
        .collect()
    }

    /// The two registration-mode flags plus the token, as the stored row
    /// says them: what `POST /api/v1/accounts` reads back to pick its
    /// answer.
    async fn stored_state(pool: &PgPool, id: Uuid) -> (bool, bool, Option<String>) {
        let row = get_by_id(pool, id).await.unwrap().expect("the row exists");
        (
            row.approval_pending.get(),
            row.email_confirmed.get(),
            row.confirmation_token,
        )
    }

    /// A plain registration on a plain instance is the account the token
    /// response promises: active, approved, confirmed, nothing queued.
    #[tokio::test]
    async fn a_modeless_registration_lands_ready() {
        let db = crate::test_db::test_db().await.expect("test database slot");
        let pool = db.pool().clone();
        let state = test_state_with(&pool, mode_config(Toggle::new(false), Toggle::new(false)));

        let user = create_local_user(&state, params_for("plain"), None)
            .await
            .expect("the registration lands");

        assert!(user.is_active.get(), "the account is active");
        assert_eq!(
            stored_state(&pool, user.id).await,
            (false, true, None),
            "nothing waits on it: no approval, no confirmation, no token"
        );
        assert!(
            queued_letters(&pool).await.is_empty(),
            "and no letter was queued"
        );
    }

    #[tokio::test]
    async fn an_approval_mode_registration_lands_pending() {
        let db = crate::test_db::test_db().await.expect("test database slot");
        let pool = db.pool().clone();
        let state = test_state_with(&pool, mode_config(Toggle::new(true), Toggle::new(false)));

        let user = create_local_user(&state, params_for("awaiting"), None)
            .await
            .expect("the registration lands");

        assert!(user.is_active.get(), "approval does not deactivate");
        assert_eq!(
            stored_state(&pool, user.id).await,
            (true, true, None),
            "the account awaits staff, not a mailbox"
        );
        assert!(
            queued_letters(&pool).await.is_empty(),
            "approval queues no letter: the admin flow is a later ticket"
        );
    }

    #[tokio::test]
    async fn an_activation_mode_registration_lands_unconfirmed_with_a_letter() {
        let db = crate::test_db::test_db().await.expect("test database slot");
        let pool = db.pool().clone();
        let state = test_state_with(&pool, mode_config(Toggle::new(false), Toggle::new(true)));

        let user = create_local_user(&state, params_for("confirm-me"), None)
            .await
            .expect("the registration lands");

        let (approval, confirmed, token) = stored_state(&pool, user.id).await;
        assert!(!approval, "confirmation does not ask staff");
        assert!(!confirmed, "the address is not confirmed yet");
        let token = token.expect("a confirmation token was minted");
        assert_eq!(token.len(), 43, "the same credential shape as a token");
        let letters = queued_letters(&pool).await;
        assert_eq!(letters.len(), 1, "exactly one letter was queued");
        assert_eq!(
            letters[0]["to"], "confirm-me@example.org",
            "to the new account"
        );
        assert!(
            letters[0]["text_body"]
                .as_str()
                .unwrap_or_default()
                .contains(&format!(
                    "https://test.example/api/account/confirm_email/{}/{}",
                    user.id, token
                )),
            "the letter carries the confirmation link: {}",
            letters[0]["text_body"]
        );
    }

    #[tokio::test]
    async fn both_modes_together_land_pending_and_unconfirmed() {
        let db = crate::test_db::test_db().await.expect("test database slot");
        let pool = db.pool().clone();
        let state = test_state_with(&pool, mode_config(Toggle::new(true), Toggle::new(true)));

        let user = create_local_user(&state, params_for("both-waits"), None)
            .await
            .expect("the registration lands");

        let (approval, confirmed, token) = stored_state(&pool, user.id).await;
        assert!(approval && !confirmed, "both waits are recorded");
        assert!(token.is_some(), "and the letter is owed all the same");
        assert_eq!(
            queued_letters(&pool).await.len(),
            1,
            "the confirmation letter went out even though staff must answer first"
        );
    }

    /// An invite is a staff decision made in advance, so it skips the
    /// approval wait — but it proves nothing about a mailbox, so the
    /// confirmation gate still closes over the invited account.
    #[tokio::test]
    async fn an_invited_registration_skips_approval_but_not_confirmation() {
        let db = crate::test_db::test_db().await.expect("test database slot");
        let pool = db.pool().clone();
        let state = test_state_with(&pool, mode_config(Toggle::new(true), Toggle::new(true)));
        let invite = invite_token::create(&pool, &invite_token::InviteParams::one_time())
            .await
            .expect("the invite mints");

        let user = create_local_user(&state, params_for("invited-pending"), Some(&invite))
            .await
            .expect("the invited registration lands");

        let (approval, confirmed, token) = stored_state(&pool, user.id).await;
        assert!(!approval, "the invite answered the approval question");
        assert!(!confirmed, "it says nothing about the address");
        assert!(token.is_some(), "so the confirmation flow still runs");
    }

    /// An instance that lets registration omit the email cannot also demand
    /// it be confirmed: with no address there is nothing a link could
    /// prove, so the account lands confirmed rather than locked out.
    #[tokio::test]
    async fn activation_without_an_address_lands_confirmed() {
        let db = crate::test_db::test_db().await.expect("test database slot");
        let pool = db.pool().clone();
        let mut config = mode_config(Toggle::new(false), Toggle::new(true));
        config.accounts_email_optional = true;
        let state = test_state_with(&pool, config);
        let params = RegisterParams {
            email: None,
            ..params_for("no-address")
        };

        let user = create_local_user(&state, params, None)
            .await
            .expect("the addressless registration lands");

        assert_eq!(
            stored_state(&pool, user.id).await,
            (false, true, None),
            "unconfirmable is not the same as unconfirmed"
        );
        assert!(
            queued_letters(&pool).await.is_empty(),
            "and there was nothing to send"
        );
    }

    /// A provider-verified address needs no link to prove it: the same
    /// activation mode that leaves every other registration unconfirmed
    /// lands this one confirmed, and no letter is queued.
    #[tokio::test]
    async fn a_provider_verified_email_lands_confirmed() {
        let db = crate::test_db::test_db().await.expect("test database slot");
        let pool = db.pool().clone();
        let state = test_state_with(&pool, mode_config(Toggle::new(false), Toggle::new(true)));
        let params = RegisterParams {
            provider_email_verified: true,
            ..params_for("external")
        };

        let user = create_local_user(&state, params, None)
            .await
            .expect("the external registration lands");

        assert_eq!(
            stored_state(&pool, user.id).await,
            (false, true, None),
            "confirmed right away: no approval, no confirmation, no token"
        );
        assert!(
            queued_letters(&pool).await.is_empty(),
            "and nothing was mailed to ask for a click"
        );
    }

    /// Approval is a staff decision an external provider cannot vouch
    /// for: the verified email confirms the address, not the wait.
    #[tokio::test]
    async fn a_provider_verified_email_still_waits_for_approval() {
        let db = crate::test_db::test_db().await.expect("test database slot");
        let pool = db.pool().clone();
        let state = test_state_with(&pool, mode_config(Toggle::new(true), Toggle::new(true)));
        let params = RegisterParams {
            provider_email_verified: true,
            ..params_for("awaiting-external")
        };

        let user = create_local_user(&state, params, None)
            .await
            .expect("the external registration lands");

        assert_eq!(
            stored_state(&pool, user.id).await,
            (true, true, None),
            "confirmed, and still awaiting approval"
        );
    }
}
