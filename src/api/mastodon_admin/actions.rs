//! The `action` endpoint: its parameters, the five action types and
//! their side effects.

use std::slice;

use axum::{
    Json,
    extract::{Path, State},
};
use serde::Deserialize;
use serde_json::{Value, json};
use tracing::debug;

use super::{
    AREA_ACCOUNTS, CLASS_WRITE, INVALID_TYPE, MISSING_TYPE, NOT_ALLOWED, ONLY_LOCAL, WRONG_TARGET,
    reads::named_account,
};
use crate::{
    api::helpers,
    auth::admin::AdminUser,
    error::{AppError, AppResult},
    extract::ApiBody,
    models::{
        moderation_log,
        report::{self, ReportState},
        user::{User, admin::Standing},
    },
    services::report_moderation::set_report_state,
    state::AppState,
    util::id::parse_id,
};

/// `POST /api/v1/admin/accounts/{id}/action` — Pleroma's `account_action`,
/// Mastodon's moderation umbrella: one submitted type, applied as this
/// schema can apply it and recorded whatever the answer.
///
/// The order is Pleroma's `with` chain: the type and its target are
/// validated ([`validate_target`]) before the report is looked up and before
/// anything moves, so a request naming an unknown type, the caller's own
/// account or a remote mirror for a deactivation changes nothing at all
/// and leaves its report open. The answer is the empty object
/// where Pleroma answers `204` — the ticket's pinning — and the moderation
/// log is what makes the request traceable.
///
/// # Errors
///
/// [`AppError::Unauthorized`] / [`AppError::Forbidden`] from [`AdminUser`]
/// and its `admin:write` gate; [`AppError::Unprocessable`] with
/// [`INVALID_TYPE`] / [`MISSING_TYPE`] from [`AccountAction::parse`];
/// [`AppError::NotFound`] from [`named_account`] or [`resolve_report`];
/// [`AppError::Forbidden`] with [`NOT_ALLOWED`] and [`AppError::BadRequest`]
/// with [`ONLY_LOCAL`] from [`validate_target`];
/// [`AppError::BadRequest`] with [`WRONG_TARGET`];
/// [`AppError::Unprocessable`] from the body extractor for a `type` that is
/// no text; and [`AppError::Database`] from the writes and the log.
pub(super) async fn account_action(
    State(state): State<AppState>,
    admin: AdminUser,
    Path(raw): Path<String>,
    ApiBody(params): ApiBody<ActionParams>,
) -> AppResult<Json<Value>> {
    admin.require(CLASS_WRITE, AREA_ACCOUNTS)?;
    let account = named_account(&state, &raw).await?;
    let action = AccountAction::parse(params.kind.as_deref())?;
    validate_target(&admin.0, &account, action)?;
    resolve_report(&state, &admin.0, &account, params.report_id.as_deref()).await?;
    apply_action(&state, &admin.0, &account, action).await?;
    Ok(Json(json!({})))
}

/// Pleroma's `validate_account_action`, plus the self-protection its
/// controller lacks: an action that deactivates is refused on the caller's
/// own account (a sole admin could lock themselves out) and on a remote
/// mirror (the flag means nothing for an account this instance does not
/// own). The other three types touch no flag and pass anywhere.
///
/// # Errors
///
/// [`AppError::Forbidden`] with [`NOT_ALLOWED`] for the caller's own
/// account; [`AppError::BadRequest`] with [`ONLY_LOCAL`] for a remote one.
fn validate_target(actor: &User, account: &User, action: AccountAction) -> AppResult<()> {
    if !action.deactivates() {
        return Ok(());
    }
    if account.id == actor.id {
        return Err(AppError::Forbidden(NOT_ALLOWED.to_owned()));
    }
    if !account.local.get() {
        return Err(AppError::BadRequest(ONLY_LOCAL.to_owned()));
    }
    Ok(())
}

/// The effect of one action type: the flag its two real actions move, each
/// with the `deactivate` entry every other writer of that flag leaves, and the
/// entry naming the submitted type that records the request itself. The
/// flag's `Suspended` event is what tells the protocols (`ActivityPub` queues
/// an actor `Update`); this function itself queues no job.
///
/// # Errors
///
/// [`AppError::NotFound`] when the activation write matches no row (the
/// account went away mid-request); [`AppError::Database`] from the writes
/// and the log.
async fn apply_action(
    state: &AppState,
    actor: &User,
    account: &User,
    action: AccountAction,
) -> AppResult<()> {
    if let Some(standing) = action.standing() {
        let moved = helpers::set_standing(state, actor, slice::from_ref(account), standing)
            .await?
            .pop()
            .ok_or(AppError::NotFound)?;
        debug!(
            account = %moved.id,
            suspended = moved.suspended_at.is_some(),
            "admin action deactivated an account"
        );
    }
    let logged = moderation_log::account_action(&state.db, actor, account, action.as_str()).await?;
    debug!(
        log = %logged.id,
        account = %account.id,
        action = action.as_str(),
        "admin action recorded"
    );
    Ok(())
}

/// Pleroma's `associated_report` and `resolve_report` pair for the umbrella
/// request: no `report_id` is no report to resolve, an id that names none is
/// the same 404 the account half gives, a report about another account is
/// the 400 [`WRONG_TARGET`], and a report that passes both is moved to
/// `resolved` through [`set_report_state`] — the write, its `report_update`
/// entry and its `report.updated` event, exactly what the console's own
/// state change does.
///
/// # Errors
///
/// [`AppError::NotFound`] for an unusable or unknown id, and for a row that
/// went between the two reads; [`AppError::BadRequest`] with
/// [`WRONG_TARGET`]; [`AppError::Database`] from the reads, the write and
/// the log.
async fn resolve_report(
    state: &AppState,
    actor: &User,
    account: &User,
    raw: Option<&str>,
) -> AppResult<()> {
    let Some(text) = raw else {
        return Ok(());
    };
    let Some(id) = parse_id(text) else {
        return Err(AppError::NotFound);
    };
    let filed = report::get(&state.db, id)
        .await?
        .ok_or(AppError::NotFound)?;
    if filed.target_id != account.id {
        return Err(AppError::BadRequest(WRONG_TARGET.to_owned()));
    }
    let resolved = set_report_state(state, actor, id, ReportState::Resolved)
        .await?
        .ok_or(AppError::NotFound)?;
    debug!(
        report = %resolved.id,
        "admin action resolved a report"
    );
    Ok(())
}

/// The body of `POST /api/v1/admin/accounts/{id}/action`: the `type` the
/// action is named by, and the one report it resolves on the way.
///
/// Mastodon sends three more keys — `text` (the warning letter's body),
/// `warning_preset_id` and `send_email_notification` — and no field names
/// them, because nothing here reads them: this instance has no moderation
/// warning to write a letter for and Pleroma's controller — whose
/// `CastAndValidate` schema lists only `type` and `report_id` — reads none
/// either. The body parser drops the unclaimed keys, so a client sending
/// the full set is answered exactly as one sending the two that matter.
#[derive(Debug, Deserialize)]
pub(super) struct ActionParams {
    /// Mastodon's `type`: one of the five [`AccountAction`] words, and
    /// required — but read as an [`Option`] so that its absence is this
    /// endpoint's [`MISSING_TYPE`] 422 rather than the extractor's generic
    /// "Invalid parameters" one.
    #[serde(rename = "type", default)]
    kind: Option<String>,
    /// The report to resolve beside the action: this schema's `UUIDv7` text,
    /// not Mastodon's integer, and `null` or absent for none.
    #[serde(default)]
    report_id: Option<String>,
}

/// The moderation type one umbrella request states.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum AccountAction {
    /// `none`: nothing to do but record that the admin looked.
    NoAction,
    /// `sensitive`: Mastodon's "mark this account's media sensitive". No
    /// column holds such a state here, so the entry is all of it.
    Sensitive,
    /// `silence`: Mastodon's limit-the-reach state. This schema has no
    /// silence to model — the reads answer `silenced: false` on every row —
    /// so the entry is all of it, documented as the deliberate no-op.
    Silence,
    /// `disable`: [`is_active`](User::is_active) off, nothing else; the
    /// account reads `disabled`, and `enable` undoes it.
    Disable,
    /// `suspend`: [`is_active`](User::is_active) off and
    /// [`suspended_at`](User::suspended_at) stamped, nothing deleted; the
    /// account reads `suspended` and federates as such, and `unsuspend` undoes
    /// it (WBE-878).
    Suspend,
}

impl AccountAction {
    /// The word Mastodon spells this action with, and the action name the
    /// moderation-log entry records it under.
    const fn as_str(self) -> &'static str {
        match self {
            Self::NoAction => "none",
            Self::Sensitive => "sensitive",
            Self::Disable => "disable",
            Self::Silence => "silence",
            Self::Suspend => "suspend",
        }
    }

    /// Whether this action is one of the two that deactivate the account.
    const fn deactivates(self) -> bool {
        self.standing().is_some()
    }

    /// The standing this action gives the account, `None` for the three
    /// types that touch no flag.
    const fn standing(self) -> Option<Standing> {
        match self {
            Self::Disable => Some(Standing::Disabled),
            Self::Suspend => Some(Standing::Suspended),
            Self::NoAction | Self::Sensitive | Self::Silence => None,
        }
    }

    /// Reads the submitted `type`: [`MISSING_TYPE`] for a request that
    /// states none (Pleroma's operation requires the key) and
    /// [`INVALID_TYPE`] for a word off the five (its `CastAndValidate` plug
    /// refuses anything outside the enum). The words are the enum's own, so
    /// `Disable` is no action.
    ///
    /// # Errors
    ///
    /// [`AppError::Unprocessable`] with either pinned message.
    fn parse(text: Option<&str>) -> AppResult<Self> {
        match text {
            None => Err(AppError::Unprocessable(MISSING_TYPE.to_owned())),
            Some("none") => Ok(Self::NoAction),
            Some("sensitive") => Ok(Self::Sensitive),
            Some("disable") => Ok(Self::Disable),
            Some("silence") => Ok(Self::Silence),
            Some("suspend") => Ok(Self::Suspend),
            Some(_) => Err(AppError::Unprocessable(INVALID_TYPE.to_owned())),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::test_fixtures::{local_user, remote_user};

    /// A deactivation of the caller's own account is the 403, and one of a
    /// remote mirror the 400; the three types that move no flag pass on
    /// the caller, and a local account other than the caller's passes
    /// every type.
    #[tokio::test]
    async fn validate_target_guards_the_caller_and_remote_mirrors() {
        let db = crate::test_db::test_db().await.expect("test database slot");
        let pool = db.pool().clone();
        let admin = local_user(&pool, "root").await;
        let other = local_user(&pool, "plain").await;
        let mirror = remote_user(&pool, "faraway").await;

        for action in [AccountAction::Disable, AccountAction::Suspend] {
            let own = validate_target(&admin, &admin, action).expect_err("self is protected");
            assert_eq!(own.to_string(), NOT_ALLOWED, "{action:?} on self");
            assert!(matches!(own, AppError::Forbidden(_)), "as a 403");
            validate_target(&admin, &other, action).expect("another local account passes");
        }
        let remote = validate_target(&admin, &mirror, AccountAction::Disable)
            .expect_err("a mirror cannot be disabled");
        assert_eq!(remote.to_string(), ONLY_LOCAL, "the pinned wording");
        assert!(matches!(remote, AppError::BadRequest(_)), "as a 400");
        validate_target(&admin, &admin, AccountAction::NoAction).expect("none on self passes");
        validate_target(&admin, &mirror, AccountAction::Silence).expect("silence on a mirror");
    }

    /// The umbrella's five words each read as their own action, spell back
    /// identically (that spelling is the moderation log's action name), and
    /// report whether they move `is_active` — the flag write and the
    /// `deactivate` entry beside it stay in step on that one answer.
    #[test]
    fn the_five_types_are_read_and_spelled_back() {
        for (word, action, moves_the_flag) in [
            ("none", AccountAction::NoAction, false),
            ("sensitive", AccountAction::Sensitive, false),
            ("silence", AccountAction::Silence, false),
            ("disable", AccountAction::Disable, true),
            ("suspend", AccountAction::Suspend, true),
        ] {
            let parsed = AccountAction::parse(Some(word))
                .unwrap_or_else(|_| panic!("`{word}` is a known type"));
            assert_eq!(parsed, action, "`{word}` is its own action");
            assert_eq!(parsed.as_str(), word, "and the entry names it back");
            assert_eq!(
                parsed.deactivates(),
                moves_the_flag,
                "`{word}`'s answer on the activation flag"
            );
        }
    }

    /// A word off the enum is the pinned 422 — Pleroma's `CastAndValidate`
    /// shape for a value outside the operation's enum — and no word at all
    /// is the other one, its `required: [:type]`. Neither reading is
    /// case-folded, trimmed or defaulted: `Disable` is no action.
    #[test]
    fn an_unusable_type_refuses_the_request() {
        for word in ["freeze", "", " ", "None", "DISABLE", " delete", "suspend "] {
            let err = AccountAction::parse(Some(word))
                .expect_err("no such word names an action to perform");
            assert_eq!(err.to_string(), INVALID_TYPE, "`{word}` is no type");
        }
        let absent = AccountAction::parse(None).expect_err("the key is required");
        assert_eq!(absent.to_string(), MISSING_TYPE, "absence refuses too");
    }

    /// The action body reads the two keys this endpoint owns — Mastodon's
    /// `type`, under its own name on the wire, and `report_id` — and lets
    /// the three it ignores go unclaimed, while a JSON `null` reads as
    /// absence for both. A `type` that is no text is the extractor's 422.
    #[test]
    fn the_action_body_reads_two_keys_and_lets_three_go() {
        let full: ActionParams = serde_json::from_value(json!({
            "type": "suspend",
            "report_id": "01934567-89ab-7cde-f012-3456789abcde",
            "text": "read the rules",
            "warning_preset_id": "7",
            "send_email_notification": true,
        }))
        .expect("the full Mastodon body");
        assert_eq!(
            full.kind.as_deref(),
            Some("suspend"),
            "the key is spelled `type` on the wire"
        );
        assert_eq!(
            full.report_id.as_deref(),
            Some("01934567-89ab-7cde-f012-3456789abcde"),
            "and the report it names arrives"
        );
        let bare: ActionParams =
            serde_json::from_value(json!({ "type": "none" })).expect("the body of the no-op");
        assert_eq!(bare.kind.as_deref(), Some("none"), "only the type");
        assert!(bare.report_id.is_none(), "and no report to resolve");
        for stated in [json!({}), json!({"type": null, "report_id": null})] {
            let none: ActionParams =
                serde_json::from_value(stated.clone()).expect("a null is absence here too");
            assert!(
                none.kind.is_none() && none.report_id.is_none(),
                "`{stated}` states nothing"
            );
        }
        assert!(
            serde_json::from_value::<ActionParams>(json!({ "type": 7 })).is_err(),
            "a type that is no text never reaches the parser"
        );
    }
}
