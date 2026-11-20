//! The fixtures the `notification` module's unit tests share: the
//! account-and-status trio a fixture notification needs, a plain local
//! account, and the one inserted row many reads start from. Test-only.

use sqlx::PgPool;
use uuid::Uuid;

use crate::models::user;

use super::{NewNotification, Notification, create};

/// The (recipient, actor, status) id trio a fixture notification needs,
/// three fresh rows of the test slot.
pub(super) async fn trio(pool: &PgPool) -> (Uuid, Uuid, Uuid) {
    use crate::{
        config::TEST_BASE_URL,
        models::{
            status::{self, NewStatus, Visibility},
            user::{NewUser, insert, local_ap_id},
        },
    };
    let recipient = insert(
        pool,
        &NewUser::local(
            "notif-recipient",
            &local_ap_id(TEST_BASE_URL, "notif-recipient"),
        ),
    )
    .await
    .expect("recipient fixture");
    let actor = insert(
        pool,
        &NewUser::local("notif-actor", &local_ap_id(TEST_BASE_URL, "notif-actor")),
    )
    .await
    .expect("actor fixture");
    let status = status::insert(
        pool,
        TEST_BASE_URL,
        &NewStatus {
            visibility: Visibility::Public,
            ..NewStatus::local(&actor, "a favourited post")
        },
    )
    .await
    .expect("status fixture");
    (recipient.id, actor.id, status.id)
}

/// Inserts a local user named `nickname`.
pub(super) async fn solo(pool: &PgPool, nickname: &str) -> user::User {
    use crate::config::TEST_BASE_URL;
    user::insert(
        pool,
        &user::NewUser::local(nickname, &user::local_ap_id(TEST_BASE_URL, nickname)),
    )
    .await
    .expect("local user fixture")
}

/// A `notification_type` notification of `recipient` by `actor` about
/// `status_id`.
pub(super) async fn note(
    pool: &PgPool,
    recipient: Uuid,
    actor: Uuid,
    notification_type: &str,
    status_id: Option<Uuid>,
) -> Notification {
    create(
        pool,
        &NewNotification {
            user_id: recipient,
            from_account_id: actor,
            notification_type,
            status_id,
            emoji: None,
            report_id: None,
            chat_message_id: None,
        },
    )
    .await
    .expect("the notification fixture inserts")
}
