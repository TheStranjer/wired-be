//! Integration tests for the `/oauth` endpoints: the `POST /oauth/token`
//! `password`, `client_credentials`, `refresh_token` and
//! `authorization_code` grants, the `POST /oauth/revoke` token revocation,
//! and the `GET`/`POST /oauth/authorize` authorization-code flow.
//!
//! Success over JSON, form and HTTP Basic bodies; the 400 "Invalid
//! credentials" shared by wrong secrets, wrong passwords and unknown
//! users; the 400 "Invalid scope" for a request outside the app's
//! registration; the 403 `account_is_disabled` for a deactivated
//! account; the `unsupported_grant_type` and "Missing parameter:
//! `grant_type`" refusals; and the response shape, headers and stored
//! token row (Pleroma's `OAuthController.token_exchange`). The
//! `client_credentials` grant adds the app-only success over body
//! credentials and HTTP Basic (no `refresh_token`/`me` keys, stored
//! `user_id = NULL`) and the token's `AppAuth` acceptance and `AuthUser`
//! refusal. The `refresh_token` grant adds the rotation: the fresh pair
//! over the old one (same user and scopes, the old access and refresh
//! credentials dead), expired access tokens still refreshable, and the
//! 400 "Invalid credentials" for a foreign app's, an unknown or a
//! deactivated owner's refresh token, with its own "Missing parameter:
//! `refresh_token`" refusal. The revoke half covers the deletion of the
//! app's own token (the bearer stops answering 200 on an authenticated
//! endpoint), the 400 "Invalid credentials" for a foreign app's token
//! and for failed client authentication, the RFC 7009 `{}` answers for
//! unknown tokens and second revocations, the "Missing parameter:
//! `token`" refusal and the HTTP Basic and form request shapes
//! (Pleroma's `OAuthController.token_revoke`). The authorize
//! half covers the browser `GET`'s 302 to the frontend's consent screen
//! with its query intact, the JSON 400 "Invalid client", "Invalid
//! `redirect_uri`" and "Invalid scope" refusals that never redirect, the
//! refusal of the retired form `POST`, and the JSON half the frontend
//! drives (`OAuthController.authorize` and `create_authorization`).
//!
//! The area lives below `tests/api/oauth/`, one submodule per section:
//! `support` holds the consts, the fixture pair and the request helpers
//! every section draws on; `password_grant` and `password_grant_scopes`
//! run the `password` grant's successes, credential refusals, scope
//! rules and the grant-type refusals; `client_credentials` the app-only
//! grant and what its token may do; `revoke` the revocation endpoint;
//! `refresh` the rotating `refresh_token` grant; and `authorize_get`,
//! `authorize_post` and `json_authorize` the browser redirect, the
//! refused form post and the frontend's JSON verdict.

#![cfg(test)]

/// The browser `GET /oauth/authorize`: the redirect to the frontend and
/// the refusals, plus the app, query and code helpers the other authorize
/// tests share.
mod authorize_get;
/// The retired form `POST /oauth/authorize`: refused with a 400 that
/// mints nothing.
mod authorize_post;
/// The `client_credentials` grant and the app-only token it mints.
mod client_credentials;
/// The `authorization_code` grant's `redirect_uri` binding.
mod code_grant;
/// The JSON `GET`/`POST /oauth/authorize` a trusted frontend drives.
mod json_authorize;
/// The `password` grant over JSON, form and Basic: successes and the
/// shared credential refusals.
mod password_grant;
/// The `password` grant's scope rules, the deactivated account and the
/// grant-type refusals.
mod password_grant_scopes;
/// The `refresh_token` grant: the rotation, the dead old pair and the
/// refusals.
mod refresh;
/// The `POST /oauth/revoke` endpoint: deletion, refusals and the RFC
/// 7009 `{}` answers.
mod revoke;
/// Shared consts and helpers every submodule of this area draws on.
mod support;
