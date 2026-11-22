//! Integration tests for the OAuth MFA challenge flow (WBE-132): the
//! `mfa_required` 403 of the password grant, the
//! `POST /oauth/mfa/challenge` exchange (TOTP codes, recovery codes,
//! wrong/expired/foreign refusals), and the `POST /oauth/authorize`
//! detour through the `/oauth/mfa` page and its
//! `POST /oauth/mfa/verify` completion (Pleroma's
//! `lib/pleroma/web/o_auth/mfa_controller.ex` and the `o_auth/mfa/`
//! templates).
//!
//! The fixture of every test is an account that can log in with
//! [`PASSWORD`] and has MFA confirmed (secret + issued backup codes),
//! plus a registered application. The password grant refuses it the 403
//! challenge; the challenge half then runs the second factor and
//! answers the same token JSON the grant withholds. The authorize half
//! redirects the browser to the code page and delivers the parked
//! authorization code once the verification passes.

#![cfg(test)]

mod support;
