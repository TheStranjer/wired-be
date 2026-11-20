//! The sentences: the nickname and mention lists and every message the
//! entries store, spelled word for word as Pleroma's
//! `get_log_entry_message` clauses spell them.

use uuid::Uuid;

use crate::models::user::User;

/// The `@alice, @bob` list Pleroma's `users_to_nicknames_string` and
/// `nicknames_to_string` build.
pub(super) fn mention_list(nicknames: &[String]) -> String {
    nicknames
        .iter()
        .map(|nickname| format!("@{nickname}"))
        .collect::<Vec<String>>()
        .join(", ")
}

/// The nicknames of `users`, in order.
pub(super) fn nicknames_of(users: &[User]) -> Vec<String> {
    users.iter().map(|user| user.nickname.clone()).collect()
}

/// `"{actor} {verb} users: {mentions}"` — the five user-list sentences.
pub(super) fn users_sentence(actor: &str, verb: &str, nicknames: &[String]) -> String {
    format!("@{actor} {verb} users: {}", mention_list(nicknames))
}

/// `"{actor} added tags: {tags} to users: {mentions}"`.
pub(super) fn tag_sentence(actor: &str, tags: &[String], nicknames: &[String]) -> String {
    format!(
        "@{actor} added tags: {} to users: {}",
        tags.join(", "),
        mention_list(nicknames)
    )
}

/// `"{actor} removed tags: {tags} from users: {mentions}"`.
pub(super) fn untag_sentence(actor: &str, tags: &[String], nicknames: &[String]) -> String {
    format!(
        "@{actor} removed tags: {} from users: {}",
        tags.join(", "),
        mention_list(nicknames)
    )
}

/// `"{actor} made {mentions} {permission}"`.
pub(super) fn grant_sentence(actor: &str, nicknames: &[String], permission: &str) -> String {
    format!("@{actor} made {} {permission}", mention_list(nicknames))
}

/// `"{actor} revoked {permission} role from {mentions}"`.
pub(super) fn revoke_sentence(actor: &str, nicknames: &[String], permission: &str) -> String {
    format!(
        "@{actor} revoked {permission} role from {}",
        mention_list(nicknames)
    )
}

/// `"{actor} updated report ##{report_id}"`, the ` ( on user @{subject})`
/// Pleroma's `subject_actor_nickname/3` splices in when the report names
/// its target, and the ` with '{state}' state` close.
pub(super) fn report_update_sentence(
    actor: &str,
    report_id: Uuid,
    state: &str,
    subject_nickname: Option<&str>,
) -> String {
    format!(
        "@{actor} updated report ##{report_id}{} with '{state}' state",
        subject_nickname.map_or_else(String::new, |target| format!(" ( on user @{target})"))
    )
}

/// `"{actor} assigned report ##{report_id}"`, the ` ( on user @{subject})`
/// Pleroma's `subject_actor_nickname/3` splices in when the report names
/// its target, and the ` to user {assignee}` close — Pleroma's
/// `report_assigned` clause, whose assignee is a bare nickname, not a
/// mention.
pub(super) fn report_assigned_sentence(
    actor: &str,
    report_id: Uuid,
    assignee: &str,
    subject_nickname: Option<&str>,
) -> String {
    format!(
        "@{actor} assigned report ##{report_id}{} to user {assignee}",
        subject_nickname.map_or_else(String::new, |target| format!(" ( on user @{target})"))
    )
}

/// `"{actor} unassigned report ##{report_id}"` with the same spliced
/// ` ( on user @{subject})` and the ` from a user` close — Pleroma's
/// `report_unassigned` clause.
pub(super) fn report_unassigned_sentence(
    actor: &str,
    report_id: Uuid,
    subject_nickname: Option<&str>,
) -> String {
    format!(
        "@{actor} unassigned report ##{report_id}{} from a user",
        subject_nickname.map_or_else(String::new, |target| format!(" ( on user @{target})"))
    )
}

/// `"{actor} added note '{text}' to report ##{report_id}"` with Pleroma's
/// ` on user @{subject}` tail (`subject_actor_nickname/2`, no paren).
pub(super) fn report_note_sentence(
    actor: &str,
    report_id: Uuid,
    text: &str,
    subject_nickname: Option<&str>,
) -> String {
    format!(
        "@{actor} added note '{text}' to report ##{report_id}{}",
        subject_nickname.map_or_else(String::new, |target| format!(" on user @{target}"))
    )
}

/// `"{actor} deleted note '{text}' from report ##{report_id}"` with the
/// same ` on user @{subject}` tail [`report_note_sentence`] gives —
/// Pleroma's `report_note_delete` clause reads the pair with
/// `subject_actor_nickname/2` too.
pub(super) fn report_note_delete_sentence(
    actor: &str,
    report_id: Uuid,
    text: &str,
    subject_nickname: Option<&str>,
) -> String {
    format!(
        "@{actor} deleted note '{text}' from report ##{report_id}{}",
        subject_nickname.map_or_else(String::new, |target| format!(" on user @{target}"))
    )
}

/// `"{actor} updated status ##{status_id}"` with the changed parts after
/// a comma, in Pleroma's clause order: sensitive, then visibility; when
/// neither is named, Pleroma's sensitive-`nil` clause still renders the
/// visibility of `''`.
pub(super) fn status_update_sentence(
    actor: &str,
    status_id: Uuid,
    sensitive: Option<bool>,
    visibility: Option<&str>,
) -> String {
    let base = format!("@{actor} updated status ##{status_id}");
    match (sensitive, visibility) {
        (Some(sensitive), Some(visibility)) => {
            format!("{base}, set sensitive: '{sensitive}', visibility: '{visibility}'")
        }
        (Some(sensitive), None) => format!("{base}, set sensitive: '{sensitive}'"),
        (None, Some(visibility)) => format!("{base}, set visibility: '{visibility}'"),
        (None, None) => format!("{base}, set visibility: ''"),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::models::moderation_log::test_support::{names, report_id};

    #[test]
    fn user_list_sentences_read_like_pleroma() {
        let subjects = names(&["alice", "bob"]);
        assert_eq!(
            users_sentence("admin", "deactivated", &subjects),
            "@admin deactivated users: @alice, @bob",
            "the deactivate clause"
        );
        assert_eq!(
            users_sentence("admin", "activated", &subjects),
            "@admin activated users: @alice, @bob",
            "the activate clause"
        );
        assert_eq!(
            users_sentence("admin", "deleted", &subjects),
            "@admin deleted users: @alice, @bob",
            "the delete clause"
        );
        assert_eq!(
            users_sentence("admin", "approved", &subjects),
            "@admin approved users: @alice, @bob",
            "the approve clause"
        );
        assert_eq!(
            users_sentence("admin", "rejected", &subjects),
            "@admin rejected users: @alice, @bob",
            "the reject clause"
        );
        assert_eq!(
            users_sentence("admin", "created", &subjects),
            "@admin created users: @alice, @bob",
            "the create clause"
        );
        assert_eq!(
            users_sentence("admin", "forced password reset for", &subjects),
            "@admin forced password reset for users: @alice, @bob",
            "the force_password_reset clause"
        );
        assert_eq!(
            users_sentence("admin", "updated", &subjects),
            "@admin updated users: @alice, @bob",
            "the updated_users clause"
        );
        assert_eq!(
            users_sentence("admin", "confirmed email for", &subjects),
            "@admin confirmed email for users: @alice, @bob",
            "the confirm_email clause"
        );
        assert_eq!(
            users_sentence("admin", "re-sent confirmation email for", &subjects),
            "@admin re-sent confirmation email for users: @alice, @bob",
            "the resend_confirmation_email clause"
        );
        assert_eq!(
            users_sentence("admin", "disabled MFA for", &subjects),
            "@admin disabled MFA for users: @alice, @bob",
            "the disable_mfa clause the ticket adds"
        );
        assert_eq!(
            users_sentence("admin", "added suggested", &subjects),
            "@admin added suggested users: @alice, @bob",
            "the add_suggestion clause"
        );
        assert_eq!(
            users_sentence("admin", "removed suggested", &subjects),
            "@admin removed suggested users: @alice, @bob",
            "the remove_suggestion clause"
        );
        assert_eq!(
            users_sentence("admin", "deactivated", &names(&["solo"])),
            "@admin deactivated users: @solo",
            "a lone subject carries no comma"
        );
    }

    #[test]
    fn tag_and_role_sentences_read_like_pleroma() {
        let subjects = names(&["alice", "bob"]);
        let tags = names(&["staff", "vip"]);
        assert_eq!(
            tag_sentence("admin", &tags, &subjects),
            "@admin added tags: staff, vip to users: @alice, @bob",
            "the tag clause"
        );
        assert_eq!(
            untag_sentence("admin", &tags, &subjects),
            "@admin removed tags: staff, vip from users: @alice, @bob",
            "the untag clause"
        );
        assert_eq!(
            grant_sentence("admin", &subjects, "moderator"),
            "@admin made @alice, @bob moderator",
            "the grant clause"
        );
        assert_eq!(
            revoke_sentence("admin", &subjects, "moderator"),
            "@admin revoked moderator role from @alice, @bob",
            "the revoke clause"
        );
    }

    #[test]
    fn report_and_status_sentences_read_like_pleroma() {
        let id = report_id();
        assert_eq!(
            report_update_sentence("admin", id, "closed", None),
            format!("@admin updated report ##{id} with 'closed' state"),
            "the report_update clause without a subject actor"
        );
        assert_eq!(
            report_update_sentence("admin", id, "resolved", Some("target")),
            format!("@admin updated report ##{id} ( on user @target) with 'resolved' state"),
            "the report_update clause with the parenthesised target"
        );
        assert_eq!(
            report_assigned_sentence("admin", id, "moddy", None),
            format!("@admin assigned report ##{id} to user moddy"),
            "the report_assigned clause without a subject actor"
        );
        assert_eq!(
            report_assigned_sentence("admin", id, "moddy", Some("target")),
            format!("@admin assigned report ##{id} ( on user @target) to user moddy"),
            "the report_assigned clause with the parenthesised target"
        );
        assert_eq!(
            report_unassigned_sentence("admin", id, None),
            format!("@admin unassigned report ##{id} from a user"),
            "the report_unassigned clause without a subject actor"
        );
        assert_eq!(
            report_unassigned_sentence("admin", id, Some("target")),
            format!("@admin unassigned report ##{id} ( on user @target) from a user"),
            "the report_unassigned clause with the parenthesised target"
        );
        assert_eq!(
            report_note_sentence("admin", id, "noted", None),
            format!("@admin added note 'noted' to report ##{id}"),
            "the report_note clause without a subject actor"
        );
        assert_eq!(
            report_note_sentence("admin", id, "noted", Some("target")),
            format!("@admin added note 'noted' to report ##{id} on user @target"),
            "the report_note clause with the bare tail"
        );
        assert_eq!(
            report_note_delete_sentence("admin", id, "noted", None),
            format!("@admin deleted note 'noted' from report ##{id}"),
            "the report_note_delete clause without a subject actor"
        );
        assert_eq!(
            report_note_delete_sentence("admin", id, "noted", Some("target")),
            format!("@admin deleted note 'noted' from report ##{id} on user @target"),
            "the report_note_delete clause with the bare tail"
        );
        assert_eq!(
            status_update_sentence("admin", id, Some(true), Some("private")),
            format!("@admin updated status ##{id}, set sensitive: 'true', visibility: 'private'"),
            "the status_update clause naming both parts"
        );
        assert_eq!(
            status_update_sentence("admin", id, Some(false), None),
            format!("@admin updated status ##{id}, set sensitive: 'false'"),
            "the sensitive-only arm"
        );
        assert_eq!(
            status_update_sentence("admin", id, None, Some("public")),
            format!("@admin updated status ##{id}, set visibility: 'public'"),
            "the visibility-only arm"
        );
        assert_eq!(
            status_update_sentence("admin", id, None, None),
            format!("@admin updated status ##{id}, set visibility: ''"),
            "Pleroma's nil-sensitive clause renders the empty visibility"
        );
    }
}
