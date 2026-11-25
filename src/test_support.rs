//! The [`AppState`] builders the crate's unit tests share, so a change to
//! [`AppState::for_tests`] is made in one place instead of in every test
//! module that used to carry its own copy.

use sqlx::{PgPool, postgres::PgPoolOptions};

use crate::{
    config::{Config, Toggle},
    state::AppState,
};

/// Test state over `pool` with the fixed test config.
#[must_use]
pub fn app_state(pool: &PgPool) -> AppState {
    AppState::for_tests(pool.clone(), Config::for_tests())
}

/// Test state over `pool` with the fixed test config after `tweak` adjusted it.
///
/// For the tests that read a setting away from its test default.
#[must_use]
pub fn app_state_with<F>(pool: &PgPool, tweak: F) -> AppState
where
    F: FnOnce(&mut Config),
{
    let mut config = Config::for_tests();
    tweak(&mut config);
    AppState::for_tests(pool.clone(), config)
}

/// Test state over a pool that cannot answer.
///
/// The pool is lazy and closed before it ever connected, which is how a
/// handler sees a database that went away: every query fails at once.
///
/// # Panics
///
/// Never in practice: a lazy pool needs no database to be built.
pub async fn broken_state() -> AppState {
    let pool = PgPoolOptions::new()
        .connect_lazy("postgres://localhost/unused")
        .expect("a lazy pool needs no database");
    pool.close().await;
    app_state(&pool)
}

/// Test state over a lazy pool that never connects.
///
/// For tests whose routes never reach the database: a state that does must
/// fail.
#[must_use]
pub fn lazy_state() -> AppState {
    AppState::lazy_for_tests(Config::for_tests())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn app_state_carries_the_test_config() {
        let db = crate::test_db::test_db().await.expect("test database slot");

        let state = app_state(db.pool());

        assert!(
            state.config.federation_enabled.get(),
            "the stock test config federates"
        );
    }

    #[tokio::test]
    async fn app_state_with_applies_the_override() {
        let db = crate::test_db::test_db().await.expect("test database slot");

        let state = app_state_with(db.pool(), |config| {
            config.federation_enabled = Toggle::new(false);
        });

        assert!(
            !state.config.federation_enabled.get(),
            "the override is applied"
        );
    }

    #[tokio::test]
    async fn broken_state_fails_every_query() {
        let state = broken_state().await;

        let answer = sqlx::query("SELECT 1").execute(&state.db).await;

        assert!(answer.is_err(), "a closed pool answers nothing");
    }

    #[tokio::test]
    async fn lazy_state_never_connects_a_pool() {
        let state = lazy_state();

        assert_eq!(state.db.size(), 0, "the lazy pool holds no connection");
        assert!(
            state.config.federation_enabled.get(),
            "the stock test config"
        );
    }
}
