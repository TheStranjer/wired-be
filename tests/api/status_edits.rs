//! Status edits over HTTP (WBE-89): the write `PUT /api/v1/statuses/{id}`,
//! the version listing `GET /api/v1/statuses/{id}/history` and the source
//! read `GET /api/v1/statuses/{id}/source`.
//!
//! The edit is Pleroma's `update`: a user token with `write:statuses`, the
//! shared status gate first (the malformed, unknown, deleted and invisible
//! are the module's one 404), and then Pleroma's ownership stack — only the
//! author's own **local** post, answered with the 403 twin of the
//! `"Record not found"` body, and never a reblog row (422 `"Cannot edit a
//! reblog"`). The body's six parameters (`status`, `spoiler_text`,
//! `sensitive`, `language`, `content_type`, `media_ids`) merge over the
//! stored row — only the presented ones change — and run through the
//! creation's own checks and render, so every refusal is the creation's and
//! the rewritten post linkifies, tags and mentions the way a fresh post
//! would. The write is one transaction that records the superseded version
//! (only when the content, warning, sensitive flag or media actually
//! changed: an edit of nothing records no revision and sets no `edited_at`)
//! and replaces the post's mentions, hashtags and attachments; the answer
//! is the [`Status`] entity with `edited_at` set.
//!
//! The history is Pleroma's `show_history` with optional auth
//! (`read:statuses` on a presented user token): the superseded versions
//! oldest-authored-first with the current one last, each entry a
//! `StatusEdit` — content, warning, flag, authored time, author account,
//! media, and the still-empty `emojis` and `poll` placeholders. A never-
//! edited status is the one-element array. The source is Pleroma's
//! `show_source` under this ticket's author-only gate: the stored raw text,
//! the warning and the `MIME` type, as `{"id", "text", "spoiler_text",
//! "content_type"}`.
//!
//! The `update` telling of the post's local boosters already lands at the
//! module's `after_status_edited` hook (WBE-113, tested in
//! `notification_generation.rs`), and so does the federated `Update` of the
//! re-drafted note (WBE-176, tested in `status_federation.rs`), beside the
//! `status.update` stream push the ticket-190 hook test below pins.
//!
//! [`Status`]: wired_be::entities::status::Status

#![cfg(test)]

mod support;
