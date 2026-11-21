//! Integration tests for the search endpoints (WBE-115, extended by
//! WBE-128): `GET /api/v1/accounts/search` (the flat account array), and
//! `GET /api/v1/search` / `GET /api/v2/search` (the combined document
//! with all three arms live). The routes answer anonymously (Pleroma's
//! `fallback: :proceed_unauthenticated`), demand `read:accounts` from a
//! presented user token, normalise the query (leading `@`, case), page by
//! `limit` (the accounts' default 40 and cap 80, the statuses' and
//! hashtags' default 20 and cap 40) and `offset`, restrict the account
//! search to the viewer's follows on `following=true`, search statuses
//! only for a viewer unless `SEARCH_STATUSES_UNAUTHENTICATED` is on,
//! treat a `#`-leading query as a hashtag-only term, render hashtags as
//! v1 name strings and v2 tag entities, and count into the limiter's
//! `search` bucket.
//!
//! `resolve=true` is the federation arm of the same routes (WBE-192), and it
//! runs only for an authenticated viewer: an actor URL the instance has
//! never seen is fetched from a mock `ActivityPub` peer on loopback and rides
//! first in `accounts`, a status URL is fetched and rendered into `statuses`
//! with its author left out of `accounts`, a URL on this instance's own
//! origin is read from the database alone, and a peer that cannot be reached
//! answers the same empty lists an unresolving search would. `type` keeps the
//! readings to the resource it names, and a resolving request spends the
//! `search` bucket like any other.

#![cfg(test)]

/// The v1 flat account array: its matches, ranking, pages, restrictions,
/// exclusions, auth gates and empty-query answers.
mod accounts;
/// The combined v1/v2 documents: their shape, explicit-`type` routing,
/// malformed cursor and shared `search` bucket.
mod documents;
/// The hashtag arm and the untyped combined search: v1 names and v2 tag
/// entities, the three lists at once, the `#`-only reading, the anonymous
/// statuses gate and the empty-query answers.
mod hashtags_untyped;
/// The `resolve=true` arm: fetched actors and notes, local database reads,
/// the visibility and authentication gates and the bucket it spends.
mod resolve;
/// The loopback mock `ActivityPub` peer and the database readers the
/// resolve tests probe through it.
mod resolve_mock;
/// The `statuses` arm: its full-text matches, entity rendering, visibility,
/// exclusions, `account_id` narrowing and paging.
mod statuses;
/// Shared consts and fixture helpers every submodule of this area draws on.
mod support;
