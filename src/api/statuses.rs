//! The status endpoints: create, the reads, the state changes and listings,
//! translation and the delete, all over one shared status gate
//! (`docs/ARCHITECTURE.md` decision 1).
//!
//! The routes are the write `POST /api/v1/statuses`, the lookups
//! `GET /api/v1/statuses/{id}` and the batch
//! `GET /api/v1/statuses?id[]=a&id[]=b`, the thread read
//! `GET /api/v1/statuses/{id}/context`, the card read
//! `GET /api/v1/statuses/{id}/card`, the quoting listing
//! `GET /api/v1/statuses/{id}/quotes` and its Pleroma twin
//! `GET /api/v1/pleroma/statuses/{id}/quotes`, the favourite writes
//! `POST /api/v1/statuses/{id}/favourite` and its `…/unfavourite` twin,
//! the state listing `GET /api/v1/statuses/{id}/favourited_by`, the
//! caller's own favourites list `GET /api/v1/favourites`, the reblog
//! writes `POST /api/v1/statuses/{id}/reblog` and its `…/unreblog` twin,
//! the state listing `GET /api/v1/statuses/{id}/reblogged_by`, the bookmark
//! writes `POST /api/v1/statuses/{id}/bookmark` and its `…/unbookmark`
//! twin, the caller's own bookmarks list `GET /api/v1/bookmarks`, the pin
//! writes `POST /api/v1/statuses/{id}/pin` and its `…/unpin` twin, the
//! thread-mute writes `POST /api/v1/statuses/{id}/mute` and its
//! `…/unmute` twin,
//! `POST /api/v1/statuses/{id}/translate`, the write
//! `DELETE /api/v1/statuses/{id}`, the edit write
//! `PUT /api/v1/statuses/{id}`, the version listing
//! `GET /api/v1/statuses/{id}/history` and the source read
//! `GET /api/v1/statuses/{id}/source`.
//!
//! The reference is Pleroma's `StatusController` (`lib/pleroma/web/
//! mastodon_api/controllers/status_controller.ex`): `create` for the new
//! post, `show` and `.index` for the reads, `favourite` / `unfavourite` /
//! `favourited_by` / `favourites` for the likes, `reblog` / `unreblog` /
//! `reblogged_by` for the boosts, `bookmark` / `unbookmark` / `bookmarks`
//! for the bookmarks, `mute_conversation` / `unmute_conversation` for the
//! thread mutes, and `delete` — with `Pleroma.Web.CommonAPI.delete/2`
//! (`lib/pleroma/web/common_api.ex`, whose `repeat` / `unrepeat` stand
//! behind the boosts) — for the writes. Those, `create` apart, reduce to
//! the same two moves per surface: find the status, ask
//! `Visibility.visible_for_user?`, and either render it or answer
//! `{:error, :not_found}`. Here finding and asking live in the shared
//! [`status_for_viewer_or_404`], which every
//! `/api/v1/statuses/{id}/...` route reuses, and rendering lives in the
//! batched page renderer [`crate::services::render_status`].
//!
//! # The create
//!
//! `POST /api/v1/statuses` is the module's other half, and Pleroma's
//! `create`: a user token granting `write:statuses` (its own scope, not the
//! root) on the `statuses_actions` throttle, the body read through
//! [`ApiParams`] so JSON, form and query parameters arrive the same way,
//! and the whole decision — what the text says, who it mentions, what it
//! tags, how visible it is and what the author's settings default to —
//! delegated to [`crate::services::create_status`]. The handler adds the
//! gate, the client behind the token, the JSON envelope, and the two
//! request-level arms Pleroma hangs off this route: an `Idempotency-Key`
//! header (its own [`IdempotencyPlug`] stands the create's replay on a
//! cache; here the claim is a row in
//! [`crate::models::idempotency_key`], inserted inside the status's own
//! transaction, so a retry of a key that created a live status answers that
//! status and creates nothing) and a `preview` parameter (Pleroma's
//! `draft.preview?`: every check and the full render run, and the entity a
//! create would have answered is built in memory without a write or a
//! hook). A post that passes answers **200** with the [`Status`] entity
//! rendered for its author. Everything a fresh status still owes the
//! streaming sockets, the notifications and the wider federation hangs off
//! that service's single documented hook, so this file never grows a second
//! write path. The body may also carry Pleroma's `expires_in` (top-level
//! only, whole seconds): the service turns an accepted span into the row's
//! `expires_at`, and a post wearing one schedules its own delete — a
//! [`statuses.expire`](crate::jobs::statuses::KIND_EXPIRE) job due at that
//! instant, with the periodic
//! [`statuses.expire_sweep`](crate::jobs::statuses::KIND_SWEEP) pass behind
//! it (see [`crate::jobs::statuses`]). And `in_reply_to_id` makes the post
//! an answer to a stored status: the service reads the parent with this
//! route's own [`status_for_viewer_or_404`] rules (so a malformed, unknown,
//! deleted or poster-invisible parent is the same 404 the show route
//! answers, and a reblog row folds to its original) and inherits the
//! thread, author link, capped visibility, fallback language and
//! auto-mentions described in that module. And `media_ids` — decision 10's
//! id-array, JSON's `["a","b"]` or the form's `media_ids[]=a` — posts with
//! attachments: at most [`Config::max_media_attachments`] of them (422
//! `Too many attachments`), each one the poster's own upload no status has
//! claimed (422 `Media not found` for an unknown, foreign or already-used
//! id), claimed for the post on the insert's own transaction at the order
//! the body asked for and answered back in the entity's
//! `media_attachments`. A post that carries at least one attachment may
//! leave its text empty, and attachments ride every visibility, `direct`
//! included. And `in_reply_to_conversation_id` — WBE-123's Pleroma arm —
//! posts into a DM thread by the poster's own
//! [`Participation`](crate::models::conversation::Participation) id: the
//! participation's recipients join its mentions, its conversation becomes
//! the new status's `direct_conversation_id`, and the visibility is forced
//! to `direct` whatever the body asked for; a malformed, unknown or
//! somebody-else's participation id is the same shared 404. And
//! `scheduled_at` — an RFC3339 instant — parks the post instead of writing
//! it: [`crate::services::schedule_status`] runs the very same body checks,
//! then the instance's future window, the account's schedule limits and the
//! media reservation, and stores the normalised request in
//! [`scheduled_statuses`](crate::models::scheduled_status) with one
//! [`scheduled_statuses.publish`](crate::jobs::scheduled_statuses::KIND_PUBLISH)
//! job due at that instant. The answer is **200** with the
//! [`ScheduledStatus`] entity — its `media_attachments` read back from
//! `media_attachments` in the order the body submitted them, though no
//! attachment row is touched — and nothing else happens: no `statuses`
//! row, no `after_status_created` hook, no expiration job, and an
//! `Idempotency-Key` on the request is only echoed into the stored params
//! and the entity — claimed by nobody, here or at the fire, because the
//! publish replays through [`create_status::create_status`] rather than
//! its replayable twin.
//!
//! [`Config::max_media_attachments`]: crate::config::Config::max_media_attachments
//!
//! [`ApiParams`]: crate::extract::ApiParams
//! [`IdempotencyPlug`]: crate::models::idempotency_key
//!
//! # The reads
//!
//! `GET /api/v1/statuses/{id}` is Pleroma's `show`: authentication is
//! optional ([`MaybeAuthUser`], with `read:statuses` enforced on a
//! presented user token through
//! [`require_user_scope`](MaybeAuthUser::require_user_scope)) and the answer
//! is the [`Status`] entity as `viewer` sees it. A malformed id, an id that
//! names no row, a deleted row and a row the visibility model hides from
//! this viewer are one indistinguishable 404
//! `{"error":"Record not found"}`, so the endpoint never tells a stranger
//! that somebody's private post exists. A reblog's id answers the *reblog*
//! entity — the reblogger's own row, with the original nested in `reblog`
//! — which is what the `Announce` arm of Pleroma's `show.json` renders.
//!
//! The batch route is Pleroma's `index`: the same optional auth and scope,
//! the ids read by [`requested_status_ids`] (`id[]`, and the deprecated
//! `ids[]` alias, both also accepted unwrapped), and the answer an array of
//! [`Status`] entities for the ids this viewer may see *in the order they
//! were requested*. Ids that are malformed, unknown, deleted or invisible
//! contribute nothing and are never an error, a request that carries no
//! usable id is the empty array, and only the first [`MAX_STATUS_IDS`] ids
//! are acted on — Pleroma's `limit = 100` over `Enum.take(ids, limit)`,
//! which drops the rest silently rather than refusing the call.
//!
//! The card read is Mastodon's deprecated `GET /api/v1/statuses/{id}/card`,
//! which Pleroma keeps: the same optional auth and scope as the show route
//! and the same [`status_for_viewer_or_404`] gate, and the answer the
//! status's stored rich-media card — the same cached JSON the batched
//! renderer emits into the entity's `card` key — or, for a status that
//! links no card, JSON `null` with 200: having no card is an answer, not
//! a miss, so only the 404 gate can refuse this read.
//!
//! # The context
//!
//! `GET /api/v1/statuses/{id}/context` is Pleroma's `context`
//! (`StatusController.context` over `ActivityPub.fetch_activities_for_context`):
//! the same optional auth and scope as the show route, the same
//! [`status_for_viewer_or_404`] gate on the named status, and the answer
//! `{"ancestors": […], "descendants": […]}` of [`Status`] entities.
//! *Ancestors* are the walk up the named status's `in_reply_to_id` chain —
//! at most [`MAX_CONTEXT_DEPTH`] hops, root first, the named status never —
//! and *descendants* are the walk down through every reply of it, across
//! every branch — at most [`MAX_CONTEXT_DEPTH`] hops and
//! [`MAX_CONTEXT_DESCENDANTS`] rows, in flat chronological thread order
//! (id ascending), the named status never. Both walks run raw and both
//! answers run through [`filter_visible`], so an invisible row is *skipped
//! but walked past*: it drops out of the arrays and the thread on the far
//! side of it still arrives. Descendants lose one more class: statuses by
//! accounts the viewer has blocked or muted, unless the author is the
//! viewer. Each descendant's `pleroma.parent_visible` says whether its own
//! direct parent actually came back in this answer (the named status
//! itself counting as come back), so a client can tell a continuous stretch
//! of thread from one with a hole in it. Both arrays render through the
//! shared [`render_statuses`] batch.
//!
//! # The favourite writes
//!
//! `POST …/favourite` and `POST …/unfavourite` are Pleroma's `favourite`
//! and `unfavourite`: a user token ([`AuthUser`]) with `write:favourites`,
//! both routes riding the `statuses_actions` throttle bucket like the
//! reports route, and both answering the [`Status`] entity as the caller
//! sees it — `favourited` true after a like, false after a withdrawal, and
//! the stepped `favourites_count`, since the row is re-read for the render.
//! Both writes ([`favourite::favourite`] / [`favourite::unfavourite`]) are
//! idempotent on `(account, status)`: a repeat favourite counts once, and
//! unfavouriting a status the caller never liked is the unchanged entity
//! with 200. Favouriting a status the caller may not see is the shared
//! 404, and a reblog's id favourites the *original* (Mastodon's behaviour,
//! which is where Pleroma's `Like` lands too) while the answer stays the
//! reblog entity.
//!
//! # The favourites listings
//!
//! `GET …/favourited_by` is Pleroma's `favourited_by`: optional auth
//! (again `read:statuses` on a presented user token), the same
//! [`status_for_viewer_or_404`] gate on the status, and the answer an
//! array of [`Account`] entities — the accounts that favourited, newest
//! like first — paged by [`PageParams`] keyed on the *favourite row id*
//! (the `Link` cursors carry like ids, not account ids), 20 by default
//! and 80 at most. The author's
//! [`hide_favorites`](crate::models::user::User::hide_favorites) flag
//! hides *their own* favourites list (the Mastodon rule); it never gates
//! this endpoint.
//!
//! `GET /api/v1/favourites` is the caller's own list: [`AuthUser`] with
//! `read:favourites`, the statuses they favourited newest-favourite
//! first, [`PageParams`] again keyed on the favourite row id, and only
//! the statuses still visible to them — the shared [`filter_visible`]
//! step drops what a later block, a visibility change or a delete took
//! out of their view (Pleroma folds the same narrowing into
//! `ActivityPub.fetch_favourites`). A short page ends the walk, and the
//! `Link` header speaks only of the entries that came back.
//!
//! # The reblog writes and the rebloggers listing
//!
//! `POST …/reblog` and `POST …/unreblog` are Pleroma's `reblog` /
//! `unreblog` (`CommonAPI.repeat` / `unrepeat`): a user token
//! ([`AuthUser`]) with `write:statuses`, both routes riding the
//! `statuses_actions` throttle bucket like the favourite writes, and both
//! resolving the named status through the shared
//! [`status_for_viewer_or_404`] — the invisible, unknown and deleted are
//! the same 404 `{"error":"Record not found"}` as ever. The boost is a
//! row of [`crate::models::status`] with `reblog_of_id` set (decision 6),
//! and the mapping is the like's with the roles swapped: **a reblog row's
//! id reblogs (and unreblogs) its original** — nobody boosts a boost —
//! while `POST …/reblog` answers the *reblog entity*, the outer row with
//! the original nested and `reblogged: true`, and `POST …/unreblog`
//! answers the *original* with `reblogged: false`.
//!
//! Two refusals stand behind the gate, both 422 in the ticket's words: a
//! `visibility` parameter outside `public` / `unlisted` / `private`
//! (default `public`, the parameter naming the boost's own visibility,
//! Pleroma's `announce_visibility` trimmed to the three) is
//! `{"error":"Invalid visibility"}`, and a `private` or `direct` target —
//! even the caller's own `direct`, which the visibility model hands them
//! as visible — is `{"error":"Could not reblog private status"}`, since a
//! boost re-broadcasts what it boosts. Both writes are idempotent on the
//! partial unique index over `(user_id, reblog_of_id)`: a repeat reblog
//! restates the same row, count and 200, and a withdrawal that finds
//! nothing is the unchanged original with 200.
//!
//! `GET …/reblogged_by` is Pleroma's `reblogged_by`: optional auth (again
//! `read:statuses` on a presented user token), the same
//! [`status_for_viewer_or_404`] gate — with a reblog row's id answering
//! its original's list — and the answer an array of [`Account`] entities,
//! newest boost first, paged by [`PageParams`] keyed on the *reblog row
//! id* (the `Link` cursors carry boost ids, not account ids).
//!
//! A boost tells the original's author — the `after_reblog_changed` hook
//! lands [`crate::services::notify`]'s reblog telling (and its withdrawal),
//! Pleroma's `Announce` notification riding the same pipeline. The same
//! hook federates the write: [`crate::services::federation_out`] sends the
//! `Announce` the reblog row *is* (and its `Undo`), never for a boost of a
//! `local`-visibility original.
//!
//! # The bookmark writes and the bookmarks listing
//!
//! `POST …/bookmark` and `POST …/unbookmark` are Pleroma's `bookmark`
//! and `unbookmark`: a user token ([`AuthUser`]) with `write:bookmarks`,
//! both answering the [`Status`] entity as the caller sees it —
//! `bookmarked` true after the write, false after the withdrawal, since
//! the row is re-read for the render and the renderer's batched
//! bookmarks read now names the caller. The bookmark write also takes a
//! `folder_id` (body or query): it must name one of the caller's folders
//! ([`crate::models::bookmark_folder`]) or the write is the shared 404,
//! and since [`bookmark::add`] restates the folder on a repeat,
//! re-bookmarking with a different folder *moves* the bookmark — and one
//! naming no folder frees it. Both writes ([`bookmark::add`] /
//! [`bookmark::remove`]) are idempotent on `(account, status)`: a repeat
//! bookmark is one row and one 200, and withdrawing a bookmark the caller
//! never made is the unchanged entity with 200. Bookmarking a status the
//! caller may not see is the shared 404. Unlike a like, a bookmark of a
//! reblog's id lands on *the reblog row itself* — Pleroma's
//! `Bookmark.create` takes the activity the client named, and the flag
//! renders off that same id, the folder with it
//! (`pleroma.bookmark_folder`). And unlike a like, a bookmark federates
//! nothing: it is the account's own private note, Pleroma sends no
//! activity for it, so these routes have no post-write hook to await.
//!
//! `GET /api/v1/bookmarks` is the caller's own list: [`AuthUser`] with
//! `read:bookmarks`, the statuses they bookmarked newest-BOOKMARK first,
//! [`PageParams`] again keyed on the bookmark row id (20 by default, 40
//! at most), and only the statuses still visible to them — the same
//! [`filter_visible`] step as the favourites listing. The optional
//! `folder_id` query parameter narrows the list to that folder's
//! bookmarks ([`bookmark::page_for_user`]); a folder that names none of
//! the caller's rows answers the empty page.
//!
//! # The pin writes
//!
//! `POST …/pin` and `POST …/unpin` are Pleroma's `pin` and `unpin`: a user
//! token ([`AuthUser`]) with `write:accounts` (the scope Pleroma's
//! `StatusController` pipeline names for these two actions, not
//! `write:statuses`), both answering the [`Status`] entity as the caller
//! sees it — `pinned` true and `pleroma.pinned_at` the moment of the write
//! after a pin, both empty after a withdrawal, since the row is re-read for
//! the render and the renderer's batched pins read now names the caller.
//! Behind the shared [`status_for_viewer_or_404`] gate stand Pleroma's three
//! refusals, in its order and with its bodies: a status of somebody else's
//! is 422 `"Someone else's status cannot be pinned"`, a `private`, `direct`
//! or `local` one is 422 `"Non-public status cannot be pinned"` (`public`
//! and `unlisted` pass, as Pleroma's `Visibility.public?/1` admits exactly
//! those two), and a reblog row is 422 `"Reblogs cannot be pinned"` —
//! Pleroma refuses anything but a `Note`, `Article` or `Question` object,
//! which here means a boost has no pin. One more limit is this instance's
//! own: past [`max_pinned_statuses`](crate::config::Config::max_pinned_statuses)
//! the write answers 422 `"You have already pinned the maximum number of
//! statuses"`. A reply may be pinned (Pleroma allows it), anything the
//! caller may not see is the shared 404 before any of these rules run, and
//! re-pinning what is already pinned is the unchanged entity with 200 —
//! Pleroma's `User.add_pinned_object_id/2` skips its limit check for a
//! pin that is already there, and so does this route.
//!
//! `POST …/unpin` keeps Pleroma's two arms: the shared 404 for what the
//! caller may not see, 422 `"Someone else's status cannot be unpinned"` for
//! what they may see but do not own, and — unlike the pin — no visibility
//! or reblog rule, because withdrawing a fact that cannot exist is
//! harmless: [`pinned_status::unpin`] deletes nothing and the answer is the
//! unchanged entity with 200.
//!
//! Neither pin write announces itself to the network except through the
//! `Featured` collection's `Add` and `Remove`, which
//! [`crate::services::federation_out::after_pin_changed`] sends to the
//! pinning account's remote followers — the pair lands in this module's
//! `after_pin_changed` hook.
//!
//! # The thread-mute writes
//!
//! `POST …/mute` and `POST …/unmute` are Pleroma's `mute_conversation` and
//! `unmute_conversation` (`StatusController` over `Pleroma.ThreadMute`,
//! `lib/pleroma/thread_mute.ex`): a user token ([`AuthUser`]) with
//! `write:mutes` (the grant Pleroma's `StatusController` pipeline names for
//! exactly these two actions), both answering the [`Status`] entity as the
//! caller sees it — `muted` true and `pleroma.thread_muted` true after the
//! mute, both false after the withdrawal, since the row is re-read for the
//! render and the renderer's batched thread-mute read now names the caller.
//! The mute is a fact about the status's **thread** (its
//! [`conversation_id`](crate::models::status::Status::conversation_id)),
//! never about the named row alone, and it is stored by
//! [`crate::models::conversation_mute`] — the same table the streaming
//! socket filters on and the notification tickets suppress through.
//!
//! Behind the shared [`status_for_viewer_or_404`] gate stands the
//! participation rule Mastodon requires of these routes: only an account
//! that *is part of* the conversation may mute it — authored a status in it
//! or mentioned in one ([`participates_in`](crate::models::conversation_mute::participates_in),
//! the ticket's two-way query). A caller who is neither answers the same
//! 404 `{"error":"Record not found"}` as an invisible status, so the route
//! never tells a stranger what exists or who posts in a thread. `…/mute`
//! applies the rule; `…/unmute` does not need it — an open thread withdraws
//! to nothing, and [`unmute`](crate::models::conversation_mute::unmute)
//! deletes whatever pair it is given, so both writes are idempotent: a
//! repeat mute is one row and one 200, and a withdrawal that found nothing
//! is the unchanged entity with 200. Neither write federates anything and
//! neither rides a throttle bucket: a thread mute is the account's own
//! private view choice, exactly like a bookmark.
//!
//! A favourite tells the status's author — the `after_favourite_changed`
//! hook lands [`crate::services::notify`]'s favourite telling (and its
//! withdrawal), Pleroma's `Like` notification riding the same pipeline.
//! The same hook federates the write: [`crate::services::federation_out`]
//! sends the `Like` (and its `Undo`) to the status author — and, for a
//! public status, the actor's followers. The reactions'
//! [`crate::api::emoji_reactions`] hook lands the same pair of halves for
//! its `EmojiReact`. The bookmark writes have no such hook to wait
//! for — Pleroma's bookmark is a private row, never an activity.
//!
//! # The translation
//!
//! `POST /api/v1/statuses/{id}/translate` is Pleroma's `translate`: a user
//! token with `read:statuses`, the same [`status_for_viewer_or_404`] gate (so
//! a post the caller may not read is the shared 404, where Pleroma answers its
//! visibility arm 404 too), and the body's `lang` as the target language —
//! falling back to the caller's own `language` and then to `en`. Which
//! provider that asks, and what its call looks like on the wire, is
//! [`crate::services::translate`]'s business; this route adds only the gate,
//! the answer's [`Translation`] entity and the two 503s an instance without a
//! working provider owes: `"Translation is not enabled"` while none is
//! configured, `"Translation service unavailable"` when the configured one
//! fails.
//!
//! # The delete
//!
//! `DELETE /api/v1/statuses/{id}` is Pleroma's `delete`: a user token
//! ([`AuthUser`]) with `write:statuses`, on the same `statuses_actions`
//! bucket the favourite writes ride, and the same
//! [`status_for_viewer_or_404`] gate in front of the row. Then Pleroma's
//! ownership rule: only the author may delete their own post, while an
//! admin or a moderator ([`is_admin`](crate::models::user::User::is_admin)
//! or [`is_moderator`](crate::models::user::User::is_moderator) — its
//! `messages_delete` privilege) may delete anybody's. The two refusals keep
//! Pleroma's two bodies: the shared 404 for a status that is malformed,
//! unknown, deleted or hidden from this caller, and a 403 that carries the
//! same `"Record not found"` wording for one that is visible but somebody
//! else's.
//!
//! The answer is the [`Status`] entity *as it was before the delete*, with
//! the status's raw source text beside it under the top-level `text` key —
//! Mastodon's "delete and redraw" payload, which is what Pleroma's
//! `with_source: true` renders here. What the write leaves is the tombstone
//! of that status and of every live reblog of it, all counters stepped down
//! in one transaction ([`soft_delete_with_reblogs`]); the media attachment
//! rows of the deleted statuses stay in the table, referenced by nothing
//! live, until the cleanup job removes them.
//!
//! What leaves this instance is the withdrawal: a deleted REBLOG row
//! federates the `Undo` of the `Announce` it was and a deleted plain status
//! its `Delete` — the reserved object id as a `Tombstone`, under the
//! addressing the post travelled with — both attached at the deletion
//! service's `after_status_deleted` hook
//! ([`crate::services::delete_status`] — the same landing point the
//! expiration jobs run through), where the notification ticket has wired
//! the cleanup that takes the deleted status's notifications with it. The
//! `status_deleted` stream push still waits for its ticket there.
//!
//! # The edit, its history and its source
//!
//! `PUT /api/v1/statuses/{id}` is Pleroma's `update`
//! (`StatusController.update` over `CommonAPI.update`, whose
//! `make_update_data` re-drafts the post from the submitted parameters on
//! top of the stored object): a user token with `write:statuses`, the
//! shared [`status_for_viewer_or_404`] gate first — the malformed, unknown,
//! deleted and invisible are the module's one 404 — and then the ownership
//! rules, in Pleroma's order: only the author's own **local** post is
//! editable (a mirrored remote post is nobody's to rewrite here, and
//! Pleroma's `Create`-type arm admits no reblog row as its subject), which
//! this route answers with the 403 twin of the `"Record not found"` body
//! the delete route makes; the reblog of one's own post is the edit's one
//! 422, `"Cannot edit a reblog"`, because a boost carries no text of its
//! own to revise. The body — `status`, `spoiler_text`, `sensitive`,
//! `language`, `content_type`, `media_ids` — is merged over the stored row
//! by [`crate::services::create_status::edit_status`] and pushed through
//! the creation's own checks and render (the shared
//! [`render_status_fields`](crate::services::create_status::render_status_fields)),
//! so only the parameters the body presents change and every refusal is
//! the one creation would have answered the submitted value with. The
//! write ([`status_edit::update_status`]) is one transaction that records
//! the superseded version — only when the content, warning, sensitive flag
//! or media actually changed, so an edit of nothing writes no revision and
//! marks nothing — stamps `edited_at`, and replaces the post's mentions,
//! hashtags and attachments. The answer is the [`Status`] entity as the
//! author sees the edited post, `edited_at` set. The route's
//! `after_status_edited` hook already lands
//! [`crate::services::notify`]'s `update` telling of the post's local
//! boosters and the `Update` activity that re-drafts the post for
//! everyone else; the `status.update` stream push awaits the same point.
//!
//! `GET /api/v1/statuses/{id}/history` is Pleroma's `show_history`:
//! optional auth (`read:statuses` on a presented user token), the same
//! status gate, and the answer the [`StatusEdit`]
//! array — the superseded versions oldest-authored-first with the current
//! one last, each carrying its content, warning, flag, author, media, and
//! the empty `emojis` and `poll` placeholders the status renderer still
//! answers. A status nobody edited is the one-element array of its current
//! version.
//!
//! `GET /api/v1/statuses/{id}/source` is Pleroma's `show_source` with the
//! author-only gate this ticket pins: a user token with `read:statuses`,
//! the shared status gate, 403 `"Record not found"` for a visible post of
//! somebody else's, and the answer `{"id", "text", "spoiler_text",
//! "content_type"}` — the stored raw source text and the type it was
//! written in, which is how a client re-edits a post without its HTML.
//!
//! What the reads owe no 422: this module answers an unusable id with the
//! show route's 404 and the batch route's silent skip, and the only
//! request shapes beyond those are the pagination cursors' 400. The create
//! and the edit route do owe them: every refusal
//! [`crate::services::create_status`] documents is their 422 — plus the
//! edit's own `"Cannot edit a reblog"` — on top of the extractor's 401 and
//! the scope gate's 403.
//!
//! [`status_for_viewer_or_404`]: crate::api::helpers::status_for_viewer_or_404
//! [`soft_delete_with_reblogs`]: crate::models::status::soft_delete_with_reblogs
//! [`filter_visible`]: crate::models::status_visibility::filter_visible
//! [`Status`]: crate::entities::status::Status
//! [`ScheduledStatus`]: crate::entities::scheduled_status::ScheduledStatus
//! [`Account`]: crate::entities::account::Account
//! [`Translation`]: crate::entities::translation::Translation

use std::collections::{HashMap, HashSet};

use axum::{
    Json, Router,
    extract::{OriginalUri, Path, State},
    http::{HeaderMap, HeaderName, Uri},
    response::{IntoResponse as _, Response},
    routing::{delete, get, post, put},
};
use serde::{Deserialize, Serialize};
use serde_json::Value;
use tracing::{debug, warn};
use uuid::Uuid;

use crate::{
    api::helpers::{requested_status_ids, status_for_viewer_or_404},
    auth::{AuthUser, MaybeAuthUser},
    config::Config,
    entities::{
        account::{Account, AccountViewer, JsonBool},
        media_attachment::MediaAttachment,
        scheduled_status::ScheduledStatus,
        status::Status,
        status_edit::{StatusEdit, StatusEditSource},
        translation::Translation,
    },
    error::{AppError, AppResult},
    extract::{ApiBody, ApiParams},
    models::{
        block, bookmark, bookmark_folder, conversation_mute,
        favourite::{self, Favourite as FavouriteRow},
        link_card,
        media_attachment::{self, MediaAttachment as AttachmentRow},
        mute,
        oauth_app::{self, OauthApp},
        pinned_status::{self, PinnedStatus},
        status::{self, Status as StatusRow, Visibility},
        status_edit, status_visibility,
        user::{self, User},
    },
    oauth::scopes::{
        READ_BOOKMARKS, READ_FAVOURITES, READ_STATUSES, WRITE_ACCOUNTS, WRITE_BOOKMARKS,
        WRITE_FAVOURITES, WRITE_MUTES, WRITE_STATUSES,
    },
    pagination::{PageParams, link_header, set_link_header},
    services::{
        create_status::{self, CreateStatusParams, EditStatusParams},
        federation_out,
        filter::{self, FilterContext},
        notify,
        render_status::{render_status, render_statuses},
        rich_media::stored_card_json,
        schedule_status,
        translate::Translator,
    },
    state::AppState,
    util::id::parse_id,
};

/// How many ids one batch lookup acts on: Pleroma's `index` `limit`, with
/// every id past it ignored rather than refused.
pub const MAX_STATUS_IDS: usize = 100;

/// How many reply links either walk of `GET …/context` follows.
///
/// The thread depth this endpoint walks, up or down: a chain longer than
/// this reads as if the oldest ancestors (or the deepest replies) did not
/// exist.
pub const MAX_CONTEXT_DEPTH: i32 = 100;

/// How many rows the descendant walk of `GET …/context` collects.
///
/// The cap applies to the raw walk, before any filtering, so a thread
/// wider than this answers its oldest walked rows and drops the rest.
pub const MAX_CONTEXT_DESCENDANTS: i64 = 1000;

/// The 503 body of `POST …/translate` on an instance with no translation
/// provider configured (Pleroma's "Translation service not configured", in
/// the ticket's wording).
const NOT_ENABLED: &str = "Translation is not enabled";
/// The 503 body of `POST …/translate` when a configured provider did not
/// answer, or answered an error: the reason goes to the log, never here.
const UNAVAILABLE: &str = "Translation service unavailable";
/// The language `POST …/translate` translates into when the body names none
/// and the caller's account has no default of its own either.
const DEFAULT_TARGET_LANGUAGE: &str = "en";

/// The body of the delete, edit and source routes' ownership refusals:
/// Pleroma's `StatusController.delete` answers `{:error, :not_found}` for
/// an activity it cannot find, and `CommonAPI.delete`'s ownership arm — the
/// one that [`AppError::Forbidden`] status carries here — renders the same
/// `"Record not found"` wording, so a probe cannot tell "not yours" from
/// "not there". The edit and the source read keep that twin deliberately:
/// Pleroma's `update` refuses a status that is not the caller's own (and a
/// remote one, whose author never is) through the same body, and the
/// ticket pins it for `PUT` and `GET …/source` alike.
const RECORD_NOT_FOUND: &str = "Record not found";

/// The 422 body of `PUT /api/v1/statuses/{id}` for a reblog row: Pleroma's
/// `update` asks `activity.data["type"] == "Create"` before anything else
/// (the ticket's 422 with its own words), and decision 6's boost carries no
/// text of its own — there is nothing to revise, only a post to un-boost.
const CANNOT_EDIT_REBLOG: &str = "Cannot edit a reblog";

/// The 422 body of `POST …/pin` for a status of somebody else's: Pleroma's
/// `CommonAPI.pin` ownership arm (`{:error, :ownership_error}`), which its
/// `StatusController.pin` renders in exactly these words.
const NOT_YOURS: &str = "Someone else's status cannot be pinned";
/// The 422 body of `POST …/unpin` for the same case, Pleroma's `unpin`
/// ownership arm — the withdrawal is refused for the same reason the pin
/// is, and a pin of somebody else's post cannot exist to begin with.
const NOT_YOURS_UNPIN: &str = "Someone else's status cannot be unpinned";
/// The 422 body of `POST …/pin` for a `private`, `direct` or `local`
/// status: Pleroma's `{:error, :non_public_error}` arm, whose wording the
/// ticket pins (its `Visibility.public?/1` admits `public` and `unlisted`
/// and nothing else).
const NOT_PUBLIC: &str = "Non-public status cannot be pinned";
/// The 422 body of `POST …/pin` for a reblog row: Pleroma's
/// `object_type_is_allowed_for_pin` admits an `Announce`'s object as
/// little as a flag's, and the ticket gives the refusal its own words.
/// A reply is allowed, as it is there.
const NO_REBLOG: &str = "Reblogs cannot be pinned";
/// The 422 body of `POST …/pin` past
/// [`max_pinned_statuses`](crate::config::Config::max_pinned_statuses):
/// Pleroma's `:pinned_statuses_limit_reached` arm, whose message its
/// `User.add_pinned_object_id` carries verbatim.
const PIN_LIMIT: &str = "You have already pinned the maximum number of statuses";

/// The 422 body of `POST …/reblog` for a `visibility` parameter outside
/// the three the boost takes: only `public`, `unlisted` and `private` are
/// spellings a reblog can wear, and anything else — `direct` and `local`
/// included — is refused in these words rather than quietly defaulted.
const INVALID_VISIBILITY: &str = "Invalid visibility";
/// The 422 body of `POST …/reblog` for a `private` or `direct` status: the
/// ticket's wording of Pleroma's refusal to `Announce` anything but a
/// publicly-addressed object — a boost is for the followers and the world,
/// never for a followers-only or addressed post, the caller's own included.
///
/// Shared with the C2S outbox's `Announce` arm in
/// [`crate::api::activitypub`], which refuses the same targets in the same
/// words.
pub(crate) const CANNOT_REBLOG: &str = "Could not reblog private status";

/// Page size cap of `favourited_by`: the ticket pins 80, above
/// [`crate::pagination::MAX_LIMIT`], with Pleroma's global 20 as the
/// default the plain [`PageParams::from_query`] keeps.
const FAVOURITED_BY_MAX_LIMIT: i64 = 80;

/// The `Link` headers' query parameters carry nothing but the cursor:
/// none of the favourites and bookmarks listings repeats a filter on
/// either link (the relationship listings' constant, same rule) — the
/// bookmarks listing's `folder_id` is not repeated either, the cursors
/// alone pick up the walk where the page left it.
const NO_EXTRA_QUERY: [(String, String); 0] = [];

/// The `/api/v1` statuses router, merged into [`crate::api::v1`].
///
/// `state` reaches the seven status writes that mount the
/// `statuses_actions` throttle bucket the way the reports route does (see
/// [`crate::rate_limit`]): the create one, the two favourite ones, the two
/// reblog ones and `DELETE /api/v1/statuses/{id}` — Pleroma's
/// `@rate_limited_status_actions` keeps the new post, those state changes
/// and the report filing in one window. The other writes ride no bucket —
/// the two thread-mute ones like the bookmark and pin ones do, because a
/// thread mute is a private per-account view choice no client can hammer on
/// anyone else's data with; the tickets pin the throttle for the create
/// write, the like writes, the boost writes and the delete only, and a
/// bookmark is a private per-account fact — which a pin is too, since
/// `pin_refusal` admits only the caller's own statuses. The edit write is
/// the same kind of own-post-only change (`edit_refusal` admits nothing but
/// the caller's local posts, and the history and source reads behind it are
/// plain reads), and its ticket pins no bucket for it either.
pub fn router(state: &AppState) -> Router<AppState> {
    Router::new()
        .route("/favourites", get(list_favourites))
        .route("/bookmarks", get(list_bookmarks))
        .route("/statuses/{id}", get(show_status))
        .route("/statuses/{id}", put(edit_status))
        .route("/statuses/{id}/card", get(get_status_card))
        .route("/statuses/{id}/context", get(get_status_context))
        .route("/statuses/{id}/history", get(get_status_history))
        .route("/statuses/{id}/source", get(get_status_source))
        .route("/statuses/{id}/quotes", get(list_quotes))
        .route(
            "/statuses/{id}",
            delete(delete_status).layer(crate::rate_limit::rate_limit(state, "statuses_actions")),
        )
        .route("/statuses", get(list_statuses))
        .route(
            "/statuses",
            post(create_status).layer(crate::rate_limit::rate_limit(state, "statuses_actions")),
        )
        .route(
            "/statuses/{id}/favourite",
            post(favourite_status).layer(crate::rate_limit::rate_limit(state, "statuses_actions")),
        )
        .route(
            "/statuses/{id}/unfavourite",
            post(unfavourite_status)
                .layer(crate::rate_limit::rate_limit(state, "statuses_actions")),
        )
        .route("/statuses/{id}/favourited_by", get(list_favourited_by))
        .route(
            "/statuses/{id}/reblog",
            post(reblog_status).layer(crate::rate_limit::rate_limit(state, "statuses_actions")),
        )
        .route(
            "/statuses/{id}/unreblog",
            post(unreblog_status).layer(crate::rate_limit::rate_limit(state, "statuses_actions")),
        )
        .route("/statuses/{id}/reblogged_by", get(list_reblogged_by))
        .route("/statuses/{id}/bookmark", post(bookmark_status))
        .route("/statuses/{id}/unbookmark", post(unbookmark_status))
        .route("/statuses/{id}/pin", post(pin_status))
        .route("/statuses/{id}/unpin", post(unpin_status))
        .route("/statuses/{id}/mute", post(mute_conversation))
        .route("/statuses/{id}/unmute", post(unmute_conversation))
        .route("/statuses/{id}/translate", post(translate_status))
}

/// The `/api/v1/pleroma/statuses` side of the statuses router, nested in
/// [`crate::api::v1`] alongside [`crate::api::emoji_reactions`]'s router.
///
/// Pleroma serves the quoting listing under both prefixes
/// (`lib/pleroma/web/mastodon_api/controllers/status_controller.ex`'s
/// `quotes` action, routed in the mastodon and the pleroma scopes alike),
/// so this one route is the counterpart of the `/api/v1` path's listing.
/// Like the Mastodon mount it is a read and rides no throttle bucket.
pub fn pleroma_router() -> Router<AppState> {
    Router::new().route("/{id}/quotes", get(list_quotes))
}

/// `POST /api/v1/statuses` — create the status the body describes and answer
/// its [`Status`] entity as its author sees it, or — when the body names a
/// `scheduled_at` — park the whole create and answer a [`ScheduledStatus`]
/// entity instead.
///
/// The gate is Pleroma's pipeline for the `create` action:
/// [`AuthUser`] then `write:statuses` then the `statuses_actions` throttle
/// (mounted by [`router`]). Everything after that is
/// [`create_status::create_status`]: the parameters, their defaults and
/// their refusals, the render, the mentions and hashtags, the stored row and
/// the post-write hook. The client behind the presenting token is looked up
/// here because only the request knows it, and it travels to the service as
/// the status's `application` pair.
///
/// Three request-level decisions are this handler's own, all read before the
/// service is asked for anything, in this order. A body naming
/// `scheduled_at` is not a create: [`schedule_status::schedule_status`]
/// checks it with the create's own rules, then the window, the account's
/// schedule limits and the media reservation, stores the normalised request
/// as a pending schedule and queues its fire ([`ScheduledStatus`] answers,
/// with the reserved attachments loaded from their untouched rows in
/// submission order; see [`scheduled_media`]); the request's
/// `Idempotency-Key` rides along as the entity's echo only — a scheduled
/// create claims no replay key, and neither does its publish, which posts
/// through [`create_status::create_status`] like any other fire. The
/// body's `preview` (a [`flex`] boolean, body or query, default false)
/// answers the entity a create would have
/// returned — validated, rendered, mentions and tags resolved, a fresh
/// minted id, `created_at` now and zero counters — without persisting
/// anything, firing a hook or consulting a key ([`create_status::preview_status`],
/// Pleroma's `draft.preview?` arm). An `Idempotency-Key` header makes the
/// create replayable: a key this account has used for a live status answers
/// that status's entity and creates nothing; otherwise the create runs and
/// claims the key on its own transaction
/// ([`create_status::create_status_idempotent`], this instance's durable
/// stand-in for Pleroma's [`IdempotencyPlug`] cache). A preview ignores the
/// header entirely, and a header longer than [`MAX_IDEMPOTENCY_KEY_CHARS`]
/// characters, repeated, or holding non-visible bytes is no key at all —
/// the plain create.
///
/// [`flex`]: crate::extract::flex
/// [`IdempotencyPlug`]: crate::models::idempotency_key
///
/// # Errors
///
/// [`AppError::Unauthorized`] / [`AppError::Forbidden`] from [`AuthUser`]
/// and the `write:statuses` check; [`AppError::RateLimited`] from the
/// bucket; whatever [`create_status::create_status`] refuses — 422 for an
/// empty text over no attachments, an over-long status, an over-long
/// warning, an unknown `visibility`, an unknown `content_type`, more
/// `media_ids` than the instance's attachment limit, a `media_ids` entry
/// the poster may not claim, or an `expires_in` under the instance's floor
/// or past the calendar — plus the extractor's own 422 for an `expires_in`
/// that is no integer — plus its own database
/// failures as [`AppError::Database`]. An `in_reply_to_id` naming no
/// status the poster may see — malformed, unknown, deleted or invisible —
/// is the shared 404 `{"error":"Record not found"}`, exactly the show
/// route's answer for the same id, and so is an
/// `in_reply_to_conversation_id` naming no participation the poster owns.
/// A preview owes the same 422s: validation runs on it exactly as on a
/// post. A scheduled create owes the same body 422s and the same 404s
/// before its own: 422 `Invalid scheduled_at` for text that is no RFC3339
/// instant, 422 naming the instance's future window for a moment inside
/// it (a past one included), 422 `Too many scheduled statuses` for an
/// account at either schedule limit, and 422 `Media not found` for an
/// attachment it may not reserve.
async fn create_status(
    State(state): State<AppState>,
    auth: AuthUser,
    headers: HeaderMap,
    ApiParams(params): ApiParams<CreateStatusParams>,
) -> AppResult<Response> {
    auth.require(WRITE_STATUSES)?;
    let app: Option<OauthApp> = oauth_app::get_by_id(&state.db, auth.1.app_id).await?;
    if params.scheduled_at.is_some() {
        let entity = schedule_create(&state, &auth.0, app.as_ref(), &headers, params).await?;
        return Ok(Json(entity).into_response());
    }
    if params.previews() {
        return Ok(Json(
            create_status::preview_status(&state, &auth.0, app.as_ref(), &params).await?,
        )
        .into_response());
    }
    let entity = match idempotency_key(&headers) {
        Some(key) => {
            create_status::create_status_idempotent(&state, &auth.0, app.as_ref(), params, &key)
                .await?
        }
        None => create_status::create_status(&state, &auth.0, app.as_ref(), params).await?,
    };
    Ok(Json(entity).into_response())
}

/// The `scheduled_at` arm of the create: park the request through
/// [`schedule_status::schedule_status`] — the request's `Idempotency-Key`,
/// if it had one, travels along for the stored params' echo only, claimed
/// by nobody — and finish the [`ScheduledStatus`] entity with the
/// schedule's reserved attachments, read by
/// [`media_attachment::get_many`] and answered in the order the body
/// submitted them (the read is id-ordered; this one is not). No media
/// row is written or moved by this route: the reservation is the pending
/// row's params.
///
/// # Errors
///
/// Those of [`schedule_status::schedule_status`] and any database failure
/// behind the media read, as [`AppError::Database`].
async fn schedule_create(
    state: &AppState,
    user: &User,
    app: Option<&OauthApp>,
    headers: &HeaderMap,
    params: CreateStatusParams,
) -> AppResult<ScheduledStatus> {
    let key = idempotency_key(headers);
    let (mut entity, _row) =
        schedule_status::schedule_status(state, user, app, key.as_deref(), params).await?;
    entity.media_attachments = scheduled_media(state, &entity.params.media_ids).await?;
    Ok(entity)
}

/// The `media_attachments` of a schedule answer: the reserved ids the
/// stored params name, in that submitted order, each rendered through
/// the [`MediaAttachment`] entity from its untouched row; an id whose
/// row has since been deleted simply contributes nothing.
///
/// # Errors
///
/// Any database failure arrives through `?` as [`AppError::Database`].
async fn scheduled_media(state: &AppState, texts: &[String]) -> AppResult<Vec<MediaAttachment>> {
    let ids: Vec<Uuid> = texts.iter().filter_map(|text| parse_id(text)).collect();
    if ids.is_empty() {
        return Ok(Vec::new());
    }
    let rows = media_attachment::get_many(&state.db, &ids).await?;
    let by_id: HashMap<Uuid, AttachmentRow> =
        rows.into_iter().map(|found| (found.id, found)).collect();
    Ok(media_entities(&ids, &by_id, &state.config))
}

/// The header whose value makes a create replayable (Pleroma's
/// `IdempotencyPlug` key, [`crate::models::idempotency_key`]'s table rows).
const IDEMPOTENCY_KEY_HEADER: HeaderName = HeaderName::from_static("idempotency-key");

/// Longest `Idempotency-Key` this endpoint honors: a longer value is ignored
/// (the create is a plain one and no key is stored), so the `key` column
/// never holds more than this many characters and no client can turn the
/// replay lookup into a large-value index probe.
const MAX_IDEMPOTENCY_KEY_CHARS: usize = 255;

/// The `Idempotency-Key` header, read Pleroma's way (its plug's
/// `get_req_header` single-value arm): exactly one visible-text value of at
/// most [`MAX_IDEMPOTENCY_KEY_CHARS`] characters is the key; an absent
/// header, a repeated one, bytes no header text can hold and an over-long
/// value are all no key at all.
fn idempotency_key(headers: &HeaderMap) -> Option<String> {
    let mut values = headers.get_all(IDEMPOTENCY_KEY_HEADER).iter();
    let key = values.next()?.to_str().ok()?.to_owned();
    values
        .next()
        .is_none()
        .then_some(key)
        .filter(|key| key.chars().count() <= MAX_IDEMPOTENCY_KEY_CHARS)
}

/// `GET /api/v1/statuses/{id}` — the status `id` names, as the viewer
/// behind an optional token may see it (see
/// [`status_for_viewer_or_404`] for what that admits) rendered as the
/// [`Status`] entity.
///
/// # Errors
///
/// [`AppError::NotFound`] for a malformed, unknown, deleted or invisible
/// id, and for the one row a render cannot answer (a reblog whose original
/// went away between the visibility read and the batch load);
/// [`AppError::Forbidden`] when a presented user token lacks
/// `read:statuses`; [`AppError::Unauthorized`] from the extractor for a
/// token that does not resolve. Any database failure arrives through `?`
/// as [`AppError::Database`].
async fn show_status(
    State(state): State<AppState>,
    auth: MaybeAuthUser,
    Path(id): Path<String>,
) -> AppResult<Json<Status>> {
    auth.require_user_scope(READ_STATUSES)?;
    let viewer = auth.0.as_ref();
    let row = status_for_viewer_or_404(&state, viewer, &id).await?;
    let rendered = render_status(&state, viewer, &row).await?;
    Ok(Json(rendered.ok_or(AppError::NotFound)?))
}

/// `GET /api/v1/statuses/{id}/card` — the stored rich-media card of the
/// status `id`, or JSON `null` when it has none (Mastodon's deprecated
/// endpoint, which Pleroma keeps).
///
/// The gate is the show route's: optional auth ([`MaybeAuthUser`], with
/// `read:statuses` enforced on a presented user token) and the shared
/// [`status_for_viewer_or_404`], so a malformed, unknown, deleted or
/// invisible id is the module's one 404 and never a `null` that hints at
/// an existing private post. Behind it one [`link_card`] read by the
/// status's own id answers the row the fetch job linked; [`stored_card_json`]
/// renders its cached [`card_json`] output verbatim — the same reading
/// the entity's `card` key uses — and a status that links no card, or
/// links only a failed fetch, answers JSON `null` with 200.
///
/// [`link_card`]: crate::models::link_card
/// [`card_json`]: crate::services::rich_media::card_json
///
/// # Errors
///
/// [`AppError::NotFound`] for a malformed, unknown, deleted or invisible
/// id; [`AppError::Forbidden`] when a presented user token lacks
/// `read:statuses`; [`AppError::Unauthorized`] from the extractor for a
/// token that does not resolve. Any database failure arrives through `?`
/// as [`AppError::Database`].
async fn get_status_card(
    State(state): State<AppState>,
    auth: MaybeAuthUser,
    Path(id): Path<String>,
) -> AppResult<Json<Value>> {
    auth.require_user_scope(READ_STATUSES)?;
    let row = status_for_viewer_or_404(&state, auth.0.as_ref(), &id).await?;
    let cards = link_card::for_statuses(&state.db, &[row.id]).await?;
    debug!(
        status = %row.id,
        linked = cards.contains_key(&row.id),
        "status card read"
    );
    Ok(Json(
        cards
            .get(&row.id)
            .and_then(stored_card_json)
            .unwrap_or(Value::Null),
    ))
}

/// `DELETE /api/v1/statuses/{id}` — delete the status `id` and answer the
/// [`Status`] entity *as it was before the deletion*, with the status's raw
/// source text beside it under the top-level `text` key (Mastodon's "delete
/// and redraw").
///
/// The gate is the shared [`status_for_viewer_or_404`], so a malformed,
/// unknown, already deleted or invisible id is this module's one 404 —
/// Pleroma's `delete` finds no activity and answers `{:error, :not_found}`.
/// Behind it stands Pleroma's ownership rule, [`may_delete`]: one's own post,
/// or anything at all for staff. Somebody else's visible status is the same
/// `"Record not found"` body under a 403.
///
/// The entity renders from the row the gate read, before
/// [`crate::services::delete_status`] turns that row and every live reblog of
/// it into a tombstone; the whole write is one transaction, so a caller that
/// gets a 200 never meets a half-deleted post. A status that another delete
/// took between the gate and the write answers the 404, as does the one row
/// the renderer cannot answer. The expiration jobs run the same service —
/// an expiring post leaves exactly what this route leaves.
///
/// # Errors
///
/// [`AppError::Unauthorized`] / [`AppError::Forbidden`] from [`AuthUser`] and
/// the `write:statuses` check; [`AppError::NotFound`] for a malformed,
/// unknown, deleted or invisible status, for one a concurrent delete reached
/// first, and for the row the render cannot answer;
/// [`AppError::Forbidden`] for a status that is visible but not the caller's;
/// the bucket's 429 before this handler runs. Any database failure arrives
/// through `?` as [`AppError::Database`].
async fn delete_status(
    State(state): State<AppState>,
    auth: AuthUser,
    Path(id): Path<String>,
) -> AppResult<Json<DeletedStatus>> {
    auth.require(WRITE_STATUSES)?;
    let row = status_for_viewer_or_404(&state, Some(&auth.0), &id).await?;
    if !may_delete(&auth.0, &row) {
        return Err(AppError::Forbidden(RECORD_NOT_FOUND.to_owned()));
    }
    let entity = render_status(&state, Some(&auth.0), &row)
        .await?
        .ok_or(AppError::NotFound)?;
    if !crate::services::delete_status::delete_status(&state, &row).await? {
        return Err(AppError::NotFound);
    }
    debug!(
        user = %auth.0.id,
        staff = auth.0.is_admin.get() || auth.0.is_moderator.get(),
        "status deleted over HTTP"
    );
    Ok(Json(DeletedStatus {
        entity,
        text: row.raw_text,
    }))
}

/// Whether `caller` may delete `row`: its author may, and an admin or a
/// moderator may delete anything — Pleroma's
/// `User.privileged?(user, :messages_delete) || user.ap_id == object.data["actor"]`,
/// whose `messages_delete` privilege the two role flags stand in for here.
/// The rest of the gate (does the row exist, is it live, may this caller see
/// it) is [`status_for_viewer_or_404`]'s, which is why a refusal is a 403 on
/// top of the shared 404 rather than one indistinguishable answer.
fn may_delete(caller: &User, row: &StatusRow) -> bool {
    caller.id == row.user_id || caller.is_admin.get() || caller.is_moderator.get()
}

/// The answer of `DELETE /api/v1/statuses/{id}`: the [`Status`] entity as the
/// caller saw it before the row became a tombstone, with the raw source text
/// beside it under the top-level `text` key. That key is Mastodon's
/// delete-and-redraw payload — Pleroma renders the same one from the object's
/// `source` for this action (`with_source: true`) and never on a plain read,
/// which is why it is this response's own shape rather than a field of the
/// shared entity.
#[derive(Debug, Serialize)]
struct DeletedStatus {
    /// The pre-delete entity, its keys at the top level of the response.
    #[serde(flatten)]
    entity: Status,
    /// The status's stored source text ([`raw_text`](StatusRow::raw_text)) as
    /// it was before the delete; empty for a reblog, which carries none.
    text: String,
}

/// `PUT /api/v1/statuses/{id}` — edit the status `id` and answer the
/// [`Status`] entity as its author now sees it, `edited_at` set when the
/// edit superseded a version.
///
/// The gate is the shared [`status_for_viewer_or_404`] and then
/// [`edit_refusal`]: the author's own local post only (403 `"Record not
/// found"` for anything else that is visible), and never a reblog row
/// (422 [`CANNOT_EDIT_REBLOG`]). What the body presents,
/// [`create_status::edit_status`] merges over the stored row and runs
/// through the creation's own checks and render — so every 422 is the one
/// creation answers the submitted value with — and one transaction records
/// the superseded version, writes the new state and replaces the post's
/// mentions, hashtags and attachments.
///
/// # Errors
///
/// [`AppError::Unauthorized`] / [`AppError::Forbidden`] from [`AuthUser`]
/// and the `write:statuses` check; [`AppError::NotFound`] for a malformed,
/// unknown, deleted or invisible status, and for one a delete took between
/// the gate and the write; [`AppError::Forbidden`] for a visible status
/// that is not the caller's own or not local; [`AppError::Unprocessable`]
/// for the reblog row and for every merged-body refusal
/// [`create_status::edit_status`] documents. Any database failure arrives
/// through `?` as [`AppError::Database`].
async fn edit_status(
    State(state): State<AppState>,
    auth: AuthUser,
    Path(id): Path<String>,
    ApiBody(params): ApiBody<EditStatusParams>,
) -> AppResult<Json<Status>> {
    auth.require(WRITE_STATUSES)?;
    let row = status_for_viewer_or_404(&state, Some(&auth.0), &id).await?;
    if let Some(refusal) = edit_refusal(auth.0.id, &row) {
        return Err(refusal);
    }
    let entity = create_status::edit_status(&state, &auth.0, &row, params)
        .await?
        .ok_or(AppError::NotFound)?;
    after_status_edited(&state, &row).await;
    Ok(Json(entity))
}

/// Why `PUT /api/v1/statuses/{id}` refuses `row` for the account
/// `caller_id`, or `None` to let the edit through: behind the shared
/// visibility gate (which already answered the invisible, the unknown and
/// the deleted with the 404) stand Pleroma's ownership arms — the post must
/// be the caller's own (`own_status`) and this instance's own copy of it,
/// since a mirrored remote row belongs to its origin's `Object.edit`
/// pipeline and not to here — and its type arm: a reblog row is not a
/// `Create`, and decision 6 gives it no text of its own to revise. The two
/// refusals keep their ticket's bodies: the 403 twin of the shared
/// `"Record not found"` wording for the ownership one (so the route never
/// says whose post it thinks this is, nor that a remote one exists), the
/// 422 for the reblog.
fn edit_refusal(caller_id: Uuid, row: &StatusRow) -> Option<AppError> {
    if !row.local || row.user_id != caller_id {
        return Some(AppError::Forbidden(RECORD_NOT_FOUND.to_owned()));
    }
    row.reblog_of_id
        .is_some()
        .then(|| AppError::Unprocessable(CANNOT_EDIT_REBLOG.to_owned()))
}

/// `GET /api/v1/statuses/{id}/history` — every version of the status `id`,
/// oldest authored first and the CURRENT one last, as the viewer behind an
/// optional token may see it.
///
/// The gate is the show route's ([`status_for_viewer_or_404`], with
/// `read:statuses` on a presented user token); Pleroma's `show_history`
/// renders the object's `state` list plus the live object the same way, a
/// never-edited status as the one-element array. The superseded versions
/// come from [`status_edit::list_for_status`], the live row supplies the
/// last one — its `edited_at` the moment that version was authored, its
/// creation time before any edit — and the author's [`Account`] entity,
/// rendered as this viewer sees them, names every element. The media each
/// version wore resolves in one batch read ([`history_attachments`]); ids
/// whose attachment row is gone render nothing beside the ones still
/// stored.
///
/// # Errors
///
/// [`AppError::Forbidden`] when a presented user token lacks
/// `read:statuses`; [`AppError::Unauthorized`] from the extractor for a
/// token that does not resolve; [`AppError::NotFound`] for a malformed,
/// unknown, deleted or invisible status, and for the author an account
/// purge left behind (a foreign key the revisions keep, the live row's
/// author this read cannot render without). Any database failure arrives
/// through `?` as [`AppError::Database`].
async fn get_status_history(
    State(state): State<AppState>,
    auth: MaybeAuthUser,
    Path(id): Path<String>,
) -> AppResult<Json<Vec<StatusEdit>>> {
    auth.require_user_scope(READ_STATUSES)?;
    let viewer = auth.0.as_ref();
    let row = status_for_viewer_or_404(&state, viewer, &id).await?;
    let revisions = status_edit::list_for_status(&state.db, row.id).await?;
    let author = user::get_by_id(&state.db, row.user_id)
        .await?
        .ok_or(AppError::NotFound)?;
    let worn = media_attachment::for_statuses(&state.db, &[row.id]).await?;
    let current: Vec<Uuid> = worn
        .get(&row.id)
        .map_or_default(|rows| rows.iter().map(|attachment| attachment.id).collect());
    let attachments = history_attachments(&state, &revisions, &current).await?;
    let mut history = Vec::with_capacity(revisions.len() + 1);
    for revision in &revisions {
        history.push(StatusEdit::from_source(StatusEditSource {
            content: &revision.content,
            spoiler_text: &revision.spoiler_text,
            sensitive: revision.sensitive,
            created_at: revision.created_at,
            account: Account::from_user_for(viewer, &author, &state.config, None),
            media: media_entities(&revision.media_ids, &attachments, &state.config),
        }));
    }
    history.push(StatusEdit::from_source(StatusEditSource {
        content: &row.content,
        spoiler_text: &row.spoiler_text,
        sensitive: row.sensitive,
        created_at: row.edited_at.unwrap_or(row.inserted_at),
        account: Account::from_user_for(viewer, &author, &state.config, None),
        media: media_entities(&current, &attachments, &state.config),
    }));
    debug!(
        status = %row.id,
        versions = history.len(),
        "status history read"
    );
    Ok(Json(history))
}

/// Every attachment one history answer may render: the ids the revisions
/// name plus the ones the live post wears, read in one batch and keyed by
/// id ([`media_attachment::get_many`]). An id whose row has since been
/// deleted is simply absent, and the entry that named it renders one
/// attachment fewer.
///
/// # Errors
///
/// Any database failure arrives through `?` as [`AppError::Database`].
async fn history_attachments(
    state: &AppState,
    revisions: &[status_edit::StatusEdit],
    current: &[Uuid],
) -> AppResult<HashMap<Uuid, AttachmentRow>> {
    let asked: Vec<Uuid> = revisions
        .iter()
        .flat_map(|revision| revision.media_ids.iter().copied())
        .chain(current.iter().copied())
        .collect();
    if asked.is_empty() {
        return Ok(HashMap::new());
    }
    let rows = media_attachment::get_many(&state.db, &asked).await?;
    Ok(rows.into_iter().map(|found| (found.id, found)).collect())
}

/// The `media_attachments` array of one history entry: `ids` in their
/// stored order, each answered by its batched row, and an id the batch
/// could not answer contributing nothing.
fn media_entities(
    ids: &[Uuid],
    by_id: &HashMap<Uuid, AttachmentRow>,
    config: &Config,
) -> Vec<MediaAttachment> {
    ids.iter()
        .filter_map(|id| by_id.get(id))
        .map(|row| MediaAttachment::from_model(row, config))
        .collect()
}

/// `GET /api/v1/statuses/{id}/source` — the source text behind the status
/// `id`, for its author alone.
///
/// The gate is the shared [`status_for_viewer_or_404`] (so the malformed,
/// unknown, deleted and invisible are the module's one 404) and then
/// ownership: Pleroma's `show_source` asks only visibility, and the ticket
/// sharpens that to author-only — a stranger may read a public post's HTML
/// but never its source, and the 403 keeps the `"Record not found"` body
/// the delete and the edit use for the same refusal.
///
/// # Errors
///
/// [`AppError::Unauthorized`] / [`AppError::Forbidden`] from [`AuthUser`]
/// and the `read:statuses` check; [`AppError::NotFound`] for a malformed,
/// unknown, deleted or invisible status; [`AppError::Forbidden`] for a
/// visible status that is not the caller's own. Any database failure
/// arrives through `?` as [`AppError::Database`].
async fn get_status_source(
    State(state): State<AppState>,
    auth: AuthUser,
    Path(id): Path<String>,
) -> AppResult<Json<SourceBody>> {
    auth.require(READ_STATUSES)?;
    let row = status_for_viewer_or_404(&state, Some(&auth.0), &id).await?;
    if row.user_id != auth.0.id {
        return Err(AppError::Forbidden(RECORD_NOT_FOUND.to_owned()));
    }
    Ok(Json(SourceBody {
        id: row.id.to_string(),
        text: row.raw_text,
        spoiler_text: row.spoiler_text,
        content_type: row.content_type,
    }))
}

/// The answer of `GET /api/v1/statuses/{id}/source`: the status's stored
/// source text beside the warning and the `MIME` type that text was
/// written in — Pleroma's `source.json` shape
/// (`StatusView.render("source.json")`), whose `content_type` key
/// Mastodon's own answer lacks and the ticket pins. It carries no render:
/// what the client reads here is exactly what an edit sends back.
#[derive(Debug, Serialize)]
struct SourceBody {
    /// The status id as the hyphenated lowercase `UUIDv7` string
    /// (decision 4).
    id: String,
    /// The stored source text ([`raw_text`](StatusRow::raw_text)).
    text: String,
    /// The stored content warning; `""` when there is none.
    spoiler_text: String,
    /// The `MIME` type [`text`](Self::text) was written in.
    content_type: String,
}

/// `GET /api/v1/statuses?id[]=a&id[]=b` (or `ids[]=`) — the [`Status`]
/// entities of the requested ids this viewer may see, in the requested
/// order, with the malformed, unknown, deleted and invisible ids skipped.
/// No usable id at all is the empty array, as Pleroma's `index` answers it
/// for `ids = []`; only the first [`MAX_STATUS_IDS`] ids are looked up.
///
/// # Errors
///
/// [`AppError::Forbidden`] when a presented user token lacks
/// `read:statuses`; [`AppError::Unauthorized`] from the extractor for a
/// token that does not resolve. Any database failure arrives through `?`
/// as [`AppError::Database`].
async fn list_statuses(
    State(state): State<AppState>,
    auth: MaybeAuthUser,
    uri: Uri,
) -> AppResult<Json<Vec<Status>>> {
    auth.require_user_scope(READ_STATUSES)?;
    let viewer = auth.0.as_ref();
    let requested: Vec<Uuid> = requested_status_ids(uri.query())
        .into_iter()
        .take(MAX_STATUS_IDS)
        .collect();
    if requested.is_empty() {
        return Ok(Json(Vec::new()));
    }
    // One batch read, one batch visibility clause, then the page is put
    // back into the order the client asked in (`get_many` answers id order,
    // `filter_visible` keeps whatever order it was handed).
    let live = status::get_many(&state.db, &requested).await?;
    let visible = status_visibility::filter_visible(&state.db, live, viewer).await?;
    let rows = in_requested_order(&requested, visible);
    Ok(Json(render_statuses(&state, viewer, &rows).await?))
}

/// `GET /api/v1/statuses/{id}/context` — the thread around the status `id`:
/// `{"ancestors": [...], "descendants": [...]}` as the viewer behind an
/// optional token may see it.
///
/// The status resolves through [`status_for_viewer_or_404`] like every
/// other route keyed by id (Pleroma's `context` likewise 404s what
/// `get_by_id` cannot find), and both walks then run from that row's id:
/// [`status::ancestor_chain`] up its `in_reply_to_id` links to the root and
/// [`status::descendant_tree`] down through every reply, both capped at
/// [`MAX_CONTEXT_DEPTH`] hops and the descent additionally at
/// [`MAX_CONTEXT_DESCENDANTS`] rows. Neither walk is filtered as it goes —
/// an invisible link is *skipped but walked past* — and [`filter_visible`]
/// narrows each raw chain to what the viewer may see afterwards, keeping
/// the order the walk produced: ancestors root-first (oldest first) and
/// descendants in flat chronological thread order, exactly like Pleroma's
/// context read. Descendants additionally drop what the viewer's own blocks
/// and mutes silence ([`is_silenced`]), the named status's id counts as
/// shown, and each descendant's `pleroma.parent_visible` is then set to
/// whether its own direct parent came back in this answer
/// ([`mark_invisible_parents`]). Both arrays render through the shared
/// [`render_statuses`] batch and then pass through
/// [`apply_active`](crate::services::filter::apply_active) in the `thread`
/// filter context — the viewer's warning matches arrive annotated, their
/// irreversible matches leave the arrays, and a parent one of those matches
/// removed is what the then-computed `parent_visible` reads as absent.
/// The named status itself is not filtered here: it is the read's anchor,
/// and the single-status route is the filter contexts' business, not this
/// one's.
///
/// # Errors
///
/// [`AppError::Forbidden`] when a presented user token lacks
/// `read:statuses`; [`AppError::Unauthorized`] from the extractor for a
/// token that does not resolve; [`AppError::NotFound`] for a malformed,
/// unknown, deleted or invisible status. Any database failure arrives
/// through `?` as [`AppError::Database`].
async fn get_status_context(
    State(state): State<AppState>,
    auth: MaybeAuthUser,
    Path(id): Path<String>,
) -> AppResult<Json<StatusContext>> {
    auth.require_user_scope(READ_STATUSES)?;
    let viewer = auth.0.as_ref();
    let row = status_for_viewer_or_404(&state, viewer, &id).await?;
    let ancestor_rows = visible_ancestors(&state, viewer, row.id).await?;
    let descendant_rows = visible_descendants(&state, viewer, row.id).await?;
    let rendered_ancestors = render_statuses(&state, viewer, &ancestor_rows).await?;
    let rendered_descendants = render_statuses(&state, viewer, &descendant_rows).await?;
    let ancestors =
        filter::apply_active(&state, viewer, rendered_ancestors, FilterContext::Thread).await?;
    let mut descendants =
        filter::apply_active(&state, viewer, rendered_descendants, FilterContext::Thread).await?;
    mark_invisible_parents(&mut descendants, &ancestors, row.id);
    debug!(
        status = %row.id,
        ancestors = ancestors.len(),
        descendants = descendants.len(),
        "status context read"
    );
    Ok(Json(StatusContext {
        ancestors,
        descendants,
    }))
}

/// The ancestors array's rows: the raw chain above `status_id` (root
/// first), narrowed to what `viewer` may see. The narrowing drops rows
/// without breaking the chain the walk already finished, so the thread
/// beyond an invisible link still arrives.
///
/// # Errors
///
/// Any database failure arrives through `?` as [`AppError::Database`].
async fn visible_ancestors(
    state: &AppState,
    viewer: Option<&User>,
    status_id: Uuid,
) -> AppResult<Vec<StatusRow>> {
    let chain = status::ancestor_chain(&state.db, status_id, MAX_CONTEXT_DEPTH).await?;
    status_visibility::filter_visible(&state.db, chain, viewer).await
}

/// The descendants array's rows: the raw tree below `status_id` (id order),
/// narrowed to what `viewer` may see and then to what their eye still
/// allows. The second step is the one the visibility model does not carry:
/// [`filter_visible`] shuts out an author who blocked the *viewer* (its
/// rule 2), but statuses by accounts the *viewer* blocked or muted are a
/// timeline courtesy, and Pleroma's context read applies it too
/// (`restrict_blocked` over its `blocking_user`) — with the author's own
/// exemption of [`is_silenced`]. An anonymous viewer blocks and mutes
/// nobody, so their answer is the visible tree entire.
///
/// # Errors
///
/// Any database failure arrives through `?` as [`AppError::Database`].
async fn visible_descendants(
    state: &AppState,
    viewer: Option<&User>,
    status_id: Uuid,
) -> AppResult<Vec<StatusRow>> {
    let tree = status::descendant_tree(
        &state.db,
        status_id,
        MAX_CONTEXT_DEPTH,
        MAX_CONTEXT_DESCENDANTS,
    )
    .await?;
    let visible = status_visibility::filter_visible(&state.db, tree, viewer).await?;
    let Some(viewer) = viewer else {
        return Ok(visible);
    };
    let silenced = silenced_authors(state, viewer.id).await?;
    Ok(visible
        .into_iter()
        .filter(|row| !is_silenced(row, viewer.id, &silenced))
        .collect())
}

/// The accounts `viewer_id` has silenced: their blocks unioned with their
/// still-live mutes (an expired mute is no row to [`mute::muted_ids`], so
/// its author is back in the thread), the two reads' lists merged into one
/// membership set.
///
/// # Errors
///
/// Any database failure arrives through `?` as [`AppError::Database`].
async fn silenced_authors(state: &AppState, viewer_id: Uuid) -> AppResult<HashSet<Uuid>> {
    let blocked = block::blocked_ids(&state.db, viewer_id).await?;
    let muted = mute::muted_ids(&state.db, viewer_id).await?;
    Ok(blocked.into_iter().chain(muted).collect())
}

/// Whether `row` is silenced out of the descendants array: its author is in
/// `silenced` and is not the viewer — one's own posts are never hidden from
/// oneself, whatever the edge tables may one day hold.
fn is_silenced(row: &StatusRow, viewer_id: Uuid, silenced: &HashSet<Uuid>) -> bool {
    row.user_id != viewer_id && silenced.contains(&row.user_id)
}

/// Rewrites every descendant's `pleroma.parent_visible` to say whether its
/// own direct parent actually reached this answer: the shown set is the two
/// arrays plus the named status itself (the caller has it, so it is the one
/// parent that can never be the gap). Pleroma computes the same fact with
/// `visible_for_user?` per activity; here the walks already ran, so the
/// answer is membership. A descendant answering a row that was skipped —
/// invisible, silenced, or a tombstone the walk passed through — says
/// `false` so the client can draw the break.
fn mark_invisible_parents(descendants: &mut [Status], ancestors: &[Status], context_id: Uuid) {
    let shown: HashSet<String> = std::iter::once(context_id.to_string())
        .chain(ancestors.iter().map(|status| status.id.clone()))
        .chain(descendants.iter().map(|status| status.id.clone()))
        .collect();
    for status in descendants {
        let parent_shown = status
            .in_reply_to_id
            .as_deref()
            .is_some_and(|parent| shown.contains(parent));
        status.pleroma.parent_visible = JsonBool(parent_shown);
    }
}

/// The answer of `GET /api/v1/statuses/{id}/context`: the thread around the
/// named status as its viewer may see it, both halves rendered
/// [`Status`] entities.
#[derive(Debug, Serialize)]
struct StatusContext {
    /// Everything above the named status, root first — its `in_reply_to_id`
    /// chain, narrowed to what the viewer may see, the named status
    /// excluded.
    ancestors: Vec<Status>,
    /// Everything below it, every branch, in flat chronological thread
    /// order (id ascending) — narrowed the same way and then further by
    /// the viewer's blocks, mutes and the [`MAX_CONTEXT_DESCENDANTS`]
    /// row cap, the named status excluded. Each entry's
    /// `pleroma.parent_visible` says whether its direct parent is in this
    /// answer.
    descendants: Vec<Status>,
}

/// `POST /api/v1/statuses/{id}/favourite` — favourite the status `id` and
/// answer it as the caller sees it: `favourited` true and
/// `favourites_count` stepped, because the row is re-read for the render
/// and the renderer's batched favourites read now names the caller.
///
/// The status resolves through [`status_for_viewer_or_404`] — Pleroma's
/// `CommonAPI.favorite` checks the post is visible to the actor before it
/// builds the `Like`, and what the caller may not see is the same 404 as
/// what does not exist. A reblog row favourites its *original*: Mastodon's
/// behaviour for liking a boost, and the row's id — not the original's —
/// is what the entity answers with. The write is idempotent, so a repeat
/// favourite is one row, one count and one 200.
///
/// # Errors
///
/// [`AppError::Unauthorized`] / [`AppError::Forbidden`] from [`AuthUser`]
/// and the `write:favourites` check; [`AppError::NotFound`] for a
/// malformed, unknown, deleted or invisible status, for the row a delete
/// took between the gate and the re-read, and for the one row the render
/// cannot answer; the bucket's 429 before the handler runs. Any database
/// failure arrives through `?` as [`AppError::Database`].
async fn favourite_status(
    State(state): State<AppState>,
    auth: AuthUser,
    Path(id): Path<String>,
) -> AppResult<Json<Status>> {
    auth.require(WRITE_FAVOURITES)?;
    let row = status_for_viewer_or_404(&state, Some(&auth.0), &id).await?;
    let liked_id = action_target_id(&row);
    if let Some(created) =
        favourite::favourite(&state.db, &state.config.base_url, auth.0.id, liked_id).await?
    {
        debug!(
            status = %liked_id,
            user = %auth.0.id,
            "status favourited"
        );
        after_favourite_changed(&state, &created, true).await;
    }
    current_entity(&state, &auth.0, row.id).await
}

/// `POST /api/v1/statuses/{id}/unfavourite` — withdraw the caller's
/// favourite of the status `id` and answer it as they see it, `favourited`
/// false and `favourites_count` stepped back.
///
/// The same gate and the same reblog mapping as [`favourite_status`], and
/// [`favourite::unfavourite`] is content with nothing to delete: a
/// withdrawal of a favourite that was never made (or already withdrawn)
/// changes nothing and is the unchanged [`Status`] with 200, where
/// Pleroma's `unfavorite` answers its error arm.
///
/// # Errors
///
/// Those of [`favourite_status`]. Any database failure arrives through
/// `?` as [`AppError::Database`].
async fn unfavourite_status(
    State(state): State<AppState>,
    auth: AuthUser,
    Path(id): Path<String>,
) -> AppResult<Json<Status>> {
    auth.require(WRITE_FAVOURITES)?;
    let row = status_for_viewer_or_404(&state, Some(&auth.0), &id).await?;
    let liked_id = action_target_id(&row);
    if let Some(removed) = favourite::unfavourite(&state.db, auth.0.id, liked_id).await? {
        debug!(
            status = %liked_id,
            user = %auth.0.id,
            "favourite withdrawn"
        );
        after_favourite_changed(&state, &removed, false).await;
    }
    current_entity(&state, &auth.0, row.id).await
}

/// `GET /api/v1/statuses/{id}/favourited_by` — the [`Account`] entities
/// that favourited the status `id`, newest like first, one [`PageParams`]
/// page at a time (20 by default, at most 80) with the `Link` header over
/// the *favourite row ids*.
///
/// Auth is optional: a presented user token must grant `read:statuses`
/// and decides whose view the accounts render for, an anonymous request
/// sees the same list. The status resolves through
/// [`status_for_viewer_or_404`], so an invisible post hides its favouriters
/// behind the shared 404 — and the author's `hide_favorites` does *not*:
/// that flag hides the author's own favourites list, not a list of who
/// favourited a post of theirs (Mastodon's rule, and Pleroma's
/// `favourited_by` never consults it).
///
/// # Errors
///
/// [`AppError::Forbidden`] when a presented user token lacks
/// `read:statuses`; [`AppError::Unauthorized`] for a token that does not
/// resolve; [`AppError::NotFound`] for a status this route may not answer;
/// [`AppError::BadRequest`] from the page parameters for a malformed
/// cursor. Any database failure arrives through `?` as
/// [`AppError::Database`], and [`set_link_header`] reports a `Link` value
/// that is not header-safe as [`AppError::Internal`].
async fn list_favourited_by(
    State(state): State<AppState>,
    auth: MaybeAuthUser,
    Path(id): Path<String>,
    OriginalUri(uri): OriginalUri,
) -> AppResult<(HeaderMap, Json<Vec<Account>>)> {
    auth.require_user_scope(READ_STATUSES)?;
    let page = PageParams::from_query(uri.query(), FAVOURITED_BY_MAX_LIMIT)?;
    let row = status_for_viewer_or_404(&state, auth.0.as_ref(), &id).await?;
    let entries = favourite::users_for_status(&state.db, row.id, page).await?;
    debug!(
        status = %row.id,
        favouriters = entries.len(),
        "status favouriters listed"
    );
    let ids: Vec<Uuid> = entries.iter().map(|entry| entry.favourite_id).collect();
    let mut headers = HeaderMap::new();
    set_link_header(
        &mut headers,
        page_link(&state.config.base_url, uri.path(), &ids).as_deref(),
    )?;
    // The list names no relationship of the viewer to the favouriters;
    // Pleroma's `favourited_by` renders plain `show.json` accounts.
    let viewer: AccountViewer<'_> = auth.0.as_ref();
    let accounts = entries
        .iter()
        .map(|entry| Account::from_user_for(viewer, &entry.user, &state.config, None))
        .collect();
    Ok((headers, Json(accounts)))
}

/// `GET /api/v1/favourites` — the statuses the caller favourited, newest
/// favourite first, one [`PageParams`] page at a time with the `Link`
/// header over the *favourite row ids*, and narrowed to what their eye
/// still allows: [`filter_visible`] drops the posts a later block or a
/// visibility change took out of their view, and the renderer answers the
/// rest exactly as any other page.
///
/// # Errors
///
/// [`AppError::Unauthorized`] from [`AuthUser`] without a valid user
/// token; [`AppError::Forbidden`] when the token does not grant
/// `read:favourites`; [`AppError::BadRequest`] from the page parameters
/// for a malformed cursor. Any database failure arrives through `?` as
/// [`AppError::Database`], and [`set_link_header`] reports a `Link` value
/// that is not header-safe as [`AppError::Internal`].
async fn list_favourites(
    State(state): State<AppState>,
    auth: AuthUser,
    page: PageParams,
    OriginalUri(uri): OriginalUri,
) -> AppResult<(HeaderMap, Json<Vec<Status>>)> {
    auth.require(READ_FAVOURITES)?;
    let pairs = favourite::page_for_user(&state.db, auth.0.id, page).await?;
    let rows: Vec<StatusRow> = pairs.iter().map(|(_, row)| row.clone()).collect();
    let visible = status_visibility::filter_visible(&state.db, rows, Some(&auth.0)).await?;
    let visible_ids: HashSet<Uuid> = visible.iter().map(|row| row.id).collect();
    // The cursors key on the like, so they survive the visibility step:
    // only favourites whose status came back visible name the page edges.
    let cursors: Vec<Uuid> = pairs
        .iter()
        .filter(|(favourite, _)| visible_ids.contains(&favourite.status_id))
        .map(|(favourite, _)| favourite.id)
        .collect();
    debug!(
        user = %auth.0.id,
        favourites = visible.len(),
        "favourites listed"
    );
    let mut headers = HeaderMap::new();
    set_link_header(
        &mut headers,
        page_link(&state.config.base_url, uri.path(), &cursors).as_deref(),
    )?;
    Ok((
        headers,
        Json(render_statuses(&state, Some(&auth.0), &visible).await?),
    ))
}

/// `POST /api/v1/statuses/{id}/reblog` — reblog the status `id` and answer
/// the **reblog row's** entity: the outer [`Status`] whose `reblog` is the
/// original and whose `reblogged` is `true`, its nested original carrying
/// the stepped `reblogs_count`.
///
/// The gate is the shared [`status_for_viewer_or_404`] — Pleroma's
/// `CommonAPI.repeat` refuses what the actor may not see, and here the
/// invisible, the unknown and the deleted are the same 404 as ever. Behind
/// it Pleroma's `Announce` arm resolves through: **a reblog row's id
/// reblogs its original**, so nobody boosts a boost and the announcements
/// of a thread of boosts all land on the one post. The refusal the ticket
/// pins behind that is Pleroma's address rule: a `private` or `direct`
/// status — even the caller's own — answers 422 [`CANNOT_REBLOG`].
///
/// The body may name a [`visibility`] for the boost itself (`public`,
/// `unlisted` or `private`; the default is `public`); anything else —
/// `direct`, `local`, any other spelling — is 422 [`INVALID_VISIBILITY`]
/// before anything is written. The write is idempotent on
/// `(user_id, reblog_of_id)`: a repeat reblog is the same row, the same
/// count and the same entity with 200, and the post-write hook runs only
/// when the row was actually made.
///
/// # Errors
///
/// [`AppError::Unauthorized`] / [`AppError::Forbidden`] from [`AuthUser`]
/// and the `write:statuses` check; [`AppError::Unprocessable`] for a bad
/// `visibility` parameter or a `private`/`direct` target;
/// [`AppError::NotFound`] for a malformed, unknown, deleted or invisible
/// status, for an original that vanished under a reblog row mid-flight,
/// for the row a delete took between the gate and the re-read, and for the
/// one row the render cannot answer; the bucket's 429 before the handler
/// runs. Any database failure arrives through `?` as [`AppError::Database`].
async fn reblog_status(
    State(state): State<AppState>,
    auth: AuthUser,
    Path(id): Path<String>,
    ApiBody(params): ApiBody<ReblogParams>,
) -> AppResult<Json<Status>> {
    auth.require(WRITE_STATUSES)?;
    let row = status_for_viewer_or_404(&state, Some(&auth.0), &id).await?;
    let original = reblog_target(&state, &row).await?;
    let visibility = reblog_visibility(params.visibility.as_deref())?;
    if not_rebloggable(&original) {
        return Err(AppError::Unprocessable(CANNOT_REBLOG.to_owned()));
    }
    let (reblog, created) = status::create_reblog(
        &state.db,
        &state.config.base_url,
        &auth.0,
        &original,
        visibility,
    )
    .await?;
    if created {
        debug!(
            status = %original.id,
            user = %auth.0.id,
            visibility = visibility.as_str(),
            "status reblogged"
        );
        after_reblog_changed(&state, &reblog, true).await;
    }
    current_entity(&state, &auth.0, reblog.id).await
}

/// `POST /api/v1/statuses/{id}/unreblog` — withdraw the caller's reblog of
/// the status `id` and answer the **original's** [`Status`] entity,
/// `reblogged` false and `reblogs_count` stepped back, because the row is
/// re-read for the render and the renderer's batched reblogs read no longer
/// names it.
///
/// The same gate and the same reblog-row-through-to-original mapping as
/// [`reblog_status`], and [`status::remove_reblog`] is content with nothing
/// to delete: a withdrawal of a reblog that was never made (or already
/// withdrawn) changes nothing and is the unchanged original with 200 —
/// where Pleroma's `unrepeat` answers its error arm, the ticket gives the
/// restatement the same peace the unfavourite keeps.
///
/// # Errors
///
/// Those of [`reblog_status`] minus the two 422s this route has no reason
/// to ask: withdrawing needs no target visibility and no parameter. Any
/// database failure arrives through `?` as [`AppError::Database`].
async fn unreblog_status(
    State(state): State<AppState>,
    auth: AuthUser,
    Path(id): Path<String>,
) -> AppResult<Json<Status>> {
    auth.require(WRITE_STATUSES)?;
    let row = status_for_viewer_or_404(&state, Some(&auth.0), &id).await?;
    let original_id = action_target_id(&row);
    if let Some(removed) = status::remove_reblog(&state.db, auth.0.id, original_id).await? {
        debug!(
            status = %original_id,
            user = %auth.0.id,
            "reblog withdrawn"
        );
        after_reblog_changed(&state, &removed, false).await;
    }
    current_entity(&state, &auth.0, original_id).await
}

/// `GET /api/v1/statuses/{id}/reblogged_by` — the [`Account`] entities
/// that reblogged the status `id`, newest boost first, one [`PageParams`]
/// page at a time with the `Link` header over the *reblog row ids*.
///
/// Auth is optional: a presented user token must grant `read:statuses` and
/// decides whose view the accounts render for, an anonymous request sees
/// the same list. The status resolves through [`status_for_viewer_or_404`],
/// so an invisible post hides its boosters behind the shared 404, and —
/// Pleroma's `reblogged_by` walking the original object's `announcements`
/// — a reblog row's id answers its original's list.
///
/// # Errors
///
/// [`AppError::Forbidden`] when a presented user token lacks
/// `read:statuses`; [`AppError::Unauthorized`] for a token that does not
/// resolve; [`AppError::NotFound`] for a status this route may not answer;
/// [`AppError::BadRequest`] from the page parameters for a malformed
/// cursor. Any database failure arrives through `?` as
/// [`AppError::Database`], and [`set_link_header`] reports a `Link` value
/// that is not header-safe as [`AppError::Internal`].
async fn list_reblogged_by(
    State(state): State<AppState>,
    auth: MaybeAuthUser,
    Path(id): Path<String>,
    page: PageParams,
    OriginalUri(uri): OriginalUri,
) -> AppResult<(HeaderMap, Json<Vec<Account>>)> {
    auth.require_user_scope(READ_STATUSES)?;
    let row = status_for_viewer_or_404(&state, auth.0.as_ref(), &id).await?;
    let original_id = action_target_id(&row);
    let entries = status::reblogger_page(&state.db, original_id, page).await?;
    debug!(
        status = %original_id,
        rebloggers = entries.len(),
        "status rebloggers listed"
    );
    let ids: Vec<Uuid> = entries.iter().map(|entry| entry.reblog_id).collect();
    let mut headers = HeaderMap::new();
    set_link_header(
        &mut headers,
        page_link(&state.config.base_url, uri.path(), &ids).as_deref(),
    )?;
    // The list names no relationship of the viewer to the rebloggers;
    // Pleroma's `reblogged_by` renders plain `show.json` accounts.
    let viewer: AccountViewer<'_> = auth.0.as_ref();
    let accounts = entries
        .iter()
        .map(|entry| Account::from_user_for(viewer, &entry.user, &state.config, None))
        .collect();
    Ok((headers, Json(accounts)))
}

/// `GET /api/v1/statuses/{id}/quotes` (and its Pleroma twin, mounted by
/// [`pleroma_router`]) — the [`Status`] entities quoting the status `id`,
/// newest quoter first, one [`PageParams`] page at a time with the `Link`
/// header over the *quoter row ids*.
///
/// Auth is optional: a presented user token must grant `read:statuses` and
/// decides whose view the quoters render for, an anonymous request sees the
/// public and unlisted ones. The status resolves through
/// [`status_for_viewer_or_404`], so an invisible post hides its quoters
/// behind the shared 404; [`status::quoting_page`] answers the live quoting
/// rows newest-first and [`filter_visible`] then drops what this viewer may
/// not see — the same narrowing step as [`list_favourites`]'s, keeping the
/// page's order. Being a read, the route rides no throttle bucket.
///
/// # Errors
///
/// [`AppError::Forbidden`] when a presented user token lacks
/// `read:statuses`; [`AppError::Unauthorized`] for a token that does not
/// resolve; [`AppError::NotFound`] for a status this route may not answer;
/// [`AppError::BadRequest`] from the page parameters for a malformed
/// cursor. Any database failure arrives through `?` as
/// [`AppError::Database`], and [`set_link_header`] reports a `Link` value
/// that is not header-safe as [`AppError::Internal`].
async fn list_quotes(
    State(state): State<AppState>,
    auth: MaybeAuthUser,
    Path(id): Path<String>,
    page: PageParams,
    OriginalUri(uri): OriginalUri,
) -> AppResult<(HeaderMap, Json<Vec<Status>>)> {
    auth.require_user_scope(READ_STATUSES)?;
    let viewer = auth.0.as_ref();
    let target = status_for_viewer_or_404(&state, viewer, &id).await?;
    let rows = status::quoting_page(&state.db, target.id, page).await?;
    let visible = status_visibility::filter_visible(&state.db, rows, viewer).await?;
    debug!(
        status = %target.id,
        quoters = visible.len(),
        "status quoters listed"
    );
    // The cursors key on the quoter row itself, so the first and last
    // rendered rows name the page edges directly.
    let ids: Vec<Uuid> = visible.iter().map(|row| row.id).collect();
    let mut headers = HeaderMap::new();
    set_link_header(
        &mut headers,
        page_link(&state.config.base_url, uri.path(), &ids).as_deref(),
    )?;
    Ok((
        headers,
        Json(render_statuses(&state, viewer, &visible).await?),
    ))
}

/// The body of `POST /api/v1/statuses/{id}/reblog`.
#[derive(Debug, Deserialize)]
struct ReblogParams {
    /// The visibility to boost at; absent asks for [`Visibility::Public`]
    /// — see [`reblog_visibility`].
    #[serde(default)]
    visibility: Option<String>,
}

/// The body or query of `POST /api/v1/statuses/{id}/bookmark`.
#[derive(Debug, Deserialize)]
struct BookmarkParams {
    /// The folder to file the bookmark in; it must name one of the caller's
    /// folders or the write is the shared 404 — see [`requested_folder`].
    /// Absent (or JSON `null`) files the bookmark in no folder, which on a
    /// repeat frees it from whichever folder held it.
    #[serde(default)]
    folder_id: Option<String>,
}

/// The query of `GET /api/v1/bookmarks`.
#[derive(Debug, Deserialize)]
struct BookmarksQuery {
    /// Narrow the page to one folder's bookmarks; absent (or malformed) asks
    /// for everything — see [`list_bookmarks`].
    #[serde(default)]
    folder_id: Option<String>,
}

/// The [`Visibility`] `POST …/reblog` boosts at: the body's `visibility`,
/// or `public` when it names none. Only `public`, `unlisted` and `private`
/// are boost visibilities — the ticket's trim of Pleroma's
/// `announce_visibility`, which would also take `direct` and `local` — and
/// every other spelling is 422 [`INVALID_VISIBILITY`] rather than a quiet
/// default, so a client that meant `private` never learns its boost was
/// public by reading the entity back.
fn reblog_visibility(requested: Option<&str>) -> AppResult<Visibility> {
    match requested {
        None | Some("public") => Ok(Visibility::Public),
        Some("unlisted") => Ok(Visibility::Unlisted),
        Some("private") => Ok(Visibility::Private),
        Some(_) => Err(AppError::Unprocessable(INVALID_VISIBILITY.to_owned())),
    }
}

/// The status a reblog lands on: `row` itself, or its **original** when the
/// client named a reblog row — nobody boosts a boost, so the write folds to
/// the post underneath (Pleroma's `repeat` fetching the `Create` behind the
/// id, and the ticket's rule for this route). The row comes back live: rule
/// 7 of [`status_visibility`] admitted the named reblog only because this
/// original is visible to the same caller, and the one way this read finds
/// nothing is a delete in the window between the two — the shared 404.
///
/// # Errors
///
/// [`AppError::NotFound`] for an original that is no longer a live row;
/// any database failure arrives through `?` as [`AppError::Database`].
async fn reblog_target(state: &AppState, row: &StatusRow) -> AppResult<StatusRow> {
    match row.reblog_of_id {
        None => Ok(row.clone()),
        Some(original_id) => {
            let original = status::get_by_id(&state.db, original_id)
                .await?
                .ok_or(AppError::NotFound)?;
            Ok(original)
        }
    }
}

/// Whether `POST …/reblog` must refuse the target: a `private` or `direct`
/// status is addressed to followers or to the mentioned accounts alone,
/// and a boost re-broadcasts it to whoever sees the booster's timeline —
/// Pleroma's Announce of anything but a publicly-addressed object, which
/// the ticket refuses even when the caller is the author (the one case its
/// own visibility model hands back as visible). `public`, `unlisted` and —
/// this instance's extension — `local` pass.
const fn not_rebloggable(original: &StatusRow) -> bool {
    matches!(
        original.visibility,
        Visibility::Private | Visibility::Direct
    )
}

/// `POST /api/v1/statuses/{id}/bookmark` — bookmark the status `id` and
/// answer it as the caller sees it: `bookmarked` true, because the row is
/// re-read for the render and the renderer's batched bookmarks read now
/// names the caller. A `folder_id` (body or query) files the bookmark in
/// that folder, and `pleroma.bookmark_folder` renders it back.
///
/// The status resolves through [`status_for_viewer_or_404`] — what the
/// caller may not see is the same 404 as what does not exist; a `folder_id`
/// that is not the caller's own is the same 404 ([`requested_folder`]). A
/// reblog's id bookmarks *the reblog row itself* (see the [module docs](self)),
/// and the write is idempotent on the pair, so a repeat bookmark is one row
/// and one 200 — its folder restated to whatever the repeat names.
///
/// # Errors
///
/// [`AppError::Unauthorized`] / [`AppError::Forbidden`] from [`AuthUser`]
/// and the `write:bookmarks` check; [`AppError::NotFound`] for a
/// malformed, unknown, deleted or invisible status, for the row a delete
/// took between the gate and the re-read, for the one row the render cannot
/// answer, and for a `folder_id` that is malformed, unknown or foreign. Any
/// database failure arrives through `?` as [`AppError::Database`].
async fn bookmark_status(
    State(state): State<AppState>,
    auth: AuthUser,
    Path(id): Path<String>,
    ApiParams(params): ApiParams<BookmarkParams>,
) -> AppResult<Json<Status>> {
    auth.require(WRITE_BOOKMARKS)?;
    let row = status_for_viewer_or_404(&state, Some(&auth.0), &id).await?;
    // A `folder_id` must name the caller's own folder: an unknown or foreign
    // id is the same 404 the status gate gives, and nothing is written. No
    // `folder_id` files the bookmark in no folder — and, since the write is
    // an upsert on the folder too, re-bookmarking with a different folder
    // moves it (or, with none, frees it).
    let folder_id = requested_folder(&state, auth.0.id, params.folder_id.as_deref()).await?;
    let bookmark = bookmark::add(&state.db, auth.0.id, row.id, folder_id).await?;
    debug!(
        status = %row.id,
        user = %auth.0.id,
        folder = bookmark.folder_id.is_some(),
        "status bookmarked"
    );
    current_entity(&state, &auth.0, row.id).await
}

/// The folder `GET /api/v1/bookmarks` filters by: absent or empty names no
/// folder (the whole list, the `folder_id=` spelling clients leave behind),
/// a well-formed id is the filter whatever it names — an unknown or foreign
/// folder holds none of the caller's bookmarks, and the narrowed walk
/// answers the empty page — and a malformed one is the page cursor's own
/// 400.
///
/// # Errors
///
/// [`AppError::BadRequest`] with `"Invalid id"` for a `folder_id` that is
/// present and not an id.
fn folder_filter(text: Option<&str>) -> AppResult<Option<Uuid>> {
    match text {
        None | Some("") => Ok(None),
        Some(id) => parse_id(id)
            .map(Some)
            .ok_or_else(|| AppError::BadRequest("Invalid id".to_owned())),
    }
}

/// The `folder_id` a bookmark write names, resolved to the caller's folder:
/// absent (or JSON `null`, which the extractor reads as absent) is no
/// folder; a present id must parse and must be a folder this `user_id`
/// owns, or the whole write is the shared 404 — Pleroma's
/// `BookmarkFolder.belongs_to_user?` check demotes a folder it cannot name
/// to no folder; the ticket sharpens that silence into the 404, so a client
/// can tell a filed bookmark from one that quietly was not.
///
/// # Errors
///
/// [`AppError::NotFound`] for a malformed, unknown or foreign folder id; any
/// database failure arrives through `?` as [`AppError::Database`].
async fn requested_folder(
    state: &AppState,
    user_id: Uuid,
    folder_id: Option<&str>,
) -> AppResult<Option<Uuid>> {
    let Some(text) = folder_id else {
        return Ok(None);
    };
    let Some(id) = parse_id(text) else {
        return Err(AppError::NotFound);
    };
    if bookmark_folder::get_owned(&state.db, user_id, id)
        .await?
        .is_none()
    {
        return Err(AppError::NotFound);
    }
    Ok(Some(id))
}

/// `POST /api/v1/statuses/{id}/unbookmark` — withdraw the caller's
/// bookmark of the status `id` and answer it as they see it, `bookmarked`
/// false.
///
/// The same gate and the same row the bookmark write names, and
/// [`bookmark::remove`] is content with nothing to delete: a withdrawal of
/// a bookmark that was never made (or already withdrawn) changes nothing
/// and is the unchanged [`Status`] with 200.
///
/// # Errors
///
/// Those of [`bookmark_status`]. Any database failure arrives through `?`
/// as [`AppError::Database`].
async fn unbookmark_status(
    State(state): State<AppState>,
    auth: AuthUser,
    Path(id): Path<String>,
) -> AppResult<Json<Status>> {
    auth.require(WRITE_BOOKMARKS)?;
    let row = status_for_viewer_or_404(&state, Some(&auth.0), &id).await?;
    if bookmark::remove(&state.db, auth.0.id, row.id)
        .await?
        .is_some()
    {
        debug!(
            status = %row.id,
            user = %auth.0.id,
            "bookmark withdrawn"
        );
    }
    current_entity(&state, &auth.0, row.id).await
}

/// `GET /api/v1/bookmarks` — the statuses the caller bookmarked, newest
/// bookmark first, one [`PageParams`] page at a time (20 by default, 40 at
/// most) with the `Link` header over the *bookmark row ids*, and narrowed
/// to what their eye still allows: the same [`filter_visible`] step as
/// [`list_favourites`].
///
/// The `folder_id` query parameter narrows the walk to one folder
/// ([`bookmark::page_for_user`]'s filter): the walk asks for `None` when no
/// folder is named, and for a *present* id it asks for that folder's
/// bookmarks alone. An id that is well formed but names nothing — unknown,
/// or another account's folder — matches none of the caller's rows and
/// answers the empty page; an empty `folder_id=` asks for nothing and is
/// the same as absent. A malformed folder id is the same 400 a malformed
/// cursor gets.
///
/// # Errors
///
/// [`AppError::Unauthorized`] from [`AuthUser`] without a valid user
/// token; [`AppError::Forbidden`] when the token does not grant
/// `read:bookmarks`; [`AppError::BadRequest`] from the page parameters
/// for a malformed cursor or from [`folder_filter`] for a malformed
/// `folder_id`. Any database failure arrives through `?` as
/// [`AppError::Database`], and [`set_link_header`] reports a `Link` value
/// that is not header-safe as [`AppError::Internal`].
async fn list_bookmarks(
    State(state): State<AppState>,
    auth: AuthUser,
    page: PageParams,
    OriginalUri(uri): OriginalUri,
    ApiParams(query): ApiParams<BookmarksQuery>,
) -> AppResult<(HeaderMap, Json<Vec<Status>>)> {
    auth.require(READ_BOOKMARKS)?;
    let folder_id = folder_filter(query.folder_id.as_deref())?;
    let pairs = bookmark::page_for_user(&state.db, auth.0.id, page, folder_id).await?;
    let rows: Vec<StatusRow> = pairs.iter().map(|(_, row)| row.clone()).collect();
    let visible = status_visibility::filter_visible(&state.db, rows, Some(&auth.0)).await?;
    let visible_ids: HashSet<Uuid> = visible.iter().map(|row| row.id).collect();
    // The cursors key on the bookmark, so they survive the visibility
    // step: only bookmarks whose status came back visible name the edges.
    let cursors: Vec<Uuid> = pairs
        .iter()
        .filter(|(bookmark, _)| visible_ids.contains(&bookmark.status_id))
        .map(|(bookmark, _)| bookmark.id)
        .collect();
    debug!(
        user = %auth.0.id,
        bookmarks = visible.len(),
        "bookmarks listed"
    );
    let mut headers = HeaderMap::new();
    set_link_header(
        &mut headers,
        page_link(&state.config.base_url, uri.path(), &cursors).as_deref(),
    )?;
    Ok((
        headers,
        Json(render_statuses(&state, Some(&auth.0), &visible).await?),
    ))
}

/// `POST /api/v1/statuses/{id}/pin` — pin the caller's own status `id` to
/// their profile and answer it as they see it: `pinned` true and
/// `pleroma.pinned_at` the moment of the write, because the row is re-read
/// for the render and the renderer's batched pins read now names the caller.
///
/// The status resolves through [`status_for_viewer_or_404`] like every other
/// route keyed by id — Pleroma's `CommonAPI.pin` asks
/// `activity_visible_to_actor` first and answers its `:visibility_error`
/// arm as a 404 — and [`pin_refusal`] then applies the three rules behind
/// that gate, whose 422s are this route's own bodies. [`at_pin_limit`] is
/// the last of them, weighed only for a pin that is not already there: a
/// restatement is the unchanged entity with 200 even at the limit, which is
/// what Pleroma's `User.add_pinned_object_id/2` does when the object is
/// already in the map.
///
/// # Errors
///
/// [`AppError::Unauthorized`] / [`AppError::Forbidden`] from [`AuthUser`]
/// and the `write:accounts` check; [`AppError::NotFound`] for a malformed,
/// unknown, deleted or invisible status, for the row a delete took between
/// the gate and the re-read, and for the one row the render cannot answer;
/// [`AppError::Unprocessable`] for one of the four refusals. Any database
/// failure arrives through `?` as [`AppError::Database`].
async fn pin_status(
    State(state): State<AppState>,
    auth: AuthUser,
    Path(id): Path<String>,
) -> AppResult<Json<Status>> {
    auth.require(WRITE_ACCOUNTS)?;
    let row = status_for_viewer_or_404(&state, Some(&auth.0), &id).await?;
    if let Some(refusal) = pin_refusal(auth.0.id, &row) {
        return Err(AppError::Unprocessable(refusal.to_owned()));
    }
    let already_pinned = pinned_status::pinned_ids(&state.db, auth.0.id, &[row.id])
        .await?
        .contains_key(&row.id);
    if !already_pinned && at_pin_limit(&state, auth.0.id).await? {
        return Err(AppError::Unprocessable(PIN_LIMIT.to_owned()));
    }
    if let Some(pinned) = pinned_status::pin(&state.db, auth.0.id, row.id).await? {
        debug!(
            status = %row.id,
            user = %auth.0.id,
            "status pinned"
        );
        after_pin_changed(&state, &pinned, true).await;
    }
    current_entity(&state, &auth.0, row.id).await
}

/// `POST /api/v1/statuses/{id}/unpin` — withdraw the caller's pin of the
/// status `id` and answer it as they see it: `pinned` false and
/// `pleroma.pinned_at` `null`, since the row is re-read for the render and
/// the renderer's batched pins read no longer names it.
///
/// The same [`status_for_viewer_or_404`] gate, then Pleroma's one remaining
/// rule — a pin is a fact about the *author's* profile, so somebody else's
/// visible status is refused with its own body, [`NOT_YOURS_UNPIN`], the
/// same rule [`pin_refusal`] applies under a different wording. What the
/// unpin does **not** ask is the visibility or the reblog rule: neither can
/// describe a stored pin, and Pleroma's `unpin` runs neither.
/// [`pinned_status::unpin`] is content with nothing to delete, so a pin that
/// was never made (or already withdrawn) changes nothing and is the
/// unchanged [`Status`] with 200.
///
/// # Errors
///
/// Those of [`pin_status`] minus the refusals it does not run:
/// [`AppError::Unprocessable`] only for a status the caller does not own.
/// Any database failure arrives through `?` as [`AppError::Database`].
async fn unpin_status(
    State(state): State<AppState>,
    auth: AuthUser,
    Path(id): Path<String>,
) -> AppResult<Json<Status>> {
    auth.require(WRITE_ACCOUNTS)?;
    let row = status_for_viewer_or_404(&state, Some(&auth.0), &id).await?;
    if row.user_id != auth.0.id {
        return Err(AppError::Unprocessable(NOT_YOURS_UNPIN.to_owned()));
    }
    if let Some(removed) = pinned_status::unpin(&state.db, auth.0.id, row.id).await? {
        debug!(
            status = %row.id,
            user = %auth.0.id,
            "pin withdrawn"
        );
        after_pin_changed(&state, &removed, false).await;
    }
    current_entity(&state, &auth.0, row.id).await
}

/// `POST /api/v1/statuses/{id}/mute` — mute the thread the status `id`
/// belongs to and answer that status as the caller sees it: `muted` true
/// and `pleroma.thread_muted` true, because the row is re-read for the
/// render and the renderer's batched thread-mute read now names the caller.
///
/// The status resolves through the shared [`status_for_viewer_or_404`], so
/// a malformed, unknown, deleted or invisible id is the module's one 404 —
/// Pleroma's `mute_conversation` renders its `:not_found` and `:visibility_error`
/// arms alike. Behind that gate stands the participation rule this instance
/// takes from Mastodon: the caller must have authored a status in the
/// conversation or be mentioned in one, and [`participates_in`] answers for
/// both halves in one read. A stranger to the thread gets the same 404 the
/// invisible status gets — the route says nothing about threads the caller
/// is not part of. The write
/// ([`mute`](crate::models::conversation_mute::mute)) is idempotent on
/// `(account, thread)`: a repeat mute is one row, the same flags and one
/// 200. Nothing federates, and the route rides no throttle bucket.
///
/// [`participates_in`]: crate::models::conversation_mute::participates_in
///
/// # Errors
///
/// [`AppError::Unauthorized`] / [`AppError::Forbidden`] from [`AuthUser`]
/// and the `write:mutes` check; [`AppError::NotFound`] for a malformed,
/// unknown, deleted or invisible status, for a caller who is not part of
/// the thread, for the row a delete took between the gate and the re-read,
/// and for the one row the render cannot answer. Any database failure
/// arrives through `?` as [`AppError::Database`].
async fn mute_conversation(
    State(state): State<AppState>,
    auth: AuthUser,
    Path(id): Path<String>,
) -> AppResult<Json<Status>> {
    auth.require(WRITE_MUTES)?;
    let row = status_for_viewer_or_404(&state, Some(&auth.0), &id).await?;
    if !conversation_mute::participates_in(&state.db, auth.0.id, row.conversation_id).await? {
        return Err(AppError::NotFound);
    }
    conversation_mute::mute(&state.db, auth.0.id, row.conversation_id).await?;
    debug!(
        conversation = %row.conversation_id,
        user = %auth.0.id,
        "thread muted"
    );
    current_entity(&state, &auth.0, row.id).await
}

/// `POST /api/v1/statuses/{id}/unmute` — withdraw the caller's mute of the
/// thread the status `id` belongs to and answer that status as they see it:
/// `muted` false and `pleroma.thread_muted` false, since the row is re-read
/// for the render and the renderer's batched thread-mute read no longer
/// names the thread.
///
/// The same [`status_for_viewer_or_404`] gate as [`mute_conversation`], and
/// the same 404 for everything it hides; the participation rule is not
/// re-asked here — Pleroma's `unmute_conversation` asks only visibility, and
/// withdrawing a fact that cannot exist is harmless either way.
/// [`unmute`](crate::models::conversation_mute::unmute) deletes whatever
/// pair it is given and is content with none, so a mute that was never made
/// (or already withdrawn) changes nothing and is the unchanged [`Status`]
/// with 200.
///
/// # Errors
///
/// Those of [`mute_conversation`] minus the participation refusal: an
/// account with no place in the thread can still withdraw a mute nobody
/// else's call ever wrote. Any database failure arrives through `?` as
/// [`AppError::Database`].
async fn unmute_conversation(
    State(state): State<AppState>,
    auth: AuthUser,
    Path(id): Path<String>,
) -> AppResult<Json<Status>> {
    auth.require(WRITE_MUTES)?;
    let row = status_for_viewer_or_404(&state, Some(&auth.0), &id).await?;
    conversation_mute::unmute(&state.db, auth.0.id, row.conversation_id).await?;
    debug!(
        conversation = %row.conversation_id,
        user = %auth.0.id,
        "thread unmuted"
    );
    current_entity(&state, &auth.0, row.id).await
}

/// Why `POST …/pin` refuses `row` for the account `caller_id`, in Pleroma's
/// order and with its bodies: ownership first — a pin is a fact about the
/// author's own profile (`activity_belongs_to_actor`) — then the visibility
/// rule, whose `public` and `unlisted` are the only two [`Visibility`]
/// values Pleroma's `Visibility.public?/1` admits, then the shape rule its
/// `object_type_is_allowed_for_pin` serves by admitting a `Note`, an
/// `Article` and a `Question` and no `Announce`, which here means a reblog
/// row. `None` is the pass, and a reply passes: Pleroma pins one.
///
/// Whether `caller_id` may see `row` at all came before this, at
/// [`status_for_viewer_or_404`] — the shared 404 Pleroma's
/// `:visibility_error` arm renders — which is why an invisible post of
/// somebody else's is never one of these 422s.
fn pin_refusal(caller_id: Uuid, row: &StatusRow) -> Option<&'static str> {
    if row.user_id != caller_id {
        return Some(NOT_YOURS);
    }
    if !matches!(row.visibility, Visibility::Public | Visibility::Unlisted) {
        return Some(NOT_PUBLIC);
    }
    if row.reblog_of_id.is_some() {
        return Some(NO_REBLOG);
    }
    None
}

/// Whether `user_id` has already pinned as many statuses as this instance
/// lets them: their [`count`](pinned_status::count) at or above
/// [`max_pinned_statuses`](crate::config::Config::max_pinned_statuses),
/// Pleroma's `[:instance, :max_pinned_statuses]`.
async fn at_pin_limit(state: &AppState, user_id: Uuid) -> AppResult<bool> {
    Ok(pinned_status::count(&state.db, user_id).await? >= pinned_limit(&state.config))
}

/// The configured pin limit as the `i64` the count read answers with. A
/// limit too wide for `i64` is a limit nobody can reach, so it becomes the
/// largest number the comparison can hold rather than a refusal of the
/// configuration.
fn pinned_limit(config: &Config) -> i64 {
    i64::try_from(config.max_pinned_statuses).unwrap_or(i64::MAX)
}

/// `POST /api/v1/statuses/{id}/translate` — translate the status `id` into
/// the body's `lang` and answer the [`Translation`] entity.
///
/// The status resolves through [`status_for_viewer_or_404`] like every other
/// route keyed by id: Pleroma's `translate` refuses anything but a `public`
/// or `unlisted` post, and here the visibility model's answer is the shared
/// 404, so a private post translation cannot see is also a post that
/// translation cannot say exists. What is translated is the status's HTML
/// `content`, in the language the provider's own protocol offers for it, and
/// the answer's `detected_source_language` is what the provider says the
/// original was written in.
///
/// Without a configured provider the call cannot succeed: 503
/// `{"error":"Translation is not enabled"}`. A provider that is configured
/// but does not answer — transport failure, timeout, or a 4xx/5xx of its own —
/// is 503 `{"error":"Translation service unavailable"}`, with the real reason
/// in the log and never in the body.
///
/// [`Translation`]: crate::entities::translation::Translation
async fn translate_status(
    State(state): State<AppState>,
    auth: AuthUser,
    Path(id): Path<String>,
    ApiBody(params): ApiBody<TranslateParams>,
) -> AppResult<Json<Translation>> {
    auth.require(READ_STATUSES)?;
    let row = status_for_viewer_or_404(&state, Some(&auth.0), &id).await?;
    let translator = Translator::from_config(&state.config, &state.http_client);
    if translator.provider_name().is_none() {
        return Err(AppError::ServiceUnavailable(NOT_ENABLED.to_owned()));
    }
    let target = target_language(params.lang.as_deref(), auth.0.language.as_deref());
    let translated = translator
        .translate(&row.content, row.language.as_deref(), &target)
        .await
        .map_err(|err| {
            warn!(
                %err,
                status = %row.id,
                target = %target,
                "status translation failed"
            );
            AppError::ServiceUnavailable(UNAVAILABLE.to_owned())
        })?;
    debug!(
        status = %row.id,
        provider = translated.provider,
        target = %target,
        "status translated"
    );
    Ok(Json(Translation::for_status(&row, translated)))
}

/// The body of `POST /api/v1/statuses/{id}/translate`.
#[derive(Debug, Deserialize)]
struct TranslateParams {
    /// The language to translate into; absent (or blank) asks for the
    /// caller's own default — see [`target_language`].
    #[serde(default)]
    lang: Option<String>,
}

/// The language `POST …/translate` moves a status into: the body's `lang`,
/// else the caller's account default — the `users.language` column the
/// `Status` entity shows as `source.language` — else
/// [`DEFAULT_TARGET_LANGUAGE`]. That is Pleroma's
/// `Map.get(params, :lang) || user.language` with a fallback for the account
/// that never named a language, since a blank target is no language to ask a
/// provider for; a blank spelling of either is no language either.
fn target_language(requested: Option<&str>, preferred: Option<&str>) -> String {
    let asked = requested.map(str::trim).filter(|lang| !lang.is_empty());
    let default = preferred.map(str::trim).filter(|lang| !lang.is_empty());
    asked
        .or(default)
        .unwrap_or(DEFAULT_TARGET_LANGUAGE)
        .to_owned()
}

/// The one status a write answers: the row `viewer` just changed —
/// a reblog row still answers *itself* — re-read first so the render
/// carries the counter the write stepped, then rendered for them.
///
/// # Errors
///
/// [`AppError::NotFound`] when the row is gone — deleted between the
/// visibility gate and this re-read — and for the one row the renderer
/// answers `None` for (a reblog whose original went away in the same
/// window; [`crate::api::emoji_reactions`] answers it the same way); any
/// database failure arrives through `?` as [`AppError::Database`].
async fn current_entity(state: &AppState, viewer: &User, id: Uuid) -> AppResult<Json<Status>> {
    let row = status::get_by_id(&state.db, id)
        .await?
        .ok_or(AppError::NotFound)?;
    let entity = render_status(state, Some(viewer), &row)
        .await?
        .ok_or(AppError::NotFound)?;
    Ok(Json(entity))
}

/// The `Link` header of a favourites or bookmarks page: [`link_header`]
/// over the first (newest) and last (oldest) like or bookmark row id of
/// the page, the follow lists' [`page_link`](crate::api::accounts) shape
/// with the row id in place of the relationship id's. An empty page names
/// no cursor and links nowhere.
fn page_link(base_url: &str, path: &str, entry_ids: &[Uuid]) -> Option<String> {
    let newest = entry_ids.first()?.to_string();
    let oldest = entry_ids.last()?.to_string();
    link_header(base_url, path, &NO_EXTRA_QUERY, &newest, &oldest)
}

/// The status a like or a boost lands on: `row` itself for a plain post,
/// and its **original** for a reblog row — Mastodon's behaviour for liking
/// a boost, mirrored here for the like and for the reblog write
/// (nobody boosts a boost), and in the renderer's flag lookups, where the
/// same rule keys `favourited` and `reblogged`. The endpoints still answer
/// the row the client named.
///
/// `pub(crate)` because the C2S outbox in [`crate::api::activitypub`] folds
/// its posted `Like`s and `Announce`s with the very same rule.
pub(crate) const fn action_target_id(row: &status::Status) -> Uuid {
    match row.reblog_of_id {
        Some(original) => original,
        None => row.id,
    }
}

/// The `visible` rows in the order `requested` names them, each id answered
/// at most once: an id with no row here (unknown, deleted, invisible)
/// contributes nothing, and one named twice contributes one entry, which is
/// what Pleroma's `WHERE id IN (…)` read does for a duplicated id.
fn in_requested_order(requested: &[Uuid], visible: Vec<status::Status>) -> Vec<status::Status> {
    let mut by_id: HashMap<Uuid, status::Status> =
        visible.into_iter().map(|row| (row.id, row)).collect();
    requested.iter().filter_map(|id| by_id.remove(id)).collect()
}

/// Post-write hook of both favourite routes: the favourite telling of the
/// status's author — and its withdrawal, when a favourite is undone — runs
/// through here, landed from [`notify::on_favourite`] (Pleroma's
/// `CommonAPI.favorite` pipeline runs the same pair), and the federation
/// of the `Like` (and its `Undo`) lands from
/// [`federation_out::after_favourite_changed`]: the activity to send is
/// [`FavouriteRow::ap_id`], the pair it names is [`FavouriteRow::user_id`]
/// and [`FavouriteRow::status_id`], and the queueing happens through
/// `state` (decision 11). The inbound `Like` handler runs it too
/// ([`crate::federation::handlers::interactions`]): a mirrored remote like
/// tells the local author exactly as a local one does, and the like's
/// `Undo` takes that telling back — the federation half answering nothing
/// for it, since a mirror row belongs to its maker's instance.
///
/// `favouriting` states which way the write moved the fact: `true` for the
/// favourite a `POST …/favourite` left behind — a repeat favourite never
/// reaches this hook, since it changed nothing, which is why one like
/// tells once — and `false` for the `Undo` of the favourite a
/// `POST …/unfavourite` removed. A withdrawal that found no row to remove
/// never reaches it either.
///
/// [`notify::on_favourite`]: crate::services::notify::on_favourite
pub(crate) async fn after_favourite_changed(
    state: &AppState,
    favourite: &FavouriteRow,
    favouriting: bool,
) {
    notify::on_favourite(state, favourite, favouriting).await;
    federation_out::after_favourite_changed(state, favourite, favouriting).await;
}

/// Post-write hook of both reblog routes: the federation of the
/// `Announce` activity (and its `Undo`, when a reblog is withdrawn) lands
/// here, through [`federation_out::after_reblog_changed`] — the activity
/// to send is the row's own [`uri`](StatusRow::uri) (a reblog row *is* the
/// `Announce`: decision 6 stores it under `{BASE_URL}/activities/{id}`),
/// the pair it names is the row's [`user_id`](StatusRow::user_id) and its
/// [`reblog_of_id`](StatusRow::reblog_of_id), and the queueing happens
/// through `state` (decision 11).
/// The telling of the original's author — Pleroma's `Announce`
/// notification rides the same `CommonAPI.repeat` pipeline — lands in the
/// same body, from [`notify::on_reblog`]. The inbound `Announce` handler
/// runs it too ([`crate::federation::handlers::interactions`]): a remote
/// boost tells the local author of the boosted post exactly as a local one
/// does, and the boost's `Undo` takes that telling back — the federation
/// half answering nothing, since a mirror boost belongs to its maker's
/// instance.
///
/// `reblog` is the row the write just created or tombstoned: on the
/// withdrawal it is the tombstone, whose `uri` the `Undo` must name.
///
/// `reblogging` states which way the write moved the fact: `true` for the
/// reblog a `POST …/reblog` left behind — a repeat reblog never reaches
/// this hook, since it changed nothing, which is why one boost tells once —
/// and `false` for the `Undo` of the reblog a `POST …/unreblog` removed. A
/// withdrawal that found no row to remove never reaches it either.
///
/// [`notify::on_reblog`]: crate::services::notify::on_reblog
pub(crate) async fn after_reblog_changed(state: &AppState, reblog: &StatusRow, reblogging: bool) {
    notify::on_reblog(state, reblog, reblogging).await;
    federation_out::after_reblog_changed(state, reblog, reblogging).await;
}

/// Post-write hook of both pin routes: announces the featured collection's
/// `Add` or `Remove` through [`federation_out::after_pin_changed`] —
/// Pleroma's `Builder.pin` and `Builder.unpin`, delivered to the pinning
/// account's remote followers (decision 11 queues the fan-out).
///
/// `pinned` is the row the write just stored or removed: its
/// [`user_id`](PinnedStatus::user_id) names the actor whose collection moves
/// and its [`status_id`](PinnedStatus::status_id) the post to add to or
/// withdraw from it.
///
/// `pinning` states which way the write moved the fact: `true` for the pin a
/// `POST …/pin` left behind — a repeat pin never reaches this hook, since it
/// changed nothing — and `false` for the `Remove` of the pin a
/// `POST …/unpin` withdrew. A withdrawal that found no pin to remove never
/// reaches it either.
///
/// Intentionally the federation half alone: unlike the favourite and boost
/// hooks this one has no notification to send, and the routes ride no
/// throttle bucket.
pub(crate) async fn after_pin_changed(state: &AppState, pinned: &PinnedStatus, pinning: bool) {
    federation_out::after_pin_changed(state, pinned, pinning).await;
}

/// Post-write hook of the edit route: the `update` telling of every local
/// account still boosting the edited post — Pleroma's `Update` arm of
/// `get_potential_receiver_ap_ids`, the object's local `Announce` actors
/// minus the author (`lib/pleroma/notification.ex`) — is landed here from
/// [`notify::on_status_edited`], and the federated re-draft of the post
/// itself — the `Update` activity carrying the new note, addressed as the
/// status is — from [`federation_out::after_status_edited`].
///
/// The federated edit handler runs it too
/// ([`crate::federation::handlers::note`]): an inbound `Update` of a
/// mirrored status is an edit like any other, and the boosters of the post
/// are told from either door alike. The announcement of the edit is the
/// local door's alone: a mirrored row belongs to its origin, and
/// [`federation_out::after_status_edited`] says nothing for one.
///
/// The one consumer still waiting wires itself in here: the streaming
/// ticket that pushes the edited status to `status.update`; it ships the
/// re-rendered entity, so it will grow this one call's signature when it
/// lands.
///
/// `row` is the pre-edit stored row: the id every consumer names — the
/// write keeps it — and the author whose boosters the telling reads off
/// the live reblog rows. The federation arm needs no field of this
/// snapshot but that id: it re-reads the row the write left behind, since
/// the document it announces is the post as edited.
///
/// [`notify::on_status_edited`]: crate::services::notify::on_status_edited
pub(crate) async fn after_status_edited(state: &AppState, row: &StatusRow) {
    notify::on_status_edited(state, row).await;
    federation_out::after_status_edited(state, row).await;
}

#[cfg(test)]
mod tests {
    use chrono::{DateTime, Utc};
    use serde_json::{Value, json};
    use uuid::Uuid;

    use super::*;
    use crate::{config::TEST_BASE_URL, models::status::Visibility, util::time::parse_time};

    /// The instant every fixture row in these tests is stamped with.
    fn stamped() -> DateTime<Utc> {
        parse_time("2024-01-31T12:34:56.789Z").expect("the fixture timestamp parses")
    }

    /// A stored row whose only fact of interest is its `id`, so the
    /// ordering helper is tested on the ordering and nothing else.
    fn row(id: Uuid) -> status::Status {
        status::Status {
            id,
            uri: format!("{TEST_BASE_URL}/objects/{id}"),
            url: None,
            user_id: Uuid::nil(),
            content: String::new(),
            raw_text: String::new(),
            content_type: status::DEFAULT_CONTENT_TYPE.to_owned(),
            spoiler_text: String::new(),
            sensitive: false,
            visibility: Visibility::Public,
            language: None,
            in_reply_to_id: None,
            in_reply_to_account_id: None,
            in_reply_to_ap_id: None,
            conversation_id: id,
            ap_context: None,
            direct_conversation_id: None,
            reblog_of_id: None,
            quote_of_id: None,
            local: true,
            application_name: None,
            application_website: None,
            replies_count: 0,
            reblogs_count: 0,
            favourites_count: 0,
            quotes_count: 0,
            edited_at: None,
            deleted_at: None,
            expires_at: None,
            inserted_at: stamped(),
            updated_at: stamped(),
        }
    }

    /// The ids of `rows`, in order.
    fn ids(rows: &[status::Status]) -> Vec<Uuid> {
        rows.iter().map(|row| row.id).collect()
    }

    #[test]
    fn the_answer_follows_the_request_not_the_row_order() {
        let (a, b, c) = (Uuid::now_v7(), Uuid::now_v7(), Uuid::now_v7());
        let visible = vec![row(b), row(c), row(a)];
        assert_eq!(
            ids(&in_requested_order(&[a, b, c], visible)),
            vec![a, b, c],
            "rows the batch read answered in id order come back in the \
             order they were asked for"
        );
    }

    #[test]
    fn unknown_ids_drop_out_and_duplicates_answer_once() {
        let (a, b, gone) = (Uuid::now_v7(), Uuid::now_v7(), Uuid::now_v7());
        let visible = vec![row(b), row(a)];
        assert_eq!(
            ids(&in_requested_order(&[gone, b, a, a], visible)),
            vec![b, a],
            "an id with no visible row contributes nothing, and one asked \
             twice contributes one entry"
        );
    }

    #[test]
    fn nothing_visible_is_the_empty_page() {
        let requested = [Uuid::now_v7()];
        assert!(
            in_requested_order(&requested, Vec::new()).is_empty(),
            "a request whose every id was skipped answers the empty page"
        );
    }

    #[test]
    fn the_link_pages_off_the_first_and_last_like_ids() {
        let (newest, middle, oldest) = (Uuid::now_v7(), Uuid::now_v7(), Uuid::now_v7());
        let link = page_link(
            "https://host",
            "/api/v1/favourites",
            &[newest, middle, oldest],
        )
        .expect("a page with ids links");
        assert_eq!(
            link,
            format!(
                "<https://host/api/v1/favourites?max_id={oldest}>; rel=\"next\", \
                 <https://host/api/v1/favourites?min_id={newest}>; rel=\"prev\""
            ),
            "next walks to the oldest like, prev back to the newest, and \
             the middle id names nothing"
        );
    }

    #[test]
    fn an_empty_page_links_nowhere() {
        assert_eq!(
            page_link("https://host", "/api/v1/favourites", &[]),
            None,
            "no rows, no cursors, no Link header"
        );
    }

    #[test]
    fn a_reblog_like_is_keyed_to_its_target() {
        // The mapping all four writes hand their model: the reblog row's
        // id is what the client named, the original's is where the like —
        // or the boost — lives.
        let (reblog, original) = (Uuid::now_v7(), Uuid::now_v7());
        assert_eq!(
            action_target_id(&row(reblog)),
            reblog,
            "a plain post likes itself"
        );
        let mut boost = row(reblog);
        boost.reblog_of_id = Some(original);
        assert_eq!(
            action_target_id(&boost),
            original,
            "a boost is liked through its original"
        );
    }

    /// The 422 `POST …/reblog` answers for an unusable `visibility`.
    fn invalid_visibility(err: &AppError) -> bool {
        matches!(err, AppError::Unprocessable(message) if message == INVALID_VISIBILITY)
    }

    #[test]
    fn the_visibility_parameter_admits_three_boosts_and_defaults_public() {
        for (asked, wanted) in [
            (None, Visibility::Public),
            (Some("public"), Visibility::Public),
            (Some("unlisted"), Visibility::Unlisted),
            (Some("private"), Visibility::Private),
        ] {
            assert_eq!(
                reblog_visibility(asked).ok(),
                Some(wanted),
                "`{asked:?}` boosts at {wanted}"
            );
        }
    }

    #[test]
    fn any_other_visibility_parameter_is_refused() {
        for asked in [
            Some("direct"),
            Some("local"),
            Some(""),
            Some("Public"),
            Some("unlist"),
        ] {
            let err =
                reblog_visibility(asked).expect_err(&format!("`{asked:?}` is no boost visibility"));
            assert!(
                invalid_visibility(&err),
                "the refusal is 422 Invalid visibility, not {err}"
            );
        }
    }

    #[test]
    fn a_private_or_direct_target_refuses_the_boost() {
        for visibility in Visibility::ALL {
            let mut target = row(Uuid::now_v7());
            target.visibility = visibility;
            assert_eq!(
                not_rebloggable(&target),
                matches!(visibility, Visibility::Private | Visibility::Direct),
                "the boost refuses exactly `private` and `direct`, not `{visibility}`"
            );
        }
    }

    /// The body of a translate request, as `ApiBody` would hand it over:
    /// parsed from the JSON shape the extractor builds for either accepted
    /// content type.
    fn params(body: Value) -> TranslateParams {
        serde_json::from_value(body).expect("the extractor's shape parses")
    }

    #[test]
    fn the_body_carries_one_optional_target_language() {
        assert_eq!(
            params(json!({"lang": "de"})).lang.as_deref(),
            Some("de"),
            "the documented key"
        );
        assert_eq!(
            params(json!({})).lang,
            None,
            "an empty body — which is also what an absent body and a bare              `lang=` form parameter stand for — names no language"
        );
    }

    #[test]
    fn the_asked_language_beats_the_account_default() {
        assert_eq!(
            target_language(Some("eo"), Some("de")),
            "eo",
            "the body's `lang` wins"
        );
        assert_eq!(
            target_language(None, Some("de")),
            "de",
            "without it the caller's own default"
        );
        assert_eq!(
            target_language(Some(" fr "), Some("de")),
            "fr",
            "and a padded spelling is trimmed, not rejected"
        );
    }

    #[test]
    fn english_is_the_last_resort() {
        for (asked, preferred) in [
            (None, None),
            (Some(""), None),
            (Some("   "), Some("")),
            (None, Some("  ")),
        ] {
            assert_eq!(
                target_language(asked, preferred),
                "en",
                "a blank is no language to ask a provider for, so `{asked:?}`                  with `{preferred:?}` falls back to English"
            );
        }
    }

    /// `row(id)` as `author` wrote it, public: the row the pin gate asks
    /// about, before a test narrows it to the case it names.
    fn authored(id: Uuid, author: Uuid) -> status::Status {
        let mut own = row(id);
        own.user_id = author;
        own
    }

    #[test]
    fn your_own_public_or_unlisted_post_is_the_one_a_pin_admits() {
        let owner = Uuid::now_v7();
        let own = authored(Uuid::now_v7(), owner);
        assert_eq!(pin_refusal(owner, &own), None, "a public post of yours");
        let mut quiet = authored(Uuid::now_v7(), owner);
        quiet.visibility = Visibility::Unlisted;
        assert_eq!(pin_refusal(owner, &quiet), None, "and an unlisted one");
    }

    #[test]
    fn a_post_that_is_not_public_has_no_pin() {
        let owner = Uuid::now_v7();
        for visibility in [Visibility::Private, Visibility::Direct, Visibility::Local] {
            let mut hidden = authored(Uuid::now_v7(), owner);
            hidden.visibility = visibility;
            assert_eq!(
                pin_refusal(owner, &hidden),
                Some(NOT_PUBLIC),
                "Pleroma's `Visibility.public?/1` admits neither `{visibility}`"
            );
        }
    }

    #[test]
    fn ownership_is_asked_before_anything_else() {
        // Somebody else's *private* status is refused as somebody else's: the
        // visibility rule is the author's own business, and Pleroma's `pin`
        // pipeline asks `activity_belongs_to_actor` first.
        let (owner, caller) = (Uuid::now_v7(), Uuid::now_v7());
        let mut theirs = authored(Uuid::now_v7(), owner);
        theirs.visibility = Visibility::Private;
        assert_eq!(pin_refusal(caller, &theirs), Some(NOT_YOURS));
        theirs.visibility = Visibility::Public;
        assert_eq!(
            pin_refusal(caller, &theirs),
            Some(NOT_YOURS),
            "and a visible post of theirs is still not yours to pin"
        );
    }

    #[test]
    fn a_reblog_has_no_pin_and_a_reply_still_has_one() {
        let owner = Uuid::now_v7();
        let mut boost = authored(Uuid::now_v7(), owner);
        boost.reblog_of_id = Some(Uuid::now_v7());
        assert_eq!(pin_refusal(owner, &boost), Some(NO_REBLOG));

        // The shape rule comes after the visibility rule, and a reply is
        // untouched by either: Pleroma pins one.
        let mut hidden_boost = authored(Uuid::now_v7(), owner);
        hidden_boost.visibility = Visibility::Direct;
        hidden_boost.reblog_of_id = Some(Uuid::now_v7());
        assert_eq!(pin_refusal(owner, &hidden_boost), Some(NOT_PUBLIC));

        let mut answer = authored(Uuid::now_v7(), owner);
        answer.in_reply_to_id = Some(Uuid::now_v7());
        answer.in_reply_to_account_id = Some(Uuid::now_v7());
        assert_eq!(pin_refusal(owner, &answer), None, "a reply is pinnable");
    }

    #[test]
    fn the_pin_limit_reaches_the_count_read_in_its_own_type() {
        let mut config = Config::for_tests();
        assert_eq!(pinned_limit(&config), 1, "the shipped default");
        config.max_pinned_statuses = 5;
        assert_eq!(pinned_limit(&config), 5, "a configured number");
        config.max_pinned_statuses = usize::MAX;
        assert_eq!(
            pinned_limit(&config),
            i64::MAX,
            "a limit too wide for the count's own type is no limit anyone \
             can reach"
        );
    }

    /// The descendants array's silencing step: rows by an author the viewer
    /// blocked or muted drop, the viewer's own rows stay even when the set
    /// (defensively) names them, and everybody else's stay.
    #[test]
    fn only_silenced_other_authors_leave_the_descendants() {
        let (viewer, silenced_author, other) = (Uuid::now_v7(), Uuid::now_v7(), Uuid::now_v7());
        let silenced: HashSet<Uuid> = [silenced_author, viewer].into_iter().collect();

        let mut theirs = row(Uuid::now_v7());
        theirs.user_id = silenced_author;
        assert!(
            is_silenced(&theirs, viewer, &silenced),
            "a post by an account the viewer silenced"
        );

        theirs.user_id = viewer;
        assert!(
            !is_silenced(&theirs, viewer, &silenced),
            "the viewer's own post stays even when the set names them"
        );

        let mut foreign = row(Uuid::now_v7());
        foreign.user_id = other;
        assert!(
            !is_silenced(&foreign, viewer, &silenced),
            "an author nobody silenced stays"
        );
    }

    /// The 403 `PUT …` answers for a status that is visible but not the
    /// caller's own local post: the shared `"Record not found"` wording.
    fn forbidden(refusal: Option<AppError>) -> bool {
        matches!(
            refusal,
            Some(AppError::Forbidden(message)) if message == RECORD_NOT_FOUND
        )
    }

    /// The 422 `PUT …` answers for a reblog row of the caller's own.
    fn reblog_refused(refusal: Option<AppError>) -> bool {
        matches!(
            refusal,
            Some(AppError::Unprocessable(message)) if message == CANNOT_EDIT_REBLOG
        )
    }

    #[test]
    fn your_own_local_post_is_the_one_an_edit_admits() {
        let owner = Uuid::now_v7();
        assert!(
            edit_refusal(owner, &authored(Uuid::now_v7(), owner)).is_none(),
            "a local post of yours is editable"
        );
        let mut answer = authored(Uuid::now_v7(), owner);
        answer.in_reply_to_id = Some(Uuid::now_v7());
        assert!(
            edit_refusal(owner, &answer).is_none(),
            "and so is a reply of yours"
        );
    }

    #[test]
    fn a_foreign_or_remote_post_is_forbidden_in_the_shared_wording() {
        let (owner, caller) = (Uuid::now_v7(), Uuid::now_v7());
        assert!(
            forbidden(edit_refusal(caller, &authored(Uuid::now_v7(), owner))),
            "a visible post of somebody else's is the 403 twin of the 404"
        );
        let mut mirrored = authored(Uuid::now_v7(), caller);
        mirrored.local = false;
        assert!(
            forbidden(edit_refusal(caller, &mirrored)),
            "a row this instance does not own is nobody's to edit here"
        );
    }

    #[test]
    fn a_reblog_refuses_the_edit_with_its_own_words_after_ownership() {
        let owner = Uuid::now_v7();
        let mut boost = authored(Uuid::now_v7(), owner);
        boost.reblog_of_id = Some(Uuid::now_v7());
        assert!(
            reblog_refused(edit_refusal(owner, &boost)),
            "your own boost has no text of its own to revise"
        );
        assert!(
            forbidden(edit_refusal(Uuid::now_v7(), &boost)),
            "and somebody else's boost is not yours before it is a boost"
        );
    }

    /// A header map carrying `values` under the idempotency header.
    fn keyed(values: &[&str]) -> HeaderMap {
        let mut headers = HeaderMap::new();
        for value in values {
            let _added: bool = headers.append(
                IDEMPOTENCY_KEY_HEADER,
                axum::http::HeaderValue::from_str(value).expect("a visible test value"),
            );
        }
        headers
    }

    #[test]
    fn exactly_one_idempotency_key_header_is_the_key() {
        assert!(idempotency_key(&keyed(&[])).is_none(), "absent is none");
        assert_eq!(
            idempotency_key(&keyed(&["k-1"])).as_deref(),
            Some("k-1"),
            "one value is the key, verbatim"
        );
        assert_eq!(
            idempotency_key(&keyed(&[""])).as_deref(),
            Some(""),
            "the empty value is a key like any other text — Pleroma's plug \
             caches the header's spelling without asking what it means"
        );
        assert!(
            idempotency_key(&keyed(&["a", "b"])).is_none(),
            "a repeated header names no single key and is no key at all"
        );
    }

    #[test]
    fn the_key_cap_is_255_characters_and_longer_is_ignored() {
        let edge = "x".repeat(MAX_IDEMPOTENCY_KEY_CHARS);
        assert_eq!(
            idempotency_key(&keyed(&[&edge])).as_deref(),
            Some(edge.as_str()),
            "the cap itself is a key"
        );
        let over = "x".repeat(MAX_IDEMPOTENCY_KEY_CHARS + 1);
        assert!(
            idempotency_key(&keyed(&[&over])).is_none(),
            "one character past the cap is ignored, not truncated: the \
             create runs as if no header had arrived"
        );
    }

    #[test]
    fn the_preview_flag_is_the_body_s_own_default() {
        let absent = serde_json::from_value::<CreateStatusParams>(json!({"status": "hi"}))
            .expect("the documented shape parses");
        assert!(!absent.previews(), "no key means a real post");
        for value in [json!(true), json!("true"), json!("1"), json!("on")] {
            let body = serde_json::from_value::<CreateStatusParams>(
                json!({"status": "hi", "preview": value}),
            )
            .unwrap_or_else(|_| panic!("`{value}` is a flex-bool spelling"));
            assert!(body.previews(), "`{value}` previews");
        }
        for value in [json!(false), json!("0"), json!("off"), Value::Null] {
            let body = serde_json::from_value::<CreateStatusParams>(
                json!({"status": "hi", "preview": value}),
            )
            .unwrap_or_else(|_| panic!("`{value}` is a flex-bool spelling"));
            assert!(!body.previews(), "`{value}` is as absent as no key");
        }
    }
}
