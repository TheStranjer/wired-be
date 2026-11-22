//! Fixtures the relationship model tests share: local, locked and remote
//! users and the one-target relationship read.

use sqlx::PgPool;

use super::*;
use crate::{
    config::TEST_BASE_URL,
    models::{
        follow::{self, FollowToggles},
        user::{Flag, NewUser, User, insert, local_ap_id},
    },
    util::id::new_id,
};

/// The two per-follow toggles [`set_toggles`] stores, passed as a
/// struct so the helper does not read as a pair of switch parameters.
#[derive(Debug)]
pub(super) struct Toggles {
    /// Value to store in `show_reblogs`.
    pub(super) show_reblogs: bool,
    /// Value to store in `notify`.
    pub(super) notify: bool,
}

/// Inserts a local, unlocked user named `nickname`.
pub(super) async fn local_user(pool: &PgPool, nickname: &str) -> User {
    insert(
        pool,
        &NewUser::local(nickname, &local_ap_id(TEST_BASE_URL, nickname)),
    )
    .await
    .expect("the local fixture inserts")
}

/// Inserts a local user named `nickname` whose account is locked, so a
/// follow of it waits for an answer.
pub(super) async fn locked_user(pool: &PgPool, nickname: &str) -> User {
    insert(
        pool,
        &NewUser {
            locked: Flag::new(true),
            ..NewUser::local(nickname, &local_ap_id(TEST_BASE_URL, nickname))
        },
    )
    .await
    .expect("the locked fixture inserts")
}

/// Inserts a mirror of the remote actor `nickname` on `host`: the
/// non-local row a fetched actor leaves behind (decision 7), with its
/// host in `domain` — the column the domain-block join reads.
pub(super) async fn remote_user(pool: &PgPool, nickname: &str, host: &str) -> User {
    insert(
        pool,
        &NewUser {
            nickname: format!("{nickname}@{host}"),
            ap_id: format!("https://{host}/users/{nickname}"),
            domain: Some(host.to_owned()),
            name: nickname.to_owned(),
            ..NewUser::default()
        },
    )
    .await
    .expect("the remote fixture inserts")
}

/// The relationship [`Relationship::load_many`] answers for `viewer`
/// to `target`, which the test expects to be the only row out.
pub(super) async fn one_relationship(pool: &PgPool, viewer: &User, target: &User) -> Relationship {
    let loaded = Relationship::load_many(pool, viewer.id, &[target.id])
        .await
        .unwrap();
    assert_eq!(loaded.len(), 1, "one target in answers one row out");
    let relationship = loaded.first().expect("the batch answered one row");
    assert_eq!(
        relationship.id,
        target.id.to_string(),
        "the row is about the requested target"
    );
    relationship.clone()
}

/// Stores `toggles` on the `viewer → target` row through the model's own
/// writer, [`follow::set_toggles`] — the write Mastodon's
/// `POST /api/v1/accounts/{id}/follow` puts its `reblogs` / `notify`
/// parameters through. [`follow::follow`] itself only ever writes the
/// column defaults, so a fixture that wants the non-default side of a
/// toggle goes this way.
pub(super) async fn set_toggles(pool: &PgPool, viewer: &User, target: &User, toggles: Toggles) {
    let stored = follow::set_toggles(
        pool,
        viewer.id,
        target.id,
        FollowToggles {
            show_reblogs: Some(toggles.show_reblogs),
            notify: Some(toggles.notify),
        },
    )
    .await
    .unwrap();
    assert!(
        stored.is_some(),
        "the fixture's open row is there to update"
    );
}

/// Hides reblogs and turns notifications on: the non-default side of
/// both toggles the loader must read off the viewer's row.
pub(super) fn flipped() -> Toggles {
    Toggles {
        show_reblogs: false,
        notify: true,
    }
}

/// Writes a `rejected` row by hand: [`follow::reject`] deletes the row
/// rather than storing that state, so only a mirrored remote rejection
/// produces one (see the [`crate::models::follow`] docs).
pub(super) async fn rejected_relation(pool: &PgPool, follower: &User, target: &User) {
    let written = sqlx::query(
        "INSERT INTO follows (id, follower_id, followee_id, state)
         VALUES ($1, $2, $3, 'rejected')",
    )
    .bind(new_id())
    .bind(follower.id)
    .bind(target.id)
    .execute(pool)
    .await
    .unwrap();
    assert_eq!(written.rows_affected(), 1, "the rejected fixture row lands");
}

/// Asserts the flags no source has computed (yet) keep their defaults:
/// the constants the ticket promises until each flag's own ticket
/// stores its data. `note` is computed now, but these fixtures store
/// no note, so its source answers the empty default here too;
/// `domain_blocking` is computed as well, and its dedicated fixtures
/// below test it — these tests pair viewers with local targets, which
/// that flag can never light up for; `endorsed` is computed too, and
/// its dedicated fixture below tests it — these tests record no
/// endorsement, so the flag keeps its default; `blocking` and
/// `blocked_by` are computed as well, and their dedicated fixture
/// below tests them — these tests record no block, so both flags keep
/// their defaults; `muting` and `muting_notifications` are computed as
/// well, and their dedicated fixture below tests them — these tests
/// record no mute, so both flags keep their defaults.
pub(super) fn assert_placeholder_flags(relationship: &Relationship) {
    assert!(!relationship.endorsed.0, "no endorsement recorded");
    assert!(!relationship.blocking.0, "no block recorded, outgoing");
    assert!(!relationship.blocked_by.0, "no block recorded, incoming");
    assert!(!relationship.muting.0, "no mute recorded");
    assert!(
        !relationship.muting_notifications.0,
        "no notification mute recorded"
    );
    assert!(
        relationship.note.is_empty(),
        "no stored note reads back empty"
    );
}
