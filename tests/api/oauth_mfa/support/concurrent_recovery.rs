//! Backup codes are spent atomically (WBE-473): two simultaneous
//! challenges presenting the same recovery code cannot both pass.

use super::*;

/// Two challenges on the same account race the same recovery code through
/// `/oauth/mfa/challenge`: exactly one answers 200 with a token, the other
/// the 400 refusal, and the code is gone from the stored list while the
/// rest stay.
#[tokio::test]
async fn one_recovery_code_cannot_pass_two_concurrent_challenges() {
    let db = common::test_db().await;
    let pool = db.pool().clone();
    let fix = mfa_account(&pool, "oscar").await;
    let app = common::create_app(&pool, "read").await;
    let http = common::test_app(pool.clone());
    let first = mfa_grant(&http, &app, "oscar", "read").await;
    let second = mfa_grant(&http, &app, "oscar", "read").await;

    let first_fields = challenge_fields(&first, "recovery", &fix.codes[0], &app);
    let second_fields = challenge_fields(&second, "recovery", &fix.codes[0], &app);
    let (a, b) = tokio::join!(
        post_challenge(&http, &first_fields),
        post_challenge(&http, &second_fields),
    );

    let statuses = [a.status, b.status];
    assert_eq!(
        statuses
            .iter()
            .filter(|status| **status == StatusCode::OK)
            .count(),
        1,
        "exactly one challenge passes: {statuses:?}"
    );
    let (passed, refused) = if a.status == StatusCode::OK {
        (&a, &b)
    } else {
        (&b, &a)
    };
    assert!(
        passed.json_value()["access_token"].is_string(),
        "the winner gets the token JSON"
    );
    assert_invalid_code(refused, "the losing challenge");

    let stored = mfa::MfaSettings::for_user(
        &wired_be::models::user::get_by_id(&pool, fix.user.id)
            .await
            .expect("the lookup reaches the database")
            .expect("the account exists"),
    );
    assert_eq!(
        stored.backup_codes.len(),
        fix.codes.len() - 1,
        "exactly one hash was spent"
    );
    assert!(
        !stored
            .backup_codes
            .contains(&totp::hash_backup_code(&fix.codes[0])),
        "and it is the raced one"
    );
}
