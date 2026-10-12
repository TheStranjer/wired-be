//! OAuth token/application scopes: parsing, hierarchy matching, validation.
//!
//! Scope strings are Mastodon's (`docs/ARCHITECTURE.md` decision 9): names
//! separated by spaces or, when they came through a URL-encoded form field,
//! `+` signs. The six valid top-level scopes are `read`, `write`, `follow`,
//! `push`, `admin:read` and `admin:write`, each of which also covers its
//! `:subscope` forms (`read:accounts`, `admin:read:reports`, ...).
//!
//! Matching follows Pleroma's `filter_descendants`
//! (`lib/pleroma/web/plugs/o_auth_scopes_plug.ex`): a granted scope satisfies
//! itself and everything under it, plus Mastodon's special case where the
//! `follow` scope also satisfies `read:follows` and `write:follows` (see
//! `Pleroma.Web.OAuth.Scopes`, `lib/pleroma/web/o_auth/scopes.ex`). A
//! narrower scope never satisfies a broader one: a `read:accounts` token
//! cannot reach anything that asks for plain `read`.

use std::fmt;

/// Top-level read scope: grants `read` and every `read:*` sub-scope.
pub const READ: &str = "read";
/// Top-level write scope: grants `write` and every `write:*` sub-scope.
pub const WRITE: &str = "write";
/// Top-level follow scope: grants `follow` itself and the two follow
/// actions, [`READ_FOLLOWS`] and [`WRITE_FOLLOWS`].
pub const FOLLOW: &str = "follow";
/// Top-level push scope: standalone, grants `push` and its own sub-scopes
/// and nothing outside them.
pub const PUSH: &str = "push";
/// Admin scope granting `admin:read` and every `admin:read:*` sub-scope.
pub const ADMIN_READ: &str = "admin:read";
/// Admin scope granting `admin:write` and every `admin:write:*` sub-scope.
pub const ADMIN_WRITE: &str = "admin:write";

/// The read side of the follow actions, granted by `read` and by `follow`.
pub const READ_FOLLOWS: &str = "read:follows";
/// The write side of the follow actions, granted by `write` and by `follow`.
pub const WRITE_FOLLOWS: &str = "write:follows";

/// The read side of account data: `GET /api/v1/announcements`, the list a
/// client shows its user (Pleroma's own pipeline asks only for a user token
/// here, the ticket for its `read:accounts` sub-scope).
pub const READ_ACCOUNTS: &str = "read:accounts";
/// The write side of account data: `POST /api/v1/announcements/{id}/dismiss`,
/// a dismissal a user stores about themselves (Pleroma's `write:accounts`).
pub const WRITE_ACCOUNTS: &str = "write:accounts";

/// The read side of domain blocks: `GET /api/v1/domain_blocks`, the list
/// a client shows its user (Pleroma's `read:blocks` on the
/// `DomainBlockController` `index` action, there aliased by `follow`).
pub const READ_BLOCKS: &str = "read:blocks";
/// The write side of domain blocks: `POST` and `DELETE`
/// `/api/v1/domain_blocks` (Pleroma's `write:blocks` on the controller's
/// remaining actions, with the `follow` root as an accepted alternative).
pub const WRITE_BLOCKS: &str = "write:blocks";

/// The read side of chats: the `/api/v1` and `/api/v2` chat lists and
/// `GET /api/v1/pleroma/chats/{id}` (Pleroma's `read:chats` on `ChatController`'s
/// `index`, `index2` and `show` actions).
pub const READ_CHATS: &str = "read:chats";
/// The write side of chats: opening a chat and the read markers and
/// pin/unpin toggles of `/api/v1/pleroma/chats` (Pleroma's `write:chats`
/// on the matching `ChatController` actions).
pub const WRITE_CHATS: &str = "write:chats";

/// The read side of filters: `GET /api/v1/filters` and
/// `GET /api/v1/filters/{id}`, the list a client shows its user
/// (Pleroma's `read:filters` on the `show`/`index` actions).
pub const READ_FILTERS: &str = "read:filters";
/// The write side of filters: creating, updating and deleting a filter
/// (Pleroma's `write:filters` on the remaining actions).
pub const WRITE_FILTERS: &str = "write:filters";

/// The read side of lists: `GET /api/v1/lists`, `GET /api/v1/lists/{id}`
/// and `GET /api/v1/accounts/{id}/lists`, the collections a client shows
/// its user (Pleroma's `read:lists` on the read actions).
pub const READ_LISTS: &str = "read:lists";
/// The write side of lists: creating, updating and deleting a list
/// (Pleroma's `write:lists` on the remaining actions).
pub const WRITE_LISTS: &str = "write:lists";

/// The write side of media: `POST /api/v1/media` and `POST /api/v2/media`,
/// the upload of a file a later status attaches (Pleroma's `write:media` on
/// the media `create`/`create2` actions).
pub const WRITE_MEDIA: &str = "write:media";

/// The read side of statuses and the positions in them:
/// `GET /api/v1/markers`, the read markers a client shows its user
/// (Pleroma's `read:statuses` on the marker `index` action).
pub const READ_STATUSES: &str = "read:statuses";
/// The write side of statuses and the positions in them:
/// `POST /api/v1/markers`, storing where the account left off reading
/// (Pleroma's `write:statuses` on the marker `upsert` action).
pub const WRITE_STATUSES: &str = "write:statuses";

/// The read side of account security: `GET /api/pleroma/accounts/mfa`
/// (Pleroma's `read:security` scope on the MFA settings action).
pub const READ_SECURITY: &str = "read:security";
/// The write side of account security: the MFA setup, confirmation,
/// backup-code and disable actions (Pleroma's `write:security`).
pub const WRITE_SECURITY: &str = "write:security";

/// The roots of the scope trees [`Scopes::validate`] accepts.
const VALID_ROOTS: [&str; 6] = [READ, WRITE, FOLLOW, PUSH, ADMIN_READ, ADMIN_WRITE];

/// A scope list: the scopes an OAuth token carries, or the ones an
/// application registers.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Scopes(Vec<String>);

impl Scopes {
    /// Parses a scope string: names separated by spaces and/or `+` signs.
    ///
    /// Empty pieces (leading, trailing or repeated separators) are dropped,
    /// duplicate names keep only their first position, and everything else
    /// is preserved verbatim in order; validation is separate
    /// ([`Scopes::validate`]).
    #[must_use]
    pub fn parse(text: &str) -> Self {
        let mut scopes: Vec<String> = Vec::new();
        for name in text.split([' ', '+']) {
            if name.is_empty() || scopes.iter().any(|kept| kept == name) {
                continue;
            }
            scopes.push(name.to_owned());
        }
        Self(scopes)
    }

    /// Whether a token carrying these scopes satisfies the required scope
    /// `required`, per the Mastodon hierarchy.
    ///
    /// A granted scope satisfies itself and every `granted:sub:scope` below
    /// it, never anything above or beside it. `follow` additionally
    /// satisfies [`READ_FOLLOWS`] and [`WRITE_FOLLOWS`], and `push` is
    /// standalone: it reaches nothing outside its own tree.
    #[must_use]
    pub fn contains(&self, required: &str) -> bool {
        self.0.iter().any(|granted| grants(granted, required))
    }

    /// The scope names, in parse order, duplicates already dropped: what
    /// the consent page renders one checkbox for
    /// (`crate::oauth::authorize_page`).
    #[must_use]
    pub fn names(&self) -> &[String] {
        &self.0
    }

    /// Whether every scope in `self` is satisfied by some scope of `other`
    /// (through [`Scopes::contains`] and the same hierarchy): requested
    /// scopes are allowed by an application's registered scopes when the
    /// request is a subset of the registration.
    #[must_use]
    pub fn is_subset_of(&self, other: &Self) -> bool {
        self.0
            .iter()
            .all(|requested| other.contains(requested.as_str()))
    }

    /// Checks that every scope is a known top-level scope or a sub-scope
    /// below one: the roots `read`, `write`, `follow`, `push`, `admin:read`
    /// and `admin:write`, with non-empty lowercase `:subscope` pieces after
    /// them. `admin` without `read`/`write` is not
    /// a scope, and neither is anything else.
    ///
    /// # Errors
    ///
    /// The first offending scope, named in the message; `Ok(())` for the
    /// empty list, which holds nothing to reject.
    pub fn validate(&self) -> Result<(), String> {
        for scope in &self.0 {
            validate_scope(scope)?;
        }
        Ok(())
    }
}

/// A freshly built scope list is `read`: what Pleroma and Mastodon clients
/// assume when a request carries no `scope` parameter.
impl Default for Scopes {
    fn default() -> Self {
        Self(vec![READ.to_owned()])
    }
}

/// The scope list renders as its names joined by single spaces, the form
/// APIs send it in.
impl fmt::Display for Scopes {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.0.join(" "))
    }
}

/// Whether the single granted scope `granted` satisfies `required`.
fn grants(granted: &str, required: &str) -> bool {
    if granted == required {
        return true;
    }
    // Mastodon's `follow` also covers the follow actions of the other trees.
    if granted == FOLLOW && (required == READ_FOLLOWS || required == WRITE_FOLLOWS) {
        return true;
    }
    // Broader grants narrower: `read` covers `read:accounts` — and only a
    // `:` boundary may follow, so `read` never grants a sibling like
    // `readx`, while `read:accounts` never grants `read`.
    required
        .strip_prefix(granted)
        .is_some_and(|rest| rest.starts_with(':'))
}

/// Validates a single scope name against the known trees and the sub-scope
/// grammar.
fn validate_scope(scope: &str) -> Result<(), String> {
    if !in_known_tree(scope) {
        return Err(format!("unknown top-level scope: {scope}"));
    }
    if !scope.split(':').all(is_valid_segment) {
        return Err(format!("malformed scope: {scope}"));
    }
    Ok(())
}

/// Whether `scope` is one of the roots or hangs below one of them (with the
/// `:` boundary that keeps `readx` out of the `read` tree).
fn in_known_tree(scope: &str) -> bool {
    VALID_ROOTS.iter().any(|root| {
        scope == *root
            || scope
                .strip_prefix(*root)
                .is_some_and(|rest| rest.starts_with(':'))
    })
}

/// Sub-scope pieces are non-empty lowercase names: ASCII letters, digits
/// and underscores.
fn is_valid_segment(segment: &str) -> bool {
    !segment.is_empty()
        && segment
            .chars()
            .all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == '_')
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn names_returns_the_parsed_order() {
        let scopes = Scopes::parse("read write+follow read");
        assert_eq!(
            scopes.names().join(" "),
            "read write follow",
            "deduplicated, in parse order"
        );
        assert_eq!(
            Scopes::default().names().join(" "),
            "read",
            "the default holds just `read`"
        );
        assert!(Scopes::parse("").names().is_empty(), "nothing parsed");
    }

    #[test]
    fn parse_splits_on_spaces_and_plusses() {
        assert_eq!(
            Scopes::parse("read write").to_string(),
            "read write",
            "spaces separate"
        );
        assert_eq!(
            Scopes::parse("read+write").to_string(),
            "read write",
            "plusses separate too"
        );
        assert_eq!(
            Scopes::parse("read+write follow push+admin:read").to_string(),
            "read write follow push admin:read",
            "the separators mix freely"
        );
    }

    #[test]
    fn parse_drops_empties_and_dedupes_keeping_order() {
        assert_eq!(
            Scopes::parse("  read ++ write ").to_string(),
            "read write",
            "stray separators vanish"
        );
        assert_eq!(
            Scopes::parse("+push+").to_string(),
            "push",
            "a lone scope survives its separators"
        );
        assert_eq!(
            Scopes::parse("write read+write").to_string(),
            "write read",
            "the duplicate keeps only its first position"
        );
        assert_eq!(Scopes::parse("").to_string(), "", "nothing to join");
        assert_eq!(
            Scopes::parse(" + ").to_string(),
            "",
            "separators alone are nothing"
        );
    }

    #[test]
    fn default_is_read() {
        let scopes = Scopes::default();
        assert_eq!(scopes.to_string(), READ, "the default list is just read");
        assert!(
            scopes.contains("read:statuses"),
            "and it works like any read token"
        );
        if let Err(reason) = scopes.validate() {
            panic!("the default must validate, got: {reason}");
        }
    }

    #[test]
    fn display_joins_with_spaces() {
        assert_eq!(
            Scopes::parse("read write follow").to_string(),
            "read write follow",
            "round-trip of a plain string"
        );
        assert_eq!(
            Scopes::parse("read:accounts+write:statuses").to_string(),
            "read:accounts write:statuses",
            "sub-scopes pass through verbatim"
        );
    }

    #[test]
    fn read_grants_the_read_tree() {
        let scopes = Scopes::parse(READ);
        for required in [READ, "read:accounts", "read:statuses", READ_FOLLOWS] {
            assert!(scopes.contains(required), "`read` grants {required}");
        }
        for absent in [WRITE, FOLLOW, PUSH, ADMIN_READ, ADMIN_WRITE] {
            assert!(!scopes.contains(absent), "`read` does not grant {absent}");
        }
    }

    #[test]
    fn write_grants_the_write_tree_only() {
        let scopes = Scopes::parse(WRITE);
        for required in [WRITE, "write:accounts", "write:statuses", WRITE_FOLLOWS] {
            assert!(scopes.contains(required), "`write` grants {required}");
        }
        for absent in [READ, READ_FOLLOWS, FOLLOW, ADMIN_WRITE] {
            assert!(
                !scopes.contains(absent),
                "`write` does not reach into other trees ({absent})"
            );
        }
    }

    #[test]
    fn follow_grants_both_follows_actions() {
        let scopes = Scopes::parse(FOLLOW);
        for required in [FOLLOW, READ_FOLLOWS, WRITE_FOLLOWS] {
            assert!(scopes.contains(required), "`follow` grants {required}");
        }
        for absent in [READ, WRITE, "read:accounts", PUSH] {
            assert!(
                !scopes.contains(absent),
                "`follow` stays inside its own actions ({absent} is elsewhere)"
            );
        }
    }

    #[test]
    fn admin_scopes_grant_their_own_trees() {
        let read = Scopes::parse(ADMIN_READ);
        for required in [ADMIN_READ, "admin:read:reports", "admin:read:me"] {
            assert!(read.contains(required), "`admin:read` grants {required}");
        }
        for absent in [ADMIN_WRITE, "admin:write:reports", READ, WRITE] {
            assert!(
                !read.contains(absent),
                "`admin:read` does not grant {absent}"
            );
        }
        let write = Scopes::parse(ADMIN_WRITE);
        for required in [ADMIN_WRITE, "admin:write:accounts"] {
            assert!(write.contains(required), "`admin:write` grants {required}");
        }
        assert!(
            !write.contains(ADMIN_READ),
            "`admin:write` does not grant `admin:read`"
        );
    }

    #[test]
    fn push_is_standalone() {
        let scopes = Scopes::parse(PUSH);
        assert!(scopes.contains(PUSH), "`push` grants itself");
        for absent in [
            READ,
            WRITE,
            FOLLOW,
            ADMIN_READ,
            ADMIN_WRITE,
            "read:accounts",
        ] {
            assert!(
                !scopes.contains(absent),
                "`push` grants neither {absent} nor its tree"
            );
        }
    }

    #[test]
    fn narrower_never_grants_broader() {
        let narrow = Scopes::parse("read:accounts");
        assert!(narrow.contains("read:accounts"), "it still grants itself");
        for absent in [READ, "read:statuses", WRITE, FOLLOW, ADMIN_READ] {
            assert!(
                !narrow.contains(absent),
                "`read:accounts` grants neither `read` nor siblings ({absent})"
            );
        }
        let deep = Scopes::parse("admin:read:reports");
        assert!(
            !deep.contains(ADMIN_READ),
            "`admin:read:reports` does not grant `admin:read`"
        );
        assert!(
            !deep.contains("admin:read:report"),
            "prefix matching is per `:` piece, not per character"
        );
    }

    #[test]
    fn subset_checks_use_the_hierarchy() {
        assert!(
            Scopes::parse("read:accounts write:statuses")
                .is_subset_of(&Scopes::parse("read write")),
            "sub-scopes fit inside their trees"
        );
        assert!(
            Scopes::parse("read:follows write:follows").is_subset_of(&Scopes::parse(FOLLOW)),
            "the follows pair fits inside `follow`"
        );
        assert!(
            Scopes::parse("read+write admin:read")
                .is_subset_of(&Scopes::parse("read write admin:read")),
            "identical lists are subsets of each other"
        );
        assert!(
            !Scopes::parse(READ).is_subset_of(&Scopes::parse(READ_FOLLOWS)),
            "a broad request is not covered by a narrow registration"
        );
        assert!(
            !Scopes::parse("read follow").is_subset_of(&Scopes::parse(READ)),
            "`follow` is not inside `read`'s tree"
        );
        assert!(
            !Scopes::parse("read write").is_subset_of(&Scopes::parse(READ)),
            "missing `write` fails the subset check"
        );
    }

    #[test]
    fn empty_is_a_subset_of_everything() {
        let empty = Scopes::parse("");
        assert!(
            empty.is_subset_of(&Scopes::default()),
            "nothing requested, nothing missing"
        );
        assert!(empty.is_subset_of(&empty), "vacuously of itself too");
    }

    #[test]
    fn validate_accepts_the_known_trees() {
        for scope in [
            READ,
            WRITE,
            FOLLOW,
            PUSH,
            ADMIN_READ,
            ADMIN_WRITE,
            "read:accounts",
            "write:statuses",
            "read:follows",
            "follow:graph",
            "push:web",
            "admin:read:reports",
            "admin:write:accounts",
            "admin:read:me",
        ] {
            let parsed = Scopes::parse(scope);
            if let Err(reason) = parsed.validate() {
                panic!("`{scope}` must validate, got: {reason}");
            }
        }
    }

    #[test]
    fn validate_rejects_garbage() {
        for scope in [
            "bogus",          // unknown top-level
            "admin",          // admin only exists split into read and write
            "admin:foo",      // unknown admin branch
            "admin:readx",    // not a root and not under one
            "readx",          // a root is not granted by a longer word
            "READ",           // wrong case
            "read:",          // empty sub-scope
            ":read",          // empty top-level
            "read::accounts", // empty piece in the middle
            "read:Accounts",  // wrong case in a sub-scope
            "read:ac-counts", // not a name character
        ] {
            let reason = Scopes::parse(scope)
                .validate()
                .expect_err(&format!("`{scope}` must not validate"));
            assert!(reason.contains(scope), "the error names {scope}");
        }
    }

    #[test]
    fn validate_names_the_first_offender_in_a_list() {
        let scopes = Scopes::parse("read+write+bogus+push");
        let reason = scopes
            .validate()
            .expect_err("`bogus` must not validate the list");
        assert!(reason.contains("bogus"), "the message names the offender");
    }

    #[test]
    fn the_empty_list_has_nothing_to_reject() {
        if let Err(reason) = Scopes::parse("").validate() {
            panic!("an empty list must validate, got: {reason}");
        }
    }
}
