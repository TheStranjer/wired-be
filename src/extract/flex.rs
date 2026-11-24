//! Serde helpers that accept the string spellings form requests deliver for
//! booleans and integers.

use serde::{Deserialize as _, Deserializer, de::Error as SerdeError};
use serde_json::Value;

use crate::util::params::truthy;

/// A `bool` that also accepts the form spellings `true`/`false` (any
/// case), `1`/`0`, `on`/`off`, `yes`/`no` and `t`/`f`.
///
/// # Errors
///
/// A serde error when the value is neither a JSON boolean nor one of
/// the listed strings.
pub fn bool<'de, D>(deserializer: D) -> Result<bool, D::Error>
where
    D: Deserializer<'de>,
{
    let value = Value::deserialize(deserializer)?;
    coerce_bool(&value).ok_or_else(|| SerdeError::custom(expected("a boolean", &value)))
}

/// [`bool()`] for an `Option<bool>` field; a `null` stays `None`.
///
/// # Errors
///
/// As [`bool()`].
pub fn opt_bool<'de, D>(deserializer: D) -> Result<Option<bool>, D::Error>
where
    D: Deserializer<'de>,
{
    let value = Option::<Value>::deserialize(deserializer)?;
    value.map_or_else(
        || Ok(None),
        |value| {
            coerce_bool(&value)
                .map(Some)
                .ok_or_else(|| SerdeError::custom(expected("a boolean", &value)))
        },
    )
}

/// A `u32` that also accepts a numeric string.
///
/// # Errors
///
/// A serde error when the value is not an integer in `u32`'s range,
/// string or number alike.
pub fn u32<'de, D>(deserializer: D) -> Result<u32, D::Error>
where
    D: Deserializer<'de>,
{
    let value = Value::deserialize(deserializer)?;
    coerce_int::<u32>(&value).ok_or_else(|| SerdeError::custom(expected("a number", &value)))
}

/// [`u32()`] for an `Option<u32>` field; a `null` stays `None`.
///
/// # Errors
///
/// As [`u32()`].
pub fn opt_u32<'de, D>(deserializer: D) -> Result<Option<u32>, D::Error>
where
    D: Deserializer<'de>,
{
    let value = Option::<Value>::deserialize(deserializer)?;
    value.map_or(Ok(None), |value| {
        coerce_int::<u32>(&value)
            .map(Some)
            .ok_or_else(|| SerdeError::custom(expected("a number", &value)))
    })
}

/// An `i32` that also accepts a numeric string: the spelling of a column
/// the schema gives as `integer`, like a scrobble's `length`.
///
/// # Errors
///
/// A serde error when the value is not an integer in `i32`'s range,
/// string or number alike.
pub fn opt_i32<'de, D>(deserializer: D) -> Result<Option<i32>, D::Error>
where
    D: Deserializer<'de>,
{
    let value = Option::<Value>::deserialize(deserializer)?;
    value.map_or(Ok(None), |value| {
        coerce_int::<i32>(&value)
            .map(Some)
            .ok_or_else(|| SerdeError::custom(expected("a number", &value)))
    })
}

/// An `i64` that also accepts a numeric string.
///
/// # Errors
///
/// A serde error when the value is not an integer in `i64`'s range,
/// string or number alike.
pub fn i64<'de, D>(deserializer: D) -> Result<i64, D::Error>
where
    D: Deserializer<'de>,
{
    let value = Value::deserialize(deserializer)?;
    coerce_int::<i64>(&value).ok_or_else(|| SerdeError::custom(expected("a number", &value)))
}

/// [`i64()`] for an `Option<i64>` field; a `null` stays `None`.
///
/// # Errors
///
/// As [`i64()`].
pub fn opt_i64<'de, D>(deserializer: D) -> Result<Option<i64>, D::Error>
where
    D: Deserializer<'de>,
{
    let value = Option::<Value>::deserialize(deserializer)?;
    value.map_or(Ok(None), |value| {
        coerce_int::<i64>(&value)
            .map(Some)
            .ok_or_else(|| SerdeError::custom(expected("a number", &value)))
    })
}

/// The boolean behind a JSON value: real booleans and the form
/// spellings, case- and whitespace-insensitively; anything else `None`.
fn coerce_bool(value: &Value) -> Option<bool> {
    match value {
        Value::Bool(found) => Some(*found),
        Value::String(text) => truthy(text),
        Value::Null | Value::Number(_) | Value::Array(_) | Value::Object(_) => None,
    }
}

/// The integer behind a JSON value, from a number or a numeric string,
/// `None` when neither fits `T`.
fn coerce_int<T>(value: &Value) -> Option<T>
where
    T: TryFrom<i64> + TryFrom<u64> + std::str::FromStr,
{
    match value {
        Value::Number(number) => number
            .as_i64()
            .and_then(|int| T::try_from(int).ok())
            .or_else(|| number.as_u64().and_then(|uint| T::try_from(uint).ok())),
        Value::String(text) => text.trim().parse::<T>().ok(),
        Value::Bool(_) | Value::Null | Value::Array(_) | Value::Object(_) => None,
    }
}

/// The serde error text naming what was expected and what was found.
fn expected(what: &str, value: &Value) -> String {
    format!("expected {what}, found {value}")
}
