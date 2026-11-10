//! Services: subsystems that are neither one table's DB access
//! ([`crate::models`]) nor HTTP handling ([`crate::api`])
//! (`docs/ARCHITECTURE.md` decision 3).
//!
//! A service module owns its data's on-disk shape and the operations that
//! reconcile disk and database; handlers call it, never the other way
//! round. The cross-table read side of the same rule lives here too:
//! [`render_status`] is the batched loader-and-renderer that composes
//! several models into one entity page, which is more than any single
//! table module owns and less than an endpoint. [`media_proxy`] is the
//! cross-cutting resident of the same home: it rewrites remote media URLs
//! into signed proxy links (used by the [`entities`](crate::entities)
//! builders) and serves those links over HTTP (its router is merged at
//! the top level by [`crate::app`]).
//!
//! [`create_status`] is the write side of the same idea: it decides what a
//! `POST /api/v1/statuses` body means — its limits, its defaults, its
//! refusals — from the request parameters alone, so the endpoint that stores
//! the result never re-decides any of it. The route's two request-level
//! arms live here for the same reason: the `Idempotency-Key` replay claims
//! its row inside the status's own transaction, and the `preview` dry run
//! answers the would-be entity without writing anything.
//!
//! [`delete_status`] is the write side's twin: the one deletion — tombstone,
//! counters and post-write hook — shared by `DELETE /api/v1/statuses/{id}`
//! and the expiration jobs, so an expiring post leaves exactly what a
//! hand-deleted one leaves.
//!
//! [`notify`] is the write side of the notification inbox: the five tellings
//! the action hooks land — follow, follow request, mention, reblog,
//! favourite — decided by one set of rules (local recipients, no
//! self-notifications, every insert through the skip rules of
//! [`notification::maybe_notify`](crate::models::notification::maybe_notify))
//! and their withdrawals, so the hooks themselves stay one call long.
//!
//! [`translate`] is the outbound half of status translation: which provider
//! the instance talks to, what a translate call and a language list look like
//! over there, and the sanitizer pass every provider answer goes through
//! before an endpoint serves it.
//!
//! [`timeline`] is the read side every status timeline shares: the one
//! parametrized query builder that composes a source, the request's filters,
//! the viewer's block/mute/domain-block/thread-mute exclusions and the
//! visibility clause into a single keyset-paged statement.
//!
//! [`streaming`] is the audience side of the streaming bus: for a stored
//! status and its author it computes the hub stream names the event would
//! ride (`public`, `user:<uuid>`, `list:<uuid>`, `hashtag:<tag>`, …), the
//! [`Audience`](crate::streaming::hub::Audience) that gates its delivery
//! and the local-only flag, so the publishing slice that hands the event to
//! [`Hub::publish`](crate::streaming::hub::Hub::publish) never re-decides
//! who may see what.
//!
//! [`vapid`] is the instance's Web Push identity: the ES256 key pair kept in
//! `instance_secrets`, generated on first use and overridable through the
//! `VAPID_*` settings, of which the endpoints serve only the public half.

pub mod create_status;
pub mod delete_status;
pub mod emoji_packs;
pub mod instance_actor;
pub mod instance_meta;
pub mod media_processing;
pub mod media_proxy;
pub mod mentions;
pub mod notify;
pub mod render_status;
pub mod streaming;
pub mod timeline;
pub mod translate;
pub mod vapid;
