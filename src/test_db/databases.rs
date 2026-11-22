//! The `CREATE`/`DROP DATABASE` statements and catalog lookups the fixture
//! runs on the maintenance connection.

use sqlx::PgConnection;

use super::{
    TestDbError,
    reset::{execute_ddl, quoted},
};

/// The name of the database `conn` is connected to.
///
/// # Errors
///
/// Returns [`TestDbError::Database`] if the lookup fails.
pub(super) async fn current_database(conn: &mut PgConnection) -> Result<String, TestDbError> {
    Ok(sqlx::query_scalar::<_, String>("SELECT current_database()")
        .fetch_one(&mut *conn)
        .await?)
}

/// Whether `conn`'s server has a database named `name`.
///
/// # Errors
///
/// Returns [`TestDbError::Database`] if the lookup fails.
pub(super) async fn database_exists(
    conn: &mut PgConnection,
    name: &str,
) -> Result<bool, TestDbError> {
    let found: Option<String> =
        sqlx::query_scalar("SELECT datname FROM pg_database WHERE datname = $1")
            .bind(name)
            .fetch_optional(&mut *conn)
            .await?;
    Ok(found.is_some())
}

/// `CREATE DATABASE <name> TEMPLATE <template>`.
///
/// # Errors
///
/// Returns [`TestDbError::Database`] if the statement fails, e.g. because the
/// template is still in use.
pub(super) async fn create_database(
    conn: &mut PgConnection,
    name: &str,
    template: &str,
) -> Result<(), TestDbError> {
    let sql = format!(
        "CREATE DATABASE {} TEMPLATE {}",
        quoted(name),
        quoted(template)
    );
    execute_ddl(conn, sql).await
}

/// `DROP DATABASE IF EXISTS <name> WITH (FORCE)`.
///
/// # Errors
///
/// Returns [`TestDbError::Database`] if the statement fails.
pub(super) async fn drop_database(conn: &mut PgConnection, name: &str) -> Result<(), TestDbError> {
    let sql = format!("DROP DATABASE IF EXISTS {} WITH (FORCE)", quoted(name));
    execute_ddl(conn, sql).await
}
