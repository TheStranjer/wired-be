//! List members over HTTP (`/api/v1/lists/{id}/accounts`, WBE-104): the
//! Mastodon v1 membership routes — read a list's members, add accounts to it,
//! remove them again.
//!
//! The tests pin what the ticket promises. `GET` answers the members as
//! `Account` entities, newest membership first, paged over
//! `list_members.position`: default page 20, `limit=0` every member, `max_id`
//! walking to the older memberships and `min_id` back to the newer ones, a
//! `Link` header whose cursors are those position ordinals and no header at
//! all for an empty page. `POST` takes `account_ids[]` as JSON or as a form,
//! refuses an account the caller does not follow with the exact
//! `{"error":"Validation failed: Account is not followed"}` 422 — writing
//! nothing of a refused batch — 404s on an id that names no account, keeps a
//! member who is already there in the place they hold, and answers `{}`.
//! `DELETE` takes the ids in the body or in the query, answers `{}`, and
//! ignores ids that are not members of that list. Every route 404s on a list
//! that is malformed, unknown or another account's — a member of it included —
//! and every one of them runs the `AuthUser` + `read:lists`/`write:lists`
//! pipeline. Unfollowing an account prunes the unfollowing account's own
//! lists, through [`wired_be::models::follow::unfollow`], the write the
//! unfollow endpoint makes.

#![cfg(test)]

mod support;
