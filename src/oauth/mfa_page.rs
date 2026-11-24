//! The HTML of `/oauth/mfa`: the second-factor code-entry page.
//!
//! `GET /oauth/mfa` renders it, and a failed `POST /oauth/mfa/verify`
//! re-renders it with an error line above the form. The behavioural
//! reference is Pleroma's `lib/pleroma/web/templates/o_auth/mfa/` —
//! `totp.html.eex` asks for the authenticator code, `recovery.html.eex`
//! for a printed backup code, and each links to the other. Built with
//! the same plain `String` pushes and the same helpers as
//! [`crate::oauth::page`]: no template engine, and every
//! dynamic value HTML-escaped, so an attacker-supplied `state`, token
//! or URI cannot break out of its attribute (decision 1).

use url::form_urlencoded;

use crate::oauth::page::{push_error, push_escaped, push_field, push_head, push_hidden, push_tail};

/// Where the form posts.
const FORM_ACTION: &str = "/oauth/mfa/verify";
/// The page (and link base) both forms live at.
const MFA_PATH: &str = "/oauth/mfa";
/// The heading of the authenticator-code form (Pleroma's
/// "mfa auth page title").
const TOTP_TITLE: &str = "Two-factor authentication";
/// The heading of the backup-code form (Pleroma's
/// "mfa recover page title").
const RECOVERY_TITLE: &str = "Two-factor recovery";
/// The code input's label (Pleroma's "mfa auth code prompt").
const TOTP_LABEL: &str = "Authentication code";
/// The recovery input's label (Pleroma's "mfa recover recovery code prompt").
const RECOVERY_LABEL: &str = "Recovery code";
/// The link text to the authenticator form (Pleroma's "use 2fa code
/// link").
const TOTP_LINK_TEXT: &str = "Enter a two-factor code";
/// The link text to the recovery form (Pleroma's "use recovery code
/// link").
const RECOVERY_LINK_TEXT: &str = "Enter a two-factor recovery code";
/// The submit button's text (Pleroma's "verify" button).
const SUBMIT_TEXT: &str = "Verify";
/// The hidden `mfa[challenge_type]` value of the authenticator form.
const CHALLENGE_TYPE_TOTP: &str = "totp";
/// The hidden `mfa[challenge_type]` value of the recovery form.
const CHALLENGE_TYPE_RECOVERY: &str = "recovery";

/// Everything [`render`] puts on the code-entry page.
#[derive(Debug)]
pub struct MfaPage<'a> {
    /// Value of the hidden `mfa[mfa_token]` input.
    pub mfa_token: &'a str,
    /// Value of the hidden `mfa[redirect_uri]` input.
    pub redirect_uri: &'a str,
    /// Value of the hidden `mfa[state]` input; absent renders an empty
    /// one.
    pub state: Option<&'a str>,
    /// Value of the hidden `mfa[client_id]` input.
    pub client_id: &'a str,
    /// Value of the hidden `mfa[scope]` input.
    pub scope: &'a str,
    /// Whether this is the recovery form; `false` renders the
    /// authenticator-code one.
    pub recovery: bool,
    /// The error line shown above the form (a failed verification
    /// re-render); `None` renders none.
    pub error: Option<&'a str>,
}

/// Renders the code-entry page for `page`.
///
/// One labelled text input under `mfa[code]`, the hidden
/// `mfa[mfa_token]`/`mfa[state]`/`mfa[redirect_uri]`/`mfa[client_id]`/
/// `mfa[scope]`/`mfa[challenge_type]` round-trip fields, a Verify button
/// and the link to the other form — Pleroma's two `o_auth/mfa`
/// templates, in one document.
#[must_use]
pub fn render(page: &MfaPage<'_>) -> String {
    let mut out = String::new();
    push_head(&mut out, title(page.recovery));
    out.push_str("<main>\n<h1>");
    out.push_str(title(page.recovery));
    out.push_str("</h1>\n");
    push_error(&mut out, page.error);
    push_form(&mut out, page);
    push_switch_link(&mut out, page);
    out.push_str("</main>\n");
    push_tail(&mut out);
    out
}

/// The heading of one of the two forms.
const fn title(recovery: bool) -> &'static str {
    if recovery { RECOVERY_TITLE } else { TOTP_TITLE }
}

/// Appends the code form: the input, the hidden round-trip fields and
/// the Verify button.
fn push_form(out: &mut String, page: &MfaPage<'_>) {
    out.push_str("<form method=\"post\" action=\"");
    out.push_str(FORM_ACTION);
    out.push_str("\">\n");
    let label = if page.recovery {
        RECOVERY_LABEL
    } else {
        TOTP_LABEL
    };
    push_field(out, "mfa_code", "mfa[code]", "text", label);
    push_hidden(out, "mfa[mfa_token]", page.mfa_token);
    push_hidden(out, "mfa[state]", page.state.unwrap_or_default());
    push_hidden(out, "mfa[redirect_uri]", page.redirect_uri);
    push_hidden(out, "mfa[client_id]", page.client_id);
    push_hidden(out, "mfa[scope]", page.scope);
    let challenge_type = if page.recovery {
        CHALLENGE_TYPE_RECOVERY
    } else {
        CHALLENGE_TYPE_TOTP
    };
    push_hidden(out, "mfa[challenge_type]", challenge_type);
    out.push_str("<div class=\"actions\">\n<input type=\"submit\" value=\"");
    out.push_str(SUBMIT_TEXT);
    out.push_str("\">\n</div>\n");
    out.push_str("</form>\n");
}

/// Appends the paragraph linking to the other form (Pleroma's
/// recovery/code switch link), carrying every round-trip value over.
fn push_switch_link(out: &mut String, page: &MfaPage<'_>) {
    out.push_str("<p><a href=\"");
    push_escaped(out, &switch_link(page));
    out.push_str("\">");
    let text = if page.recovery {
        TOTP_LINK_TEXT
    } else {
        RECOVERY_LINK_TEXT
    };
    push_escaped(out, text);
    out.push_str("</a></p>\n");
}

/// The urlencoded `GET /oauth/mfa` URL of the other form, with every
/// round-trip value of `page` in its query.
fn switch_link(page: &MfaPage<'_>) -> String {
    let mut pairs: Vec<(&str, &str)> = vec![
        (
            "challenge_type",
            if page.recovery { "totp" } else { "recovery" },
        ),
        ("mfa_token", page.mfa_token),
        ("redirect_uri", page.redirect_uri),
        ("client_id", page.client_id),
        ("scope", page.scope),
    ];
    if let Some(client_state) = page.state {
        pairs.push(("state", client_state));
    }
    let query = form_urlencoded::Serializer::new(String::new())
        .extend_pairs(pairs.iter().copied())
        .finish();
    format!("{MFA_PATH}?{query}")
}

#[cfg(test)]
mod tests {
    use super::*;

    /// An authenticator-code page over the fixture values.
    fn page() -> MfaPage<'static> {
        MfaPage {
            mfa_token: "mfa-T0k3n_value-xyz",
            redirect_uri: "https://client.example/callback",
            state: Some("xyz"),
            client_id: "client-id",
            scope: "read write",
            recovery: false,
            error: None,
        }
    }

    #[test]
    fn the_totp_form_asks_for_the_authentication_code() {
        let html = render(&page());
        assert!(
            html.contains("<h1>Two-factor authentication</h1>"),
            "Pleroma's title"
        );
        assert!(
            html.contains("<form method=\"post\" action=\"/oauth/mfa/verify\">"),
            "the form posts to the verify endpoint"
        );
        assert!(
            html.contains("name=\"mfa[code]\""),
            "the code input is under mfa[code]"
        );
        assert!(html.contains(">Authentication code</label>"), "its label");
        for field in [
            "mfa[mfa_token]",
            "mfa[state]",
            "mfa[redirect_uri]",
            "mfa[client_id]",
            "mfa[scope]",
            "mfa[challenge_type]",
        ] {
            assert!(html.contains(field), "the hidden field `{field}` is there");
        }
        assert!(
            html.contains("name=\"mfa[challenge_type]\" value=\"totp\">"),
            "the form challenges totp"
        );
        assert!(html.contains("value=\"Verify\""), "and a Verify button");
    }

    #[test]
    fn the_recovery_form_swaps_labels_and_challenge_type() {
        let html = render(&MfaPage {
            recovery: true,
            ..page()
        });
        assert!(
            html.contains("<h1>Two-factor recovery</h1>"),
            "Pleroma's recovery title"
        );
        assert!(html.contains(">Recovery code</label>"), "and prompt");
        assert!(
            html.contains("name=\"mfa[challenge_type]\" value=\"recovery\">"),
            "the form challenges recovery"
        );
    }

    #[test]
    fn the_forms_link_to_each_other_with_the_round_trip_values() {
        let totp = render(&page());
        assert!(
            totp.contains("challenge_type=recovery")
                && totp.contains("mfa_token=mfa-T0k3n_value-xyz")
                && totp.contains("state=xyz"),
            "the totp page links to recovery carrying the values: {totp}"
        );
        assert!(
            totp.contains(">Enter a two-factor recovery code</a>"),
            "Pleroma's link text"
        );
        let recovery = render(&MfaPage {
            recovery: true,
            ..page()
        });
        assert!(
            recovery.contains("challenge_type=totp"),
            "and back to the code form"
        );
        assert!(
            recovery.contains(">Enter a two-factor code</a>"),
            "Pleroma's other link text"
        );
    }

    #[test]
    fn every_dynamic_value_is_html_escaped() {
        let evil = MfaPage {
            mfa_token: "\"><script>alert('token')</script>",
            redirect_uri: "https://x.example/cb?a=1&b=2",
            state: Some("<b>state</b>"),
            client_id: "ci&dc",
            scope: "<script>scope</script>",
            recovery: false,
            error: Some("<em>bad</em>"),
        };
        let html = render(&evil);
        assert!(
            !html.contains("<script>alert") && !html.contains("<script>scope"),
            "no value can open a tag"
        );
        assert!(!html.contains("<b>state</b>"), "nor a bold one: {html}");
        assert!(
            html.contains(
                "value=\"&quot;&gt;&lt;script&gt;alert(&#39;token&#39;)&lt;/script&gt;\">"
            ),
            "the token shows escaped inside its attribute"
        );
        assert!(
            html.contains("value=\"&lt;b&gt;state&lt;/b&gt;\">"),
            "so does the state"
        );
        assert!(
            html.contains("value=\"ci&amp;dc\""),
            "ampersands escape too"
        );
        assert!(
            html.contains("role=\"alert\">&lt;em&gt;bad&lt;/em&gt;</p>"),
            "the error line renders escaped"
        );
    }

    #[test]
    fn the_error_line_appears_only_when_set() {
        assert!(
            !render(&page()).contains("role=\"alert\""),
            "a fresh page has no error"
        );
        let failed = render(&MfaPage {
            error: Some("Two-factor authentication failed."),
            ..page()
        });
        assert!(
            failed.contains("Two-factor authentication failed."),
            "the re-render shows the failure"
        );
    }

    #[test]
    fn an_absent_state_renders_an_empty_hidden_field() {
        let html = render(&MfaPage {
            state: None,
            ..page()
        });
        assert!(
            html.contains("name=\"mfa[state]\" value=\"\">"),
            "Pleroma's empty hidden input"
        );
    }

    #[test]
    fn the_page_is_a_self_contained_document() {
        let html = render(&page());
        assert!(html.starts_with("<!DOCTYPE html>"), "a doctype");
        assert!(html.contains("<meta charset=\"utf-8\">"), "its encoding");
        assert!(html.contains("<style>"), "inline CSS");
        assert!(!html.contains("<script"), "and no scripts at all");
        assert!(html.ends_with("</body>\n</html>\n"), "a closed document");
    }
}
