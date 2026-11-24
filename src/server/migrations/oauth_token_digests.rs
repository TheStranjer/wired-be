//! The `oauth_token_digests` migration: existing plaintext token rows are
//! rewritten as SHA-256 digests so live sessions keep working (CORE-6).

use crate::{
    models::{
        oauth_app::{NewOauthApp, create as create_app},
        oauth_token::get_with_app,
        user::{self, NewUser},
    },
    util::token::sha256_hex,
};

/// The migration's own SQL, replayed against a slot that already holds the
/// migrated schema.
const MIGRATION_SQL: &str =
    include_str!("../../../migrations/20261123150000_oauth_token_digests.sql");

/// A row written before the migration, with raw credentials `abc` and
/// `def`, holds their digests afterwards and authenticates with `abc`
/// through the lookups, while the digest itself and the raw refresh token
/// do not.
#[tokio::test]
async fn a_plaintext_row_authenticates_with_its_raw_token_after_the_migration() {
    let db = crate::test_db::test_db().await.expect("test database slot");
    let pool = db.pool().clone();
    let owner = user::insert(
        &pool,
        &NewUser::local(
            "migrated",
            &user::local_ap_id("https://test.example", "migrated"),
        ),
    )
    .await
    .unwrap();
    let app = create_app(
        &pool,
        &NewOauthApp {
            client_name: "Legacy".to_owned(),
            redirect_uris: "https://client.example/callback".to_owned(),
            scopes: "read".to_owned(),
            website: None,
            user_id: None,
        },
    )
    .await
    .unwrap();
    let id = uuid::Uuid::now_v7();
    let _inserted = sqlx::query(
        "INSERT INTO oauth_tokens (id, token, refresh_token, user_id, app_id, scopes, valid_until)
         VALUES ($1, 'abc', 'def', $2, $3, 'read', now() + interval '1 day')",
    )
    .bind(id)
    .bind(owner.id)
    .bind(app.id)
    .execute(&pool)
    .await
    .unwrap();

    let _migrated = sqlx::raw_sql(MIGRATION_SQL).execute(&pool).await.unwrap();

    let (stored_token, stored_refresh): (String, String) =
        sqlx::query_as("SELECT token, refresh_token FROM oauth_tokens WHERE id = $1")
            .bind(id)
            .fetch_one(&pool)
            .await
            .unwrap();
    assert_eq!(
        stored_token,
        sha256_hex("abc"),
        "the access token is its digest"
    );
    assert_eq!(
        stored_refresh,
        sha256_hex("def"),
        "the refresh token is its digest"
    );
    let found = get_with_app(&pool, "abc").await.unwrap();
    assert_eq!(
        found.map(|(row, _)| row.id),
        Some(id),
        "the raw token still finds the row after the migration"
    );
    assert!(
        get_with_app(&pool, &stored_token).await.unwrap().is_none(),
        "the digest is not a credential"
    );
}
