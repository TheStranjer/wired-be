//! Migration tests, run against a slot of the pooled fixture in
//! [`crate::test_db`].
//!
//! Each test inspects the schema [`MIGRATOR`] leaves behind (the
//! `MIGRATOR` itself lives in [`crate::server`]): one child module per
//! table area holds the `#[tokio::test]` functions that read
//! `pg_catalog` and `information_schema`, and the shared row shapes,
//! expectation tables and assertion helpers live here.

#[cfg(test)]
mod auth;
#[cfg(test)]
mod base;
#[cfg(test)]
mod canonical_reactions;
#[cfg(test)]
mod chats;
#[cfg(test)]
mod oauth_token_digests;
#[cfg(test)]
mod poll_notifications;
#[cfg(test)]
mod push;
#[cfg(test)]
mod reaction_emoji_origin;
#[cfg(test)]
mod status_fk_indexes;
#[cfg(test)]
mod sweep_indexes;
#[cfg(test)]
mod users;

/// One foreign key of the three tables: its name, the table it
/// was declared on, its delete action and what it points at.
#[derive(Debug, sqlx::FromRow)]
struct ForeignKey {
    /// The table carrying the constraint.
    relname: String,
    /// The constraint's name.
    conname: String,
    /// Its `ON DELETE` action, as PostgreSQL spells it.
    confdeltype: String,
    /// The table it references.
    target: String,
}

/// One column of the three tables, as `information_schema`
/// describes it.
#[derive(Debug, sqlx::FromRow)]
struct SchemaColumn {
    /// The table it belongs to.
    table_name: String,
    /// Its own name.
    column_name: String,
    /// The default expression, `None` when it has none.
    column_default: Option<String>,
    /// `NO` for a `NOT NULL` column, `YES` otherwise.
    is_nullable: String,
}

/// One column of the table, as `information_schema` describes
/// it.
#[derive(Debug, sqlx::FromRow)]
struct RuleColumn {
    /// Its name.
    column_name: String,
    /// The declared type.
    data_type: String,
    /// `NO` for a `NOT NULL` column, `YES` otherwise.
    is_nullable: String,
    /// The default expression, `None` when it has none.
    column_default: Option<String>,
}

/// One column of the table, as `information_schema` describes it.
#[derive(Debug, sqlx::FromRow)]
struct StateColumn {
    /// Its name.
    column_name: String,
    /// The declared type.
    data_type: String,
    /// `NO` for a `NOT NULL` column, `YES` otherwise.
    is_nullable: String,
    /// The default expression, `None` when it has none.
    column_default: Option<String>,
}

/// A column as a `(name, type, nullability, default)` tuple, the shape
/// the `oauth_login_states` expectation is written in.
type Described = (String, String, String, Option<String>);
/// One expected foreign key: the table it was declared on, its name,
/// its delete action and the table it points at.
type ExpectedKey = (&'static str, &'static str, &'static str, &'static str);

/// The indexes the statuses migrations create on `statuses`: the
/// primary key, the uri lookup, the six the timeline and thread
/// queries read, and the partial unique key that lets an account
/// reblog a given status once.
const STATUS_INDEXES: [&str; 9] = [
    "statuses_pkey",
    "statuses_uri_key",
    "statuses_user_id_idx",
    "statuses_in_reply_to_id_idx",
    "statuses_conversation_id_idx",
    "statuses_reblog_of_id_idx",
    "statuses_visibility_idx",
    "statuses_quote_of_id_idx",
    "statuses_user_id_reblog_of_id_key",
];
/// The live-row-partial statuses indexes: the two newest-first
/// listings, so tombstones drop out of them for free, and the
/// unique reblog pair, so a withdrawn boost frees the pair again.
const PARTIAL_STATUS_INDEXES: [&str; 3] = [
    "statuses_user_id_idx",
    "statuses_visibility_idx",
    "statuses_user_id_reblog_of_id_key",
];
/// The foreign keys the chats migration declares: table, constraint,
/// `ON DELETE` action and target.
const CHAT_FOREIGN_KEYS: [ExpectedKey; 6] = [
    ("chats", "chats_user_id_fkey", "c", "users"),
    ("chats", "chats_recipient_id_fkey", "c", "users"),
    (
        "chat_messages",
        "chat_messages_sender_id_fkey",
        "c",
        "users",
    ),
    (
        "chat_messages",
        "chat_messages_attachment_media_id_fkey",
        "n",
        "media_attachments",
    ),
    (
        "chat_message_references",
        "chat_message_references_chat_id_fkey",
        "c",
        "chats",
    ),
    (
        "chat_message_references",
        "chat_message_references_message_id_fkey",
        "c",
        "chat_messages",
    ),
];
/// The unique keys the chats migration declares.
const CHAT_UNIQUE_KEYS: [(&str, &str); 3] = [
    ("chats", "chats_user_id_recipient_id_key"),
    (
        "chat_message_references",
        "chat_message_references_chat_id_message_id_key",
    ),
    ("chat_messages", "chat_messages_ap_id_key"),
];
/// The indexes the chats migration declares for its list queries.
const CHAT_INDEXES: [&str; 6] = [
    "chats_user_id_updated_at_idx",
    "chats_recipient_id_idx",
    "chat_messages_sender_id_idx",
    "chat_message_references_chat_id_idx",
    "chat_message_references_chat_id_unread_idx",
    "chat_message_references_message_id_idx",
];
/// The `chat_messages` columns the migration defaults for the model,
/// and the columns it makes `NOT NULL`.
const CHAT_DEFAULTS: [(&str, &str, &str); 4] = [
    ("chats", "pinned", "false"),
    ("chat_messages", "content", "''::text"),
    ("chat_messages", "raw_text", "''::text"),
    ("chat_message_references", "unread", "true"),
];
/// The columns of the three chat tables no row may leave unset.
const CHAT_REQUIRED_COLUMNS: [(&str, &str); 5] = [
    ("chats", "user_id"),
    ("chats", "recipient_id"),
    ("chat_messages", "sender_id"),
    ("chat_message_references", "chat_id"),
    ("chat_message_references", "message_id"),
];
/// The `instance_rules` columns and their types, in the order the
/// migration declares them.
const RULE_COLUMNS: [(&str, &str); 6] = [
    ("id", "uuid"),
    ("text", "text"),
    ("hint", "text"),
    ("priority", "integer"),
    ("inserted_at", "timestamp with time zone"),
    ("updated_at", "timestamp with time zone"),
];
/// The timestamps of a table, defaulted by the database to `now()`.
const NOW_COLUMNS: [&str; 2] = ["inserted_at", "updated_at"];

/// The `indexname`s of `rows`, the `pg_indexes` rows of one table.
fn index_names(rows: &[(String, String)]) -> Vec<&str> {
    rows.iter().map(|(name, _)| name.as_str()).collect()
}

/// The `indexdef` of the index named `name` in `rows`.
///
/// # Panics
///
/// If `rows` carries no such index.
fn index_definition<'rows>(rows: &'rows [(String, String)], name: &str) -> &'rows str {
    rows.iter()
        .find(|(indexname, _)| indexname == name)
        .map_or_else(
            || panic!("{name} exists: {rows:?}"),
            |(_, definition)| definition.as_str(),
        )
}

/// The row of `columns` describing `table`'s column `name`.
///
/// # Panics
///
/// If `columns` does not carry that column.
fn column_of<'columns>(
    columns: &'columns [SchemaColumn],
    table: &str,
    name: &str,
) -> &'columns SchemaColumn {
    columns
        .iter()
        .find(|row| row.table_name == table && row.column_name == name)
        .unwrap_or_else(|| panic!("{table}.{name} exists"))
}

/// The row of `columns` describing the `instance_rules` column `name`.
///
/// # Panics
///
/// If `columns` does not carry that column.
fn rule_column<'columns>(columns: &'columns [RuleColumn], name: &str) -> &'columns RuleColumn {
    columns
        .iter()
        .find(|column| column.column_name == name)
        .unwrap_or_else(|| panic!("instance_rules.{name} exists"))
}

/// `columns` as `(name, type, nullability, default)` tuples.
fn described_of(columns: Vec<StateColumn>) -> Vec<Described> {
    columns
        .into_iter()
        .map(|column| {
            (
                column.column_name,
                column.data_type,
                column.is_nullable,
                column.column_default,
            )
        })
        .collect()
}

/// Every index the statuses migrations create, the three partial on
/// live rows only, and the one live reblog pair per account unique.
fn assert_statuses_indexes(rows: &[(String, String)]) {
    let names = index_names(rows);
    for expected in STATUS_INDEXES {
        assert!(names.contains(&expected), "{expected} indexes statuses");
    }
    for partial in PARTIAL_STATUS_INDEXES {
        assert!(
            index_definition(rows, partial).contains("deleted_at IS NULL"),
            "{partial} is partial on live rows only"
        );
    }
    assert!(
        index_definition(rows, "statuses_user_id_reblog_of_id_key").contains("UNIQUE"),
        "the live reblog pair indexes unique, so an account reblogs a status once"
    );
}

/// Every foreign key the chats migration declares, with its delete
/// action and target.
fn assert_chat_foreign_keys(keys: &[ForeignKey]) {
    for (table, constraint, action, target) in CHAT_FOREIGN_KEYS {
        let found = keys
            .iter()
            .find(|key| key.relname == table && key.conname == constraint)
            .unwrap_or_else(|| panic!("{constraint} links {table}"));
        assert_eq!(
            found.confdeltype, action,
            "{constraint} deletes as {action}"
        );
        assert_eq!(found.target, target, "{constraint} points at {target}");
    }
}

/// The unique keys that keep one chat per participant and account, one
/// reference per chat and message, and one `ap_id` per message.
fn assert_chat_unique_keys(uniques: &[(String, String)]) {
    for (table, constraint) in CHAT_UNIQUE_KEYS {
        assert!(
            uniques
                .iter()
                .any(|(relname, conname)| relname == table && conname == constraint),
            "{constraint} keeps {table} free of duplicates"
        );
    }
}

/// The indexes the chat list queries read.
fn assert_chat_indexes(indexes: &[(String, String)]) {
    for name in CHAT_INDEXES {
        assert!(
            indexes.iter().any(|(indexname, _)| indexname == name),
            "{name} serves a chat query"
        );
    }
}

/// The defaults the chats migration leaves the model to rely on, and
/// the `NOT NULL`s it fills in for them.
fn assert_chat_column_defaults(columns: &[SchemaColumn]) {
    for (table, column, default) in CHAT_DEFAULTS {
        let row = column_of(columns, table, column);
        assert_eq!(
            row.column_default.as_deref(),
            Some(default),
            "{table}.{column} defaults to {default}"
        );
        assert_eq!(row.is_nullable, "NO", "{table}.{column} is NOT NULL");
    }
}

/// The columns of the three chat tables no row may leave unset.
fn assert_chat_required_columns(columns: &[SchemaColumn]) {
    for (table, column) in CHAT_REQUIRED_COLUMNS {
        assert_eq!(
            column_of(columns, table, column).is_nullable,
            "NO",
            "{table}.{column} cannot be null"
        );
    }
}

/// The type and nullability of every `instance_rules` column.
fn assert_rule_column_types(columns: &[RuleColumn]) {
    for (name, data_type) in RULE_COLUMNS {
        assert_eq!(
            rule_column(columns, name).data_type,
            data_type,
            "instance_rules.{name} is {data_type}"
        );
        assert_eq!(
            rule_column(columns, name).is_nullable,
            "NO",
            "instance_rules.{name} is NOT NULL"
        );
    }
}

/// The two timestamps the `instance_rules` migration stamps itself.
fn assert_rule_timestamps(columns: &[RuleColumn]) {
    for name in NOW_COLUMNS {
        assert_eq!(
            rule_column(columns, name).column_default.as_deref(),
            Some("now()"),
            "{name} defaults to now()"
        );
    }
}
