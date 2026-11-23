//! Follow, block and mute imports from Mastodon-style CSV lists.
//!
//! `POST /api/pleroma/follow_import`, `POST /api/pleroma/blocks_import`
//! and `POST /api/pleroma/mutes_import` (WBE-138) are Pleroma's
//! `UserImportController`
//! (`lib/pleroma/web/pleroma_api/controllers/user_import_controller.ex`)
//! feeding `User.Import` (`lib/pleroma/user/import.ex`).
//!
//! Each route takes a user token ([`AuthUser`]) and the scopes Pleroma's
//! plug asks for the action: `follow | write:follows` for
//! `follow_import`, `follow | write:blocks` for `blocks_import`,
//! `follow | write:mutes` for `mutes_import`. The body carries one `list`
//! field ([`MultipartForm`], decision 10): a file upload, a multipart
//! text field, or a JSON / urlencoded `list` parameter — the same three
//! spellings Pleroma accepts through `Plug.Upload` and the plain string.
//!
//! The controller half of Pleroma's split is synchronous and thin: parse
//! the list, enqueue a job, answer the JSON string `"jobs started"` that
//! Pleroma's `json(conn, "jobs started")` renders. The work happens in
//! [`crate::jobs::imports`]: one job on [`QUEUE_BACKGROUND`] of kind
//! `imports.follows` / `imports.blocks` / `imports.mutes` with the
//! `{user_id, accts, options}` document [`job_args`] builds. An empty
//! list enqueues nothing (Pleroma's `[_ | _]` clauses) and is still
//! answered `"jobs started"`.
//!
//! Two limits answer 422 `{"error":"Import too large"}` with the ticket's
//! exact wording: the list content may not pass `MAX_IMPORT_BYTES`
//! (1 MiB) and the parsed list may not hold more than
//! `MAX_IMPORT_ENTRIES` accounts. Pleroma has neither cap; this server
//! adds them because one request can otherwise enqueue unbounded work.
//!
//! The CSV is Mastodon's export shape: a first line naming the columns —
//! `Account address,Show boosts,Notify on new posts,Languages` for
//! follows, `Account address` for blocks and mutes — skipped when it
//! contains the word `address`, then one account per line. The
//! first column is the `user@domain` handle (a leading `@` allowed,
//! trimmed), the second is follows' `Show boosts` flag, and any further
//! column is ignored; blank lines and structurally invalid lines are
//! skipped (Pleroma trims and rejects blanks the same way; the strictness
//! here is that a follows row naming something other than `true`/`false`
//! is dropped, and a handle with interior whitespace is never a handle).
//!
//! All three routes share the `imports` rate-limit bucket (WBE-504, one
//! import per user per hour by default, see [`crate::rate_limit`]): every
//! entry may cost a `WebFinger` and an actor fetch, so an unthrottled client
//! could make this server flood third-party hosts. Over the budget a
//! request is refused 429 `{"error":"Throttled"}` before any list is read.

use axum::{
    Json, Router,
    extract::{DefaultBodyLimit, State},
    routing::post,
};
use serde::Deserialize;
use uuid::Uuid;

use crate::{
    auth::AuthUser,
    error::{AppError, AppResult},
    extract::multipart::{MultipartForm, UploadedFiles as _},
    jobs::{
        QUEUE_BACKGROUND,
        imports::{ImportOption, job_args},
    },
    models::job::{NewJob, enqueue},
    oauth::scopes::{FOLLOW, WRITE_BLOCKS, WRITE_FOLLOWS, WRITE_MUTES},
    state::AppState,
};

/// The rate-limit bucket the three import routes share.
const IMPORTS_BUCKET: &str = "imports";
/// The form field the list arrives in, Pleroma's required `:list`.
const LIST_FIELD: &str = "list";
/// 400 body when the request names no `list` at all (Pleroma's
/// `Missing parameters` refusal, spelled like this server's other
/// missing-required-parameter bodies).
const MISSING_LIST: &str = "Missing parameter: list";
/// The success body: Pleroma's `json(conn, "jobs started")` is a JSON
/// string, not an object.
const JOBS_STARTED: &str = "jobs started";
/// The 422 body both size limits answer, the ticket's exact wording.
const IMPORT_TOO_LARGE: &str = "Import too large";
/// Maximum bytes of list content (1 MiB): the file part or the text
/// field, before parsing.
const MAX_IMPORT_BYTES: usize = 1024 * 1024;
/// Maximum parsed accounts in one import.
const MAX_IMPORT_ENTRIES: usize = 5000;
/// Body limit the routes carry: [`MAX_IMPORT_BYTES`] of content doubled
/// for multipart framing and field headers, so an oversized list reaches
/// this module's 422 check instead of the multipart extractor's 413.
const MAX_IMPORT_BODY_BYTES: usize = 2 * MAX_IMPORT_BYTES;
/// A first list line containing this word (case-insensitively) is the
/// Mastodon export header and is skipped.
const HEADER_WORD: &str = "address";

/// The `/api/pleroma` user-import router, merged into
/// [`crate::api::pleroma`].
///
/// Do not call [`Router::fallback`] here (or on anything merged into this
/// tree): the app-wide JSON fallback in [`crate::app`] must answer the
/// unmatched paths.
///
/// Each route is layered with the `imports` rate-limit bucket built from
/// `state`.
pub fn router(state: &AppState) -> Router<AppState> {
    Router::new()
        .route(
            "/follow_import",
            post(follow_import).layer(crate::rate_limit::rate_limit(state, IMPORTS_BUCKET)),
        )
        .route(
            "/blocks_import",
            post(blocks_import).layer(crate::rate_limit::rate_limit(state, IMPORTS_BUCKET)),
        )
        .route(
            "/mutes_import",
            post(mutes_import).layer(crate::rate_limit::rate_limit(state, IMPORTS_BUCKET)),
        )
        .layer(DefaultBodyLimit::max(MAX_IMPORT_BODY_BYTES))
}

/// Which of the three imports a request names: the job kind it enqueues
/// and whether its CSV carries the `Show boosts` column.
#[derive(Clone, Copy, Debug)]
enum ImportKind {
    /// `follow_import`: kind `imports.follows`, options per row.
    Follows,
    /// `blocks_import`: kind `imports.blocks`, no options.
    Blocks,
    /// `mutes_import`: kind `imports.mutes`, no options.
    Mutes,
}

impl ImportKind {
    /// The job kind this import enqueues (handled in
    /// [`crate::jobs::imports`]).
    const fn job_kind(self) -> &'static str {
        match self {
            Self::Follows => crate::jobs::imports::KIND_FOLLOWS,
            Self::Blocks => crate::jobs::imports::KIND_BLOCKS,
            Self::Mutes => crate::jobs::imports::KIND_MUTES,
        }
    }

    /// Whether the CSV's second column is this kind's `Show boosts` flag.
    const fn has_options(self) -> bool {
        matches!(self, Self::Follows)
    }

    /// The per-entry options the kind travels with: follows carry their
    /// `Show boosts` value, blocks and mutes carry an empty list.
    fn options_for(self, entries: &[ImportEntry]) -> Vec<ImportOption> {
        if !self.has_options() {
            return Vec::new();
        }
        entries
            .iter()
            .map(|entry| ImportOption {
                show_reblogs: entry.show_reblogs,
            })
            .collect()
    }
}

/// The `list` parameter of the import routes: the CSV content sent
/// inline (a multipart text field, a JSON string or a form value) rather
/// than as a file upload. Absence is [`MISSING_LIST`]'s 400, which the
/// type cannot express, so it is optional here.
#[derive(Debug, Deserialize)]
struct ImportParams {
    /// The list content, in any of the spellings [`list_text`] accepts.
    #[serde(default)]
    list: Option<String>,
}

/// One parsed line of an import list.
#[derive(Clone, Debug, PartialEq, Eq)]
struct ImportEntry {
    /// The account handle: trimmed, leading `@` stripped.
    acct: String,
    /// Mastodon's `Show boosts` column, `None` for kinds and rows
    /// without the value.
    show_reblogs: Option<bool>,
}

/// `POST /api/pleroma/follow_import` — import follows.
///
/// # Errors
///
/// [`AppError::Unauthorized`] / [`AppError::Forbidden`] from [`AuthUser`]
/// and the `follow | write:follows` check; [`AppError::Unprocessable`]
/// with `IMPORT_TOO_LARGE` past either limit; [`AppError::BadRequest`]
/// for a missing [`LIST_FIELD`] or non-UTF-8 content;
/// [`AppError::PayloadTooLarge`] from the multipart extractor past
/// [`MAX_IMPORT_BODY_BYTES`] or the configured upload limits. Any
/// database failure arrives through `?` as [`AppError::Database`].
async fn follow_import(
    State(state): State<AppState>,
    auth: AuthUser,
    form: MultipartForm<ImportParams>,
) -> AppResult<Json<&'static str>> {
    auth.require_any(&[FOLLOW, WRITE_FOLLOWS])?;
    import(state, auth.0.id, form, ImportKind::Follows).await
}

/// `POST /api/pleroma/blocks_import` — import blocks.
///
/// # Errors
///
/// As [`follow_import`]'s, with the `follow | write:blocks` check.
async fn blocks_import(
    State(state): State<AppState>,
    auth: AuthUser,
    form: MultipartForm<ImportParams>,
) -> AppResult<Json<&'static str>> {
    auth.require_any(&[FOLLOW, WRITE_BLOCKS])?;
    import(state, auth.0.id, form, ImportKind::Blocks).await
}

/// `POST /api/pleroma/mutes_import` — import mutes.
///
/// # Errors
///
/// As [`follow_import`]'s, with the `follow | write:mutes` check.
async fn mutes_import(
    State(state): State<AppState>,
    auth: AuthUser,
    form: MultipartForm<ImportParams>,
) -> AppResult<Json<&'static str>> {
    auth.require_any(&[FOLLOW, WRITE_MUTES])?;
    import(state, auth.0.id, form, ImportKind::Mutes).await
}

/// The shared import flow: read the [`LIST_FIELD`], parse it, check the
/// entry limit, enqueue unless the list came out empty, and answer
/// [`JOBS_STARTED`] as Pleroma does.
async fn import(
    state: AppState,
    user_id: Uuid,
    form: MultipartForm<ImportParams>,
    kind: ImportKind,
) -> AppResult<Json<&'static str>> {
    let text = list_text(form)?;
    let entries = parse_csv_list(&text, kind.has_options());
    if entries.len() > MAX_IMPORT_ENTRIES {
        return Err(import_too_large());
    }
    if !entries.is_empty() {
        enqueue_import(&state, user_id, kind, &entries).await?;
    }
    Ok(Json(JOBS_STARTED))
}

/// The `list` content as text: the `list` file part if one arrived
/// (Pleroma's `Plug.Upload` clause comes first), else the `list` text
/// field or parameter. Content past [`MAX_IMPORT_BYTES`] is the 422 the
/// ticket fixes, not the extractor's 413.
///
/// # Errors
///
/// [`AppError::Unprocessable`] past the byte limit,
/// [`AppError::BadRequest`] for a missing field or invalid UTF-8.
fn list_text(mut form: MultipartForm<ImportParams>) -> AppResult<String> {
    let bytes: Vec<u8> = match form.files.take(LIST_FIELD) {
        Some(file) => file.bytes.to_vec(),
        None => form
            .fields
            .list
            .map(String::into_bytes)
            .ok_or_else(|| AppError::BadRequest(MISSING_LIST.to_owned()))?,
    };
    if bytes.len() > MAX_IMPORT_BYTES {
        return Err(import_too_large());
    }
    String::from_utf8(bytes)
        .map_err(|err| AppError::BadRequest(format!("Field list is not valid UTF-8: {err}")))
}

/// Enqueues the import as one [`QUEUE_BACKGROUND`] job of `kind`'s job
/// kind, carrying the parsed accts and options.
async fn enqueue_import(
    state: &AppState,
    user_id: Uuid,
    kind: ImportKind,
    entries: &[ImportEntry],
) -> AppResult<()> {
    let accts: Vec<String> = entries.iter().map(|entry| entry.acct.clone()).collect();
    let options = kind.options_for(entries);
    let queued = NewJob {
        queue: QUEUE_BACKGROUND,
        kind: kind.job_kind(),
        args: job_args(user_id, &accts, &options),
        run_at: None,
        max_attempts: None,
        unique_key: None,
    };
    let _job = enqueue(&state.db, queued).await?;
    Ok(())
}

/// The 422 both limits share.
fn import_too_large() -> AppError {
    AppError::Unprocessable(IMPORT_TOO_LARGE.to_owned())
}

/// Parses a Mastodon-style CSV list: skip the header line when it names
/// the columns, parse the rest, drop what is blank or invalid.
///
/// `has_options` is the follows kind's `Show boosts` column: with it a
/// second column must read `true`/`false` (case-insensitively) or the
/// whole row is invalid; without it any later column is ignored.
fn parse_csv_list(text: &str, has_options: bool) -> Vec<ImportEntry> {
    let mut lines = text.lines().peekable();
    if lines
        .peek()
        .is_some_and(|line| line.to_ascii_lowercase().contains(HEADER_WORD))
    {
        let _header = lines.next();
    }
    lines
        .filter_map(|line| parse_line(line, has_options))
        .collect()
}

/// Parses one data line: `handle[,show_reblogs[,...]]`, the handle
/// trimmed with any leading `@` stripped. A blank handle, a handle with
/// interior whitespace, and a follows row whose flag is neither
/// `true` nor `false` are the invalid lines, dropped whole.
fn parse_line(line: &str, has_options: bool) -> Option<ImportEntry> {
    let mut columns = line.split(',');
    let raw_acct = columns.next().unwrap_or_default();
    let acct = raw_acct.trim().trim_start_matches('@').trim();
    if acct.is_empty() || acct.chars().any(char::is_whitespace) {
        return None;
    }
    let show_reblogs = match (has_options, columns.next()) {
        (false, _) | (true, None) => None,
        (true, Some(cell)) if cell.trim().is_empty() => None,
        (true, Some(cell)) => {
            return parse_bool(cell.trim()).map(|flag| ImportEntry {
                acct: acct.to_owned(),
                show_reblogs: Some(flag),
            });
        }
    };
    Some(ImportEntry {
        acct: acct.to_owned(),
        show_reblogs,
    })
}

/// The `Show boosts` column's value: `true` or `false`, either case;
/// anything else invalidates the row.
fn parse_bool(text: &str) -> Option<bool> {
    match text.to_ascii_lowercase().as_str() {
        "true" => Some(true),
        "false" => Some(false),
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use bytes::Bytes;

    use super::*;

    /// An empty file set, for [`list_text`] over inline parameters.
    fn form_with(list: Option<&str>) -> MultipartForm<ImportParams> {
        MultipartForm {
            fields: ImportParams {
                list: list.map(str::to_owned),
            },
            files: Vec::new(),
        }
    }

    /// A `MultipartForm` carrying `content` as a `list` file part.
    fn file_form(content: &[u8]) -> MultipartForm<ImportParams> {
        MultipartForm {
            fields: ImportParams { list: None },
            files: vec![crate::extract::multipart::UploadedFile {
                field: LIST_FIELD.to_owned(),
                filename: Some("following_accounts.csv".to_owned()),
                content_type: Some("text/csv".to_owned()),
                bytes: Bytes::from(content.to_vec()),
            }],
        }
    }

    /// A parsed entry for the expected-value shorthand.
    fn entry(acct: &str, show_reblogs: Option<bool>) -> ImportEntry {
        ImportEntry {
            acct: acct.to_owned(),
            show_reblogs,
        }
    }

    #[test]
    fn the_follows_header_is_skipped_and_the_boosts_column_parsed() {
        let csv = "Account address,Show boosts,Notify on new posts,Languages\n\
                   bob@remote.example,True,,\n\
                   carol@other.example,False,1,\n";
        let got = parse_csv_list(csv, true);
        assert_eq!(
            got,
            vec![
                entry("bob@remote.example", Some(true)),
                entry("carol@other.example", Some(false)),
            ],
            "the header goes, `True`/`False` read case-insensitively, \
             the blank column keeps the default"
        );
    }

    #[test]
    fn a_header_is_only_a_header_when_it_names_the_column() {
        let csv = "Account Address\nbob@remote.example\n";
        let got = parse_csv_list(csv, false);
        assert_eq!(
            got,
            vec![entry("bob@remote.example", None)],
            "any casing of the header word is skipped"
        );
        let no_header = "bob@remote.example\nother,stuff\n";
        let kept = parse_csv_list(no_header, false);
        assert_eq!(
            kept,
            vec![entry("bob@remote.example", None), entry("other", None),],
            "a first line that is plain data stays data"
        );
    }

    #[test]
    fn the_blocks_and_mutes_lists_ignore_every_column_after_the_handle() {
        let csv = "Account address\n@bob@remote.example,ignored,also ignored\n";
        let got = parse_csv_list(csv, false);
        assert_eq!(
            got,
            vec![entry("bob@remote.example", None)],
            "a leading `@` is stripped and extra columns are ignored"
        );
    }

    #[test]
    fn blank_lines_and_invalid_handles_are_skipped() {
        let csv = "Account address\n\
                   \n   \n\
                   @\n\
                   has space@bad.example\n\
                   trim-me@example.org  \n\
                   ,true,,\n";
        let got = parse_csv_list(csv, true);
        assert_eq!(
            got,
            vec![entry("trim-me@example.org", None)],
            "empty, bare-`@`, whitespace-inside and empty-handle lines go"
        );
    }

    #[test]
    fn a_follows_row_with_an_unreadable_boosts_flag_is_invalid() {
        let csv = "Account address,Show boosts\n\
                   good@example.org,true\n\
                   maybe@example.org,perhaps\n\
                   omitted@example.org\n\
                   blank@example.org,   \n";
        let got = parse_csv_list(csv, true);
        assert_eq!(
            got,
            vec![
                entry("good@example.org", Some(true)),
                entry("omitted@example.org", None),
                entry("blank@example.org", None),
            ],
            "a present-but-junk flag drops the row; absent and blank keep it"
        );
    }

    #[test]
    fn crlf_line_endings_parse_like_lf() {
        let csv = "Account address,Show boosts\r\nbob@remote.example,false\r\n\r\n";
        let got = parse_csv_list(csv, true);
        assert_eq!(got, vec![entry("bob@remote.example", Some(false))]);
    }

    #[test]
    fn handles_are_trimmed_and_at_stripped_before_the_options_split() {
        let csv = "  @alice@test.example , TRUE , extra, columns\n";
        let got = parse_csv_list(csv, true);
        assert_eq!(got, vec![entry("alice@test.example", Some(true))]);
    }

    #[test]
    fn an_empty_or_header_only_list_parses_to_nothing() {
        assert_eq!(parse_csv_list("", true).len(), 0, "nothing in, nothing out");
        assert_eq!(
            parse_csv_list("Account address\n", false).len(),
            0,
            "a lone header is still an empty list"
        );
    }

    #[test]
    fn the_kind_decides_the_job_kind_and_the_option_list() {
        let entries = vec![
            entry("a@x.example", Some(false)),
            entry("b@y.example", None),
        ];
        assert_eq!(ImportKind::Follows.job_kind(), "imports.follows");
        assert_eq!(ImportKind::Blocks.job_kind(), "imports.blocks");
        assert_eq!(ImportKind::Mutes.job_kind(), "imports.mutes");
        assert_eq!(
            ImportKind::Follows.options_for(&entries),
            vec![
                ImportOption {
                    show_reblogs: Some(false)
                },
                ImportOption { show_reblogs: None },
            ],
            "follows travel with one option per entry"
        );
        assert_eq!(
            ImportKind::Blocks.options_for(&entries).len(),
            0,
            "blocks carry no options"
        );
        assert_eq!(
            ImportKind::Mutes.options_for(&entries).len(),
            0,
            "and neither do mutes"
        );
    }

    #[test]
    fn list_text_prefers_the_file_part_and_checks_its_size() {
        let as_file = list_text(file_form(b"Account address\nbob@x.example\n")).unwrap();
        assert_eq!(
            as_file, "Account address\nbob@x.example\n",
            "the file body is the list"
        );

        let oversize = vec![b'a'; MAX_IMPORT_BYTES + 1];
        let err = list_text(file_form(&oversize)).expect_err("1 MiB + 1 is too large");
        assert_eq!(err.to_string(), IMPORT_TOO_LARGE, "the ticket's wording");

        let too_big = list_text(form_with(Some(&"x".repeat(MAX_IMPORT_BYTES + 1))))
            .expect_err("an inline list is capped the same");
        assert_eq!(too_big.to_string(), IMPORT_TOO_LARGE);
    }

    #[test]
    fn list_text_falls_back_to_the_parameter_and_demands_one() {
        let inline = list_text(form_with(Some("bob@x.example"))).unwrap();
        assert_eq!(inline, "bob@x.example", "the text field is the list");
        let missing = list_text(form_with(None)).expect_err("no file and no text");
        assert_eq!(missing.to_string(), MISSING_LIST);
        let not_utf8 = list_text(file_form(&[0xff, 0xfe, 0x00])).expect_err("bytes must be UTF-8");
        assert!(
            not_utf8
                .to_string()
                .starts_with("Field list is not valid UTF-8"),
            "invalid UTF-8 says so: {not_utf8}"
        );
    }

    #[test]
    fn the_limits_are_the_ones_the_ticket_fixes() {
        assert_eq!(MAX_IMPORT_BYTES, 1024 * 1024, "1 MiB");
        assert_eq!(MAX_IMPORT_ENTRIES, 5000, "5000 entries");
    }
}
