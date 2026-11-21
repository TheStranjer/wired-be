//! Integration tests for the Pleroma account extensions merged at
//! `/api/v1/pleroma`: `POST /api/v1/pleroma/accounts/{id}/subscribe` and
//! `…/unsubscribe` (both halves of the ticket's one column, with the
//! `notifying` / `subscribing` flags of the answered `Relationship` and the
//! `follows.notify` fact behind them, refused with the ticket's 422 wherever
//! the caller has no accepted follow of the target, the shared 404 for every
//! account miss), `GET /api/v1/pleroma/outgoing_follow_requests` (the
//! caller's own pending asks as `Account` entities newest request first, with
//! keyset pagination whose cursors are the `follows` row ids under Pleroma's
//! global page sizes, and no `Link` header on an empty page); `AuthUser` in
//! front of those three, `follow | write:follows` for the two writes and
//! `read:follows` for the listing, and the page parser's 400 for a malformed
//! cursor; and `GET /api/v1/pleroma/accounts/{id}/favourites` (the target's
//! favourited statuses newest favourite first as `Status` entities, optional
//! auth with `read:favourites` demanded of a presented user token, the
//! `hide_favorites` 403 for every viewer but the target, the page narrowed
//! to what the viewer may see, `Link` cursors keyed on the favourite row
//! ids under Pleroma's default 20 and cap 40, and the shared 404).

#![cfg(test)]

mod favourites;
mod favourites_paging_gates;
mod gates;
mod outgoing;
mod subscriptions;
mod support;
