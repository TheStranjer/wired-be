//! The template database: the migration fingerprint, the build under the
//! advisory lock and the cleanup of stale databases.

use sha2::{Digest as _, Sha256};
use sqlx::{Connection as _, PgConnection, postgres::PgConnectOptions};

use super::{
    DB_PREFIX, FINGERPRINT_BYTES, TEMPLATE, TEMPLATE_LOCK_KEY, TEMPLATE_SUFFIX, TestDbError,
    databases::{create_database, current_database, database_exists},
    reset::{execute_ddl, quoted},
};
use crate::MIGRATOR;

/// The fingerprint of the migrations this binary embeds and of `owner`, the
/// role it connects as: every migration's version and checksum, in order.
pub(super) fn migrator_fingerprint(owner: &str) -> String {
    let migrations = MIGRATOR
        .migrations
        .iter()
        .map(|migration| (migration.version, migration.checksum.to_vec()))
        .collect::<Vec<_>>();
    fingerprint(&migrations, owner)
}

/// The 16-hex-character fingerprint of a migration set owned by `owner`, given
/// as its `(version, checksum)` pairs in order.
///
/// The fingerprint names the template and its slots, so the same migrations
/// always reuse the same databases and changed migrations never do. The owner
/// is part of it because a slot may only be reset and dropped by the role that
/// owns its database: a suite run under another role (CI's database user is
/// not a developer's) must not inherit databases it cannot maintain.
pub(super) fn fingerprint(migrations: &[(i64, Vec<u8>)], owner: &str) -> String {
    let mut hasher = Sha256::new();
    hasher.update(owner.as_bytes());
    for (version, checksum) in migrations {
        hasher.update(version.to_le_bytes());
        hasher.update(checksum);
    }
    let mut word: u64 = 0;
    for byte in hasher.finalize().iter().copied().take(FINGERPRINT_BYTES) {
        word = (word << 8) | u64::from(byte);
    }
    format!("{word:016x}")
}

/// The name of the template database of fingerprint `fp`.
pub(super) fn template_name(fp: &str) -> String {
    format!("{DB_PREFIX}{fp}{TEMPLATE_SUFFIX}")
}

/// The fingerprint a database name of this module's own carries: the part
/// between [`DB_PREFIX`] and the last `_`, which is the slot number or
/// [`TEMPLATE_SUFFIX`]. `None` for a name this module does not own.
pub(super) fn owned_fingerprint(database: &str) -> Option<&str> {
    database
        .strip_prefix(DB_PREFIX)
        .and_then(|rest| rest.rsplit_once('_'))
        .map(|(fp, _suffix)| fp)
}

/// Prepares the template database of fingerprint `fp`, at most once per
/// process: the first caller builds it, later callers of the same fingerprint
/// — including this process' other test threads — reuse it.
///
/// # Errors
///
/// Forwards whatever [`prepare_template`] returns.
pub(super) async fn ensure_template(
    maintenance: &PgConnectOptions,
    fp: &str,
) -> Result<(), TestDbError> {
    let _prepared = TEMPLATE
        .get_or_try_init(|| prepare_template(maintenance.clone(), fp.to_owned()))
        .await?;
    Ok(())
}

/// Builds the template under the template advisory lock, remembering which
/// fingerprint was prepared.
///
/// # Errors
///
/// Forwards [`prepare_under_lock`]; the lock is released either way, and an
/// unlock that fails on the success path is reported instead.
pub(super) async fn prepare_template(
    maintenance: PgConnectOptions,
    fp: String,
) -> Result<String, TestDbError> {
    let mut conn = PgConnection::connect_with(&maintenance).await?;
    let _locked = sqlx::query("SELECT pg_advisory_lock($1::bigint)")
        .bind(TEMPLATE_LOCK_KEY)
        .execute(&mut conn)
        .await?;
    let prepared = prepare_under_lock(&mut conn, &maintenance, &fp).await;
    match prepared {
        Ok(()) => {
            release_template_lock(&mut conn).await?;
            Ok(fp)
        }
        Err(err) => {
            let _released = release_template_lock(&mut conn).await;
            Err(err)
        }
    }
}

/// The work done while holding the template lock: create and migrate the
/// template if it is not there yet, then drop every idle database of a
/// fingerprint other than `fp` — the leftovers of earlier migration sets.
///
/// # Errors
///
/// Returns [`TestDbError::Database`] if any statement fails.
pub(super) async fn prepare_under_lock(
    conn: &mut PgConnection,
    maintenance: &PgConnectOptions,
    fp: &str,
) -> Result<(), TestDbError> {
    let template = template_name(fp);
    if !database_exists(conn, &template).await? {
        create_database(conn, &template, "template1").await?;
        migrate_template(maintenance, &template).await?;
    }
    drop_stale_databases(conn, fp).await
}

/// Runs [`MIGRATOR`] against a fresh connection to database `name` and closes
/// that connection: the template must have no session attached before it can
/// be copied, and the maintenance connection `name` was created on cannot see
/// the new database's schema.
///
/// # Errors
///
/// Returns [`TestDbError::Database`] if connecting, migrating or closing
/// fails.
pub(super) async fn migrate_template(
    maintenance: &PgConnectOptions,
    name: &str,
) -> Result<(), TestDbError> {
    let options = maintenance.clone().database(name);
    let mut conn = PgConnection::connect_with(&options).await?;
    MIGRATOR.run(&mut conn).await?;
    conn.close().await.map_err(Into::into)
}

/// Releases the template lock, reporting an unlock the server refused.
///
/// # Errors
///
/// Returns [`TestDbError::Setup`] if `pg_advisory_unlock` reports `false` and
/// [`TestDbError::Database`] if the call itself fails.
pub(super) async fn release_template_lock(conn: &mut PgConnection) -> Result<(), TestDbError> {
    let released: bool = sqlx::query_scalar("SELECT pg_advisory_unlock($1::bigint)")
        .bind(TEMPLATE_LOCK_KEY)
        .fetch_one(&mut *conn)
        .await?;
    if released {
        Ok(())
    } else {
        Err(TestDbError::Setup(
            "the template lock was not held".to_owned(),
        ))
    }
}

/// Drops every database of a fingerprint other than `fp`: the leftovers of
/// earlier migration sets.
///
/// A database that still has a session connected is skipped, and a drop that
/// loses that race is logged and skipped rather than forced: on a shared
/// server a foreign-fingerprint database can belong to another branch's test
/// suite running right now, and force-dropping it would sever that suite's
/// live leases. Leaving a stale database costs disk, nothing more, and the
/// next sweep while it is idle takes it. Only databases the connecting role
/// owns are candidates: another role's leftovers are not this role's to drop.
/// The maintenance database is never dropped, however it is named.
///
/// # Errors
///
/// Returns [`TestDbError::Database`] if a lookup fails.
pub(super) async fn drop_stale_databases(
    conn: &mut PgConnection,
    fp: &str,
) -> Result<(), TestDbError> {
    let current = current_database(conn).await?;
    let owned: Vec<String> = sqlx::query_scalar(
        "SELECT d.datname FROM pg_database d \
          WHERE starts_with(d.datname, $1) \
            AND pg_get_userbyid(d.datdba) = current_user \
            AND NOT EXISTS ( \
                SELECT 1 FROM pg_stat_activity a WHERE a.datname = d.datname \
            ) \
          ORDER BY d.datname",
    )
    .bind(DB_PREFIX)
    .fetch_all(&mut *conn)
    .await?;
    for database in owned {
        let is_stale = owned_fingerprint(&database).is_some_and(|got| got != fp);
        if is_stale && database != current {
            drop_idle_database(conn, &database).await;
        }
    }
    Ok(())
}

/// `DROP DATABASE IF EXISTS <name>`, without `WITH (FORCE)`: the caller found
/// no session on the database, and a session that attached since then should
/// refuse the drop, not be killed by it. The refusal is logged at debug and
/// skipped — cleanup is opportunistic, and a database that is being used is
/// by definition not leftover garbage.
pub(super) async fn drop_idle_database(conn: &mut PgConnection, name: &str) {
    let sql = format!("DROP DATABASE IF EXISTS {}", quoted(name));
    if let Err(err) = execute_ddl(conn, sql).await {
        tracing::debug!("left {name} in place, its drop was refused: {err}");
    }
}

#[cfg(test)]
mod tests {
    use super::super::{
        databases::drop_database,
        maintenance_options,
        slots::slot_name,
        test_support::{leased_fingerprint, maintenance_conn},
    };
    use super::*;

    /// The fingerprint is a function of the migration set: the same pairs give
    /// the same name, and any change to versions, checksums or their order
    /// gives a different one, so an edited `migrations/` never reuses a
    /// template.
    #[test]
    fn the_fingerprint_follows_the_migration_set() {
        let base = vec![(1_i64, b"abc".to_vec())];
        assert_eq!(
            fingerprint(&base, "wired"),
            fingerprint(&[(1_i64, b"abc".to_vec())], "wired"),
            "the same set fingerprints the same"
        );
        let added = [(1_i64, b"abc".to_vec()), (2_i64, b"abc".to_vec())];
        let edited = [(1_i64, b"abd".to_vec())];
        let reordered = vec![(2_i64, b"abc".to_vec()), (1_i64, b"abc".to_vec())];
        assert_ne!(
            fingerprint(&base, "wired"),
            fingerprint(&added, "wired"),
            "a new migration changes it"
        );
        assert_ne!(
            fingerprint(&base, "wired"),
            fingerprint(&edited, "wired"),
            "an edited migration changes it"
        );
        assert_ne!(
            fingerprint(&added, "wired"),
            fingerprint(&reordered, "wired"),
            "the order of the set counts"
        );
        assert_ne!(
            fingerprint(&base, "wired"),
            fingerprint(&base, "postgres"),
            "another role's databases are not ours to reuse"
        );
        assert_eq!(
            fingerprint(&base, "wired").len(),
            FINGERPRINT_BYTES * 2,
            "a fingerprint is as wide as the module says"
        );
        assert_ne!(
            fingerprint(&base, "wired"),
            fingerprint(&[], "wired"),
            "no set is not the set"
        );
    }

    /// The embedded migration set fingerprints to the database names the
    /// module documents, and only its own names carry a fingerprint back.
    #[test]
    fn the_names_are_built_from_the_fingerprint() {
        let fp = migrator_fingerprint("wired");
        let again = migrator_fingerprint("wired");
        assert_eq!(fp, again, "it is the set and the role, not the call");
        assert_ne!(
            fp,
            migrator_fingerprint("postgres"),
            "the role the databases belong to counts"
        );
        let template = template_name(&fp);
        assert_eq!(template, format!("{DB_PREFIX}{fp}{TEMPLATE_SUFFIX}"));
        assert_eq!(slot_name(&fp, 7), format!("{DB_PREFIX}{fp}_7"));
        assert_eq!(
            owned_fingerprint(&template),
            Some(fp.as_str()),
            "the template names its fingerprint"
        );
        assert_eq!(
            owned_fingerprint(&slot_name(&fp, 12)),
            Some(fp.as_str()),
            "a two-digit slot does too"
        );
        assert_eq!(
            owned_fingerprint("postgres"),
            None,
            "a foreign name is not ours"
        );
        assert_eq!(
            owned_fingerprint(DB_PREFIX),
            None,
            "the bare prefix is not ours"
        );
    }

    /// The stale sweep takes only idle databases: a foreign-fingerprint
    /// database some session is connected to survives the sweep and an idle
    /// one goes, while a database of this fingerprint's own survives either
    /// way. A branch's running suite must never lose its live leases to
    /// another branch's startup cleanup.
    #[tokio::test]
    async fn stale_cleanup_spares_databases_that_are_in_use() {
        let fp = leased_fingerprint();
        let alien = "deadbeefdeadbeef";
        let busy = slot_name(alien, 40);
        let idle = slot_name(alien, 41);
        let own = slot_name(&fp, 42);
        let mut conn = maintenance_conn().await;
        let options = maintenance_options().expect("DATABASE_URL is set for the test suite");
        for database in [&busy, &idle, &own] {
            create_database(&mut conn, database, "template1")
                .await
                .expect("the test database is created");
        }
        // Sessions on the two databases that must survive the sweep: the
        // busy alien one because cleanup skips anything in use, and the
        // own-fingerprint one also against a concurrently starting foreign
        // sweep, which may only ever take idle databases.
        let busy_session = PgConnection::connect_with(&options.clone().database(&busy))
            .await
            .expect("a session attaches to the busy database");
        let own_session = PgConnection::connect_with(&options.database(&own))
            .await
            .expect("a session attaches to the own-fingerprint database");

        drop_stale_databases(&mut conn, &fp)
            .await
            .expect("the sweep runs");

        assert!(
            database_exists(&mut conn, &busy)
                .await
                .expect("the catalog answers"),
            "a database another session is using is not someone's garbage"
        );
        assert!(
            !database_exists(&mut conn, &idle)
                .await
                .expect("the catalog answers"),
            "an idle foreign database is swept away"
        );
        assert!(
            database_exists(&mut conn, &own)
                .await
                .expect("the catalog answers"),
            "this fingerprint's own databases are never stale"
        );

        drop(busy_session);
        drop(own_session);
        // Forced teardown: a session the server has not finished reaping
        // must not refuse the cleanup of this test's own databases.
        for database in [&busy, &idle, &own] {
            drop_database(&mut conn, database)
                .await
                .expect("the test database is dropped again");
        }
    }
}
