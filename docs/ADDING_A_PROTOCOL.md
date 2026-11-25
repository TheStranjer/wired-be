# Adding a protocol

Read `docs/ARCHITECTURE.md` decision 18 (the why) and decisions 7 and 17
first. This is the order of work; each numbered step is sized to be one
ticket. `<id>` is the protocol's one id string (`matrix`), `<Variant>` its
`ProtocolId` variant (`Matrix`), `<noun>` what its main table holds
(`matrix_accounts`). Copy from `src/protocols/fixture.rs` (the minimal
adapter, test builds only); `src/protocols/activitypub.rs` shows a protocol
that wraps a large existing implementation.

## 1. Register the id

- `src/protocols.rs`: add the `<Variant>` to `ProtocolId`, to `ProtocolId::ALL`
  (after `ActivityPub`, before the fixture), and to `as_str` (returns `<id>`)
  and `is_compiled` (`cfg!(feature = "<id>")`). Add `feature = "<id>"` to the
  `any(...)` list and the message of the `compile_error!`. Update the tests at
  the bottom (`as_str_gives_the_documented_ids` and the others that list ids).
- `Cargo.toml`, `[features]`: add `<id> = []`. Add it to `default` only if the
  protocol ships switched on by default.
- Docs: the `PROTOCOLS` row of the `README.md` table and the `PROTOCOLS`
  comment in `.env.example` (they name the ids an operator can use).
  `DEFAULT_PROTOCOLS` in `src/config/defaults.rs` stays `activitypub` unless
  the default really changes.
- The compiler now points at every `match` on `ProtocolId`; the one in
  `src/protocols/registry.rs` needs step 2.

## 2. Add the adapter

- Copy `src/protocols/fixture.rs` to `src/protocols/<id>.rs` and rename.
  Declare `pub mod <id>;` in `src/protocols.rs`, and add
  `ProtocolId::<Variant> => &<Adapter>` to `registry::adapter` in
  `src/protocols/registry.rs`.
- Fill in the required methods of `ProtocolAdapter`
  (`src/protocols/adapter.rs`): `id`, `router`, `identifier_field`,
  `validate_identifier`, `derive_identifier`, `identifier_taken`,
  `identifier_uri`, `identity`, `claim`. Steps 3 to 6 fill the bodies. Leave
  every defaulted hook alone until its step. Replace the fixture's in-memory
  statics (`TAKEN_IDENTIFIERS`, `ENROLLED`, the recorder) with real queries.
- Keep the file a façade when the protocol has a lot of code: the logic goes
  in its own modules, as `src/federation/` does for ActivityPub.

## 3. Create the main table

- `sqlx migrate add <id>_<noun>` creates `migrations/<timestamp>_<id>_<noun>.sql`.
  Never write the version by hand.
- Table `<id>_<noun>`, modelled on `migrations/20261125114701_activitypub_actors.sql`:
  `user_id uuid PRIMARY KEY REFERENCES users (id) ON DELETE CASCADE`, a
  `UNIQUE` identifier column, `status text NOT NULL DEFAULT 'active' CHECK
  (status IN ('pending', 'active'))`, `inserted_at`, `updated_at`, and the
  protocol's own columns. Protocol data never goes into `users`.
- Model code in `src/models/<id>_<noun>.rs` (declare it in `src/models.rs`),
  modelled on `src/models/activitypub_actor.rs`: a `get` by `user_id` and an
  `insert_new` that uses `ON CONFLICT DO NOTHING` and reports whether it wrote.

## 4. Write the identifier scheme

In `src/protocols/<id>.rs`:

- `identifier_field`: the error key under which the ID's failures are reported.
- `validate_identifier`: normalise and check the shape; return every message.
- `derive_identifier`: turn any valid nickname into an ID that passes the
  validator. `suggest_identifier` (`src/protocols/identity.rs`) makes it unique.
- `identifier_taken`: a query on the main table.
- `identifier_uri`: the ID as a URI, used to link identities across protocols.

## 5. Implement `claim` and `identity`

- `claim` inserts the identity row through the caller's `conn`
  (the transaction) with the step 3 `insert_new`, and answers
  `ClaimOutcome::Taken` when it wrote nothing. It never checks first and then
  writes.
- `identity` reads the row into an `Identity` (`identifier`, `uri`, `status`)
  and answers `None` when the account is not enrolled.
- Nothing else is needed for registration and enrolment: they already iterate
  the enabled adapters (`src/models/user/register/claims.rs`,
  `src/api/wired.rs`).

## 6. Add the routes

- `router` (`src/protocols/<id>.rs`): the protocol's own server routes, with
  full paths, merged at the top level by `app` in `src/lib.rs`. The backend
  renders no HTML (decision 1).
- `client_api_router`: routes added to the Mastodon-compatible client API,
  with full `/api/v1/...` paths. Handlers go in `src/api/<area>.rs`
  (`pub fn router() -> Router<AppState>`). A route that needs no protocol data
  is core and goes in `api::core_router` (`src/api.rs`) instead. No route may
  repeat one of core's or another protocol's.
- A Wired extension route (`/api/v1/wired/*`) gets an entry in
  `src/api/route_inventory/wired.rs`.

## 7. Add discovery

- Owned `/.well-known` paths: `well_known_routes`. Each path starts with
  `/.well-known/` and is claimed by no one else (`src/discovery.rs` tests check
  this for every adapter).
- Shared documents core serves (`src/api/well_known.rs`, `src/api/nodeinfo.rs`):
  - `webfinger`: aliases and links for a local user. **Cross-protocol identity
    URIs go in the aliases** (`identifier_uri`).
  - `host_meta_links`: links for `/.well-known/host-meta`.
  - `nodeinfo_protocol`: the name from NodeInfo's fixed vocabulary, or `None`
    when it has no name for the protocol (Matrix has none).

## 8. Add jobs

- `job_handlers`: `(kind, handler)` pairs; a kind is unique across core and all
  protocols (`job_registry` in `src/server/jobs.rs` registers them only while
  the protocol is enabled). Handlers live in the protocol's modules, not in
  `src/jobs/`.
- `job_queues`: the queues the protocol enqueues onto. Add a `QUEUE_*` constant
  in `src/jobs.rs`, append it to `DEFAULT_JOB_QUEUES` and to the constants list
  in `default_queues_list_names_every_constant` (same file), and update the
  `JOB_QUEUES` default in the `README.md` table and `.env.example`. A queue
  missing from `DEFAULT_JOB_QUEUES` fails a test in `src/server/jobs.rs`.

## 9. React to account events

- `on_account_event` (`src/protocols/<id>.rs`): one arm per `AccountEvent`
  (`src/protocols/events.rs`), no wildcard. It runs after commit; failures are
  logged and change nothing. Network work becomes a job (step 8). The
  `Deleted` hook is retried by the `users.delete` job, so make it safe to run
  twice.
- `on_password_change_in_tx`: only for a protocol that must derive its own
  credentials from the plaintext (XMPP SCRAM). It runs inside the transaction;
  an error rolls the password change back. Never store or log the plaintext.

## 10. Add the per-protocol Clippy run

- `scripts/ci.sh`: after the `Clippy` step, add a loop that runs
  `cargo clippy --quiet --locked --keep-going --no-default-features --features <id>`
  once per protocol, each as its own `step`, so a protocol that only builds
  with another one's code fails the gate. The first new protocol creates the
  loop (ActivityPub is covered by the default build); later ones add their id.
  Add the check to the script, never only to `.github/workflows/ci.yml`.

## 11. Add the reference implementation to fedlab

Before interop work, the local federation rig needs a real server of the
protocol (for Matrix, a Synapse). This is outside this repository, in the
fedlab checkout: a service in its `compose.yml`, a host in its `Caddyfile`, and
test users from its helper scripts (`lib.sh`). Point a WiredBE instance of the
rig at it with `PROTOCOLS=activitypub,<id>`.

## 12. Write the tests

Model each on the fixture's:

- `src/protocols/fixture.rs`, `mod tests`: the validator (normalises, rejects),
  derived IDs always validate, the field key and URI, claiming twice gives
  `Taken`, `identity` is `None` until claimed.
- `src/protocols.rs` and `src/protocols/registry.rs`: ids, parsing, enabled order.
- `src/discovery.rs`: no `/.well-known` path claimed twice; `src/server/jobs.rs`:
  job kinds unique, every queue drained by default.
- `src/protocols/events.rs` and `tests/api/account_events.rs`: the hooks fire.
- `tests/api/protocols.rs`: the routes exist only while the protocol is enabled
  and the app-wide fallback answers them otherwise; `tests/api/well_known.rs`
  and `tests/api/nodeinfo.rs`: the discovery contributions.
- `tests/api/wired.rs`: `GET /api/v1/wired/protocols`, claiming, and
  `wired_identifiers` at registration, with the new protocol.
- New API areas get their own `tests/api/<area>.rs` module (`mod` line in
  `tests/api/main.rs`), never a new top-level `tests/*.rs`.

## Known open questions

- **`/.well-known/oauth-authorization-server`.** Mastodon-style OAuth and
  ATProto both want this document. Answer: an ATProto authorization server on
  its own host (for example `auth.<domain>`), leaving the document on the main
  host to the Mastodon-style OAuth. Until the first ATProto ticket lands, no
  protocol claims the path.
- **Cross-protocol account linking.** How one person's identities on several
  protocols are proven to be the same account. The namespace is still to be
  chosen: an FEP under `w3id.org`, or a domain Wired owns forever. A link
  counts only when both sides point at each other.
- **Moving `ap_id` off `users`.** `ap_id` (with `domain`, `actor_type`,
  `also_known_as`, `moved_to_ap_id`, `profile_url`) stays on `users` as the
  ActivityPub lookup key and client-rendered fields (decision 7). Moving it
  into `activitypub_actors` is a possible later track.
