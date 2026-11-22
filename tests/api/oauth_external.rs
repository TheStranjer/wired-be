//! Integration tests for the external OAuth login endpoints: `GET
//! /oauth/prepare_request`, `GET /oauth/{provider}`, `GET
//! /oauth/{provider}/callback` and the registration form that picks up an
//! unlinked account — `GET /oauth/registration_details` and `POST
//! /oauth/register` (`docs/ARCHITECTURE.md` decision 13).
//!
//! A mock provider runs on a loopback port — its authorize route answers
//! 500, because the flow must only *redirect* a browser there and never
//! fetch it itself — beside its token exchange and userinfo read. The code a
//! test hands the callback is the mock's scenario switch: a plain number is
//! the uid the document answers with, and the reserved words in
//! `token_endpoint` and `userinfo_endpoint` fail one step of the chain.
//! Each test drives the three endpoints in order, as a browser would, and
//! asserts on what the router answers and what the database ends up
//! holding: the prepared state and its ten minutes, the provider redirect's
//! parameters, a code the owning application can redeem exactly once
//! (`consume_authorization` is the lookup the code exchange runs), the
//! `pending` hand-off of an unlinked account, and every refusal on the way.
//!
//! The area lives below `tests/api/oauth_external/`, one submodule per
//! section: `provider` holds the flow's consts and the mock with its three
//! endpoints; `support` holds the `Site` fixture and the helpers every
//! section draws on; `prepare` runs `prepare_request` and the provider
//! redirect; `callback` walks the callback's successes and
//! `callback_refusals` its failures and edges; and `registration` and
//! `registration_modes` cover the hand-off form and the registration modes
//! that replace its code with a message page.

#![cfg(test)]

/// The callback's successes: the login, the echo, the oob page and the
/// unlinked hand-off.
mod callback;
/// The callback of an MFA-enabled linked account: the detour and the
/// verification that delivers the code.
mod callback_mfa;
/// The callback's refusals, the provider-call failures and the route edges.
mod callback_refusals;
/// `GET /oauth/prepare_request` and the provider redirect it feeds.
mod prepare;
/// The flow's consts and the mock provider on its own loopback port.
mod provider;
/// The registration form and the hand-off that creates the user.
mod registration;
/// Closed, approval-required and activation-required registrations.
mod registration_modes;
/// Shared fixtures and helpers every submodule of this area draws on.
mod support;
