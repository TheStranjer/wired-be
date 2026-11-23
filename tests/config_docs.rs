//! Config documentation guard (decision 12): every environment variable
//! name the configuration code declares must appear in `.env.example` and
//! in the README configuration table.
//!
//! The declaration pattern is `pub const NAME_VAR: &str = "ENV_NAME"` in
//! the configuration code (`src/config.rs` and any `src/config/`
//! submodule). This crate scans `src/` recursively, collects every name
//! those constants hold, and asserts both documents carry each of them:
//!
//! - `.env.example`, as a line starting `NAME=` or `# NAME=` (secrets
//!   there must be placeholder values, never real material);
//! - the README configuration table, as a row starting `` | `NAME` | ``.
//!
//! The scan also enforces the declaration site and the naming convention:
//! an environment variable name may only be spelled in the configuration
//! module, and its constant is named `<ENV_NAME>_VAR`.
//!
//! This file is deliberately standalone — it reads text files and needs
//! no database, no router and no `wired_be` symbols — so it costs nothing
//! of the relink the one-test-crate rule (decision 13) exists to avoid.

#![cfg(test)]

use std::{
    collections::BTreeMap,
    fs::File,
    io::{BufRead as _, BufReader},
    path::{Path, PathBuf},
};

/// One `pub const <CONST>: &str = "<NAME>"` declaration: the environment
/// variable name it holds, the constant's name, and the declaration
/// location relative to `src/` (`file:line`).
#[derive(Clone, Debug, PartialEq, Eq, PartialOrd, Ord)]
struct Declaration {
    /// The constant's name, e.g. `DATABASE_URL_VAR`.
    constant: String,
    /// The environment variable name it holds, e.g. `DATABASE_URL`.
    env_name: String,
    /// Where it is declared, relative to `src/`, as `file:line`.
    location: String,
}

/// Every declaration the scan found, keyed by environment variable name.
type Declarations = BTreeMap<String, Vec<Declaration>>;

/// The declaration lines found under `src/`, keyed by the environment
/// variable name they hold.
fn scan_declarations(src: &Path) -> Declarations {
    let mut found: Declarations = BTreeMap::new();
    for file in rust_files(src) {
        let reader = BufReader::new(
            File::open(&file).unwrap_or_else(|err| panic!("{} is readable: {err}", file.display())),
        );
        for (number, line) in reader.lines().enumerate() {
            let line = line.unwrap_or_else(|err| panic!("{} reads as text: {err}", file.display()));
            if let Some(declaration) = parse_declaration(&line, &file, src, number + 1) {
                found
                    .entry(declaration.env_name.clone())
                    .or_default()
                    .push(declaration);
            }
        }
    }
    found
}

/// The `file:line` of `path` relative to `src/`.
fn location(path: &Path, src: &Path, line: usize) -> String {
    let relative = path
        .strip_prefix(src)
        .unwrap_or_else(|_| panic!("{} is under {}", path.display(), src.display()));
    format!("{}:{line}", relative.display())
}

/// The (constant, environment name) pair of one
/// `pub const NAME: &str = "VALUE";` line, and `None` for every other
/// line. Values that cannot be environment variable names are skipped so
/// the scan covers test helpers and non-config modules without noise.
fn env_var_assignment(line: &str) -> Option<(&str, &str)> {
    let body = line.trim();
    let body = body.strip_suffix(';').unwrap_or(body).trim_end();
    let rest = body.strip_prefix("pub const ")?;
    let (constant, tail) = rest.split_once(": &str = ")?;
    let value = tail.strip_prefix('"')?.strip_suffix('"')?;
    if !is_env_name(value) {
        return None;
    }
    Some((constant.trim(), value))
}

/// Whether `name` is a well-formed environment variable name: an uppercase
/// letter, then uppercase letters, digits or underscores.
fn is_env_name(name: &str) -> bool {
    let mut letters = name.chars();
    letters.next().is_some_and(|c| c.is_ascii_uppercase())
        && name
            .chars()
            .all(|c| c.is_ascii_uppercase() || c.is_ascii_digit() || c == '_')
}

/// The [`Declaration`] of one scanned line, if it declares an environment
/// variable name.
fn parse_declaration(line: &str, file: &Path, src: &Path, number: usize) -> Option<Declaration> {
    let (constant, env_name) = env_var_assignment(line)?;
    // Only the configuration naming convention is a declaration of a
    // setting: the constant `<ENV_NAME>_VAR`. Anything else that happens
    // to hold an upper-case string is none of this guard's business.
    if !constant.ends_with("_VAR") {
        return None;
    }
    Some(Declaration {
        constant: constant.to_owned(),
        env_name: env_name.to_owned(),
        location: location(file, src, number),
    })
}

/// Every `.rs` file under `dir`, recursively, in sorted order so a
/// violation names the same file on every run.
fn rust_files(dir: &Path) -> Vec<PathBuf> {
    let mut files = Vec::new();
    let mut pending = vec![dir.to_path_buf()];
    while let Some(current) = pending.pop() {
        for entry in std::fs::read_dir(&current)
            .unwrap_or_else(|err| panic!("{} is listable: {err}", current.display()))
        {
            let path = entry
                .unwrap_or_else(|err| panic!("{} has readable entries: {err}", current.display()))
                .path();
            if path.is_dir() {
                pending.push(path);
            } else if path.extension().is_some_and(|ext| ext == "rs") {
                files.push(path);
            }
        }
    }
    files.sort();
    files
}

/// The whole file at `path` with a leading newline, so a document line can
/// always be matched by its `\n`-anchored prefix.
fn read_padded(path: &Path) -> String {
    let mut text = std::fs::read_to_string(path)
        .unwrap_or_else(|err| panic!("{} is readable text: {err}", path.display()));
    text.insert(0, '\n');
    text
}

/// Whether `document` names the variable as a `.env`-style assignment at
/// the start of a line: `NAME=` or `# NAME=` (the commented spelling is
/// how an optional setting is documented).
fn documented_in_env(document: &str, name: &str) -> bool {
    document.contains(&format!("\n{name}=")) || document.contains(&format!("\n# {name}="))
}

/// Whether `document` carries the README configuration-table row for the
/// variable, which starts `` | `NAME` | ``.
fn documented_in_readme(document: &str, name: &str) -> bool {
    document.contains(&format!("| `{name}` |"))
}

/// The directory of the package under test, read when the test runs.
///
/// Cargo sets `CARGO_MANIFEST_DIR` for the test process, so this follows the
/// checkout the suite was started in. The compile-time `env!` spelling bakes
/// the path of whichever checkout built the binary into it, and checkouts that
/// share a build directory then scan each other's (possibly deleted) files.
fn manifest_dir() -> PathBuf {
    PathBuf::from(
        std::env::var_os("CARGO_MANIFEST_DIR")
            .expect("cargo sets CARGO_MANIFEST_DIR for the test process"),
    )
}

/// The configuration module is the one place an environment variable name
/// is spelled: `src/config.rs` or a `src/config/` submodule. A straggler
/// declared elsewhere must be moved there (and re-exported if needed).
#[test]
fn env_var_names_are_declared_in_the_config_module() {
    let src = manifest_dir().join("src");
    let mut strays: Vec<String> = Vec::new();
    for declarations in scan_declarations(&src).values() {
        for declaration in declarations {
            let home =
                declaration.location.split(':').next().unwrap_or_else(|| {
                    panic!("a location carries its file: {}", declaration.location)
                });
            if !declared_in_config_module(home) {
                strays.push(format!(
                    "{} = \"{}\" at {}",
                    declaration.constant, declaration.env_name, declaration.location
                ));
            }
        }
    }
    assert!(
        strays.is_empty(),
        "environment variable names must be declared in src/config.rs or a \
         src/config/ submodule; found them elsewhere:\n{}",
        strays.join("\n")
    );
}

/// Whether `home` — a file path relative to `src/`, semicolon suffix of
/// the `file:line` location — names the configuration module.
fn declared_in_config_module(home: &str) -> bool {
    home == "config.rs" || home.starts_with("config/")
}

/// A constant named `<ENV_NAME>_VAR` holds the name `<ENV_NAME>`, so the
/// two spellings never drift apart.
#[test]
fn env_var_constants_follow_the_naming_convention() {
    let src = manifest_dir().join("src");
    let mut misnamed: Vec<String> = Vec::new();
    for declarations in scan_declarations(&src).values() {
        for declaration in declarations {
            if declaration.constant != format!("{}_VAR", declaration.env_name) {
                misnamed.push(format!(
                    "{} holds \"{}\" at {}",
                    declaration.constant, declaration.env_name, declaration.location
                ));
            }
        }
    }
    assert!(
        misnamed.is_empty(),
        "every `pub const NAME_VAR: &str = \"ENV_NAME\"` must pair the constant \
         ENV_NAME_VAR with the value ENV_NAME; these do not:\n{}",
        misnamed.join("\n")
    );
}

/// The heart of the guard: every environment variable name declared in
/// `src/` is documented in both `.env.example` and the README table, and
/// the scan actually sees the settings the server cannot boot without.
#[test]
fn every_env_var_is_documented() {
    let root = manifest_dir();
    let declarations = scan_declarations(&root.join("src"));
    for core in ["DATABASE_URL", "BASE_URL", "BIND_ADDR", "SECRET_KEY_BASE"] {
        assert!(
            declarations.contains_key(core),
            "the scan must find the {core} declaration; a pattern change here \
             silently voids this guard"
        );
    }
    let env_example = read_padded(&root.join(".env.example"));
    let readme = read_padded(&root.join("README.md"));
    let mut missing: Vec<String> = Vec::new();
    for (name, entries) in &declarations {
        let where_declared = entries
            .iter()
            .map(|d| d.location.as_str())
            .collect::<Vec<_>>()
            .join(", ");
        if !documented_in_env(&env_example, name) {
            missing.push(format!(
                "{name} has no .env.example line (declared at {where_declared})"
            ));
        }
        if !documented_in_readme(&readme, name) {
            missing.push(format!(
                "{name} has no README table row (declared at {where_declared})"
            ));
        }
    }
    assert!(
        missing.is_empty(),
        "every environment variable named by a `pub const *_VAR` in src/ must be \
         documented in .env.example (`NAME=` or `# NAME=`) and in the README \
         configuration table (a row starting with `` | `NAME` | ``); missing:\n{}",
        missing.join("\n")
    );
}

/// The line scanner itself: it recognizes the declaration shape (with and
/// without the trailing semicolon, with surrounding indentation), ignores
/// everything that is not a `_VAR` constant or an environment-name value,
/// and the document matchers only answer for a line-initial spelling.
#[test]
fn scanners_recognize_the_documented_shapes() {
    let declared = "pub const SMTP_HOST_VAR: &str = \"SMTP_HOST\";";
    assert_eq!(
        env_var_assignment(declared),
        Some(("SMTP_HOST_VAR", "SMTP_HOST"))
    );
    assert_eq!(
        env_var_assignment(&format!("    {declared}")),
        Some(("SMTP_HOST_VAR", "SMTP_HOST"))
    );
    assert_eq!(
        env_var_assignment("const SMTP_HOST_VAR: &str = \"SMTP_HOST\";"),
        None,
        "a private const is not a declaration"
    );
    assert_eq!(
        env_var_assignment("pub const SMTP_HOST_LABEL: &str = \"SMTP_HOST\";"),
        Some(("SMTP_HOST_LABEL", "SMTP_HOST")),
        "the value alone is what env_var_assignment reads"
    );
    assert_eq!(
        env_var_assignment("pub const DEFAULT_MAIL_FROM_USER: &str = \"noreply\";"),
        None,
        "a lowercase value is no environment variable name"
    );
    assert_eq!(
        parse_declaration(
            "pub const SMTP_HOST_LABEL: &str = \"SMTP_HOST\";",
            Path::new("/repo/src/api/misc.rs"),
            Path::new("/repo/src"),
            3
        )
        .map(|d| (d.constant, d.env_name, d.location)),
        None,
        "only the `<ENV_NAME>_VAR` naming convention declares a setting"
    );
    assert_eq!(
        parse_declaration(
            declared,
            Path::new("/repo/src/config.rs"),
            Path::new("/repo/src"),
            7
        )
        .map(|d| (d.constant, d.env_name, d.location)),
        Some((
            "SMTP_HOST_VAR".to_owned(),
            "SMTP_HOST".to_owned(),
            "config.rs:7".to_owned()
        ))
    );
    let document = "\nSMTP_HOST=x\n# BIND_ADDR=y\nsee DATABASE_URL=z\n";
    assert!(documented_in_env(document, "SMTP_HOST"));
    assert!(documented_in_env(document, "BIND_ADDR"));
    assert!(
        !documented_in_env(document, "DATABASE_URL"),
        "a mid-line mention is not a documentation line"
    );
    assert!(
        !documented_in_readme(document, "SMTP_HOST"),
        "the configuration-table matcher needs the row pipes"
    );
    assert!(documented_in_readme(
        "| `SMTP_HOST` | no | | x |",
        "SMTP_HOST"
    ));
}
