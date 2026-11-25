//! The `wired-be` command line: argument parsing and the admin bootstrap.
//!
//! With no arguments — or with the explicit `serve` command — the process
//! runs [`crate::serve`]: the HTTP server, the job worker and the periodic
//! scheduler. With `create-admin` it instead creates one local account
//! through the very [`create_local_user`] registration path
//! `POST /api/v1/accounts` uses and grants it the admin role, so an
//! instance with closed registrations can still bootstrap its first staff
//! account:
//!
//! ```text
//! wired-be create-admin --nickname alice --email alice@example.org --password-stdin
//! ```
//!
//! The password arrives on standard input, never in the process arguments.

use std::collections::BTreeMap;

use crate::{
    error::AppResult,
    models::user::{
        Flag, User,
        admin::{Role, set_approved, set_role},
        register::{RegisterParams, create_local_user},
    },
    state::AppState,
};

/// The top-level help text, printed by `wired-be --help`.
pub const USAGE: &str = "\
wired-be — Wired's API-only fediverse backend

USAGE:
    wired-be [COMMAND]

COMMANDS:
    serve           Run the HTTP server, the job worker and the periodic
                    scheduler (the default with no command)
    create-admin    Create a local account and grant it the admin role

OPTIONS:
    -h, --help      Print this help

Configuration comes from the environment (or a `.env` file):
`DATABASE_URL` and `BASE_URL` are required by every command that
touches the database. Run `wired-be create-admin --help` for that
command's flags.
";

/// The `create-admin` help text, printed by `wired-be create-admin --help`.
pub const CREATE_ADMIN_USAGE: &str = "\
wired-be create-admin — create a local account with the admin role

USAGE:
    wired-be create-admin --nickname NICK --email EMAIL --password-stdin

OPTIONS:
        --nickname NICK     Nickname of the new account (required)
        --email EMAIL       Email address of the new account (required)
        --password-stdin    Read the account password from standard input
                            (required; the password never appears in the
                            process arguments)
    -h, --help              Print this help

The account is created through the same registration path as
`POST /api/v1/accounts` — the nickname and password rules apply — and
then granted `is_admin`. Since the address is typed by whoever holds
database access, the account starts email-confirmed and approved even
when `ACCOUNT_ACTIVATION_REQUIRED` or `REGISTRATIONS_APPROVAL_REQUIRED`
is on, so the first administrator of a closed instance can log in.
";

/// What the process was asked to do, one variant per command line shape.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Command {
    /// Run [`crate::serve`]: the default with no arguments.
    Serve,
    /// Create an admin account from these flags and the stdin password.
    CreateAdmin(CreateAdminArgs),
    /// Print this text and exit successfully: `-h` or `--help`.
    Help(&'static str),
}

/// The flags of `create-admin`, parsed and checked for shape. The password
/// is deliberately not here: it arrives separately, from standard input.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct CreateAdminArgs {
    /// `--nickname`: the handle of the new account.
    pub nickname: String,
    /// `--email`: the address of the new account.
    pub email: String,
}

/// Parses the process arguments — `args` without the program name — into
/// the [`Command`] they spell. No arguments is [`Command::Serve`], the
/// shape the server operators type.
///
/// # Errors
///
/// A human-readable message for an unknown command or flag, a repeated or
/// valueless flag, or a missing required part of `create-admin`.
pub fn parse_args(args: &[String]) -> Result<Command, String> {
    let Some((first, rest)) = args.split_first() else {
        return Ok(Command::Serve);
    };
    match first.as_str() {
        "serve" => parse_serve(rest),
        "create-admin" => parse_create_admin(rest),
        "-h" | "--help" => Ok(Command::Help(USAGE)),
        other => Err(format!("unknown command `{other}`\n\n{USAGE}")),
    }
}

/// The `serve` command line: no arguments (or a help flag).
fn parse_serve(args: &[String]) -> Result<Command, String> {
    if args.is_empty() {
        return Ok(Command::Serve);
    }
    if args.iter().any(|arg| arg == "-h" || arg == "--help") {
        return Ok(Command::Help(USAGE));
    }
    Err(format!(
        "`serve` takes no arguments (bare `wired-be` serves)\n\n{USAGE}"
    ))
}

/// The `create-admin` argument list: the two value flags, each at most
/// once, and the required `--password-stdin` switch.
fn parse_create_admin(args: &[String]) -> Result<Command, String> {
    let mut nickname: Option<String> = None;
    let mut email: Option<String> = None;
    let mut password_stdin = false;
    let mut iter = args.iter();
    while let Some(arg) = iter.next() {
        let (flag, attached) = split_flag(arg);
        match flag {
            "--nickname" => set_flag(&mut nickname, flag_value(attached, &mut iter, flag)?, flag)?,
            "--email" => set_flag(&mut email, flag_value(attached, &mut iter, flag)?, flag)?,
            "--password-stdin" => {
                if attached.is_some() {
                    return Err(format!(
                        "`--password-stdin` takes no value\n\n{CREATE_ADMIN_USAGE}"
                    ));
                }
                password_stdin = true;
            }
            "-h" | "--help" => return Ok(Command::Help(CREATE_ADMIN_USAGE)),
            other => {
                return Err(format!(
                    "unknown flag `{other}` for create-admin\n\n{CREATE_ADMIN_USAGE}"
                ));
            }
        }
    }
    if !password_stdin {
        return Err(format!(
            "create-admin requires --password-stdin: the password is read from standard input\
             \n\n{CREATE_ADMIN_USAGE}"
        ));
    }
    let nickname = nickname
        .ok_or_else(|| format!("create-admin requires --nickname\n\n{CREATE_ADMIN_USAGE}"))?;
    let email =
        email.ok_or_else(|| format!("create-admin requires --email\n\n{CREATE_ADMIN_USAGE}"))?;
    Ok(Command::CreateAdmin(CreateAdminArgs { nickname, email }))
}

/// One argument as its flag and its `=`-attached value, if any.
fn split_flag(arg: &str) -> (&str, Option<&str>) {
    match arg.split_once('=') {
        Some((flag, value)) => (flag, Some(value)),
        None => (arg, None),
    }
}

/// The value of one `--flag`: the text attached after `=`, or the next
/// argument when there is none attached.
fn flag_value<'a, I>(attached: Option<&'a str>, rest: &mut I, flag: &str) -> Result<String, String>
where
    I: Iterator<Item = &'a String>,
{
    attached.map_or_else(
        || {
            rest.next()
                .cloned()
                .ok_or_else(|| format!("`{flag}` is missing its value\n\n{CREATE_ADMIN_USAGE}"))
        },
        |text| Ok(text.to_owned()),
    )
}

/// Stores one flag's value in its slot, refusing a repeated flag and an
/// empty value.
fn set_flag(slot: &mut Option<String>, value: String, flag: &str) -> Result<(), String> {
    if slot.is_some() {
        return Err(format!("`{flag}` was given twice\n\n{CREATE_ADMIN_USAGE}"));
    }
    if value.trim().is_empty() {
        return Err(format!(
            "`{flag}` needs a non-empty value\n\n{CREATE_ADMIN_USAGE}"
        ));
    }
    *slot = Some(value);
    Ok(())
}

/// Reads the new admin's password from standard input: the first line,
/// with the trailing newline (and carriage return) trimmed.
///
/// Empty input stays empty and is refused by the registration validation
/// below, like any too-short password.
///
/// # Errors
///
/// Any [`std::io::Error`] from reading standard input.
pub fn read_password_from_stdin() -> Result<String, std::io::Error> {
    let mut line = String::new();
    std::io::stdin().read_line(&mut line).map(drop)?;
    Ok(line.trim_end_matches(['\n', '\r']).to_owned())
}

/// Creates the administrator account `create-admin` promises.
///
/// The account goes through [`create_local_user`] — every nickname, email
/// and password rule of `POST /api/v1/accounts` applies — with the two
/// staff-creation bypasses the admin `POST /api/v1/pleroma/admin/users`
/// already runs (see [`crate::api::admin::users`]):
/// [`RegisterParams::provider_email_verified`] stands for the address
/// proof, so `ACCOUNT_ACTIVATION_REQUIRED` leaves the row confirmed and
/// queues no letter (the address was typed by whoever holds database
/// access), and [`set_approved`] lifts the flag `REGISTRATIONS_APPROVAL_REQUIRED`
/// stored, so the first admin of a gated instance can log in and approve
/// everybody else. [`set_role`] then writes `is_admin = true`, and the
/// returned row carries it.
///
/// # Errors
///
/// The registration refusal (the 422-shaped field map a
/// [`RegisterError`](crate::models::user::register::RegisterError::Invalid)
/// becomes) or a hashing, key-generation or database failure, each as an
/// [`AppError`](crate::error::AppError).
pub async fn create_admin(
    state: &AppState,
    args: &CreateAdminArgs,
    password: &str,
) -> AppResult<User> {
    let params = RegisterParams {
        nickname: args.nickname.clone(),
        email: Some(args.email.clone()),
        password: password.to_owned(),
        name: None,
        bio: None,
        reason: None,
        provider_email_verified: true,
        identifiers: BTreeMap::new(),
    };
    let created = create_local_user(state, params, None).await?;
    set_role(&state.db, created.id, Role::Admin, true).await?;
    let stored = if created.approval_pending.get() {
        set_approved(&state.db, created.id)
            .await?
            .unwrap_or(created)
    } else {
        created
    };
    Ok(User {
        is_admin: Flag::new(true),
        ..stored
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    /// One `&str` slice as the owned argument list the parser reads.
    fn argv(args: &[&str]) -> Vec<String> {
        args.iter().map(|arg| (*arg).to_owned()).collect()
    }

    /// No arguments is `serve`, the shape that starts the server.
    #[test]
    fn no_arguments_serves() {
        assert_eq!(parse_args(&argv(&[])), Ok(Command::Serve));
    }

    /// The explicit `serve` command also serves, and `serve` with a help
    /// flag prints the top-level usage.
    #[test]
    fn serve_command_and_its_help() {
        assert_eq!(parse_args(&argv(&["serve"])), Ok(Command::Serve));
        assert_eq!(
            parse_args(&argv(&["serve", "--help"])),
            Ok(Command::Help(USAGE))
        );
        assert!(
            parse_args(&argv(&["serve", "--port", "1"])).is_err(),
            "serve takes no arguments"
        );
    }

    /// `-h`/`--help` as the command itself prints the usage.
    #[test]
    fn help_flags_print_usage() {
        assert_eq!(parse_args(&argv(&["--help"])), Ok(Command::Help(USAGE)));
        assert_eq!(parse_args(&argv(&["-h"])), Ok(Command::Help(USAGE)));
        assert_eq!(
            parse_args(&argv(&["create-admin", "--help"])),
            Ok(Command::Help(CREATE_ADMIN_USAGE))
        );
    }

    /// The documented full command parses into its two flags, with the
    /// space-separated spelling.
    #[test]
    fn create_admin_parses_the_documented_shape() {
        let parsed = parse_args(&argv(&[
            "create-admin",
            "--nickname",
            "alice",
            "--email",
            "alice@example.org",
            "--password-stdin",
        ]));
        assert_eq!(
            parsed,
            Ok(Command::CreateAdmin(CreateAdminArgs {
                nickname: "alice".to_owned(),
                email: "alice@example.org".to_owned(),
            }))
        );
    }

    /// The `=`-attached flag values parse the same way, in any order.
    #[test]
    fn create_admin_accepts_attached_values() {
        let parsed = parse_args(&argv(&[
            "create-admin",
            "--password-stdin",
            "--email=a@b.example",
            "--nickname=a",
        ]));
        assert_eq!(
            parsed,
            Ok(Command::CreateAdmin(CreateAdminArgs {
                nickname: "a".to_owned(),
                email: "a@b.example".to_owned(),
            }))
        );
    }

    /// Every required part is demanded by name: the two value flags and
    /// the `--password-stdin` switch.
    #[test]
    fn create_admin_requires_every_flag() {
        let missing_password =
            parse_args(&argv(&["create-admin", "--nickname", "a", "--email", "b"])).unwrap_err();
        assert!(
            missing_password.contains("--password-stdin"),
            "the error names the missing switch: {missing_password}"
        );
        let missing_nickname =
            parse_args(&argv(&["create-admin", "--email", "b", "--password-stdin"])).unwrap_err();
        assert!(
            missing_nickname.contains("--nickname"),
            "the error names the missing flag: {missing_nickname}"
        );
        let missing_email = parse_args(&argv(&[
            "create-admin",
            "--nickname",
            "a",
            "--password-stdin",
        ]))
        .unwrap_err();
        assert!(
            missing_email.contains("--email"),
            "the error names the missing flag: {missing_email}"
        );
    }

    /// A flag without its value, a repeated flag, an empty value, an
    /// attached value on the switch and an unknown flag or command are
    /// all refused.
    #[test]
    fn create_admin_refuses_broken_argument_lists() {
        for args in [
            vec!["create-admin", "--nickname"],
            vec!["create-admin", "--email"],
            vec![
                "create-admin",
                "--nickname",
                "a",
                "--nickname",
                "b",
                "--password-stdin",
            ],
            vec![
                "create-admin",
                "--nickname",
                "  ",
                "--email",
                "b",
                "--password-stdin",
            ],
            vec![
                "create-admin",
                "--password-stdin=yes",
                "--nickname",
                "a",
                "--email",
                "b",
            ],
            vec![
                "create-admin",
                "--password",
                "--nickname",
                "a",
                "--email",
                "b",
            ],
            vec!["create", "--nickname", "a"],
        ] {
            assert!(
                parse_args(&argv(&args)).is_err(),
                "`{args:?}` must be refused"
            );
        }
    }

    /// An unknown first argument is refused with the usage in its message.
    #[test]
    fn unknown_command_is_refused() {
        let err = parse_args(&argv(&["frobnicate"])).unwrap_err();
        assert!(err.contains("frobnicate"), "the error names it: {err}");
        assert!(err.contains(USAGE), "the error carries the usage: {err}");
    }
}
