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
//! [`notify`] is the write side of the notification inbox: the ten tellings
//! the action hooks and the poll-end sweep land — follow, follow request,
//! mention, reblog, favourite, poll vote, poll closing, status edit,
//! subscription and emoji reaction — decided by one set of rules (local
//! recipients, no self-notifications but the author's own closing poll,
//! every insert through the skip rules of
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
//! [`rich_media`] is the pure half of the rich media cards: the parser that
//! turns one page of HTML and its URL into a [`Card`](rich_media::Card) —
//! Open Graph tags, then Twitter card tags, then the plain title — and the
//! shaper that renders a card as the Mastodon entity JSON, the pair of
//! Pleroma's `RichMedia.Parser` and `RichMedia.Card`. The fetch, the storage
//! and the job that runs them are not here.
//!
//! [`relay`] is the instance's side of the public relays: the instance
//! actor's `Follow` of a relay actor and its withdrawal, the
//! [`relays`](crate::models::relay) subscription the publisher fans public
//! activities to, and the ledger the admin API drives from here.
//!
//! [`schedule_status`] is the same body's parked twin: it decides what a
//! `scheduled_at` create may store — the create's own checks first, then the
//! window, the per-account limits and the media reservation — and hands the
//! normalised request to the
//! [`scheduled_statuses`](crate::jobs::scheduled_statuses) queue, so the
//! endpoint's schedule arm never re-decides any of it.
//!
//! [`streaming`] is the status side of the streaming bus: for a stored
//! status and its author it computes the hub stream names the event would
//! ride (`public`, `user:<uuid>`, `list:<uuid>`, `hashtag:<tag>`, …), the
//! [`Audience`](crate::streaming::hub::Audience) that gates its delivery
//! and the local-only flag, so the publishing slice that hands the event to
//! [`Hub::publish`](crate::streaming::hub::Hub::publish) never re-decides
//! who may see what; and it is that publishing slice itself — the
//! [`status_created`](crate::services::streaming::status_created),
//! [`status_deleted`](crate::services::streaming::status_deleted) and
//! [`status_edited`](crate::services::streaming::status_edited) emitters
//! the three status hooks call.
//!
//! [`vapid`] is the instance's Web Push identity: the ES256 key pair kept in
//! `instance_secrets`, generated on first use and overridable through the
//! `VAPID_*` settings, of which the endpoints serve only the public half.
//!
//! [`web_push`] is the sender beside it: the RFC 8291 `aes128gcm` payload
//! coding, the RFC 8292 VAPID authorization and the classified POST, plus
//! the pure delivery decisions (alert flags, title verbs, payload document)
//! the [`push.deliver`](crate::jobs::web_push) job runs and the [`notify`]
//! hook queues jobs through.
//!
//! [`webhooks`] is the same shape pointed at third-party endpoints: the
//! fan-out that turns a `report.created`, `report.updated` or
//! `account.created` event into one HMAC-signed `POST` per subscribed
//! webhook, queued as a [`webhooks.deliver`](webhooks::KIND_DELIVER) job and
//! delivered by the classified attempt beside it.
//!
//! [`filter`] is the read side's last pass: the pure post-processing step
//! that applies a viewer's active v1 phrase filters to an already-rendered
//! status page — warning matches annotated, irreversible matches removed,
//! the viewer's own statuses exempt — which [`timeline_response`] and the
//! notification and thread reads run between the render and the response.
//!
//! [`config_registry`] is the whitelist of runtime-configurable settings:
//! the six `:pleroma` entries the admin config API may change, each with
//! its subkeys, its restart flag and its env-derived defaults.
//! [`runtime_config`] is the store beside it: the `config_entries` table,
//! the two value forms the API and the jsonb column exchange (keyword
//! lists as `{"tuple": […]}` arrays), and the effective-value resolution
//! the MRF pipeline and the instance documents read through — Pleroma's
//! `ConfigDB` and `Config` pair.
//!
//! [`ingest`] is the inbound half of the same post lifecycle: it folds a
//! remote `Note`-family object a `Create` delivered into a stored status —
//! the dedupe on its `uri`, the reply/quote/context links resolved against
//! what this instance already stores, the mentions settled against the
//! `users` table (with capped network lookups), the emojis, media and poll
//! written beside the status in its transaction — and runs the fresh row
//! through the create path's own post-write hook, so a mirrored post
//! notifies, cards and groups exactly as a local one does.
//!
//! [`timeline_response`]: crate::api::helpers::timeline_response

pub mod config_registry;
pub mod create_status;
pub mod delete_status;
pub mod emoji_packs;
pub mod federation_out;
pub mod filter;
pub mod ingest;
pub mod instance_actor;
pub mod instance_meta;
pub mod media_processing;
pub mod media_proxy;
pub mod mentions;
pub mod notify;
pub mod relay;
pub mod render_status;
pub mod rich_media;
pub mod runtime_config;
pub mod schedule_status;
pub mod streaming;
pub mod timeline;
pub mod translate;
pub mod vapid;
pub mod web_push;
pub mod webhooks;
