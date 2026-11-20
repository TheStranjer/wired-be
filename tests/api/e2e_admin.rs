//! End-to-end admin, moderation, MRF, config and operations scenarios
//! (WBE-225): whole journeys walked through the HTTP API with an admin
//! bearer and a moderator bearer, the background queues driven by hand
//! through [`common::run_all_jobs`], and the mock federation peer from
//! [`remote`] as the remote every MRF phase judges.
//!
//! Each scenario is its own `#[tokio::test]` so a failure names one
//! journey:
//!
//! 1. [`a_report_walks_both_admin_apis_and_the_staff_actions`] — the
//!    moderation flow: a report filed over `POST /api/v1/reports` read
//!    back through the Pleroma admin surface, the Mastodon admin surface
//!    and the legacy `/api/v0` twin; the notes/assign/resolve lifecycle
//!    walked through both APIs; the status edited and deleted through the
//!    admin statuses surface; and the account deactivated until its
//!    content vanishes from the public timeline, then reactivated. Every
//!    step's moderation-log entry is counted and worded inline.
//! 2. [`the_mrf_policies_shape_inbound_activities_as_configured`] and
//!    [`a_blocked_domain_is_refused_at_the_door_and_swept_on_deletion`] —
//!    the `SimplePolicy` (`reject`, `followers_only`, `media_removal`),
//!    the `Keyword`, `Hellthread`, `ObjectAge` and `Tag` policies
//!    configured through `POST /admin/config` and judged against inbound
//!    activities from the mock peer, the chain's transparency read through
//!    `/api/v1/instance`, `domain_blocks` and NodeInfo, and the remote
//!    domain's deletion through `DELETE /instances/{instance}`.
//! 3. [`runtime_config_rules_documents_and_announcements_reach_the_public_api`]
//!    — the runtime `:instance` overrides landing in the public instance
//!    endpoints, `GET /config/descriptions`, the instance-document
//!    upload/serve/delete cycle, and the rules and announcements CRUD read
//!    through the user-facing endpoints beside the `announcement` event.
//! 4. [`every_admin_write_records_exactly_one_moderation_log_entry`] —
//!    one write per admin surface, then the whole log: every write's
//!    action count and its full sentence.
//! 5. [`moderators_walk_the_staff_surfaces_and_are_refused_the_admin_ones`]
//!    — the moderator walks the reports and statuses surfaces and is
//!    refused every other admin group, while an `admin:read` grant never
//!    writes and an `admin:write` grant never reads, area scopes included.
//! 6. [`the_report_created_webhook_and_the_admin_relay_walk_their_handshakes`]
//!    — a subscription made through the admin API delivers its signed
//!    envelope when a real filing drains the queue, and the `/relay`
//!    lifecycle walks follow, ledger, withdrawal and the `Undo` against
//!    the mock peer's relay actor.
//! 7. [`an_admin_backup_is_queued_built_and_downloaded`],
//!    [`remote_media_is_rewritten_into_signed_proxy_links_and_served`],
//!    [`maintenance_sweeps_the_stale_rows_and_a_throttled_bucket_answers_429`]
//!    — the operations round.
//!
//! What each route refuses on its own is pinned by the area files
//! (`admin_reports.rs`, `mastodon_admin_reports.rs`, `admin_statuses.rs`,
//! `admin_users.rs`, `admin.rs`, `admin_content.rs`, `admin_ops.rs`,
//! `admin_federation.rs`, `webhooks.rs`, …); this module owns the seams —
//! the state one admin endpoint leaves behind for the next and what the
//! API answers about it.
//!
//! The area lives below `tests/api/e2e_admin/`, one submodule per section
//! of the journey list: `support` holds the consts and the [`World`]
//! every submodule draws on; `mrf` and `mrf_policies` run the `Simple`,
//! `Keyword`, `Hellthread`, `ObjectAge` and `Tag` phases; `domain_block`
//! the instance deletion and its sweeps; `runtime_config` the config,
//! documents, rules and announcements walk; `moderation_log` the log's
//! completeness over the whole admin write surface; `gates` the
//! admin-versus-staff, scope and credential gates; `webhooks` and `relays`
//! the signed hook and the relay handshake; `backup_proxy_maintenance`
//! the operations round; and `reports` with `reports_enforcement` the
//! moderation flow through both admin report APIs.
//!
//! [`World`]: support::World
//! [`common::run_all_jobs`]: crate::common::run_all_jobs
//! [`remote`]: crate::remote
//! [`a_report_walks_both_admin_apis_and_the_staff_actions`]: reports::a_report_walks_both_admin_apis_and_the_staff_actions
//! [`the_mrf_policies_shape_inbound_activities_as_configured`]: mrf::simple_policy_rewrites_refuses_and_reports_itself
//! [`a_blocked_domain_is_refused_at_the_door_and_swept_on_deletion`]: domain_block::a_blocked_domain_is_refused_at_the_door_and_swept_on_deletion
//! [`runtime_config_rules_documents_and_announcements_reach_the_public_api`]: runtime_config::runtime_config_rules_documents_and_announcements_reach_the_public_api
//! [`every_admin_write_records_exactly_one_moderation_log_entry`]: moderation_log::every_admin_write_records_exactly_one_moderation_log_entry
//! [`moderators_walk_the_staff_surfaces_and_are_refused_the_admin_ones`]: gates::the_admin_surface_refuses_moderators_scopes_and_the_anonymous
//! [`the_report_created_webhook_and_the_admin_relay_walk_their_handshakes`]: webhooks::a_filed_report_delivers_its_signed_hook_and_disable_silences_it
//! [`an_admin_backup_is_queued_built_and_downloaded`]: backup_proxy_maintenance::an_admin_backup_is_queued_built_and_downloaded
//! [`remote_media_is_rewritten_into_signed_proxy_links_and_served`]: backup_proxy_maintenance::remote_media_is_rewritten_into_signed_proxy_links_and_served
//! [`maintenance_sweeps_the_stale_rows_and_a_throttled_bucket_answers_429`]: backup_proxy_maintenance::maintenance_sweeps_the_stale_rows_and_the_throttle_answers_429

#![cfg(test)]

/// Scenario 7: the admin backup archive, the signed media-proxy rewrite
/// and the maintenance sweeps with the throttled bucket.
mod backup_proxy_maintenance;
/// Scenario 2, part 3: the refused domain's deletion and its sweeps.
mod domain_block;
/// Scenario 5: the admin-versus-staff, role, scope and credential gates.
mod gates;
/// Scenario 4: one moderation-log entry per admin write, whole log read.
mod moderation_log;
/// Scenario 2, part 1: the `simple` policy phases and the MRF machinery.
mod mrf;
/// Scenario 2, part 2: the `Keyword`, `Hellthread`, `ObjectAge` and `Tag`
/// policies judged on the inbound chain.
mod mrf_policies;
/// Scenario 6, second half: the admin relay lifecycle against the mock
/// relay actor.
mod relays;
/// Scenario 1: the report walked through both admin report APIs.
mod reports;
/// Scenario 1's enforcement half: the resolves, the admin status writes
/// and the deactivation leg.
mod reports_enforcement;
/// Scenario 3: the runtime config, documents, rules and announcements.
mod runtime_config;
/// Shared consts and helpers every submodule of this area draws on.
mod support;
/// Scenario 6, first half: the signed `report.created` webhook delivery.
mod webhooks;
