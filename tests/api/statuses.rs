//! The status endpoints over HTTP: the write `POST /api/v1/statuses` (WBE-76),
//! the reads `GET /api/v1/statuses/{id}` and the batch
//! `GET /api/v1/statuses?id[]=…` (WBE-77), the thread read
//! `GET /api/v1/statuses/{id}/context` (WBE-78), the write
//! `DELETE /api/v1/statuses/{id}` (WBE-79) and the quoting listing
//! `GET /api/v1/statuses/{id}/quotes` with its Pleroma twin
//! `GET /api/v1/pleroma/statuses/{id}/quotes` (WBE-91).
//!
//! The show route answers the `Status` entity to everyone the visibility
//! model admits: a public post to its author, a stranger and the anonymous
//! caller alike; a `private` post to the author and an accepted follower
//! only (a pending request is not one); a `direct` post to the author and
//! the accounts it @-mentions only. A deleted row, a malformed id, an id
//! that names nothing and a post the viewer may not see are one
//! indistinguishable 404 `{"error":"Record not found"}`, so a probe never
//! learns that somebody's private post exists. A reblog's own id answers
//! the reblog entity — the reblogger's row, empty content, the original
//! nested in `reblog` — and a reblog of a post the viewer may not see is
//! invisible itself. A quoting status renders the quoted original it may
//! see nested in `quote` — mirrored in `pleroma.quote` with
//! `pleroma.quote_visible` true — one level deep: a quoted status's own
//! quote never renders. What the viewer may not see renders `quote` and
//! `pleroma.quote` null with `quote_visible` false, while `quote_id`
//! still names the quoted row.
//!
//! The batch route answers the entities of the ids this viewer may see in
//! the order they were requested: it reads `id[]` and Pleroma's deprecated
//! `ids[]` (both also unwrapped, `id` winning), acts on at most a hundred
//! ids and ignores the ones past that, skips the malformed, unknown,
//! deleted and invisible ids without saying which was which, and answers
//! `[]` to a request that carries no usable id.
//!
//! `GET /api/v1/statuses/{id}/context` (WBE-78) is Pleroma's `context`: the
//! thread around the named status as `{"ancestors": [...], "descendants":
//! [...]}`. Ancestors are the reply chain above it, root first; descendants
//! are every reply below it, across every branch, in flat chronological
//! order. Both arms carry only what the visibility model admits — an
//! invisible or deleted link is skipped but walked *past*, so the thread
//! beyond it still arrives — and descendants additionally drop statuses by
//! accounts the viewer blocked or muted (the viewer's own excepted). Each
//! descendant's `pleroma.parent_visible` says whether its direct parent
//! came back in the answer. The named status itself never appears in either
//! arm, and a status this caller may not see is the shared 404.
//!
//! Both reads run Pleroma's optional-auth pipeline: no token proceeds, a
//! presented user token must carry `read:statuses` (403 for one that does
//! not, even on a public post), an app-only token proceeds unauthenticated,
//! and a token that does not resolve is the extractor's 401.
//!
//! `POST /api/v1/statuses` (WBE-76) is Pleroma's `create` and runs the strict
//! pipeline — `AuthUser` plus `write:statuses` plus the
//! `statuses_actions` throttle — and the ticket's own refusals: 422 for a
//! post with nothing to say, one over `MAX_STATUS_CHARS` (counted with every
//! URL at 23 characters), an unknown `visibility` or `content_type`, an
//! over-long `spoiler_text` or `language`. What passes is stored as a `local`
//! row with its mentions in `status_mentions` and its tags in
//! `status_hashtags`, the posting client recorded as its application, the
//! author's `statuses_count` stepped, and the whole thing answered as the
//! `Status` entity the author themselves sees — from a JSON or a form body
//! alike. WBE-87 adds the route's two request-level arms: an
//! `Idempotency-Key` header whose retry answers the status the key created
//! without writing a second one (until the key's 24 hours run out and the
//! `idempotency.cleanup` sweep frees the pair), and a `preview` parameter
//! that runs every check and the full render and answers the would-be
//! entity with no write, no hook and no key at all. WBE-90 adds Pleroma's
//! `expires_in`: a top-level integer of seconds, at least
//! `MIN_EXPIRATION_SECS` (600 by default — a shorter one is 422
//! `{"error":"expires_in must be at least 600 seconds"}`, no integer is 422
//! of the extractor's own), stored as the row's `expires_at`, rendered as
//! `pleroma.expires_at`, and scheduled for deletion as one
//! `statuses.expire` job due at that instant — with the periodic
//! `statuses.expire_sweep` pass behind it (see
//! `wired_be::jobs::statuses`).
//!
//! WBE-85 adds `in_reply_to_id`: the reply's parent is read with the show
//! route's visibility rules — a malformed, unknown, deleted or
//! poster-invisible parent is the shared 404, and a reblog row folds to the
//! original it boosts — and the post lands in the parent's conversation
//! with the parent's author auto-mentioned in `status_mentions` (the text
//! itself is never modified), an audience capped by the parent's (`direct`
//! forces `direct`, `private` pulls a requested `public`/`unlisted` down,
//! and an unsubmitted scope never goes wider than the parent), and the
//! parent's language as the fallback tag; the parent's `replies_count`
//! steps with the insert.
//!
//! WBE-86 adds `media_ids`: the post's attachments, as a JSON array or
//! `media_ids[]` form keys, are the poster's own uploads no status has
//! claimed. More than `MAX_MEDIA_ATTACHMENTS` (4 by default) is 422
//! `{"error":"Too many attachments"}`, and an id that is malformed,
//! unknown, somebody else's or already posted is 422
//! `{"error":"Media not found"}` — refused inside the insert's own
//! transaction, so a short claim writes no status and frees nothing it
//! took. Accepted ids are claimed at the order the body sent them, that
//! order is what the entity's `media_attachments` answers, a body
//! carrying at least one attachment may leave `status` empty, and
//! attachments ride every visibility, `direct` included.
//!
//! A body may also name a status to quote under `quote_id` (Pleroma's
//! `quoted_status_id` alias accepted): the target must be live and visible to
//! the poster — a malformed, unknown, deleted or unseen one is the shared
//! 404 — and public or unlisted unless it is the poster's own, and the stored
//! row carries the quotable original's id (a reblog target resolves through
//! to what it boosts) with the original's `quotes_count` stepped by the
//! insert.
//!
//! `DELETE /api/v1/statuses/{id}` (WBE-79) is Pleroma's `delete`: a user
//! token with `write:statuses`, on the same `statuses_actions` bucket the
//! favourite writes ride, behind the same status gate — a malformed, unknown,
//! deleted or invisible id is the shared 404. Past the gate only the author
//! may delete their own post, while an admin or a moderator may delete
//! anybody's; a visible post of somebody else's is a 403 that keeps Pleroma's
//! `"Record not found"` wording. The answer is the `Status` entity exactly as
//! the caller had seen it, before the row became a tombstone, plus the raw
//! source text under the top-level `text` key (Mastodon's delete-and-redraw).
//! Deleting a post takes every live reblog of it down in the same
//! transaction, steps the counters each of them fed, and leaves the media
//! attachment rows in the table for a later cleanup job. WBE-123 adds the
//! conversation arm of the delete: a deleted DM that was somebody's
//! `last_status_id` re-points their participation to the newest direct
//! status left in the thread, or withdraws it when none remain.
//!
//! WBE-123 adds the DM-thread tracking behind direct posts: a `direct`
//! status opens (or continues) a conversation the create's post-write hook
//! records — the author participates read, every mentioned local account
//! participates unread, and the thread's other addressees, remote included,
//! ride the participation's recipient list. The status entity answers
//! `pleroma.direct_conversation_id` as the viewing participant's own
//! participation id (`null` for a non-participant, a non-DM post, and in
//! the create response itself, which renders before the hook lands), and
//! `POST /api/v1/statuses` accepts `in_reply_to_conversation_id`: the
//! poster's own participation id, which forces the post to `direct`, adds
//! that participation's recipients as its mentions and lands it in the same
//! thread — a malformed, unknown or foreign id is the shared 404.
//!
//! Both mounts of `GET …/quotes` (WBE-91) answer the live quoters of the
//! named status, newest quoter first, paged by `limit`/`max_id`/`min_id`/
//! `since_id` with a `Link` header keyed on the first and last rendered
//! quoter row ids. The listing runs the show route's optional-auth gate —
//! `read:statuses` on a presented user token — and drops quoters the viewer
//! may not see; the malformed, unknown, deleted and invisible target is the
//! shared 404, and an unquoted status is the empty array with no `Link`.

#![cfg(test)]

/// The batch read: every id spelling, the cap, the gate and reblogs.
mod batch;
/// The context read: ancestors, descendants and the walk past the unseen.
mod context;
/// WBE-123: DM threads, participations and `in_reply_to_conversation_id`.
mod conversations;
/// The create: what it stores, counts, links and answers.
mod create;
/// The create's refusals: the length limit, the 422 matrix, the form body.
mod create_validation;
/// The delete: its answer, counters, staff, refusals and shared bucket.
mod delete;
/// `expires_in`: the stored instant, the scheduled delete and the floor.
mod expiry;
/// The `Idempotency-Key` header: retries, owners, lengths and release.
mod idempotency;
/// `media_ids`: claimed uploads, order, refusals and the media-only post.
mod media;
/// The `preview` parameter: the full render that writes nothing.
mod preview;
/// Quoting on the way in: `quote_id` accepted, refused and rendered.
mod quotes_create;
/// `GET …/quotes` on both mounts: pages, cursors, links and the gate.
mod quotes_list;
/// `in_reply_to_id`: the parent's thread, counters, audience and language.
mod replies;
/// `RESTRICT_UNAUTHENTICATED` `activities_*`: the anonymous reads it closes.
mod restricted;
/// The show route: visibility, tombstones, bad ids and nested reblogs.
mod show;
/// Shared consts and fixture helpers every submodule of this area draws on.
mod support;
/// The write pipeline: its auth demands and the throttle it spends.
mod write_gate;
