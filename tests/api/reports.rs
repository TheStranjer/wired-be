//! Reports over HTTP (`POST /api/v1/reports` and the path-keyed
//! `POST /api/v1/accounts/{id}/flag`, WBE-120): filing a report, and the
//! flag route's forwarding-by-default twin of it.
//!
//! The tests pin what the ticket promises: a filing succeeds with and without
//! cited statuses, answers the ten-key Mastodon `Report` entity whose
//! `target_account` is the full account render, and stores the row; `form`
//! bodies cite statuses with `status_ids[]` keys; repeat filings are two
//! reports; and every validation answers its own 422 — a self-report, a
//! `comment` over `MAX_REPORT_COMMENT`, a `category` outside the four, a
//! cited status that is another account's, invisible, deleted or not an id,
//! and a cited rule that names none. An unknown, malformed or deactivated
//! `account_id` is the shared 404, the route enforces the whole `AuthUser` +
//! `write:reports` pipeline, and it rides the `statuses_actions` bucket.

#![cfg(test)]

mod filing;
mod forwarding;
mod gates;
mod support;
mod validation;
