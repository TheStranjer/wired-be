//! The instance's email templates: plain-text messages built from the
//! instance settings, one function per letter.
//!
//! The reference is Pleroma's `lib/pleroma/emails/user_email.ex` and
//! `lib/pleroma/emails/admin_email.ex`, whose HTML bodies are kept here as
//! text: every message this module returns is an [`Email`] with a `text_body`
//! only. The instance name comes from `INSTANCE_NAME`
//! ([`Config::instance_name`]) and a link given as a path is completed
//! against `BASE_URL` ([`Config::base_url`]), so a body always carries the
//! absolute URL the recipient can open. Later tickets queue these through
//! [`deliver_later`](super::deliver_later); nothing in this module touches
//! the database or the relay.

use crate::{config::Config, mailer::Email};

/// The registration confirmation letter: `to` must prove the address by
/// opening `link` (Pleroma's `account_confirmation_email`).
#[must_use]
pub fn registration_confirmation(config: &Config, to: &str, link: &str) -> Email {
    let link = absolute_link(config, link);
    let name = config.instance_name.as_str();
    Email::new(
        to,
        &format!("{name} account confirmation"),
        &format!(
            "Thank you for registering on {name}.\n\
             \n\
             Email confirmation is required to activate the account.\n\
             Please open the following link to activate your account:\n\
             \n\
             {link}\n"
        ),
    )
}

/// The password reset letter carrying `link` (Pleroma's
/// `password_reset_email`): only the holder of the mailbox can use it, and a
/// reset is never performed by the email itself.
#[must_use]
pub fn password_reset(config: &Config, to: &str, link: &str) -> Email {
    let link = absolute_link(config, link);
    let name = config.instance_name.as_str();
    Email::new(
        to,
        "Password reset",
        &format!(
            "Someone has requested a password change for your account at {name}.\n\
             \n\
             If it was you, open the following link to proceed:\n\
             \n\
             {link}\n\
             \n\
             If it was someone else, nothing to worry about: your data is\n\
             secure and your password has not been changed.\n"
        ),
    )
}

/// The approval letter: staff let `to` onto `name` (Pleroma's
/// successful-registration notice), so the account is live.
#[must_use]
pub fn account_approved(config: &Config, to: &str) -> Email {
    let name = config.instance_name.as_str();
    Email::new(
        to,
        &format!("Your account on {name} is approved"),
        &format!(
            "Your account at {name} has been approved by the instance staff.\n\
             \n\
             You can sign in and start posting now.\n"
        ),
    )
}

/// The rejection letter: staff refused `to`'s registration on `name`.
#[must_use]
pub fn account_rejected(config: &Config, to: &str) -> Email {
    let name = config.instance_name.as_str();
    Email::new(
        to,
        &format!("Your account on {name} has been rejected"),
        &format!(
            "Your registration at {name} has been reviewed and rejected by\n\
             the instance staff.\n\
             \n\
             If you believe this is a mistake, contact the instance\n\
             administrators and explain.\n"
        ),
    )
}

/// The user invitation letter: `to` may register on `name` through `link`
/// (Pleroma's `user_invitation_email`).
#[must_use]
pub fn user_invitation(config: &Config, to: &str, link: &str) -> Email {
    let link = absolute_link(config, link);
    let name = config.instance_name.as_str();
    Email::new(
        to,
        &format!("Invitation to {name}"),
        &format!(
            "You are invited to {name}.\n\
             \n\
             Open the following link to register:\n\
             \n\
             {link}\n"
        ),
    )
}

/// The report letter to an admin mailbox `to`: `reporter` filed a report
/// about `account`, quoting their `comment` (Pleroma's
/// `AdminEmail.report`, as text).
#[must_use]
pub fn report_to_admin(
    config: &Config,
    to: &str,
    reporter: &str,
    account: &str,
    comment: &str,
) -> Email {
    let name = config.instance_name.as_str();
    Email::new(
        to,
        &format!("{name} Report"),
        &format!(
            "A report was filed on {name}.\n\
             \n\
             Reported by: @{reporter}\n\
             Reported account: @{account}\n\
             Comment: {comment}\n"
        ),
    )
}

/// The absolute URL for `link`: kept verbatim when it already carries a
/// scheme, hung off [`Config::base_url`] when it is a path (extra slashes
/// at the join collapse into one).
fn absolute_link(config: &Config, link: &str) -> String {
    if link.starts_with("http://") || link.starts_with("https://") {
        return link.to_owned();
    }
    format!("{}/{}", config.base_url, link.trim_matches('/'))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::Config;

    /// The fixed test-instance configuration the templates render against.
    fn config() -> Config {
        Config::for_tests()
    }

    /// The instance name and base URL of [`config`], the two facts every
    /// template must echo.
    const NAME: &str = "Wired";
    const BASE: &str = "https://test.example";

    #[test]
    fn registration_confirmation_carries_the_link_and_the_instance_name() {
        let email = registration_confirmation(&config(), "new@example.org", "/confirm/tok1");
        assert_eq!(email.to(), "new@example.org", "recipient");
        assert!(
            email.subject().contains(NAME),
            "subject names the instance: {}",
            email.subject()
        );
        assert!(
            email.text_body().contains(NAME),
            "body names the instance: {}",
            email.text_body()
        );
        assert!(
            email.text_body().contains(&format!("{BASE}/confirm/tok1")),
            "body carries the BASE_URL-built link: {}",
            email.text_body()
        );
        assert_eq!(email.html_body(), None, "plain text only");
    }

    #[test]
    fn password_reset_carries_the_link_and_the_instance_name() {
        let email = password_reset(&config(), "who@example.org", "/reset/token2");
        assert_eq!(email.to(), "who@example.org", "recipient");
        assert_eq!(email.subject(), "Password reset", "Pleroma's subject");
        assert!(
            email.text_body().contains(NAME)
                && email.text_body().contains(&format!("{BASE}/reset/token2")),
            "body names the instance and carries the link: {}",
            email.text_body()
        );
    }

    #[test]
    fn password_reset_carries_the_frontend_link() {
        let mut config = config();
        config.frontend_base_url = "https://fe.example".to_owned();
        let link = crate::api::password_reset::frontend_link(&config, "token3");
        let email = password_reset(&config, "who@example.org", &link);
        assert!(
            email
                .text_body()
                .contains("https://fe.example/password_reset/token3"),
            "body carries the frontend reset link: {}",
            email.text_body()
        );
    }

    #[test]
    fn account_approved_names_the_instance() {
        let email = account_approved(&config(), "waiting@example.org");
        assert_eq!(email.to(), "waiting@example.org", "recipient");
        assert!(
            email.subject().contains(NAME) && email.text_body().contains(NAME),
            "both name the instance: {} / {}",
            email.subject(),
            email.text_body()
        );
    }

    #[test]
    fn account_rejected_names_the_instance() {
        let email = account_rejected(&config(), "doomed@example.org");
        assert_eq!(email.to(), "doomed@example.org", "recipient");
        assert!(
            email.subject().contains(NAME) && email.text_body().contains(NAME),
            "both name the instance: {} / {}",
            email.subject(),
            email.text_body()
        );
    }

    #[test]
    fn user_invitation_carries_the_link_and_the_instance_name() {
        let email = user_invitation(
            &config(),
            "friend@example.org",
            "/main/auth/register?token=inv1",
        );
        assert_eq!(email.to(), "friend@example.org", "recipient");
        assert!(
            email.subject().contains(NAME),
            "subject names the instance: {}",
            email.subject()
        );
        assert!(
            email
                .text_body()
                .contains(&format!("{BASE}/main/auth/register?token=inv1")),
            "body carries the BASE_URL-built link: {}",
            email.text_body()
        );
    }

    #[test]
    fn report_to_admin_names_everyone_involved() {
        let email = report_to_admin(
            &config(),
            "admin@example.org",
            "alice",
            "bob",
            "posted spam",
        );
        assert_eq!(email.to(), "admin@example.org", "the admin receives it");
        assert!(
            email.subject().contains(NAME),
            "subject names the instance: {}",
            email.subject()
        );
        let body = email.text_body();
        assert!(
            body.contains("@alice") && body.contains("@bob") && body.contains("posted spam"),
            "body states reporter, account and comment: {body}"
        );
    }

    #[test]
    fn absolute_links_are_kept_verbatim() {
        let email = password_reset(&config(), "who@example.org", "https://other.example/r/1");
        assert!(
            email.text_body().contains("https://other.example/r/1"),
            "a full URL is not rewritten onto BASE_URL: {}",
            email.text_body()
        );
    }

    #[test]
    fn link_paths_join_the_base_url_with_one_slash() {
        for path in ["confirm/abc", "/confirm/abc", "//confirm/abc//"] {
            let email = registration_confirmation(&config(), "n@e.org", path);
            assert!(
                email.text_body().contains(&format!("{BASE}/confirm/abc")),
                "path {path:?} joins onto one slash: {}",
                email.text_body()
            );
        }
    }
}
