//! The HTTP integration test suite for wired-be, one crate.
//!
//! Every area file next to this one (`accounts.rs`, `activitypub.rs`, ...,
//! `well_known.rs`) is a module of this crate, not its own test binary.
//! Cargo compiles each top-level `tests/*.rs` file into a separate
//! executable that statically links the whole `wired_be` crate and all its
//! dependencies, so 39 files meant 39 relinks of every change to `src/` and
//! gigabytes of duplicated debuginfo; one crate means one link and
//! `cargo test --test api` runs the whole suite.
//!
//! The shared HTTP harness is [`common`]; the shape every endpoint test
//! takes is documented in `docs/ARCHITECTURE.md` (“Writing tests”), and
//! `health.rs` remains the reference example.

// `pub` because Clippy's nursery `redundant_pub_crate` forbids `pub(crate)`
// items inside a private module, while `unreachable_pub` forbids `pub` ones:
// only a `pub` module makes the harness helpers lint-clean.
pub mod common;

/// The directory, suggestions, familiar-followers and birthday listings.
mod account_lists;

/// The events suspend, unsuspend, deactivate and delete fire.
mod account_events;

/// The account note endpoints (`/api/v1/accounts/{id}/note`).
mod account_notes;

/// The registration endpoint (`POST /api/v1/accounts`), open mode and
/// invite tokens alike.
mod account_registration;

/// The account status listing (`GET /api/v1/accounts/{id}/statuses`): its
/// filters, the visibility each reader reaches, the pinned page and the
/// `RESTRICT_UNAUTHENTICATED` gate.
mod account_statuses;

/// The registration modes over HTTP: approval required, email confirmation,
/// the confirmation link, the resend, and the instance flags they drive.
mod registration_modes;

/// The out-of-the-box registration state: closed unless
/// `REGISTRATIONS_OPEN=true`.
mod registration_default;

/// The core account endpoints: `verify_credentials`, `GET /accounts/{id}` and `lookup`.
mod accounts;

/// The `ActivityPub` actor document (`GET /users/{nickname}`).
mod activitypub;

/// The admin foundation over HTTP (`/api/v1/pleroma/admin`, WBE-194): the
/// [`AdminUser`]/[`StaffUser`] extractor matrix, the moderation-log
/// listing with its filters, window, caps and refusals, and the
/// visibility `stats`.
///
/// [`AdminUser`]: wired_be::auth::admin::AdminUser
/// [`StaffUser`]: wired_be::auth::admin::StaffUser
mod admin;

/// The admin OAuth-app maintenance and instance-document endpoints over
/// HTTP (`/oauth_app`, `/instance_document/{name}` and the public JSON
/// `GET /api/v1/instance/terms_of_service`, WBE-205): the CRUD walk and its filters,
/// the `trusted` flag, the `{"errors": …}` refusals, the document
/// upload/read/delete cycle with its headers, size gate and traversal
/// refusals, and both areas' scope gates.
mod admin_apps_docs;

/// The admin chat endpoints over HTTP: the user chat listing, the chat
/// show, the message listing and the moderation withdrawal of a single
/// message (WBE-202), all on the ordinary entities without ownership
/// checks, with their pages and `Link` headers, their moderation entry
/// and the chats-area scope gate.
mod admin_chats;

/// The admin content endpoints over HTTP (WBE-204): the `/rules` quartet
/// and the `/announcements` quintet — the four-key rule entity in its
/// priority-then-creation order, the announcement entity with
/// `pleroma.raw_content` over its `limit` / `offset` window, the
/// `{"errors": …}` refusals, the moderation entries and the
/// `announcement` stream event, and both areas' scope gates.
mod admin_content;

/// The admin federation endpoints over HTTP (WBE-203): the `/relay`
/// follow / unfollow lifecycle against a mock relay and its
/// `{actor, followed_back}` ledger, `DELETE /instances/{instance}`
/// staging the domain's `users.delete_remote` sweeps and clearing the
/// `instances` row, `POST /reload_emoji` resyncing packs from disk, and
/// the `admin:read:relays`-family scope gate on all five routes.
mod admin_federation;

/// The admin ops endpoints over HTTP: the media-proxy cache ledger, the
/// admin backup trigger, and the unported frontend installer routes.
mod admin_ops;

/// The admin reports surface over HTTP (`/api/v1/pleroma/admin/reports`,
/// WBE-200): the filtered,
/// paged entity list, the entity show with its visibility-free statuses
/// and notes, the bulk assignment, the note lifecycle with its
/// moderation-log entries, and the staff-and-scope gate on every route.
mod admin_reports;

/// The admin statuses surface over HTTP (`/api/v1/pleroma/admin/statuses…`,
/// WBE-201): the network ledger with its `godmode` / `local_only` /
/// `with_reblogs` filters and its `page` / `page_size` window, the
/// visibility-free show, the scope write with its moderation entry and
/// federated `Update`, the shared deletion service behind `DELETE` with its
/// `{}` answer, the two `{total, activities}` envelopes of one account and
/// one domain, and the staff-and-scope gate on all six routes.
mod admin_statuses;

/// The admin users endpoints over HTTP (`GET /api/v1/pleroma/admin/users`
/// and `GET …/users/{nickname}`, WBE-195): the eight filters and their
/// combinations, the query and the named criteria, the page window beside
/// its total, the admin account entity, the handle and id lookups, and the
/// `admin:read:accounts` gate.
mod admin_users;

/// The activity reads over HTTP: `GET /activities/{uuid}` and the `Create`
/// of a status at `GET /users/{nickname}/statuses/{uuid}/activity`.
mod ap_activities;

/// The object reads over HTTP: `GET /objects/{uuid}`, its nickname-scoped
/// twin `GET /users/{nickname}/statuses/{uuid}` and `GET /notice/{id}`.
mod ap_objects;

/// Public-post reads of `/objects/{uuid}` and `/activities/{uuid}` are
/// decided by `can_view` for the signer: blocked signers get the 404.
mod ap_public_reads;

/// Quote consent (FEP-044f): a remote `QuoteRequest`, the `Accept` it
/// queues and the `QuoteAuthorization` served at
/// `GET /quote_authorizations/{uuid}`.
mod quote_authorizations;

/// Announcements over HTTP (`/api/v1/announcements`).
mod announcements;

/// The API router skeleton (`src/api.rs`) and its JSON fallback.
mod api_skeleton;

/// App registration and verification (`/api/v1/apps`).
mod apps;

/// The bearer extractors and scope guards of `wired_be::auth` over HTTP.
mod auth;

/// The `AUTHORIZED_FETCH` gate on the `ActivityPub` actor route.
mod authorized_fetch;

/// Account backups (`/api/v1/pleroma/backups`) over HTTP: the list, the
/// request, its rate limit and its entity.
mod backups;

/// The blocks endpoints (`block`, `unblock` and the blocks list).
mod blocks;

/// The bookmark writes and the bookmarks list over HTTP
/// (`/api/v1/statuses/{id}/bookmark` and `GET /api/v1/bookmarks`).
mod bookmarks;

/// Inbound `Create`/`Delete` of a `ChatMessage` through the federation
/// dispatcher (`src/federation/handlers/chat.rs`): the chat, message,
/// unread mark and `pleroma:chat_mention` a delivery lands, and the gates
/// that refuse it.
mod chat_federation;

/// The outbound federation of chat messages over HTTP (WBE-181): the
/// `Create{ChatMessage}` a message posted to a remote counterpart publishes
/// and the `Delete` of its reserved object id when the sender withdraws it,
/// and the local pairs that deliver nothing
/// (`src/services/federation_out.rs`).
mod chat_federation_out;

/// The chat API over HTTP (`/api/v1/pleroma/chats` and its v2 twin).
mod chats;

/// The `create-admin` CLI function against the database (its argument
/// parser is unit-tested in `src/cli.rs`).
mod cli;

/// The `ActivityPub` collections over HTTP: outbox, followers, following,
/// featured and an object's replies (`src/federation/collections.rs`).
mod collections;

/// The conversation API over HTTP (`/api/v1/conversations` and Pleroma's
/// `/api/v1/pleroma/conversations` routes).
mod conversations;

/// The custom emoji list (`GET /api/v1/custom_emojis`).
mod custom_emojis;

/// The built-in default avatar, banner, thumbnail and mascot served under
/// `/media/default` (`src/default_assets.rs`).
mod default_assets;

/// Domain blocks (`/api/v1/domain_blocks`).
mod domain_blocks;

/// The end-to-end account, auth and social-graph scenarios (WBE-222):
/// onboarding from an app registration to a new password, the OAuth code
/// flow, the MFA gate, the relationship walk over follows, blocks, mutes,
/// a domain block, endorsements, notes, lists and paged listings, the
/// registration modes, admin user management and the CSV imports.
mod e2e_accounts;

/// The end-to-end admin scenarios (WBE-225): the moderation flow walked
/// through both admin report APIs with its notes, assignments, resolves,
/// the admin status edit and delete and the deactivation that hides an
/// account's posts from the timelines; the MRF chain configured over
/// `POST /admin/config` and read off the inbox, the instance federation
/// block and `NodeInfo`; the runtime-config, descriptions, document, rule
/// and announcement walks; the moderation log's completeness over the
/// whole admin write surface; the moderator-versus-admin and scope gates;
/// the signed `report.created` webhook and the admin relay handshake; and
/// the operations round — backup archive, media proxy, maintenance sweeps
/// and the throttled bucket.
mod e2e_admin;

/// The end-to-end discovery and federation scenarios (WBE-223): the
/// search trio over the visibility matrix, the v1 phrase filters applied
/// to the timelines, and the link card through the job
/// queue, and a remote mention resolved and delivered against a loopback
/// mock peer.
mod e2e_discovery;

/// The `remote::MockPeer` harness smoke tests (WBE-224 slice 1): the mock
/// account resolving to the mock origin through `GET /api/v1/accounts/lookup`,
/// one local follow-and-post delivered signed and verified out of the mock's
/// capture, and one mock-signed `Create` folding onto the public timeline.
mod e2e_federation;

/// The end-to-end federation discovery shapes and remote resolution
/// (WBE-224 scenarios 1 and 5): the `WebFinger`/host-meta/`NodeInfo`/actor
/// surface read by a remote's own HTTP client and off the JSON-LD keys,
/// the `AUTHORIZED_FETCH` gate on the actor, object and activity reads,
/// and the `resolve=true` lookups plus the follow-by-uri write reaching the
/// mock peer over real HTTP.
mod e2e_federation_discovery;

/// The end-to-end federation of relationships (WBE-224 scenario 2).
///
/// A remote `Follow` of a local account granted and answered with a signed
/// `Accept` at the peer's inbox, a locked account's own answer, a local follow
/// of a remote account the peer accepts, the `Undo` of a follow and of a
/// block, blocks in both directions, and a relay subscription followed,
/// accepted, fanned out to, announced by and withdrawn over real HTTP.
mod e2e_federation_follow;

/// The end-to-end federation of inbound content (WBE-224 scenario 3 and the
/// chat half of scenario 7): a signed `Create Note` at each visibility and
/// with rich content read back through the timelines, context, poll, reactions
/// and chat endpoints; the `Update` edit history, the `Delete` tombstone and
/// its swallowed late create; the `Like`, `EmojiReact` and `Announce` counters
/// and tellings; the thread backfill over real HTTP and its 404 degradation;
/// a remote ballot on a local poll; and the delivered `ChatMessage` and its
/// sender's withdrawal — every fact reached by real signed HTTP and read
/// through the public API, never a row dump.
mod e2e_federation_inbound;

/// The end-to-end federation of outbound content (WBE-224 scenario 4): the
/// `Create`/`Update`/`Delete` of a status, the `Announce`, `Like` and
/// `EmojiReact` writes and their `Undo`s delivered signed to the right
/// inboxes across the visibility matrix, shared-inbox deduplication, the
/// profile `Update`, `Move`, forwarded `Flag` and list `Add`/`Remove`, and
/// the retry ladder — `500` on backoff, `503` at its `Retry-After`, `410`
/// dropping the address for good, and a refused signature retried once —
/// all read off a loopback mock peer's capture over real HTTP.
mod e2e_federation_outbound;

/// The end-to-end federation refusals (WBE-224 scenario 6): every way an
/// inbox delivery is turned away — unsigned, unknown key, malformed
/// signature, spoofed signer, tampered body or digest, ids off the signer's
/// origin, an over-cap body, a refused private-host resolution and an
/// `MRF`-blocked domain — each answered with its documented status and
/// re-read through the public API as nothing having happened.
mod e2e_federation_safety;

/// The end-to-end interaction scenarios (WBE-223): favourite, reblog,
/// bookmark + folder, pin, thread mute, reaction, poll vote, edit, expiry
/// through the job queue, and delete, each asserted across the direct
/// read and the home and public timeline representations.
mod e2e_interactions;

/// The end-to-end notification scenarios (WBE-223): every notification
/// type generated by its real action on the flat v1 page with filters and
/// cursors, the v2 grouped listing and its unread count, marker round
/// trips driving the unread linkage, the Pleroma stranger gate, and the
/// direct-message flow through conversations and chats.
mod e2e_notifications;

/// The end-to-end realtime scenarios (WBE-226): the streaming WebSocket
/// driven over a real `axum::serve` listener with `tokio-tungstenite` and
/// `reqwest` — create/edit/delete, notification, conversation, chat and
/// filter frames across the public, local, hashtag, home, list, direct and
/// chat streams with both token carriers and the audience and block/mute
/// gates — Web Push delivered end to end against a mock push service with
/// browser-key decryption, alert and policy gating and the 410 retirement,
/// the client-to-server `ActivityPub` walk (`whoami`, `upload_media`, the
/// outbox `Create` and the owner's inbox read), the `Link`-header
/// conformance sweep over every shared-`PageParams` listing, and the
/// emoji-pack lifecycles from pack creation through upload, archive,
/// remote install and loose-file import.
mod e2e_realtime;

/// The end-to-end job harness (`common::run_all_jobs`) and the scheduling,
/// idempotency and preview scenarios it proves.
mod e2e_scheduled;

/// The end-to-end posting matrix (WBE-223): the fixture world built over
/// HTTP, the show-route sweep of every single-status kind past every
/// viewer role, the thread-kind posts with their per-viewer `context`
/// reads, the account statuses filter sweep, and the membership sweep of
/// every posted kind across all five timelines per viewer — pinning the
/// visibility model's audience, the `parent_visible` holes and the one
/// shared 404.
mod e2e_statuses;

/// The emoji-pack endpoints of `src/api/emoji_packs.rs`.
mod emoji_packs;

/// Emoji reactions (`/api/v1/pleroma/statuses/{id}/reactions`).
mod emoji_reactions;

/// The endorsements endpoints (`/api/v1/endorsements`).
mod endorsements;

/// Featured hashtags (`/api/v1/featured_tags`, the account listing, the tag
/// `feature` routes and the actor's `featuredTags` collection).
mod featured_tags;

/// Pleroma's federation status (`GET /api/v1/pleroma/federation_status`):
/// the unreachable-host ledger, its empty shape, its per-host timestamps,
/// its lack of any auth gate, and its database failure.
mod federation_status;

/// Filters over HTTP (`/api/v1/filters`).
mod filters;

/// Favourites (`/api/v1/statuses/{id}/favourite`, `…/favourited_by` and `/favourites`).
mod favourites;

/// Following and unfollowing an account (`/api/v1/accounts/{id}/follow`).
mod follow_actions;

/// The legacy follow write keyed by handle or actor URL
/// (`POST /api/v1/follows`).
mod follow_by_uri;

/// Inbound follow handshake over federation (WBE-168): the dispatcher
/// folding a peer's `Follow`, `Accept`, `Reject` and `Undo`(of a `Follow`)
/// — grants with an `Accept` delivery, blocked pairs answered with a
/// `Reject`, our own pending follows answered by the followee, withdrawals
/// and duplicates — onto this instance's rows, counters, tellings and
/// delivery queue (`src/federation/handlers/follow.rs`).
mod follow_federation;

/// The outbound federation of the follow endpoints over HTTP (WBE-171): the
/// `Follow` and its `Undo` the follow and unfollow writes deliver, the
/// `Accept` / `Reject` the follow-request answers and a forced follower
/// removal deliver, and the local pairs that deliver nothing
/// (`src/services/federation_out.rs`).
mod follow_federation_out;

/// The follower and following lists of an account.
mod follow_lists;

/// Follow requests (`/api/v1/follow_requests` and the answer routes).
mod follow_requests;

/// The `/health` endpoint and the `common` harness itself.
mod health;

/// Remote-note ingestion over HTTP: fixture `Create` activities run through
/// the real dispatcher and the statuses they land, seen from the client
/// API.
mod ingest;

/// The public instance endpoints (`src/api/instance.rs`).
mod instance;

/// The instance actor document and its exclusion from public listings.
mod instance_actor;

/// Inbound `Like`, `EmojiReact`, `Announce` and their `Undo`s over federation
/// (WBE-177): the dispatcher folding a peer's interaction onto a local (or
/// freshly fetched) status — rows, counters, tellings, the entity's
/// `favourites_count`/`reblogs_count`/`pleroma.emoji_reactions` — and the
/// visibility and wrong-actor gates that refuse it (`src/federation/
/// handlers/interactions.rs`).
mod interactions;

/// List members (`/api/v1/lists/{id}/accounts`).
mod list_accounts;

/// Lists over HTTP (`/api/v1/lists`).
mod lists;

/// Read markers (`/api/v1/markers`).
mod markers;

/// The Mastodon admin account reads over HTTP (`GET /api/v1/admin/accounts`,
/// its `{id}` twin and `GET /api/v2/admin/accounts`, WBE-210): the sixteen-key
/// entity with its role object, the v1 boolean and v2 word spellings of the
/// same criteria, the criteria this schema cannot weigh and accepts anyway,
/// the page under a `Link` header keyed on the account ids, the show route's
/// two misses, and the admin-and-`admin:read:accounts` gate.
mod mastodon_admin;

/// The Mastodon admin reports surface over HTTP (`/api/v1/admin/reports` and
/// its four moderation writes, WBE-214): the filtered, cursor-paged open
/// queue under its report-id-keyed `Link` header, the fourteen-key entity
/// with its four admin accounts, rendered statuses and cited rules, the
/// action-stamping resolve and its clearing reopen, the assign-to-self and
/// unassign pair with their moderation-log entries, the one 404, and the
/// staff-and-scope gate on every route.
mod mastodon_admin_reports;

/// The media route (`src/api/media_files.rs`) and the local uploader.
mod media;

/// The media metadata endpoints (`GET`/`PUT`/`DELETE /api/v1/media/{id}`).
mod media_metadata;

/// The image processing a replacement media preview (`PUT /api/v1/media/{id}` `thumbnail`) passes.
mod media_preview_processing;

/// The media proxy route (`GET /proxy/{sig}/{url}[/{filename}]`) against a mock upstream.
mod media_proxy;

/// The media upload endpoints (`POST /api/v1/media` and `/api/v2/media`).
mod media_upload;

/// The MFA endpoints (`/api/pleroma/accounts/mfa`).
mod mfa;

/// The small utility endpoints and the removed auth endpoints' fallback 404.
mod misc;

/// The mutes endpoints.
mod mutes;

/// NodeInfo (`/.well-known/nodeinfo` and `/nodeinfo/{version}`).
mod nodeinfo;

/// Federated note edits and deletions over HTTP: inbound `Update` and
/// `Delete` activities run through the real dispatcher and what the status,
/// its history, its poll and its parent's counter then say.
mod note_edits;

/// Notification generation over HTTP: the follow, mention, reblog and
/// favourite actions and the recipient's `GET /api/v1/notifications`.
mod notification_generation;

/// The `quote` and `quoted_update` notifications of a quoted post's author.
mod notification_quotes;

/// Pleroma's notification settings (`PUT /api/pleroma/notification_settings`)
/// and the owner-only render of the flags.
mod notification_settings;

/// The flat notification API: `clear`, the dismissals, `destroy_multiple`
/// and Pleroma's `read` (`/api/v1/notifications`, `/api/pleroma`).
mod notifications;

/// The grouped v2 notification API (`/api/v2/notifications`).
mod notifications_v2;

/// The `/oauth` endpoints: the token, revoke and authorize flows.
mod oauth;

/// The external OAuth login endpoints.
mod oauth_external;

/// The OAuth MFA challenge flow (`/oauth/mfa/challenge`, the token grant's
/// challenge and the retired `/oauth/mfa` pages).
mod oauth_mfa;

/// OAuth token management (`/api/oauth_tokens` and the pleroma apps).
mod oauth_tokens;

/// Password reset (`/auth/password` and `/api/v1/pleroma/password_reset`).
mod password_reset;

/// Account aliases and `move_account` (`/api/pleroma`).
mod pleroma_account_settings;

/// The Pleroma account extensions merged at `/api/pleroma/accounts`.
mod pleroma_accounts;

/// Bookmark folders (`/api/v1/pleroma/bookmark_folders`) over HTTP.
mod pleroma_bookmark_folders;

/// CSV imports of follows, blocks and mutes (`/api/pleroma/*_import`).
mod pleroma_import;

/// The frontend settings store, mascot and preferred frontend.
mod pleroma_settings;

/// Pinned statuses (`/api/v1/statuses/{id}/pin` and its `…/unpin` twin).
mod pins;

/// The protocol adapters `app()` mounts under each `PROTOCOLS` value.
mod protocols;

/// The admin routes split into core and `ActivityPub`-backed halves under
/// each `PROTOCOLS` value.
mod protocols_admin;

/// The account-settings and misc routes split into core and
/// `ActivityPub`-backed halves under each `PROTOCOLS` value.
mod protocols_account_settings;

/// Poll parameters on `POST /api/v1/statuses`: the `CommonAPI` poll rules,
/// the empty-text skip, the form nesting, and the storage wiring — poll
/// and option rows committed with the status, rendered in the entity,
/// stored once per replayed key and never by a preview.
mod polls;

/// The posting and reading defaults (`GET /api/v1/preferences`, WBE-826).
mod preferences;

/// Inbound poll votes over federation (WBE-174): the dispatcher folding a
/// `Create` of a bodyless `Note` that names one option of this instance's
/// own poll — counts, dedupe, refusals, the author's telling — and the
/// fall-through that leaves an ordinary note reply to the create path.
mod poll_votes;

/// The outbound federation of poll votes over HTTP (WBE-181): the answer
/// `Create`s one ballot on a remote question publishes to the poll author's
/// inbox, its exact document for a single- and a multiple-choice mark, and
/// the local votes that send nothing (`src/services/federation_out.rs`).
mod poll_federation_out;

/// Web Push subscriptions (`/api/v1/push/subscription`) and the VAPID key
/// the app and instance responses advertise.
mod push;

/// The per-route rate limiter: mounted buckets, headers, and 429s.
mod rate_limit;

/// `…/reblogged_by` lists only distributable (public / unlisted) boosts.
mod reblogged_by_visibility;

/// `…/reblogged_by` drops deactivated and viewer-blocked accounts.
mod reblogged_by_filters;

/// Reblogs over HTTP (`/api/v1/statuses/{id}/reblog`, its `…/unreblog`
/// twin and the `…/reblogged_by` listing).
mod reblogs;

/// Relays over federation: the instance actor's follow of a relay, the
/// accepted relay's fan-out and Announce fold, and the `/relay` endpoints
/// (`src/services/relay.rs`, `src/federation/handlers/interactions.rs`,
/// `src/api/activitypub.rs`).
mod relays;

/// `GET /api/v1/accounts/relationships`.
mod relationships;

/// The shared loopback mock federation peer the federation scenarios drive.
///
/// Real actor documents under a real RSA key, a paged outbox, capturing
/// inboxes, a `WebFinger`, a `Service` relay, a shapeable public `Note` and a
/// `Key` mirror, plus the signed-`POST` half that lets a peer deliver to this
/// instance's inbox.
pub mod remote;

/// Remote interaction over HTTP (`src/api/remote_interaction.rs`): the
/// `POST /api/v1/pleroma/remote_interaction` refusals, the
/// retired `/ostatus_subscribe` pair, the retired `/authorize_interaction`
/// and the retired `/main/ostatus` pair.
mod remote_interaction;

/// `POST /api/v1/accounts/{id}/remove_from_followers`.
mod remove_from_followers;

/// Reports over HTTP (`POST /api/v1/reports`): filing a report.
mod reports;

/// Rich media cards over HTTP: the entity's `card` key on the status
/// reads and the deprecated `GET /api/v1/statuses/{id}/card` endpoint.
mod rich_media;

/// The route-inventory conformance gate (WBE-221): every row of
/// `src/api/route_inventory.rs` requested without credentials through the
/// real router, so a route nobody routed on, or a gated route that lets an
/// anonymous request through, fails the suite.
mod route_inventory;

/// Scheduled statuses over HTTP: the `scheduled_at` arm of
/// `POST /api/v1/statuses` (parking a create, its window, limits, media
/// reservation and entity answer) and the four `/api/v1/scheduled_statuses`
/// routes that list, read, move and cancel what was parked.
mod scheduled_statuses;

/// Scrobbles federating out as `Listen`/`Audio` documents (WBE-763): the
/// queued delivery and the served object and activity.
mod scrobble_federation;

/// Scrobbles over HTTP (`/api/v1/pleroma/scrobble`).
mod scrobbles;

/// The search endpoints (`/api/v1/accounts/search`, `/api/v1/search` and
/// `/api/v2/search` with their accounts, statuses and hashtag arms).
mod search;

/// Status edit endpoints over HTTP (`PUT /api/v1/statuses/{id}`, its
/// `…/history` and `…/source` twins).
mod status_edits;

/// The outbound federation of the status lifecycle over HTTP: the `Create`,
/// `Update` and `Delete` the post, edit and delete routes queue.
mod status_federation;

/// The status endpoints: the reads (`GET /api/v1/statuses/{id}` and its
/// batch form) and the write (`POST /api/v1/statuses`).
mod statuses;

/// The streaming WebSocket (`/api/v1/streaming`) over a real listener.
mod streaming;

/// Tags over HTTP (`/api/v1/tags/{name}`, `/api/v1/followed_tags`, trends).
mod tags;

/// Thread mutes over HTTP (`/api/v1/statuses/{id}/mute` and its `…/unmute`
/// twin) and the `muted` / `pleroma.thread_muted` entity flags.
mod thread_mutes;

/// Status timelines over HTTP (`/api/v1/timelines/*`): the tag, public,
/// home, direct and list timelines' selection, filters, cursors, auth and
/// restriction gate.
mod timelines;

/// Status translation over HTTP (`POST /api/v1/statuses/{id}/translate` and
/// the instance endpoints' translation half), against a mock provider.
mod translate;

/// `PATCH /api/v1/accounts/update_credentials`.
mod update_credentials;

/// Signed event deliveries over HTTP (WBE-212): the `report.created`,
/// `report.updated` and `account.created` jobs the real endpoints queue, the
/// staff letter a filing writes, the admin `PATCH /reports` state change with
/// its 204/400 answers and its scope gate, and one delivery performed against
/// a mock endpoint that checks the signature (`src/services/webhooks.rs`).
mod webhooks;

/// WebFinger and host-meta (`/.well-known/webfinger`, `/.well-known/host-meta`).
mod well_known;
