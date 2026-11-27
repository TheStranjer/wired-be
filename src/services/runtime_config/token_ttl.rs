//! The `:oauth_token_ttl_secs` entry: the stored per-role lifetime the
//! token endpoints read at issue time, and the check an admin's write must
//! pass.

use serde_json::Value;
use sqlx::{Executor, Postgres};
use tracing::warn;

use super::{storage::STORED_SQL, wire::to_storage};
use crate::{
    config::{Config, TokenTier, ttl_secs_valid},
    error::AppResult,
    services::config_registry::{GROUP_PLEROMA, KEY_OAUTH_TOKEN_TTL_SECS},
};

/// The lifetime in seconds an access token of `tier` gets now.
///
/// That is the stored `:oauth_token_ttl_secs` subkey when the admin config
/// store holds a valid one, otherwise the environment's value from `env`.
/// Read on `executor`, so the refresh grant asks within the transaction that
/// rotates the pair.
///
/// A stored value that is not an acceptable lifetime is warned about and
/// skipped: a broken row must never stop a login.
///
/// # Errors
///
/// Any database failure arrives as
/// [`AppError::Database`](crate::error::AppError).
pub async fn token_ttl_secs<'e, E>(executor: E, env: &Config, tier: TokenTier) -> AppResult<usize>
where
    E: Executor<'e, Database = Postgres>,
{
    let stored = sqlx::query_scalar::<_, Value>(STORED_SQL)
        .bind(GROUP_PLEROMA)
        .bind(KEY_OAUTH_TOKEN_TTL_SECS)
        .fetch_optional(executor)
        .await?;
    Ok(stored
        .as_ref()
        .and_then(|row| override_secs(row, tier))
        .unwrap_or_else(|| env.oauth_token_ttls.secs(tier)))
}

/// The acceptable lifetime `stored` holds under `tier`'s subkey, if any.
fn override_secs(stored: &Value, tier: TokenTier) -> Option<usize> {
    let value = stored.as_object()?.get(tier.as_str())?;
    let secs = value
        .as_u64()
        .filter(|secs| ttl_secs_valid(*secs))
        .and_then(|secs| usize::try_from(secs).ok());
    if secs.is_none() {
        warn!(
            tier = tier.as_str(),
            value = %value,
            "an :oauth_token_ttl_secs override is not a usable lifetime; the env value stands"
        );
    }
    secs
}

/// Checks the wire-form `value` of an upsert to `key` before it is written.
///
/// `Ok` for every key but `:oauth_token_ttl_secs`, whose subkeys must be
/// `admin`, `bot` or `user` and whose values must be positive whole numbers
/// of seconds that [`ttl_secs_valid`] accepts.
///
/// # Errors
///
/// The message the admin API answers 422 with.
pub fn check_value(key: &str, value: &Value) -> Result<(), String> {
    if key != KEY_OAUTH_TOKEN_TTL_SECS {
        return Ok(());
    }
    let decoded = to_storage(value);
    let fields = match &decoded {
        Value::Object(fields) => fields,
        Value::Array(items) if items.is_empty() => return Ok(()),
        Value::Null | Value::Bool(_) | Value::Number(_) | Value::String(_) | Value::Array(_) => {
            return Err(format!(
                "{key} takes a keyword list of admin, bot and user lifetimes"
            ));
        }
    };
    for (name, secs) in fields {
        if !TokenTier::ALL.iter().any(|tier| tier.as_str() == name) {
            return Err(format!(
                "{key} has no subkey {name}; use admin, bot or user"
            ));
        }
        if !secs.as_u64().is_some_and(ttl_secs_valid) {
            return Err(format!(
                "{key} {name} must be a positive whole number of seconds that ends in a valid date"
            ));
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use serde_json::json;

    use super::*;
    use crate::services::runtime_config::{RuntimeConfig, write};

    #[test]
    fn other_keys_are_not_checked() {
        assert!(
            check_value(":instance", &json!("anything")).is_ok(),
            "only the lifetime entry has a value check"
        );
    }

    #[test]
    fn good_lifetimes_pass_in_wire_and_storage_form() {
        for value in [
            json!([{"tuple": ["admin", 60]}, {"tuple": ["bot", 3_153_600_000_u64]}]),
            json!({"user": 1}),
            json!([{"tuple": [":user", 86_400]}]),
            json!([]),
        ] {
            assert!(
                check_value(KEY_OAUTH_TOKEN_TTL_SECS, &value).is_ok(),
                "{value} is acceptable"
            );
        }
    }

    #[test]
    fn bad_lifetimes_are_refused_with_a_message() {
        for value in [
            json!("3600"),
            json!(3600),
            json!(["admin"]),
            json!([{"tuple": ["admin", 0]}]),
            json!([{"tuple": ["admin", -5]}]),
            json!([{"tuple": ["admin", 1.5]}]),
            json!([{"tuple": ["admin", "3600"]}]),
            json!([{"tuple": ["bot", 99_999_999_999_999_u64]}]),
            json!([{"tuple": ["root", 60]}]),
        ] {
            let err = check_value(KEY_OAUTH_TOKEN_TTL_SECS, &value)
                .expect_err("the value is not acceptable");
            assert!(
                err.starts_with(KEY_OAUTH_TOKEN_TTL_SECS),
                "the message names the key: {err}"
            );
        }
    }

    #[test]
    fn a_stored_override_is_read_per_tier() {
        let stored = json!({"admin": 60, "bot": 0, "user": "soon"});
        assert_eq!(override_secs(&stored, TokenTier::Admin), Some(60), "valid");
        assert_eq!(override_secs(&stored, TokenTier::Bot), None, "zero");
        assert_eq!(
            override_secs(&stored, TokenTier::User),
            None,
            "not a number"
        );
        assert_eq!(
            override_secs(&json!({"admin": 60}), TokenTier::User),
            None,
            "an absent subkey stands aside"
        );
        assert_eq!(
            override_secs(&json!(7), TokenTier::Admin),
            None,
            "a row that is no keyword list stands aside"
        );
    }

    #[tokio::test]
    async fn the_stored_row_wins_over_the_env_value_per_tier() {
        let db = crate::test_db::test_db().await.expect("test database slot");
        let pool = db.pool().clone();
        let config = Config::for_tests();

        for tier in TokenTier::ALL {
            assert_eq!(
                token_ttl_secs(&pool, &config, tier).await.expect("read"),
                config.oauth_token_ttls.secs(tier),
                "no row: the env value for {tier:?}"
            );
        }

        write(
            &pool,
            GROUP_PLEROMA,
            KEY_OAUTH_TOKEN_TTL_SECS,
            &json!({"bot": 120}),
        )
        .await
        .expect("the row writes");
        assert_eq!(
            token_ttl_secs(&pool, &config, TokenTier::Bot)
                .await
                .expect("read"),
            120,
            "the stored subkey wins"
        );
        assert_eq!(
            token_ttl_secs(&pool, &config, TokenTier::User)
                .await
                .expect("read"),
            config.oauth_token_ttls.user,
            "a tier with no subkey keeps the env value"
        );

        let store = RuntimeConfig::new(&pool, &config);
        let _merged = store
            .set(
                GROUP_PLEROMA,
                KEY_OAUTH_TOKEN_TTL_SECS,
                &json!([{"tuple": ["bot", 240]}]),
            )
            .await
            .expect("the write runs");
        assert_eq!(
            token_ttl_secs(&pool, &config, TokenTier::Bot)
                .await
                .expect("read"),
            240,
            "the next read sees the new value, no restart"
        );
    }
}
