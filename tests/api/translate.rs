//! Status translation over HTTP (`POST /api/v1/statuses/{id}/translate`,
//! `GET /api/v1/instance/translation_languages` and the v2 instance's
//! `configuration.translation`), driven against a mock provider on loopback
//! (`docs/ARCHITECTURE.md` decision 13).
//!
//! What these tests own is the endpoint half of the feature: the visibility
//! gate (a post the caller may not read is the shared 404, and the provider is
//! never woken for it), the target language (`lang`, else the caller's own
//! `language`, else `en`), the exact answer document, the request each provider
//! is sent and the answer it returns — both shapes come from Pleroma's
//! `Libretranslate` and `Deepl` modules — the sanitizer pass over whatever the
//! provider said, the two 503s ("not enabled" with no provider, "service
//! unavailable" when the configured one fails), the language matrix with its
//! one-hour cache (a second request must not cost another provider call), the
//! v2 `translation.enabled` flag, and the auth gate.
//!
//! The provider protocol itself — URL assembly, error mapping, the matrix
//! arithmetic — is unit-tested where it lives, in `src/services/translate.rs`.

#![cfg(test)]

mod support;
