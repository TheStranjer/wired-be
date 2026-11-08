//! The instance VAPID key pair: the ES256 (P-256) identity Web Push
//! deliveries are signed with (WBE-144).
//!
//! The reference is Pleroma's `:web_push_encryption, :vapid_details`
//! runtime config (`config/runtime.exs`), whose `private`/`public_key`
//! pair the push endpoints advertise as the `server_key`. Here the pair
//! lives in [`instance_secrets`](crate::models::instance_secret) under
//! the names [`VAPID_PRIVATE_NAME`] (the signing key as PKCS#8 PEM) and
//! [`VAPID_PUBLIC_NAME`] (the public point as URL-safe base64 of its
//! uncompressed SEC1 encoding, the spelling Web Push clients consume),
//! so a fresh instance needs no key-management step:
//! [`ensure_instance_keypair`] runs at startup and [`vapid_public_key`]
//! generates on first read, the same idempotence the
//! [`instance_actor`](super::instance_actor) service follows. An operator
//! who pins their own material through [`VapidConfig`](crate::config::VapidConfig)
//! (`VAPID_PRIVATE_KEY` / `VAPID_PUBLIC_KEY`) overwrites the stored pair
//! at every startup, so config is the last word.
//!
//! The private half is secret: it is never formatted into a log line or
//! an error message, and only [`vapid_public_key`] — the public half —
//! reaches the API.

use base64::{Engine as _, engine::general_purpose::URL_SAFE_NO_PAD};
use p256::{
    SecretKey,
    elliptic_curve::{Generate as _, sec1::ToSec1Point as _},
    pkcs8::{DecodePrivateKey as _, EncodePrivateKey as _, LineEnding},
};
// `p256::pkcs8` is `elliptic_curve::pkcs8` re-exported; the three traits
// above (`DecodePrivateKey`, `EncodePrivateKey`) and `LineEnding` come
// from there.
use sqlx::PgPool;

use crate::{
    config::Config,
    error::{AppError, AppResult},
    models::instance_secret,
};

/// The `instance_secrets` row name holding the private half: the ES256
/// signing key as PKCS#8 PEM.
pub const VAPID_PRIVATE_NAME: &str = "vapid_private";
/// The `instance_secrets` row name holding the public half: URL-safe
/// base64 (no padding) of the uncompressed P-256 point.
pub const VAPID_PUBLIC_NAME: &str = "vapid_public";

/// A VAPID key pair as the two stored texts.
#[derive(Clone)]
pub struct VapidPair {
    /// The private key as PKCS#8 PEM (`-----BEGIN PRIVATE KEY-----`).
    private_pem: String,
    /// The public point as URL-safe base64 (no padding).
    public_key: String,
}

/// Test support: the pair's two texts, for integration tests that pin a
/// config override and must know the matching key material.
#[cfg(any(test, feature = "integration-tests"))]
impl VapidPair {
    /// The private half as PKCS#8 PEM.
    #[must_use]
    pub fn private_pem(&self) -> &str {
        &self.private_pem
    }

    /// The public half as URL-safe base64.
    #[must_use]
    pub fn public_key(&self) -> &str {
        &self.public_key
    }
}

/// Test support: a freshly generated pair, for tests of the
/// `VAPID_PRIVATE_KEY` / `VAPID_PUBLIC_KEY` override path.
///
/// # Errors
///
/// Whatever the module's private `generate_pair` returns.
#[cfg(any(test, feature = "integration-tests"))]
pub fn generate_pair_for_tests() -> AppResult<VapidPair> {
    generate_pair()
}

/// The pair's `Debug` never prints the private PEM.
impl std::fmt::Debug for VapidPair {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("VapidPair")
            .field("private_pem", &"<redacted>")
            .field("public_key", &self.public_key)
            .finish()
    }
}

/// Generates a fresh ES256 key pair from the system CSPRNG: the private
/// half as PKCS#8 PEM, the public half as the URL-safe base64 of the
/// uncompressed point.
///
/// # Errors
///
/// [`AppError::Internal`] if the system RNG or the PEM encoding fails;
/// the detail names the failure, never any key material.
fn generate_pair() -> AppResult<VapidPair> {
    let secret = SecretKey::try_generate()
        .map_err(|err| AppError::Internal(format!("could not generate a VAPID key pair: {err}")))?;
    pair_of_secret(&secret)
}

/// Splits a parsed [`SecretKey`] into the two stored texts.
///
/// # Errors
///
/// [`AppError::Internal`] if the PEM encoding fails.
fn pair_of_secret(secret: &SecretKey) -> AppResult<VapidPair> {
    let private_pem = secret
        .to_pkcs8_pem(LineEnding::LF)
        .map_err(|err| AppError::Internal(format!("could not PEM-encode the key: {err}")))?;
    let point = secret.public_key().to_sec1_point(false);
    Ok(VapidPair {
        private_pem: String::clone(&private_pem),
        public_key: URL_SAFE_NO_PAD.encode(point.as_bytes()),
    })
}

/// The URL-safe base64 public key of the PEM-encoded private key `pem`,
/// so a configured `VAPID_PRIVATE_KEY` needs no matching `VAPID_PUBLIC_KEY`
/// to be usable.
///
/// # Errors
///
/// [`AppError::Internal`] if `pem` does not parse as a PKCS#8 P-256 key
/// (the message says so, without quoting the key) or cannot be encoded.
fn public_key_of_private_pem(pem: &str) -> AppResult<String> {
    let secret = SecretKey::from_pkcs8_pem(pem).map_err(|err| {
        AppError::Internal(format!(
            "{VAPID_PRIVATE_NAME} override is not a PKCS#8 PEM P-256 key: {err}"
        ))
    })?;
    Ok(pair_of_secret(&secret)?.public_key)
}

/// Ensures the instance has a VAPID key pair and returns the public half.
///
/// Called once at startup: a configured pair (see the [module
/// docs](self)) overwrites the stored rows, and with nothing configured a
/// missing pair is generated into `instance_secrets` — an existing stored
/// pair answers untouched, so restarts keep the key peers and clients
/// already know.
///
/// # Errors
///
/// [`AppError::Internal`] for an unparseable configured private key, and
/// any database failure arrives through `?` as
/// [`AppError::Database`](crate::error::AppError).
pub async fn ensure_instance_keypair(pool: &PgPool, config: &Config) -> AppResult<String> {
    if let Some(private_pem) = config.vapid.private_key.as_deref() {
        let public = match config.vapid.public_key.as_deref() {
            Some(key) => key.to_owned(),
            None => public_key_of_private_pem(private_pem)?,
        };
        instance_secret::set(pool, VAPID_PRIVATE_NAME, private_pem).await?;
        instance_secret::set(pool, VAPID_PUBLIC_NAME, &public).await?;
        return Ok(public);
    }
    let public = ensure_generated(pool).await?;
    if let Some(key) = config.vapid.public_key.as_deref() {
        // A pinned public half with no private key beside it: the
        // generated pair keeps the row usable and the configured text
        // becomes the advertised one.
        instance_secret::set(pool, VAPID_PUBLIC_NAME, key).await?;
        return Ok(key.to_owned());
    }
    Ok(public)
}

/// The instance's VAPID public key, generating the pair on first use.
///
/// The lazy counterpart of [`ensure_instance_keypair`] for a reader that
/// only needs the advertised half; a lost race against a concurrent first
/// use keeps whichever pair reached the table first.
///
/// # Errors
///
/// Any database failure arrives through `?` as
/// [`AppError::Database`](crate::error::AppError); [`AppError::Internal`]
/// reports the impossible state where the row vanished between writing
/// and reading it back.
pub async fn vapid_public_key(pool: &PgPool) -> AppResult<String> {
    if let Some(public) = instance_secret::get(pool, VAPID_PUBLIC_NAME).await? {
        return Ok(public);
    }
    ensure_generated(pool).await
}

/// Reads the stored pair and returns its public half, seeding what is
/// missing: a complete pair answers as it stands, a stored private half
/// with no public one gets its public half derived and stored, and with
/// nothing stored a fresh pair is generated. Every write yields to a
/// concurrent one, and the public key finally reported is the row that
/// survived — one instance, one key pair.
///
/// # Errors
///
/// As [`vapid_public_key`]; a stored private half that does not parse as
/// a PKCS#8 P-256 key arrives as [`AppError::Internal`].
async fn ensure_generated(pool: &PgPool) -> AppResult<String> {
    let stored_private = instance_secret::get(pool, VAPID_PRIVATE_NAME).await?;
    let public_row = instance_secret::get(pool, VAPID_PUBLIC_NAME).await?;
    if let Some(public) = public_row.filter(|_| stored_private.is_some()) {
        return Ok(public);
    }
    if let Some(pem) = stored_private {
        // A stored private half with no public one: derive the advertised
        // half, so an operator-pinned key still advertises a matching key.
        let derived = public_key_of_private_pem(&pem)?;
        let _seeded: bool =
            instance_secret::insert_if_absent(pool, VAPID_PUBLIC_NAME, &derived).await?;
        return stored_public(pool).await;
    }
    // Nothing usable stored: generate a fresh pair, each half yielding to
    // a concurrent writer's row.
    let pair = generate_pair()?;
    let _private_seeded: bool =
        instance_secret::insert_if_absent(pool, VAPID_PRIVATE_NAME, &pair.private_pem).await?;
    let _public_seeded: bool =
        instance_secret::insert_if_absent(pool, VAPID_PUBLIC_NAME, &pair.public_key).await?;
    stored_public(pool).await
}

/// The stored public half, with the failure that can only mean the row
/// vanished between writing and reading it.
///
/// # Errors
///
/// As [`vapid_public_key`].
async fn stored_public(pool: &PgPool) -> AppResult<String> {
    instance_secret::get(pool, VAPID_PUBLIC_NAME)
        .await?
        .ok_or_else(|| {
            AppError::Internal(format!(
                "the {VAPID_PUBLIC_NAME} instance secret is missing after seeding"
            ))
        })
}

#[cfg(test)]
mod tests {
    use sqlx::PgPool;

    use super::*;
    use crate::config::{
        BASE_URL_VAR, DATABASE_URL_VAR, TEST_BASE_URL, TEST_DATABASE_URL, VAPID_PRIVATE_KEY_VAR,
        VAPID_PUBLIC_KEY_VAR,
    };

    /// The byte length of an uncompressed P-256 point: the `0x04` tag
    /// plus the two 32-byte coordinates, what the tests decode the
    /// generated public halves against.
    const UNCOMPRESSED_POINT_BYTES: usize = 65;

    /// A [`Config`] with the VAPID overrides `private`/`public` set (an
    /// absent one left unset), everything else at the parsing defaults.
    fn config_with(private: Option<&str>, public: Option<&str>) -> Config {
        Config::from_lookup(|key| match key {
            DATABASE_URL_VAR => Some(TEST_DATABASE_URL.to_owned()),
            BASE_URL_VAR => Some(TEST_BASE_URL.to_owned()),
            VAPID_PRIVATE_KEY_VAR => private.map(str::to_owned),
            VAPID_PUBLIC_KEY_VAR => public.map(str::to_owned),
            _ => None,
        })
        .expect("the fixed VAPID test config is valid")
    }

    /// Both stored rows of `pool` as `(private, public)`.
    async fn stored_pair(pool: &PgPool) -> (Option<String>, Option<String>) {
        (
            instance_secret::get(pool, VAPID_PRIVATE_NAME)
                .await
                .expect("the private lookup runs"),
            instance_secret::get(pool, VAPID_PUBLIC_NAME)
                .await
                .expect("the public lookup runs"),
        )
    }

    #[test]
    fn a_generated_pair_is_a_pkcs8_pem_and_a_urlsafe_point() {
        let pair = generate_pair().unwrap();
        assert!(
            pair.private_pem.starts_with("-----BEGIN PRIVATE KEY-----"),
            "the private half is PKCS#8 PEM"
        );
        let point = URL_SAFE_NO_PAD.decode(&pair.public_key).unwrap();
        assert_eq!(
            point.len(),
            UNCOMPRESSED_POINT_BYTES,
            "the public half is the uncompressed point"
        );
        assert_eq!(point.first().copied(), Some(0x04), "with the 0x04 tag");
        assert!(
            !pair.public_key.contains('+') && !pair.public_key.contains('/'),
            "URL-safe alphabet: {}",
            pair.public_key
        );
        assert!(
            !pair.public_key.contains('='),
            "no padding on the stored token"
        );
    }

    #[test]
    fn the_private_half_derives_the_public_one() {
        let pair = generate_pair().unwrap();
        let derived = public_key_of_private_pem(&pair.private_pem).unwrap();
        assert_eq!(derived, pair.public_key, "both halves describe one key");
    }

    #[test]
    fn the_public_key_survives_a_pem_round_trip() {
        let pair = generate_pair().unwrap();
        let reparsed = SecretKey::from_pkcs8_pem(&pair.private_pem).unwrap();
        let again = pair_of_secret(&reparsed).unwrap();
        assert_eq!(again.private_pem, pair.private_pem, "PEM round-trips");
        assert_eq!(again.public_key, pair.public_key, "and so does the point");
    }

    #[test]
    fn garbage_private_pems_are_refused_without_leaking() {
        let secret_marker = "not-a-key-ME1CAQIBA";
        for junk in [
            "",
            "-----BEGIN PRIVATE KEY-----\nQUJD\n-----END PRIVATE KEY-----",
            secret_marker,
        ] {
            let err = public_key_of_private_pem(junk).expect_err("junk must not parse");
            let message = err.to_string();
            assert!(
                message.contains(VAPID_PRIVATE_NAME),
                "the error names the setting: {message}"
            );
            assert!(
                !message.contains(junk) || junk.is_empty(),
                "and never quotes the input"
            );
        }
    }

    #[test]
    fn debug_never_shows_the_private_pem() {
        let pair = generate_pair().unwrap();
        let rendered = format!("{pair:?}");
        assert!(
            !rendered.contains("PRIVATE KEY") && !rendered.contains(&pair.private_pem),
            "the secret half stays out of Debug output"
        );
        assert!(rendered.contains("<redacted>"), "the marker stands in");
        assert!(rendered.contains(&pair.public_key), "the public half shows");
    }

    #[tokio::test]
    async fn ensure_generates_once_and_keeps_the_pair() {
        let db = crate::test_db::test_db().await.expect("test database slot");
        let pool = db.pool();
        let config = Config::for_tests();

        let first = ensure_instance_keypair(pool, &config)
            .await
            .expect("the first startup seeds");
        let second = ensure_instance_keypair(pool, &config)
            .await
            .expect("the second startup runs");

        assert_eq!(first, second, "the stored pair is never rewritten");
        let (private, public) = stored_pair(pool).await;
        assert!(
            private.is_some_and(|pem| pem.contains("BEGIN PRIVATE KEY")),
            "both halves are stored"
        );
        assert_eq!(public.as_deref(), Some(first.as_str()));
    }

    #[tokio::test]
    async fn a_configured_private_key_overrides_the_stored_pair() {
        let db = crate::test_db::test_db().await.expect("test database slot");
        let pool = db.pool();
        let generated = ensure_instance_keypair(pool, &Config::for_tests())
            .await
            .expect("the generated pair starts");
        let mine = generate_pair().unwrap();
        let config = config_with(Some(&mine.private_pem), None);

        let advertised = ensure_instance_keypair(pool, &config)
            .await
            .expect("the configured key takes over");

        assert_eq!(advertised, mine.public_key, "its public half is derived");
        assert_ne!(advertised, generated, "and the stored pair moved");
        let (private, public) = stored_pair(pool).await;
        assert_eq!(
            private.as_deref(),
            Some(mine.private_pem.trim()),
            "the configured PEM is stored trimmed, like every setting"
        );
        assert_eq!(public.as_deref(), Some(mine.public_key.as_str()));
    }

    #[tokio::test]
    async fn a_configured_public_key_wins_over_the_derived_one() {
        let db = crate::test_db::test_db().await.expect("test database slot");
        let pool = db.pool();
        let mine = generate_pair().unwrap();
        let config = config_with(Some(&mine.private_pem), Some("pinned-public-key"));

        let advertised = ensure_instance_keypair(pool, &config)
            .await
            .expect("the pinned pair applies");

        assert_eq!(advertised, "pinned-public-key", "verbatim");
        let (private, public) = stored_pair(pool).await;
        assert_eq!(
            private.as_deref(),
            Some(mine.private_pem.trim()),
            "the configured PEM is stored trimmed"
        );
        assert_eq!(public.as_deref(), Some("pinned-public-key"));
    }

    #[tokio::test]
    async fn a_pinned_public_key_without_a_private_one_replaces_the_advertised_half() {
        let db = crate::test_db::test_db().await.expect("test database slot");
        let pool = db.pool();
        let config = config_with(None, Some("pinned-only-public"));

        let advertised = ensure_instance_keypair(pool, &config)
            .await
            .expect("the pin applies");

        assert_eq!(
            advertised, "pinned-only-public",
            "the public row is the pin"
        );
        let (private, public) = stored_pair(pool).await;
        assert!(private.is_some(), "a private half still exists");
        assert_eq!(public.as_deref(), Some("pinned-only-public"));
    }

    #[tokio::test]
    async fn an_unparseable_configured_key_refuses_startup() {
        let db = crate::test_db::test_db().await.expect("test database slot");
        let pool = db.pool();
        let config = config_with(Some("-----BEGIN PRIVATE KEY-----\nQUJD\n"), None);
        assert!(
            ensure_instance_keypair(pool, &config).await.is_err(),
            "the operator's broken key stops the startup, loudly"
        );
    }

    #[tokio::test]
    async fn the_lazy_reader_seeds_a_pair_and_stays_stable() {
        let db = crate::test_db::test_db().await.expect("test database slot");
        let pool = db.pool();

        let first = vapid_public_key(pool).await.expect("the lazy seed runs");
        let second = vapid_public_key(pool).await.expect("the second read runs");

        assert_eq!(first, second, "one pair, however many readers");
        let point = URL_SAFE_NO_PAD.decode(&first).unwrap();
        assert_eq!(point.len(), UNCOMPRESSED_POINT_BYTES, "a real point");
        let (private, _) = stored_pair(pool).await;
        assert!(private.is_some(), "the seeding wrote the private half too");
    }
}
