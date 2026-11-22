//! Integration tests for `GET /api/v1/accounts/verify_credentials`,
//! `GET /api/v1/accounts/{id}` and
//! `GET /api/v1/accounts/lookup`: the core `Account` entity with its
//! `pleroma` extension block for local and remote rows without credentials,
//! the `source` block and owner view only `verify_credentials` renders
//! (raw note and fields, posting defaults, the pending follow-request
//! count, and the `read:accounts` scope, user-token auth and 401/403
//! rejections in front of them),
//! the privacy rules the request's token decides (hidden follow counts and
//! hidden role flags, and the owner-only keys of the block), the status
//! counters a posted status moves, an invalid token still a 401, every miss
//! (garbage id, unknown id, deactivated account, unresolvable acct) the same
//! 404 "Record not found", `acct` resolved case-insensitively with this
//! instance's own domain stripped, an absent `acct` a 400 "Missing
//! parameter: acct", `q` that parameter's alias, and `resolve=true` a

#![cfg(test)]

mod lookup;
mod show;
mod support;
mod verify_credentials;
