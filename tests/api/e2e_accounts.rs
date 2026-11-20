//! End-to-end account, auth and social-graph scenarios (WBE-222): seven
//! lifecycles walked through the public HTTP API alone. Every step is a
//! request through the test router and every check is what a client can
//! see. The one fixture a scenario plants with a model call is the first
//! admin account — no public endpoint mints an admin — and even its
//! credential is then won over HTTP by the password grant.
//!
//! The scenarios:
//!
//! 1. **Onboarding**: `POST /api/v1/apps` → the `client_credentials`
//!    token → `POST /api/v1/accounts` → `verify_credentials` →
//!    `update_credentials` (display name, bio, one metadata field and an
//!    uploaded avatar) → the per-app settings store the account carries →
//!    the public account read showing it all → the password change that
//!    revokes the account's other credential → a login with the new one.
//! 2. **The OAuth code flow**: the consent page, the approved
//!    submission's code, that code's exchange, the rotation, the
//!    revocation, and the approved scope deciding what the token may do.
//! 3. **MFA**: TOTP setup, confirmation and backup codes, the
//!    `mfa_required` challenge the password grant answers with, the
//!    challenge that hands out the withheld pair, and the disable that
//!    opens the plain grant again.
//! 4. **The social graph**: an open follow, a locked account's pending
//!    request and its authorization, the block and its unblock, a timed
//!    mute, a domain block, an endorsement, a private note, a list with
//!    members, and the following page walked by its `Link` cursors.
//! 5. **The registration modes**: approval required until staff approve
//!    through the admin API, an invite token opening a closed instance,
//!    the confirmation letter the drained mailer leaves in the memory
//!    mailbox, and a password reset through its emailed token.
//! 6. **Admin user management**: create, tag, promote, rewrite the
//!    credentials of, deactivate, re-activate and delete an account, each
//!    effect read back through the user-facing API.
//! 7. **CSV imports**: the follow, block and mute lists the import
//!    endpoints queue, run through [`common::run_all_jobs`], landing the
//!    relationships they promise.
//!
//! The per-endpoint refusals behind these journeys are pinned by the area
//! files (`account_registration.rs`, `oauth.rs`, `mfa.rs`,
//! `registration_modes.rs`, `admin_users.rs`, `pleroma_import.rs`, …);
//! like `e2e_scheduled.rs`, this module owns the *seams* — the state one
//! endpoint leaves behind for the next, and what the API answers about it.
//! The journeys are one submodule each; [`support`] holds the consts, the
//! [`World`](support::World) and the readers every one of them rides.
//!
//! [`common::run_all_jobs`]: crate::common::run_all_jobs

#![cfg(test)]

/// Scenario 6: the admin walks a user from create to delete.
mod admin_lifecycle;
/// Scenario 2: the authorization-code flow's consent, exchange, rotation
/// and revocation.
mod authorization_code;
/// Scenario 7: the CSV imports the queue turns into relationships.
mod imports;
/// Scenario 3: the MFA gate on the password grant.
mod mfa;
/// Scenario 1: the onboarding walk to a new password.
mod onboarding;
/// Scenario 5: the registration modes and the waits they end.
mod registration_modes;
/// Scenario 4: follows, blocks, mutes, a domain block and paged reads.
mod social_graph;
/// Shared consts, the [`World`](support::World) every journey opens, its
/// readers, and the loopback mock peer.
mod support;
