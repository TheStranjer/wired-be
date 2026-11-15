//! The `oauth_apps` table: client applications registered through
//! `POST /api/v1/apps` and administered through
//! `…/api/v1/pleroma/admin/oauth_app` (WBE-205).
//!
//! The reference shape is Pleroma's `App` schema
//! (`lib/pleroma/web/o_auth/app.ex`): a name, its redirect URIs stored as
//! one space- or newline-separated string, a scope string, optional
//! website, the credential pair and the `trusted` flag. Per the ticket
//! `user_id` is a plain uuid without a foreign key yet — the owner is
//! only known once OAuth authentication exists, and user-unbound app
//! tokens keep it `NULL`.
//!
//! [`create`] mints the primary key (`UUIDv7`, decision 4) and the
//! credential pair itself, mirroring Pleroma's `register_changeset`;
//! callers supply only the registration fields. [`create_as_admin`],
//! [`search`], [`update`] and [`delete`] are the admin API's half: the
//! same minting with the admin-managed `trusted` flag bound, the exact-
//! filter listing, the `COALESCE`-present-only write and the removal.
//! `client_secret` is secret material: the manual `Debug` impl redacts
//! it, so it can never reach a log line.

use std::fmt;

use chrono::{DateTime, Utc};
use sqlx::PgPool;
use uuid::Uuid;

use crate::{
    error::AppResult,
    util::{
        id::new_id,
        token::{CREDENTIAL_BYTES, random_token},
    },
};

/// One row of `oauth_apps`: a registered client application.
#[derive(sqlx::FromRow)]
pub struct OauthApp {
    /// `UUIDv7` primary key, minted by [`create`] (decision 4).
    pub id: Uuid,
    /// Human-facing application name, as the client sent it.
    pub client_name: String,
    /// Registered redirect URIs, space- or newline-separated exactly as
    /// stored.
    pub redirect_uris: String,
    /// Granted scopes, space-separated (see
    /// [`crate::oauth::scopes::Scopes`]).
    pub scopes: String,
    /// Application homepage, if set.
    pub website: Option<String>,
    /// Public OAuth client id: a 43-character URL-safe random token.
    pub client_id: String,
    /// OAuth client secret. Never in `Debug` output.
    pub client_secret: String,
    /// Whether the app skips the authorization confirmation screen.
    pub trusted: bool,
    /// Owning user; `None` for user-unbound registrations. A plain uuid,
    /// no foreign key yet.
    pub user_id: Option<Uuid>,
    /// When the row was inserted.
    pub inserted_at: DateTime<Utc>,
    /// When the row last changed.
    pub updated_at: DateTime<Utc>,
}

/// [`client_secret`](Self::client_secret) is secret material and renders
/// as `<redacted>`; everything else shows.
impl fmt::Debug for OauthApp {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("OauthApp")
            .field("id", &self.id)
            .field("client_name", &self.client_name)
            .field("redirect_uris", &self.redirect_uris)
            .field("scopes", &self.scopes)
            .field("website", &self.website)
            .field("client_id", &self.client_id)
            .field("client_secret", &"<redacted>")
            .field("trusted", &self.trusted)
            .field("user_id", &self.user_id)
            .field("inserted_at", &self.inserted_at)
            .field("updated_at", &self.updated_at)
            .finish()
    }
}

/// What [`create`] inserts, besides what it mints itself (`id`,
/// `client_id`, `client_secret`) and what the database stamps
/// (`trusted`'s default, the timestamps).
#[derive(Clone, Debug)]
pub struct NewOauthApp {
    /// Application name to store.
    pub client_name: String,
    /// Redirect URIs in their stored spelling (space/newline separated).
    pub redirect_uris: String,
    /// Scopes as one space-separated string.
    pub scopes: String,
    /// Optional homepage URL.
    pub website: Option<String>,
    /// Owner; `None` unless an authenticated registration binds the app.
    pub user_id: Option<Uuid>,
}

/// The `INSERT` behind [`create`]: every registration column is bound
/// from a [`NewOauthApp`] plus the minted id and credentials, with
/// `RETURNING *` so the row comes back with its database-side defaults
/// and timestamps.
const INSERT_SQL: &str = "INSERT INTO oauth_apps (
     id, client_name, redirect_uris, scopes, website, client_id,
     client_secret, user_id
) VALUES ($1, $2, $3, $4, $5, $6, $7, $8) RETURNING *";

/// Inserts an app registration and returns the stored row.
///
/// The id is a fresh `UUIDv7` (decision 4) and the credential pair two
/// independent 43-character random tokens (Pleroma's
/// `register_changeset`); `trusted` takes the column default `false`.
/// `client_id` is unique, so a credential collision is rejected.
///
/// # Errors
///
/// Any database failure (connection, constraint, encoding) arrives as
/// [`AppError::Database`](crate::error::AppError).
pub async fn create(pool: &PgPool, new_app: &NewOauthApp) -> AppResult<OauthApp> {
    let app = sqlx::query_as::<_, OauthApp>(INSERT_SQL)
        .bind(new_id())
        .bind(new_app.client_name.as_str())
        .bind(new_app.redirect_uris.as_str())
        .bind(new_app.scopes.as_str())
        .bind(new_app.website.as_deref())
        .bind(random_token(CREDENTIAL_BYTES))
        .bind(random_token(CREDENTIAL_BYTES))
        .bind(new_app.user_id)
        .fetch_one(pool)
        .await?;
    Ok(app)
}

/// Fetches the app whose OAuth client id is `client_id`, `None` when no
/// row matches. The comparison is exact (plain `text`).
///
/// # Errors
///
/// Any database failure (connection, encoding) arrives as
/// [`AppError::Database`](crate::error::AppError).
pub async fn get_by_client_id(pool: &PgPool, client_id: &str) -> AppResult<Option<OauthApp>> {
    let app = sqlx::query_as::<_, OauthApp>("SELECT * FROM oauth_apps WHERE client_id = $1")
        .bind(client_id)
        .fetch_optional(pool)
        .await?;
    Ok(app)
}

/// Fetches the app whose primary key is `id`, `None` when no row
/// matches.
///
/// # Errors
///
/// Any database failure (connection, encoding) arrives as
/// [`AppError::Database`](crate::error::AppError).
pub async fn get_by_id(pool: &PgPool, id: Uuid) -> AppResult<Option<OauthApp>> {
    let app = sqlx::query_as::<_, OauthApp>("SELECT * FROM oauth_apps WHERE id = $1")
        .bind(id)
        .fetch_optional(pool)
        .await?;
    Ok(app)
}

/// The `SELECT` behind [`list_for_user`]: the whole `oauth_apps` row,
/// kept when the user owns the app or holds any token issued for it —
/// the `EXISTS` subquery deduplicates multi-token apps to one row —
/// newest app first (`UUIDv7` text sorts by mint time, decision 4).
const LIST_FOR_USER_SQL: &str = "SELECT * FROM oauth_apps a
  WHERE a.user_id = $1
     OR EXISTS (
            SELECT 1 FROM oauth_tokens t
             WHERE t.app_id = a.id AND t.user_id = $1
        )
  ORDER BY a.id DESC";

/// Fetches the apps visible to `GET /api/v1/pleroma/apps` for `user_id`:
/// the ones they registered ([`OauthApp::user_id`]) plus the ones they
/// hold a token for, deduplicated and newest first.
///
/// Expiry is not considered: holding a (even lapsed) token still names
/// the app. An unknown user yields an empty `Vec`.
///
/// # Errors
///
/// Any database failure (connection, encoding) arrives as
/// [`AppError::Database`](crate::error::AppError).
pub async fn list_for_user(pool: &PgPool, user_id: Uuid) -> AppResult<Vec<OauthApp>> {
    let apps = sqlx::query_as::<_, OauthApp>(LIST_FOR_USER_SQL)
        .bind(user_id)
        .fetch_all(pool)
        .await?;
    Ok(apps)
}

/// The filter and window of `GET /api/v1/pleroma/admin/oauth_app`
/// (Pleroma's `App.search` parameter map, `lib/pleroma/web/o_auth/app.ex`).
#[derive(Clone, Debug, Default)]
pub struct AppSearchParams {
    /// Keep only apps whose `client_name` equals this exactly (Pleroma's
    /// `name` filter, mapped to the column by the controller).
    pub name: Option<String>,
    /// Keep only the app whose `client_id` equals this exactly.
    pub client_id: Option<String>,
    /// Keep only apps with this `trusted` value; `None` filters nothing.
    /// Pleroma's `OpenAPI` default of `false` quietly filters the
    /// untrusted half of the table out of an unfiltered index; the ticket
    /// spells the parameter optional, so optional it is.
    pub trusted: Option<bool>,
    /// 1-based page number.
    pub page: i64,
    /// Rows per page.
    pub page_size: i64,
}

/// One page of the admin listing: the window's rows beside the total.
#[derive(Debug)]
pub struct AppSearchPage {
    /// The apps on this page, newest first (`UUIDv7` text order,
    /// decision 4).
    pub apps: Vec<OauthApp>,
    /// How many apps the instance has in total. Pleroma's `App.search`
    /// aggregates the whole table — `Repo.aggregate(__MODULE__, :count,
    /// :id)` — not the filtered query, so the number does not move with
    /// the filters and is reproduced as written.
    pub count: i64,
}

/// The row shape of the total-count half of [`search`].
#[derive(Debug, sqlx::FromRow)]
struct AppCount {
    /// The aggregate, `bigint` as `i64`.
    count: i64,
}

/// The `SELECT` behind [`search`]: the three exact filters as
/// `$n IS NULL OR`-guards, newest app first, one `LIMIT`/`OFFSET` window.
const SEARCH_SQL: &str = "SELECT * FROM oauth_apps
   WHERE ($1::text IS NULL OR client_name = $1)
     AND ($2::text IS NULL OR client_id = $2)
     AND ($3::bool IS NULL OR trusted = $3)
   ORDER BY id DESC
   LIMIT $4 OFFSET $5";

/// The whole-table total [`search`] reports beside its window
/// (Pleroma's unfilled `count`).
const COUNT_ALL_SQL: &str = "SELECT count(*) AS count FROM oauth_apps";

/// Runs the admin listing: the filtered, windowed rows and the
/// whole-table count, in one round-trip pair.
///
/// A page past the end yields an empty `apps` beside the still-total
/// `count`; the filters are exact matches, not patterns.
///
/// # Errors
///
/// Any database failure (connection, encoding) arrives as
/// [`AppError::Database`](crate::error::AppError).
pub async fn search(pool: &PgPool, params: &AppSearchParams) -> AppResult<AppSearchPage> {
    let offset = (params.page - 1).saturating_mul(params.page_size);
    let apps = sqlx::query_as::<_, OauthApp>(SEARCH_SQL)
        .bind(params.name.as_deref())
        .bind(params.client_id.as_deref())
        .bind(params.trusted)
        .bind(params.page_size)
        .bind(offset)
        .fetch_all(pool)
        .await?;
    let total = sqlx::query_as::<_, AppCount>(COUNT_ALL_SQL)
        .fetch_one(pool)
        .await?;
    Ok(AppSearchPage {
        apps,
        count: total.count,
    })
}

/// What [`create_as_admin`] inserts, besides what it mints itself (`id`,
/// `client_id`, `client_secret`) and what the database stamps: the admin
/// registration form.
///
/// It is every [`NewOauthApp`] field plus the `trusted` flag only admins
/// manage.
#[derive(Clone, Debug)]
pub struct AdminNewApp {
    /// Application name to store.
    pub client_name: String,
    /// Redirect URIs in their stored spelling (space/newline separated).
    pub redirect_uris: String,
    /// Scopes as one space-separated string.
    pub scopes: String,
    /// Optional homepage URL.
    pub website: Option<String>,
    /// Whether the app skips the authorization consent screen.
    pub trusted: bool,
}

/// The `INSERT` behind [`create_as_admin`]: [`INSERT_SQL`]'s column list
/// with `trusted` bound from the form instead of left to the column
/// default, and `user_id` left out — an admin-registered app has no
/// owner (Pleroma's admin `App.create` passes no user either).
const INSERT_ADMIN_SQL: &str = "INSERT INTO oauth_apps (
     id, client_name, redirect_uris, scopes, website, client_id,
     client_secret, trusted
) VALUES ($1, $2, $3, $4, $5, $6, $7, $8) RETURNING *";

/// Inserts an app through the admin API and returns the stored row.
///
/// The id and the credential pair are minted exactly as [`create`] mints
/// them; the difference is the flag: `trusted` is the admin's to set.
///
/// # Errors
///
/// Any database failure (connection, constraint, encoding) arrives as
/// [`AppError::Database`](crate::error::AppError).
pub async fn create_as_admin(pool: &PgPool, form: &AdminNewApp) -> AppResult<OauthApp> {
    let app = sqlx::query_as::<_, OauthApp>(INSERT_ADMIN_SQL)
        .bind(new_id())
        .bind(form.client_name.as_str())
        .bind(form.redirect_uris.as_str())
        .bind(form.scopes.as_str())
        .bind(form.website.as_deref())
        .bind(random_token(CREDENTIAL_BYTES))
        .bind(random_token(CREDENTIAL_BYTES))
        .bind(form.trusted)
        .fetch_one(pool)
        .await?;
    Ok(app)
}

/// The present-only changes of `PATCH /api/v1/pleroma/admin/oauth_app/{id}`.
///
/// `None` keeps the stored column, `Some` writes over it. `website`
/// cannot be nulled — sending `null` reads as absent, the same way every
/// other `Option` body field in this project does.
#[derive(Clone, Debug, Default)]
pub struct AppChanges {
    /// New application name.
    pub client_name: Option<String>,
    /// New redirect URIs, in their stored spelling.
    pub redirect_uris: Option<String>,
    /// New scope string.
    pub scopes: Option<String>,
    /// New homepage URL.
    pub website: Option<String>,
    /// New trusted flag.
    pub trusted: Option<bool>,
}

/// The `UPDATE` behind [`update`]: every mutable column through a
/// `COALESCE` that keeps the stored value when the parameter is `NULL`,
/// `updated_at` re-stamped, the row handed back whole.
const UPDATE_SQL: &str = "UPDATE oauth_apps
     SET client_name   = COALESCE($2, client_name),
         redirect_uris = COALESCE($3, redirect_uris),
         scopes        = COALESCE($4, scopes),
         website       = COALESCE($5, website),
         trusted       = COALESCE($6, trusted),
         updated_at    = now()
   WHERE id = $1
RETURNING *";

/// Applies [`AppChanges`] to the app behind `id` and returns the updated
/// row, `None` when no row carries that id.
///
/// # Errors
///
/// Any database failure (connection, encoding) arrives as
/// [`AppError::Database`](crate::error::AppError).
pub async fn update(pool: &PgPool, id: Uuid, changes: &AppChanges) -> AppResult<Option<OauthApp>> {
    let app = sqlx::query_as::<_, OauthApp>(UPDATE_SQL)
        .bind(id)
        .bind(changes.client_name.as_deref())
        .bind(changes.redirect_uris.as_deref())
        .bind(changes.scopes.as_deref())
        .bind(changes.website.as_deref())
        .bind(changes.trusted)
        .fetch_optional(pool)
        .await?;
    Ok(app)
}

/// Deletes the app behind `id`, reporting whether a row went.
///
/// # Errors
///
/// Any database failure (connection, encoding) arrives as
/// [`AppError::Database`](crate::error::AppError).
pub async fn delete(pool: &PgPool, id: Uuid) -> AppResult<bool> {
    let outcome = sqlx::query("DELETE FROM oauth_apps WHERE id = $1")
        .bind(id)
        .execute(pool)
        .await?;
    Ok(outcome.rows_affected() > 0)
}

#[cfg(test)]
mod tests {
    use sqlx::PgPool;

    use super::*;

    /// A [`NewOauthApp`] named `name` with one redirect URI and `read`.
    fn new_app(name: &str) -> NewOauthApp {
        NewOauthApp {
            client_name: name.to_owned(),
            redirect_uris: "https://client.example/callback".to_owned(),
            scopes: "read".to_owned(),
            website: None,
            user_id: None,
        }
    }

    /// Whether every character is in the URL-safe base64 alphabet.
    fn is_url_safe(token: &str) -> bool {
        token
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || c == '-' || c == '_')
    }

    #[tokio::test]
    async fn create_round_trips_every_column() {
        let db = crate::test_db::test_db().await.expect("test database slot");
        let pool = db.pool().clone();
        let owner = new_id();
        let app = create(
            &pool,
            &NewOauthApp {
                client_name: "My Client".to_owned(),
                redirect_uris: "https://a.example/cb\nhttps://b.example/cb".to_owned(),
                scopes: "read write follow".to_owned(),
                website: Some("https://client.example".to_owned()),
                user_id: Some(owner),
            },
        )
        .await
        .unwrap();
        assert!(app.id.get_version_num() == 7, "id is a UUIDv7");
        assert_eq!(app.client_name, "My Client", "name round trips");
        assert_eq!(
            app.redirect_uris, "https://a.example/cb\nhttps://b.example/cb",
            "the stored redirect_uris string survives verbatim"
        );
        assert_eq!(app.scopes, "read write follow", "scopes round trip");
        assert_eq!(
            app.website.as_deref(),
            Some("https://client.example"),
            "website round trips"
        );
        assert!(
            app.client_id.len() == 43 && is_url_safe(&app.client_id),
            "client_id is a 43-char URL-safe token, got `{}`",
            app.client_id
        );
        assert!(
            app.client_secret.len() == 43 && is_url_safe(&app.client_secret),
            "client_secret is a 43-char URL-safe token"
        );
        assert_ne!(app.client_id, app.client_secret, "the tokens differ");
        assert!(!app.trusted, "trusted takes the column default");
        assert_eq!(app.user_id, Some(owner), "the owner uuid round trips");
        assert!(
            app.inserted_at <= app.updated_at,
            "the database stamps both timestamps"
        );

        let by_id = get_by_id(&pool, app.id).await.unwrap().expect("by id");
        let by_client_id = get_by_client_id(&pool, &app.client_id)
            .await
            .unwrap()
            .expect("by client_id");
        assert_eq!(by_id.client_secret, app.client_secret, "same row");
        assert_eq!(by_client_id.id, app.id, "the client id finds the same row");
    }

    #[tokio::test]
    async fn defaults_apply_when_only_the_required_fields_are_set() {
        let db = crate::test_db::test_db().await.expect("test database slot");
        let pool = db.pool().clone();
        let app = create(&pool, &new_app("Plain")).await.unwrap();
        assert_eq!(app.scopes, "read", "the caller-provided default");
        assert!(app.website.is_none(), "website stays NULL");
        assert!(app.user_id.is_none(), "an unauthenticated app has no owner");
        assert!(!app.trusted, "registrations are not trusted");
    }

    #[tokio::test]
    async fn unknown_lookups_are_none() {
        let db = crate::test_db::test_db().await.expect("test database slot");
        let pool = db.pool().clone();
        assert!(
            get_by_id(&pool, new_id()).await.unwrap().is_none(),
            "an unknown id finds nothing"
        );
        assert!(
            get_by_client_id(&pool, "no-such-client")
                .await
                .unwrap()
                .is_none(),
            "an unknown client id finds nothing"
        );
    }

    #[tokio::test]
    async fn every_registration_gets_its_own_credentials() {
        let db = crate::test_db::test_db().await.expect("test database slot");
        let pool = db.pool().clone();
        let first = create(&pool, &new_app("First")).await.unwrap();
        let second = create(&pool, &new_app("First")).await.unwrap();
        assert_eq!(first.client_name, second.client_name, "names may repeat");
        assert_ne!(first.id, second.id, "ids differ");
        assert_ne!(first.client_id, second.client_id, "client ids differ");
        assert_ne!(
            first.client_secret, second.client_secret,
            "client secrets differ"
        );
        assert_eq!(
            first.client_id.len(),
            random_token(CREDENTIAL_BYTES).len(),
            "both credentials use the fixed length"
        );
    }

    #[tokio::test]
    async fn debug_redacts_the_client_secret() {
        let db = crate::test_db::test_db().await.expect("test database slot");
        let pool = db.pool().clone();
        let app = create(&pool, &new_app("Secretive")).await.unwrap();
        let debug = format!("{app:?}");
        assert!(
            !debug.contains(&app.client_secret),
            "the client secret must not leak"
        );
        assert!(
            debug.contains(r#"client_secret: "<redacted>""#),
            "the redaction marker names the field"
        );
        assert!(
            debug.contains(&app.client_id),
            "the public client id still shows"
        );
        assert!(
            debug.contains("Secretive"),
            "the rest of the row is visible"
        );
    }

    /// Inserts a local user named `nickname` and returns its id.
    async fn insert_user(pool: &PgPool, nickname: &str) -> Uuid {
        use crate::models::user::{self, NewUser};

        let ap_id = format!("https://test.example/users/{nickname}");
        user::insert(pool, &NewUser::local(nickname, &ap_id))
            .await
            .unwrap()
            .id
    }

    /// Mints a token acting as `user_id` issued by `app` and returns it.
    async fn insert_token(
        pool: &PgPool,
        user_id: Uuid,
        app: &OauthApp,
    ) -> crate::models::oauth_token::OauthToken {
        use crate::models::oauth_token::{self, NewToken};

        oauth_token::create_token(
            pool,
            &NewToken {
                user_id: Some(user_id),
                app_id: app.id,
                scopes: "read".to_owned(),
                ttl: None,
            },
        )
        .await
        .unwrap()
    }

    #[tokio::test]
    async fn list_for_user_covers_owned_and_tokened_apps_deduped() {
        use crate::models::oauth_token;

        let db = crate::test_db::test_db().await.expect("test database slot");
        let pool = db.pool().clone();
        let alice = insert_user(&pool, "alice").await;
        let bob = insert_user(&pool, "bob").await;
        let owned = create(
            &pool,
            &NewOauthApp {
                user_id: Some(alice),
                ..new_app("Owned")
            },
        )
        .await
        .unwrap();
        let tokened = create(&pool, &new_app("Tokened")).await.unwrap();
        let foreign = create(
            &pool,
            &NewOauthApp {
                user_id: Some(bob),
                ..new_app("Foreign")
            },
        )
        .await
        .unwrap();
        let _stranger = create(&pool, &new_app("Stranger")).await.unwrap();
        let older_token = insert_token(&pool, alice, &tokened).await;
        let _newer_token = insert_token(&pool, alice, &tokened).await;
        let _bobs_token = insert_token(&pool, bob, &foreign).await;

        let listed = list_for_user(&pool, alice).await.unwrap();
        let ids: Vec<Uuid> = listed.iter().map(|app| app.id).collect();
        assert_eq!(
            ids,
            vec![tokened.id, owned.id],
            "owned plus tokened apps, deduplicated, newest first"
        );
        assert!(
            !ids.contains(&foreign.id),
            "another user's app stays out even when that user holds its token"
        );
        assert_eq!(
            list_for_user(&pool, bob).await.unwrap().len(),
            1,
            "bob sees only his own app"
        );
        assert!(
            list_for_user(&pool, new_id()).await.unwrap().is_empty(),
            "an unknown user sees no apps"
        );
        // Tokens exist for the tokened app (checked through the model).
        let (found, _) = oauth_token::get_with_app(&pool, &older_token.token)
            .await
            .unwrap()
            .expect("the fixture token is stored");
        assert_eq!(found.app_id, tokened.id, "and names the tokened app");
    }

    /// An [`AdminNewApp`] named `name`, everything else stock.
    fn admin_form(name: &str) -> AdminNewApp {
        AdminNewApp {
            client_name: name.to_owned(),
            redirect_uris: "https://client.example/callback".to_owned(),
            scopes: "read".to_owned(),
            website: None,
            trusted: false,
        }
    }

    #[tokio::test]
    async fn create_as_admin_stores_the_trusted_flag_and_mints_credentials() {
        let db = crate::test_db::test_db().await.expect("test database slot");
        let pool = db.pool().clone();
        let app = create_as_admin(
            &pool,
            &AdminNewApp {
                trusted: true,
                website: Some("https://admin.example".to_owned()),
                ..admin_form("Admin App")
            },
        )
        .await
        .unwrap();
        assert!(app.trusted, "the flag the admin set is stored");
        assert_eq!(app.client_name, "Admin App", "the name round trips");
        assert_eq!(app.user_id, None, "an admin app has no owner");
        assert_eq!(app.client_id.len(), 43, "the credential pair is minted");
        assert_eq!(app.client_secret.len(), 43, "as the client register does");

        let untrusted = create_as_admin(&pool, &admin_form("Plain Admin App"))
            .await
            .unwrap();
        assert!(!untrusted.trusted, "and false stays false");
    }

    #[tokio::test]
    async fn search_filters_each_way_and_counts_the_whole_table() {
        let db = crate::test_db::test_db().await.expect("test database slot");
        let pool = db.pool().clone();
        let keep = create_as_admin(&pool, &admin_form("Keep")).await.unwrap();
        let drop_it = create_as_admin(&pool, &admin_form("Drop")).await.unwrap();
        let trusted = create_as_admin(
            &pool,
            &AdminNewApp {
                trusted: true,
                ..admin_form("Keep")
            },
        )
        .await
        .unwrap();
        let window = |page: i64, page_size: i64| AppSearchParams {
            name: None,
            client_id: None,
            trusted: None,
            page,
            page_size,
        };

        let all = search(&pool, &window(1, 50)).await.unwrap();
        assert_eq!(all.count, 3, "the total is the whole table");
        assert_eq!(all.apps.len(), 3, "and unfiltered all three show");
        assert_eq!(all.apps[0].id, trusted.id, "newest first");

        let named = AppSearchParams {
            name: Some("Keep".to_owned()),
            ..window(1, 50)
        };
        let by_name = search(&pool, &named).await.unwrap();
        assert_eq!(by_name.apps.len(), 2, "the exact name filter");
        assert_eq!(by_name.count, 3, "the count ignores the filter");

        let by_client_id = AppSearchParams {
            client_id: Some(drop_it.client_id.clone()),
            ..window(1, 50)
        };
        let found = search(&pool, &by_client_id).await.unwrap();
        assert_eq!(found.apps.len(), 1, "the client id finds one");
        assert_eq!(found.apps[0].id, drop_it.id, "and it is the right one");

        let trusted_only = AppSearchParams {
            trusted: Some(true),
            ..window(1, 50)
        };
        let flagged = search(&pool, &trusted_only).await.unwrap();
        assert_eq!(flagged.apps.len(), 1, "the flag filter");
        assert_eq!(flagged.apps[0].id, trusted.id, "the trusted app only");

        let windowed = search(&pool, &window(2, 2)).await.unwrap();
        assert_eq!(
            windowed.apps.len(),
            1,
            "the second page takes the remainder"
        );
        assert_eq!(windowed.apps[0].id, keep.id, "oldest of the three");

        let past = search(&pool, &window(9, 50)).await.unwrap();
        assert!(past.apps.is_empty(), "a page past the end is empty");
        assert_eq!(past.count, 3, "and the count still says three");
    }

    #[tokio::test]
    async fn update_is_present_only_and_names_the_row_it_missed() {
        let db = crate::test_db::test_db().await.expect("test database slot");
        let pool = db.pool().clone();
        let app = create_as_admin(
            &pool,
            &AdminNewApp {
                website: Some("https://before.example".to_owned()),
                ..admin_form("Renamed")
            },
        )
        .await
        .unwrap();

        let touched = update(
            &pool,
            app.id,
            &AppChanges {
                trusted: Some(true),
                ..AppChanges::default()
            },
        )
        .await
        .unwrap()
        .expect("the row is there");
        assert!(touched.trusted, "the flag moved");
        assert_eq!(touched.client_name, "Renamed", "everything else stayed");
        assert_eq!(
            touched.website.as_deref(),
            Some("https://before.example"),
            "including the website"
        );
        assert_eq!(
            touched.client_id, app.client_id,
            "credentials are never rewritten"
        );

        let rewritten = update(
            &pool,
            app.id,
            &AppChanges {
                client_name: Some("Finally".to_owned()),
                redirect_uris: Some("https://after.example/cb".to_owned()),
                scopes: Some("read write".to_owned()),
                website: Some("https://after.example".to_owned()),
                trusted: Some(false),
            },
        )
        .await
        .unwrap()
        .expect("the row is still there");
        assert_eq!(rewritten.client_name, "Finally", "name");
        assert_eq!(
            rewritten.redirect_uris, "https://after.example/cb",
            "redirect uris"
        );
        assert_eq!(rewritten.scopes, "read write", "scopes");
        assert_eq!(
            rewritten.website.as_deref(),
            Some("https://after.example"),
            "website"
        );
        assert!(!rewritten.trusted, "and trusted back to false");

        let missed = update(
            &pool,
            new_id(),
            &AppChanges {
                client_name: Some("ghost".to_owned()),
                ..AppChanges::default()
            },
        )
        .await
        .unwrap();
        assert!(missed.is_none(), "an unknown id updates nothing");
    }

    #[tokio::test]
    async fn delete_removes_the_row_once() {
        let db = crate::test_db::test_db().await.expect("test database slot");
        let pool = db.pool().clone();
        let app = create_as_admin(&pool, &admin_form("Doomed")).await.unwrap();
        assert!(
            delete(&pool, app.id).await.unwrap(),
            "the first delete takes the row"
        );
        assert!(
            get_by_id(&pool, app.id).await.unwrap().is_none(),
            "and it is gone"
        );
        assert!(
            !delete(&pool, app.id).await.unwrap(),
            "a second delete finds nothing to remove"
        );
    }
}
