//! Cross-cutting services: behaviour that is neither a router area, a
//! model nor an entity, but is used across them.
//!
//! [`media_proxy`] is the first resident: it rewrites remote media URLs
//! into signed proxy links (used by the [`entities`](crate::entities)
//! builders) and serves those links over HTTP (its router is merged at
//! the top level by [`crate::app`]).

pub mod media_proxy;
