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
//! [`vapid`] is the instance's Web Push identity: the ES256 key pair kept in
//! `instance_secrets`, generated on first use and overridable through the
//! `VAPID_*` settings, of which the endpoints serve only the public half.

pub mod emoji_packs;
pub mod instance_actor;
pub mod instance_meta;
pub mod media_processing;
pub mod media_proxy;
pub mod mentions;
pub mod render_status;
pub mod timeline;
pub mod translate;
pub mod vapid;
