//! Integration tests for the account-listing endpoints (WBE-124): the
//! profile directory (`GET /api/v1/directory`, no auth required, with its
//! `offset`/`limit`/`order`/`local` parameters), the staff suggestions
//! (`GET /api/v1/suggestions` as bare accounts and `GET /api/v2/suggestions`
//! source-wrapped, both excluding the viewer and everyone they follow,
//! block, mute or have dismissed through
//! `DELETE /api/v1/suggestions/{account_id}`), the familiar-followers batch
//! (`GET /api/v1/accounts/familiar_followers`) and Pleroma's birthday list
//! (`GET /api/v1/pleroma/birthdays`: today's UTC parts by default, `day` and
//! `month` to name another day, and a leap day that matches nothing else).
//! Auth, scope, not-found, validation and page-size failures are pinned for
//! every route.

#![cfg(test)]

mod directory;
mod familiar_birthdays;
mod suggestions;
mod support;
