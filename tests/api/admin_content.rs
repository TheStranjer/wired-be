//! The admin content endpoints over HTTP (`/api/v1/pleroma/admin/rules` and
//! `/api/v1/pleroma/admin/announcements`, WBE-204): the instance-rule
//! quartet and the announcement quintet — the two lists the public API only
//! ever reads.
//!
//! The rule tests pin the four-key entity Pleroma's `AdminAPI.RuleView`
//! renders, the priority-then-creation order the listing answers in, the
//! present-only `PATCH`, the `{}` a delete leaves behind, the
//! `{"errors":{"text":["can't be blank"]}}` (and the 500-character)
//! refusals of a body, and that `GET /api/v1/instance/rules` and the
//! `rules` block of `GET /api/v1/instance` read the very list these routes
//! maintain.
//!
//! The announcement tests pin the entity the client API already serves —
//! the same fourteen keys, `all_day` and `pleroma.raw_content` included,
//! `read` false for a list an administration view computes no viewer for —
//! the `limit` / `offset` window over notices whose activity has not opened
//! and whose activity has closed, `text_to_html` for the served `content`
//! with the author's text kept verbatim beside it, the window refusals, the
//! moderation-log entry and the `announcement` stream event a create
//! publishes, and that a user's own `GET /api/v1/announcements` shows what
//! the admin wrote only while its window says so.
//!
//! Both areas then walk the gate: the `AdminUser` role half on every route,
//! and `admin:read` / `admin:write` — the root or the `rules` /
//! `announcements` sub-scope — as the scope half.

#![cfg(test)]

mod gates;
mod notice_validation_events;
mod notices;
mod rules;
mod support;
