//! The mailer unsubscribe page: `GET /mailer/unsubscribe/{token}`
//! (Pleroma's `SubscriptionController`,
//! `lib/pleroma/web/mailer/subscription_controller.ex`) takes the
//! HMAC-signed link [`unsubscribe_token`] mints, clears the user's
//! `email_notifications` flag and answers the confirmation page; every
//! refusal is the one 400 page. The shared page helpers and the
//! [`HmacSha256`] flavor ride on [`super`].

use axum::{
    extract::{Path, State},
    http::StatusCode,
    response::Response,
};
use base64::{Engine as _, engine::general_purpose::URL_SAFE_NO_PAD};
use hmac::{KeyInit as _, Mac as _};
use subtle::ConstantTimeEq as _;
use uuid::Uuid;

use crate::{error::AppResult, models::user, state::AppState};

use super::{
    HmacSha256, INVALID_TOKEN_MESSAGE, UNSUBSCRIBED_MESSAGE, message_body, protected_page,
};

/// `GET /mailer/unsubscribe/{token}` (Pleroma's `SubscriptionController`,
/// `lib/pleroma/web/mailer/subscription_controller.ex`): the link
/// [`unsubscribe_token`] mints. A verified token clears the user's
/// `email_notifications` flag through
/// [`set_email_notifications`](crate::models::user::set_email_notifications)
/// and answers 200 [`UNSUBSCRIBED_MESSAGE`] as a [`protected_page`] —
/// `X-Frame-Options: DENY`, unlike the embed pages, because a mailer
/// link is no frame's content. Every refusal is the one 400
/// [`INVALID_TOKEN_MESSAGE`] page: a token that does not verify, and,
/// since no token can verify without a signing secret, a state built off
/// the boot path with
/// [`secret_key_base`](crate::config::Config::secret_key_base) unset.
///
/// # Errors
///
/// [`AppError::Database`](crate::error::AppError::Database) when the
/// update fails.
pub(super) async fn unsubscribe(
    State(state): State<AppState>,
    Path(token): Path<String>,
) -> AppResult<Response> {
    let verified = state
        .config
        .secret_key_base
        .as_deref()
        .and_then(|secret| unsubscribe_user_id(secret, &token));
    let Some(user_id) = verified else {
        return Ok(invalid_token_page());
    };
    user::set_email_notifications(&state.db, user_id, false).await?;
    Ok(protected_page(
        StatusCode::OK,
        UNSUBSCRIBED_MESSAGE,
        &message_body(UNSUBSCRIBED_MESSAGE),
    ))
}

/// The raw HMAC-SHA256 of `message` keyed by `secret`, the recipe of
/// [`crate::services::media_proxy`]; `None` only for key material its
/// API refuses, which HMAC cannot (it hashes keys of any length).
fn hmac_signature(secret: &str, message: &str) -> Option<Vec<u8>> {
    let mut mac = HmacSha256::new_from_slice(secret.as_bytes()).ok()?;
    mac.update(message.as_bytes());
    Some(mac.finalize().into_bytes().to_vec())
}

/// Mints the unsubscribe token of user `user_id` under `secret`:
/// `base64url_nopad(HMAC-SHA256(secret, user-id-text))` + `.` + the
/// hyphenated lowercase uuid.
///
/// Both halves are path-safe verbatim — the base64url alphabet carries
/// no padding, no `/` and no `+`, and uuid text needs no escaping in a
/// path segment — so the digest mailer can drop the token straight into
/// a link. The empty string is the only other answer, for the HMAC
/// failure the signing step cannot reach.
#[must_use]
pub fn unsubscribe_token(secret: &str, user_id: Uuid) -> String {
    let id_text = user_id.to_string();
    let Some(signature) = hmac_signature(secret, &id_text) else {
        return String::new();
    };
    format!("{}.{id_text}", URL_SAFE_NO_PAD.encode(signature))
}

/// The user id `token` names when its signature verifies, and `None`
/// for every token [`unsubscribe_token`] would not mint.
///
/// Every refusal reads the same: no `.` separator (the split takes the
/// first dot, so an id segment carrying more never parses), a signature
/// that is no base64url text, one that disagrees with the HMAC-SHA256
/// of the id segment under `secret` (compared in constant time, so the
/// route leaks nothing about how far a guess was), and an id segment
/// that is no [`Uuid`].
#[must_use]
pub fn unsubscribe_user_id(secret: &str, token: &str) -> Option<Uuid> {
    let (provided, id_text) = token.split_once('.')?;
    let want = hmac_signature(secret, id_text)?;
    let given = URL_SAFE_NO_PAD.decode(provided).ok()?;
    if !bool::from(given.as_slice().ct_eq(want.as_slice())) {
        return None;
    }
    Uuid::parse_str(id_text).ok()
}

/// The one 400 answer of the unsubscribe route: [`INVALID_TOKEN_MESSAGE`]
/// as a [`protected_page`], the module headers and all.
fn invalid_token_page() -> Response {
    protected_page(
        StatusCode::BAD_REQUEST,
        INVALID_TOKEN_MESSAGE,
        &message_body(INVALID_TOKEN_MESSAGE),
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::api::html_pages::{
        CSP_HEADER, FRAME_OPTIONS_HEADER, PAGE_CONTENT_TYPE, PAGE_CSP, test_support::headers_of,
    };
    use axum::http::header;

    /// A token with its first base64url character swapped for another
    /// (both fully significant, so the decoded signature must change).
    fn flipped_signature(token: &str) -> String {
        let mut chars = token.chars();
        let first = chars.next().expect("a token is not empty");
        let swap = if first == 'A' { 'B' } else { 'A' };
        let mut flipped = String::new();
        flipped.push(swap);
        flipped.extend(chars);
        flipped
    }

    #[test]
    fn unsubscribe_tokens_roundtrip_to_their_user_id() {
        let secret = "roundtrip-secret";
        let user_id = Uuid::now_v7();
        let token = unsubscribe_token(secret, user_id);
        assert_eq!(
            unsubscribe_user_id(secret, &token),
            Some(user_id),
            "a fresh token names its user"
        );
        assert!(
            token.ends_with(&format!(".{}", user_id.as_hyphenated())),
            "the id segment is the hyphenated lowercase uuid verbatim: {token}"
        );
        assert_eq!(
            token,
            unsubscribe_token(secret, user_id),
            "minting is deterministic"
        );
    }

    #[test]
    fn unsubscribe_tokens_are_path_safe_verbatim() {
        let token = unsubscribe_token("a-secret", Uuid::now_v7());
        assert!(
            !token.contains(['=', '/', '+']),
            "no padding, no slash, no plus: {token}"
        );
        assert_eq!(
            token.split('.').count(),
            2,
            "the one signature dot splits the token in two: {token}"
        );
        assert!(
            token
                .chars()
                .all(|c| c.is_ascii_alphanumeric() || matches!(c, '-' | '_' | '.')),
            "every character is path-safe: {token}"
        );
    }

    #[test]
    fn tampered_unsubscribe_tokens_verify_as_no_one() {
        let secret = "tamper-secret";
        let alice = Uuid::now_v7();
        let bob = Uuid::now_v7();
        let token = unsubscribe_token(secret, alice);
        assert_eq!(
            unsubscribe_user_id(secret, &flipped_signature(&token)),
            None,
            "a flipped signature byte does not verify"
        );
        let (_sig, id_text) = token.split_once('.').expect("a minted token has a dot");
        assert_eq!(id_text, alice.to_string(), "the minted id segment");
        assert_eq!(
            unsubscribe_user_id(
                secret,
                &token.replace(&format!(".{alice}"), &format!(".{bob}"))
            ),
            None,
            "the id segment cannot be swapped for another user's"
        );
        assert_eq!(
            unsubscribe_user_id("another-secret", &token),
            None,
            "a different secret does not verify"
        );
    }

    #[test]
    fn malformed_unsubscribe_tokens_verify_as_no_one() {
        let secret = "malformed-secret";
        let token = unsubscribe_token(secret, Uuid::now_v7());
        let (signature, _) = token.split_once('.').expect("a minted token has a dot");
        assert_eq!(
            unsubscribe_user_id(secret, &token.replace('.', "")),
            None,
            "a token with no dot is refused"
        );
        assert_eq!(
            unsubscribe_user_id(secret, signature),
            None,
            "a bare signature with no id is refused"
        );
        assert_eq!(
            unsubscribe_user_id(secret, ""),
            None,
            "the empty token is refused"
        );
        assert_eq!(
            unsubscribe_user_id(secret, &format!("not.base64!!.{token}")),
            None,
            "an undecodable signature segment is refused"
        );
        assert_eq!(
            unsubscribe_user_id(secret, &format!("{token}=.not-base64!!")),
            None,
            "a second dot leaves an undecodable id segment"
        );
        assert_eq!(
            unsubscribe_user_id(secret, &format!("{signature}={token}")),
            None,
            "a padded signature segment is no base64url: refused"
        );
    }

    #[test]
    fn the_invalid_token_page_is_a_protected_400() {
        let response = invalid_token_page();
        assert_eq!(response.status(), StatusCode::BAD_REQUEST, "400");
        let headers = headers_of(&response);
        assert_eq!(
            headers[header::CONTENT_TYPE],
            PAGE_CONTENT_TYPE,
            "the refusal is an HTML page"
        );
        assert_eq!(headers[CSP_HEADER.clone()], PAGE_CSP, "with the CSP");
        assert_eq!(
            headers[FRAME_OPTIONS_HEADER.clone()],
            "DENY",
            "and not framable, like every non-embed page"
        );
    }
}
