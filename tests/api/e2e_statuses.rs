//! End-to-end posting matrix (WBE-223): the fixture world, the show-route
//! sweep of the single-status kinds, the thread-context reads with the
//! account statuses filter sweep, and the five-timeline membership sweep.
//!
//! One author posts every single-status kind over HTTP — `public`,
//! `unlisted`, `private`, a `direct` addressed by an @-mention of
//! `mentioned`, a media post (a multipart `POST /api/v1/media` whose id
//! the create then claims through `media_ids`), a content warning
//! (`spoiler_text` + `sensitive`), a poll (the form's nested
//! `poll[options][]` / `poll[expires_in]` keys), a hashtag post
//! (`#E2ETag`), a `local` post @-mentioning the follower and a `public`
//! quote of the `private` one through the create's `quote_id` arm — and
//! every viewer role reads each one back through
//! `GET /api/v1/statuses/{id}`.
//! The answers are pinned against the visibility model in
//! `wired_be::models::status_visibility`: the viewer a rule admits reads
//! the exact entity — its `id`, its stored `visibility`, its rendered
//! `content`, the author's `acct`, and the kind's own shape (the warning,
//! the poll, the attachment, the tag, the mention) — and every viewer the
//! rules shut out reads the one identical shared
//! `{"error":"Record not found"}` 404: `private` is invisible to the
//! stranger, the still-pending follower, the blocked account and the
//! anonymous caller, `direct` to every non-participant, `local` to the
//! anonymous caller, and everything the author posted to the account the
//! author blocked (their own aside), so a probe never learns that a
//! hidden status exists.
//!
//! Reply chains and the conversation direct are the matrix's second slice,
//! posted through the same create route and read back per viewer through
//! `GET /api/v1/statuses/{id}/context` as `wired_be::api::statuses`'s
//! handler documents it: ancestors root-first (oldest first), descendants
//! flat in chronological thread order, an invisible link skipped but walked
//! past rather than truncating the walk, and every descendant's
//! `pleroma.parent_visible` answering whether its own direct parent came
//! back in this very answer. The same slice then sweeps
//! `GET /api/v1/accounts/{author}/statuses` with every real `StatusesParams`
//! filter — `pinned`, `only_media`, `exclude_replies`, `exclude_reblogs`,
//! `with_muted`, `tagged`, `exclude_visibilities[]` — across the six sweep
//! viewers, asserting each page's membership by id set. The quote kind
//! rides every surface too: the create's gate refuses an original the
//! poster cannot reach with the one shared 404 and writes nothing,
//! `pleroma.quotes_count` counts exactly the one accepted quote, both
//! `context` arms stand empty around a post that quotes, and the quote
//! row rides the account and timeline sweeps as the `public` root it is.
//! A tombstone and a reblog — the model's rules 1
//! and 7 — are `e2e_interactions.rs`'s delete and boost scenarios (the
//! sweep's one boost is the author's own, posted here to be filtered). The
//! per-endpoint parameter refusals stay pinned by the area files
//! (`statuses.rs`, `account_statuses.rs`, `media_upload.rs`, `polls.rs`, …);
//! this module owns the kind × viewer matrix on the show route, the thread
//! reads, the filter sweep on the account's page, and the membership sweep of
//! every posted kind across all five timelines of `src/api/timelines.rs` —
//! home, public, tag, direct and list — per viewer, by id set: the author's
//! and the follower's home audiences, the muter's silence and its
//! `with_muted` restore, the blocked viewer's emptiness, the public page
//! narrowed to its public rows (plus `local` for a token this instance
//! owns) with its `only_media` cut and its documented
//! `RESTRICT_UNAUTHENTICATED` gate, the one `#E2ETag` page every named
//! viewer shares, the direct page held to its participants under
//! `read:statuses`, and the author's own list — the author added as its own
//! member — holding the whole posting under the stored `replies_policy`
//! while another account's list id answers the same shared 404 as the show
//! route.

#![cfg(test)]

/// The account status pages.
mod account_pages;
/// The quote tests.
mod quotes;
/// Shared consts and fixture helpers every submodule of this area draws on.
mod support;
/// The fixtures the thread and account-page children share.
mod thread_support;
/// The thread tests.
mod threads;
/// The fixtures the timeline tests share.
mod timeline_support;
/// The timeline tests.
mod timelines;
/// The posting matrix.
mod visibility;
