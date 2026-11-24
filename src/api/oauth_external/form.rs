//! The data the registration form moves on: the `pending` document an
//! unlinked callback leaves in the state ([`PendingRegistration`]), the form
//! the browser posts ([`RegisterForm`]), the two resolved into one
//! submission ([`Submitted`]), and [`first_text`] — the fallback rule that
//! resolves them. The routes that render and consume them are the sibling
//! `registration` child's.

use std::fmt;

use serde::Deserialize;

use crate::{
    models::{oauth_login_state::OauthLoginState, user::register::RegisterParams},
    util::token::random_token,
};

use super::EXTERNAL_PASSWORD_BYTES;

/// The `params.pending` document of an unlinked callback, read back by the
/// registration flow: everything the form prefills beside the two facts
/// that make the identity link — the provider and its account id.
#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
pub(super) struct PendingRegistration {
    /// Provider the round trip ran on; the `user_identities` key.
    pub(super) provider: String,
    /// The provider account's stable id; the link's other key.
    pub(super) uid: String,
    /// `nickname_field`, when the provider said it.
    #[serde(default)]
    pub(super) nickname: Option<String>,
    /// `email_field`, when the provider said it.
    #[serde(default)]
    pub(super) email: Option<String>,
    /// `name_field`, when the provider said it.
    #[serde(default)]
    pub(super) name: Option<String>,
}

impl PendingRegistration {
    /// The `pending` document of a live state, or `None` when the state
    /// carries none (the callback never ran on it) or names no account to
    /// link: reading a provider account out of nothing is not this flow's
    /// business.
    pub(super) fn of_state(stored: &OauthLoginState) -> Option<Self> {
        let raw = stored.params.get("pending")?;
        let pending: Self = serde_json::from_value(raw.clone()).ok()?;
        (!pending.provider.is_empty() && !pending.uid.is_empty()).then_some(pending)
    }
}

/// The `POST /oauth/register` form. The state rides hidden; every field
/// the flow keeps is optional at the shape level — blank submissions fall
/// back to the provider's info the way Pleroma's
/// `create_from_registration` reads the form above the stored
/// registration, and an absent nickname fails validation like a blank one.
#[derive(Deserialize)]
pub(super) struct RegisterForm {
    /// The state [`registration_details`] carried through the form.
    ///
    /// [`registration_details`]: super::registration_details
    #[serde(default)]
    pub(super) state: Option<String>,
    /// Desired handle.
    #[serde(default)]
    pub(super) nickname: Option<String>,
    /// Contact email.
    #[serde(default)]
    pub(super) email: Option<String>,
    /// Display name.
    #[serde(default)]
    pub(super) name: Option<String>,
    /// Profile note as source text.
    #[serde(default)]
    pub(super) bio: Option<String>,
    /// A password of the registrant's own choosing. Only the JSON
    /// registration reads it; the HTML form has no such field and ignores
    /// it. Never in `Debug` output.
    #[serde(default)]
    pub(super) password: Option<String>,
}

/// [`password`](RegisterForm::password) is secret material and renders as
/// `<redacted>`; everything else shows.
impl fmt::Debug for RegisterForm {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("RegisterForm")
            .field("state", &"<redacted>")
            .field("nickname", &self.nickname)
            .field("email", &self.email)
            .field("name", &self.name)
            .field("bio", &self.bio)
            .field("password", &"<redacted>")
            .finish()
    }
}

/// The submission's fields, resolved against the provider's `pending`
/// info: what [`RegisterForm`] says wins, the provider stands in where the
/// form left a field blank.
#[derive(Debug)]
pub(super) struct Submitted {
    /// The handle to register: the form's, the provider's, or blank —
    /// which [`create_local_user`] refuses like any other blank.
    ///
    /// [`create_local_user`]: crate::models::user::register::create_local_user
    nickname: String,
    /// The email to register, or `None` when neither says one.
    email: Option<String>,
    /// The display name, or `None` when neither says one (then the
    /// nickname stands in, as registration always does).
    name: Option<String>,
    /// The profile note, verbatim from the form.
    bio: Option<String>,
    /// Whether the provider vouched for the registered address: its
    /// `pending` email equals the resolved one, case-insensitively. That
    /// is the ticket's proof — an `ACCOUNT_ACTIVATION_REQUIRED` instance
    /// registers such an account confirmed, and no letter goes out.
    provider_email_verified: bool,
}

impl Submitted {
    /// Resolve `form`'s fields against `pending`'s. The nickname falls back
    /// to the blank that validation refuses; the rest fall back to nothing.
    pub(super) fn of_form(form: &RegisterForm, pending: &PendingRegistration) -> Self {
        let nickname =
            first_text(form.nickname.as_deref(), pending.nickname.as_deref()).unwrap_or_default();
        let email = first_text(form.email.as_deref(), pending.email.as_deref());
        let provider_email_verified = email.as_deref().is_some_and(|registered| {
            pending.email.as_deref().is_some_and(|said| {
                !said.trim().is_empty() && said.trim().eq_ignore_ascii_case(registered)
            })
        });
        Self {
            nickname,
            email,
            name: first_text(form.name.as_deref(), pending.name.as_deref()),
            bio: form.bio.clone(),
            provider_email_verified,
        }
    }

    /// The [`RegisterParams`] of a resolved submission: the password is the
    /// registrant's own when `chosen` carries one (the JSON registration),
    /// otherwise a fresh random token nobody will ever type (see
    /// [`EXTERNAL_PASSWORD_BYTES`]), and no registration reason or invite
    /// belongs to an external signup.
    pub(super) fn into_register_params(self, chosen: Option<&str>) -> RegisterParams {
        let password = chosen
            .filter(|text| !text.is_empty())
            .map_or_else(|| random_token(EXTERNAL_PASSWORD_BYTES), str::to_owned);
        RegisterParams {
            nickname: self.nickname,
            email: self.email,
            password,
            name: self.name,
            bio: self.bio,
            reason: None,
            provider_email_verified: self.provider_email_verified,
        }
    }
}

/// The first non-blank text of the two, trimmed: what the form said wins,
/// the provider's `pending` value stands in when the form left the field
/// blank — Pleroma's `create_from_registration` reads its submission above
/// the stored registration info the same way.
fn first_text(submitted: Option<&str>, stored: Option<&str>) -> Option<String> {
    submitted
        .into_iter()
        .chain(stored)
        .map(str::trim)
        .find(|text| !text.is_empty())
        .map(str::to_owned)
}

#[cfg(test)]
mod tests {
    use chrono::Utc;
    use serde_json::{Value, json};

    use super::*;

    #[test]
    fn the_pending_document_reads_back_out_of_a_state_row() {
        let row = |params: Value| OauthLoginState {
            state: "a-state".to_owned(),
            provider: "github".to_owned(),
            params,
            valid_until: Utc::now(),
        };
        let pending = PendingRegistration::of_state(&row(json!({
            "client_id": "c",
            "pending": {
                "provider": "github",
                "uid": "4242",
                "nickname": "octocat",
                "email": null,
                "name": "The Octocat",
            },
        })))
        .expect("the callback's document reads");
        assert_eq!(pending.provider, "github", "the link's provider");
        assert_eq!(pending.uid, "4242", "the link's account id");
        assert_eq!(pending.nickname.as_deref(), Some("octocat"), "the prefill");
        assert_eq!(pending.email, None, "a null is the provider saying nothing");
        assert_eq!(
            pending.name.as_deref(),
            Some("The Octocat"),
            "and the rest comes through"
        );

        for unusable in [
            json!({"client_id": "c"}),
            json!({"pending": null}),
            json!({"pending": "nope"}),
            json!({"pending": {"provider": "github", "uid": ""}}),
            json!({"pending": {"provider": "", "uid": "4242"}}),
            json!({"pending": {"uid": "4242"}}),
            json!({"pending": {"provider": 7, "uid": "4242"}}),
        ] {
            assert!(
                PendingRegistration::of_state(&row(unusable.clone())).is_none(),
                "`{unusable}` names no account to register: it is no state",
            );
        }
    }

    #[test]
    fn submitted_fields_prefer_the_form_and_fall_back_to_the_provider() {
        let pending = PendingRegistration {
            provider: "github".to_owned(),
            uid: "4242".to_owned(),
            nickname: Some("octocat".to_owned()),
            email: Some("OctoCat@GitHub.Example".to_owned()),
            name: None,
        };
        let form: RegisterForm = crate::extract::into_params(json!({
            "state": "s",
            "nickname": "  chosen  ",
            "email": "",
            "bio": " hi ",
        }))
        .expect("every field is optional");
        let submitted = Submitted::of_form(&form, &pending);
        assert_eq!(submitted.nickname, "chosen", "the form's, trimmed");
        assert_eq!(
            submitted.email.as_deref(),
            Some("OctoCat@GitHub.Example"),
            "blank falls back to the provider's"
        );
        assert_eq!(submitted.name, None, "nothing says a name");
        assert_eq!(
            submitted.bio.as_deref(),
            Some(" hi "),
            "the bio is the form's"
        );
        assert!(
            submitted.provider_email_verified,
            "the fallback email IS the provider's: the address is vouched for"
        );

        let mismatch: RegisterForm = crate::extract::into_params(json!({
            "state": "s",
            "email": "someone-else@example.org",
        }))
        .expect("the optional fields may all be absent");
        let changed = Submitted::of_form(&mismatch, &pending);
        assert!(
            !changed.provider_email_verified,
            "a different email needs the confirmation click after all"
        );

        let nothing = PendingRegistration {
            email: None,
            ..pending
        };
        assert!(
            !Submitted::of_form(&form, &nothing).provider_email_verified,
            "a provider that said no email vouched for nothing"
        );
        let both_blank = Submitted::of_form(
            &crate::extract::into_params(json!({"state": "s", "nickname": "  "}))
                .expect("a bare submission"),
            &PendingRegistration {
                provider: "github".to_owned(),
                uid: "4242".to_owned(),
                nickname: None,
                email: Some("   ".to_owned()),
                name: None,
            },
        );
        assert_eq!(both_blank.nickname, "", "no nickname anywhere is the blank");
        assert_eq!(both_blank.email, None, "and a blank provider email is none");
    }

    #[test]
    fn the_external_password_is_a_fresh_random_secret() {
        let pending = PendingRegistration {
            provider: "github".to_owned(),
            uid: "4242".to_owned(),
            nickname: Some("octocat".to_owned()),
            email: None,
            name: None,
        };
        let form: RegisterForm =
            crate::extract::into_params(json!({"state": "s"})).expect("all optional");
        let first = Submitted::of_form(&form, &pending).into_register_params(None);
        let second = Submitted::of_form(&form, &pending).into_register_params(None);
        assert_eq!(first.password.len(), 86, "64 random bytes, base64url");
        assert_ne!(
            first.password, second.password,
            "and never the same secret twice"
        );
        assert!(
            first
                .password
                .chars()
                .all(|c| c.is_ascii_alphanumeric() || c == '-' || c == '_'),
            "URL-safe by construction: {}",
            first.password
        );
        assert_eq!(
            first.nickname, "octocat",
            "the provider's nickname stands in"
        );
        assert_eq!(first.email, None, "no email is no email");
        assert!(first.reason.is_none(), "an external signup gives no reason");
        assert!(
            !first.provider_email_verified,
            "nothing was vouched for when the provider said no email"
        );
    }

    #[test]
    fn a_chosen_password_replaces_the_random_one_unless_blank() {
        let pending = PendingRegistration {
            provider: "github".to_owned(),
            uid: "4242".to_owned(),
            nickname: Some("octocat".to_owned()),
            email: None,
            name: None,
        };
        let form: RegisterForm = crate::extract::into_params(json!({"state": "s"}))
            .expect("the password is an optional field");
        let chosen =
            Submitted::of_form(&form, &pending).into_register_params(Some("my own secret"));
        assert_eq!(chosen.password, "my own secret", "the registrant's own");
        let blank = Submitted::of_form(&form, &pending).into_register_params(Some(""));
        assert_eq!(blank.password.len(), 86, "a blank one is no choice at all");
    }

    #[test]
    fn the_form_debug_output_hides_the_password_and_the_state() {
        let form: RegisterForm = crate::extract::into_params(json!({
            "state": "a-very-secret-state",
            "nickname": "octocat",
            "password": "a-very-secret-password",
        }))
        .expect("a submission");
        let debug = format!("{form:?}");
        assert!(
            !debug.contains("a-very-secret-password"),
            "no password: {debug}"
        );
        assert!(!debug.contains("a-very-secret-state"), "no state: {debug}");
        assert!(debug.contains("octocat"), "the rest shows: {debug}");
    }
}
