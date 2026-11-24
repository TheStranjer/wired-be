//! The external-identity, login-state and password-reset tables.

use super::*;

/// The external-identity migration (WBE-218) creates the table the
/// `/oauth/{provider}/callback` looks a login up by: the minted
/// `uuid` primary key, the `user_id` foreign key that takes a
/// provider link down with its account, and the `(provider, uid)`
/// pair that makes one provider account exactly one login.
#[tokio::test]
async fn user_identities_migration_links_providers_to_users() {
    let db = crate::test_db::test_db().await.expect("test database slot");
    let pool = db.pool().clone();
    let columns: Vec<(String, String, String)> = sqlx::query_as(
        "SELECT column_name, data_type, is_nullable
           FROM information_schema.columns
          WHERE table_schema = 'public' AND table_name = 'user_identities'
       ORDER BY ordinal_position",
    )
    .fetch_all(&pool)
    .await
    .unwrap();
    assert_eq!(
        columns,
        vec![
            ("id".to_owned(), "uuid".to_owned(), "NO".to_owned()),
            ("user_id".to_owned(), "uuid".to_owned(), "NO".to_owned()),
            ("provider".to_owned(), "text".to_owned(), "NO".to_owned()),
            ("uid".to_owned(), "text".to_owned(), "NO".to_owned()),
            (
                "inserted_at".to_owned(),
                "timestamp with time zone".to_owned(),
                "NO".to_owned()
            ),
        ],
        "the ticket's five columns, in the ticket's order"
    );

    let (key, key_columns): (String, String) = sqlx::query_as(
        "SELECT con.conname, string_agg(att.attname, ',' ORDER BY att.attname)
           FROM pg_constraint con
           JOIN pg_class cls ON cls.oid = con.conrelid
           JOIN pg_attribute att ON att.attrelid = cls.oid AND att.attnum = ANY(con.conkey)
          WHERE con.contype = 'p' AND cls.relname = 'user_identities'
       GROUP BY con.conname",
    )
    .fetch_one(&pool)
    .await
    .unwrap();
    assert_eq!(key_columns, "id", "the id alone is the key {key}");

    let pairs: Vec<(String, String)> = sqlx::query_as(
        "SELECT con.conname, string_agg(att.attname, ',' ORDER BY att.attname)
           FROM pg_constraint con
           JOIN pg_class cls ON cls.oid = con.conrelid
           JOIN pg_attribute att ON att.attrelid = cls.oid AND att.attnum = ANY(con.conkey)
          WHERE con.contype = 'u' AND cls.relname = 'user_identities'
       GROUP BY con.conname",
    )
    .fetch_all(&pool)
    .await
    .unwrap();
    assert_eq!(
        pairs,
        vec![(
            "user_identities_provider_uid_key".to_owned(),
            "provider,uid".to_owned()
        )],
        "one row per provider account"
    );

    let (foreign, action, target): (String, String, String) = sqlx::query_as(
        "SELECT con.conname, con.confdeltype::text, conf.relname
           FROM pg_constraint con
           JOIN pg_class cls ON cls.oid = con.conrelid
           JOIN pg_class conf ON conf.oid = con.confrelid
          WHERE con.contype = 'f'
            AND cls.relname = 'user_identities'
            AND con.conname = 'user_identities_user_id_fkey'",
    )
    .fetch_one(&pool)
    .await
    .unwrap();
    assert_eq!(action, "c", "{foreign} deletes with the user");
    assert_eq!(target, "users", "{foreign} points at users");

    let id_default: Option<String> = sqlx::query_scalar(
        "SELECT column_default FROM information_schema.columns
          WHERE table_schema = 'public' AND table_name = 'user_identities'
            AND column_name = 'id'",
    )
    .fetch_one(&pool)
    .await
    .unwrap();
    assert_eq!(id_default, None, "the id is minted in Rust (decision 4)");
}

/// The login-state migration (WBE-218) creates the table that holds
/// an `/oauth/authorize` request across a provider round trip: the
/// issued state as its text primary key (no `uuid` — the token is
/// what the provider echoes back), the provider it was issued to,
/// the jsonb parameters, and the expiry with no database default
/// because its lifetime is the model's to stamp.
#[tokio::test]
async fn oauth_login_states_migration_creates_the_state_table() {
    let db = crate::test_db::test_db().await.expect("test database slot");
    let pool = db.pool().clone();
    let columns: Vec<StateColumn> = sqlx::query_as(
        "SELECT column_name, data_type, is_nullable, column_default
           FROM information_schema.columns
          WHERE table_schema = 'public' AND table_name = 'oauth_login_states'
       ORDER BY ordinal_position",
    )
    .fetch_all(&pool)
    .await
    .unwrap();
    let described = described_of(columns);
    assert_eq!(
        described,
        vec![
            ("state".to_owned(), "text".to_owned(), "NO".to_owned(), None),
            (
                "provider".to_owned(),
                "text".to_owned(),
                "NO".to_owned(),
                None
            ),
            (
                "params".to_owned(),
                "jsonb".to_owned(),
                "NO".to_owned(),
                None
            ),
            (
                "valid_until".to_owned(),
                "timestamp with time zone".to_owned(),
                "NO".to_owned(),
                None
            ),
        ],
        "the ticket's four columns, none defaulted"
    );

    let (key, key_columns): (String, String) = sqlx::query_as(
        "SELECT con.conname, string_agg(att.attname, ',' ORDER BY att.attname)
           FROM pg_constraint con
           JOIN pg_class cls ON cls.oid = con.conrelid
           JOIN pg_attribute att ON att.attrelid = cls.oid AND att.attnum = ANY(con.conkey)
          WHERE con.contype = 'p' AND cls.relname = 'oauth_login_states'
       GROUP BY con.conname",
    )
    .fetch_one(&pool)
    .await
    .unwrap();
    assert_eq!(
        key_columns, "state",
        "the state is the key {key}: it is the only thing that comes back"
    );

    let foreign_keys: i64 = sqlx::query_scalar(
        "SELECT count(*) FROM pg_constraint con
           JOIN pg_class cls ON cls.oid = con.conrelid
          WHERE con.contype = 'f' AND cls.relname = 'oauth_login_states'",
    )
    .fetch_one(&pool)
    .await
    .unwrap();
    assert_eq!(
        foreign_keys, 0,
        "a state is nobody's child: it outlives no table"
    );
}

/// The password-reset migration (WBE-135) creates the token table
/// `src/models/password_reset.rs` reads and writes: the minted
/// `uuid` key, the secret's digest, the cascading account
/// foreign key, the `false`-defaulted spend flag and the
/// `now()`-stamped issue time the TTL counts from.
#[tokio::test]
async fn password_reset_tokens_migration_creates_the_token_table() {
    let db = crate::test_db::test_db().await.expect("test database slot");
    let pool = db.pool().clone();
    let columns: Vec<StateColumn> = sqlx::query_as(
        "SELECT column_name, data_type, is_nullable, column_default
           FROM information_schema.columns
          WHERE table_schema = 'public' AND table_name = 'password_reset_tokens'
       ORDER BY ordinal_position",
    )
    .fetch_all(&pool)
    .await
    .unwrap();
    assert_eq!(
        described_of(columns),
        vec![
            ("id".to_owned(), "uuid".to_owned(), "NO".to_owned(), None),
            (
                "token_digest".to_owned(),
                "text".to_owned(),
                "NO".to_owned(),
                None
            ),
            (
                "user_id".to_owned(),
                "uuid".to_owned(),
                "NO".to_owned(),
                None
            ),
            (
                "used".to_owned(),
                "boolean".to_owned(),
                "NO".to_owned(),
                Some("false".to_owned())
            ),
            (
                "inserted_at".to_owned(),
                "timestamp with time zone".to_owned(),
                "NO".to_owned(),
                Some("now()".to_owned())
            ),
        ],
        "the ticket's five columns, in the ticket's order"
    );

    let (key, key_columns): (String, String) = sqlx::query_as(
        "SELECT con.conname, string_agg(att.attname, ',' ORDER BY att.attname)
           FROM pg_constraint con
           JOIN pg_class cls ON cls.oid = con.conrelid
           JOIN pg_attribute att ON att.attrelid = cls.oid AND att.attnum = ANY(con.conkey)
          WHERE con.contype = 'p' AND cls.relname = 'password_reset_tokens'
       GROUP BY con.conname",
    )
    .fetch_one(&pool)
    .await
    .unwrap();
    assert_eq!(key_columns, "id", "the id alone is the key {key}");

    let (foreign, action, target): (String, String, String) = sqlx::query_as(
        "SELECT con.conname, con.confdeltype::text, conf.relname
           FROM pg_constraint con
           JOIN pg_class cls ON cls.oid = con.conrelid
           JOIN pg_class conf ON conf.oid = con.confrelid
          WHERE con.contype = 'f'
            AND cls.relname = 'password_reset_tokens'
            AND con.conname = 'password_reset_tokens_user_id_fkey'",
    )
    .fetch_one(&pool)
    .await
    .unwrap();
    assert_eq!(action, "c", "{foreign} deletes with the account");
    assert_eq!(target, "users", "{foreign} points at users");
}

/// The digest migration renames the secret column of both credential
/// tables, so no column named `token` is left to hold a raw secret, and
/// the reset table's unique constraint follows the column to the digest.
#[tokio::test]
async fn hash_reset_and_mfa_tokens_migration_renames_the_secret_columns() {
    let db = crate::test_db::test_db().await.expect("test database slot");
    let pool = db.pool().clone();
    for table in ["password_reset_tokens", "mfa_tokens"] {
        let columns: Vec<String> = sqlx::query_scalar(
            "SELECT column_name FROM information_schema.columns
              WHERE table_schema = 'public' AND table_name = $1
                AND column_name IN ('token', 'token_digest')
           ORDER BY column_name",
        )
        .bind(table)
        .fetch_all(&pool)
        .await
        .unwrap();
        assert_eq!(
            columns,
            vec!["token_digest".to_owned()],
            "{table} keeps only the digest column"
        );
    }

    let uniques: Vec<String> = sqlx::query_scalar(
        "SELECT con.conname
           FROM pg_constraint con
           JOIN pg_class cls ON cls.oid = con.conrelid
           JOIN pg_attribute att ON att.attrelid = cls.oid AND att.attnum = ANY(con.conkey)
          WHERE con.contype = 'u' AND cls.relname = 'password_reset_tokens'
            AND att.attname = 'token_digest'",
    )
    .fetch_all(&pool)
    .await
    .unwrap();
    assert_eq!(
        uniques,
        vec!["password_reset_tokens_token_key".to_owned()],
        "the secret's digest is the unique lookup key"
    );
}

/// The same migration adds `password_reset_pending` to `users`: a
/// `NOT NULL` `boolean` defaulted to `false`, the login gate
/// `check_account_ready` reads (the admin force-reset flow that
/// sets it true is a later ticket).
#[tokio::test]
async fn password_reset_tokens_migration_adds_the_pending_flag() {
    let db = crate::test_db::test_db().await.expect("test database slot");
    let pool = db.pool().clone();
    let (pending_type, pending_null, pending_default): (String, String, Option<String>) =
        sqlx::query_as(
            "SELECT data_type, is_nullable, column_default
               FROM information_schema.columns
              WHERE table_schema = 'public'
                AND table_name = 'users'
                AND column_name = 'password_reset_pending'",
        )
        .fetch_one(&pool)
        .await
        .unwrap();
    assert_eq!(pending_type, "boolean", "the login gate is a flag");
    assert_eq!(pending_null, "NO", "every account answers it");
    assert_eq!(
        pending_default.as_deref(),
        Some("false"),
        "and nothing starts pending: {pending_default:?}"
    );
}
