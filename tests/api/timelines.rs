//! Status timelines over HTTP (`GET /api/v1/timelines/home`, WBE-100,
//! `GET /api/v1/timelines/tag/{tag}`, WBE-102, `GET
//! /api/v1/timelines/direct`, WBE-103, `GET
//! /api/v1/timelines/list/{list_id}`, WBE-105, and `GET
//! /api/v1/timelines/public`, WBE-101).
//!
//! The home timeline answers the viewer's own posts and reblogs plus those
//! of their accepted follows — a follow's reblogs only while its
//! `show_reblogs` toggle stands, the followee's `private` rows within
//! reach of the visibility clause, a stranger's nowhere — under `AuthUser`
//! + `read:statuses`. `local`, `remote`, `only_media`, `with_muted` and
//! `exclude_visibilities[]` narrow the page like on the tag route,
//! `reply_visibility` (`following` or `self`) narrows the replies and any
//! other spelling is the 422, `exclude_replies` is ignored, and the `Link`
//! header repeats the request's filters on both cursors.
//!
//! The tag timeline answers the `public` statuses carrying the path tag —
//! URL-decoded, lowercased, `#`-stripped — newest first under a `Link`
//! header; `any[]` widens the match (with the path tag riding in the union),
//! `all[]` intersects, `none[]` subtracts, and `local`/`remote`/`only_media`/
//! `with_muted`/`exclude_visibilities[]` narrow the page. Auth is optional:
//! anonymous and app-only read the page, a user token must grant
//! `read:statuses`, a broken token is the 401. A tag that normalizes to
//! nothing is the 404, an unknown one the empty page, a malformed cursor
//! the 400.
//!
//! The direct timeline answers the viewer's own `direct` conversations —
//! sent and received, newest first, nothing of anyone else's — with auth
//! required: no token is the 401, an app-only or scope-less token the 403.
//! `with_muted` lifts the viewer's own block/mute exclusions and the `Link`
//! header works as on the tag route.
//!
//! The list timeline answers the statuses of one of the caller's lists —
//! members' posts without any follow read, replies narrowed by the list's
//! `replies_policy` (the SQL itself is pinned in
//! `src/services/timeline.rs`) — under `AuthUser` + `read:lists`: no token
//! is the 401, an app-only or scope-less token the 403, and a malformed,
//! unknown or foreign list id one and the same 404 "Record not found".
//! `only_media` and `with_muted` narrow the page and the `Link` header
//! works as on the other routes.
//!
//! The public timeline answers the `public` statuses — and `local` ones for
//! an authenticated local viewer — under the same optional-auth rule,
//! narrowed by `local`/`remote`/`only_media`/`with_muted`/
//! `exclude_visibilities[]` and by Pleroma's `reply_visibility` and
//! `instance` extensions. `RESTRICT_UNAUTHENTICATED` can close its two
//! anonymous reads: `timelines_local` gates `local=true` and
//! `timelines_federated` every other one, each refused with the 403
//! `{"error":"Authentication is required to access this resource."}`;
//! an authenticated request and the default configuration read normally.
//! The tag timeline shares that gate, keyed the same way on `local`.

#![cfg(test)]

/// The direct timeline: the viewer's own conversations, cursor and gate.
mod direct;
/// The home timeline: own posts and follows, toggles, cursor and narrows.
mod home;
/// The home timeline's `reply_visibility`, `exclude_visibilities[]` and gate.
mod home_filters;
/// The list timeline: membership reads, replies policy, cursor and gate.
mod list;
/// The public timeline: visibility, Pleroma narrows, cursor and auth.
mod public;
/// The `RESTRICT_UNAUTHENTICATED` gate on the public timeline's reads.
mod public_restrictions;
/// Shared consts and fixture helpers every submodule of this area draws on.
mod support;
/// The tag timeline: the path tag's page, set operations and narrows.
mod tag;
/// The `RESTRICT_UNAUTHENTICATED` gate on the tag timeline's reads.
mod tag_restrictions;
