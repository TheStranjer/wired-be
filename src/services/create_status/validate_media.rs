//! The attachment rule of [`check_body`]: `checked_media` reads the
//! submitted `media_ids` as ids, without touching the database.
//!
//! It counts them against [`Config::max_media_attachments`] and refuses id
//! text that is no id. Whether each surviving id names a row its poster may
//! claim is the write's, decided inside the status's own transaction.
//!
//! [`check_body`]: crate::services::create_status::check_body
//! [`Config::max_media_attachments`]: crate::config::Config::max_media_attachments

use uuid::Uuid;

use super::{BAD_MEDIA_ID, TOO_MANY_ATTACHMENTS};
use crate::{
    config::Config,
    error::{AppError, AppResult},
    services::create_status::CreateStatusParams,
    util::id::parse_id,
};

/// The submitted `media_ids` read as ids: the whole body's refusals that
/// need no database — more entries than [`Config::max_media_attachments`]
/// is [`TOO_MANY_ATTACHMENTS`] (counted on the submission as sent, repeats
/// and all, Pleroma's `validate_attachments_count` cap), and an entry that
/// is no [`Uuid`] text names no row anybody could claim and is
/// [`BAD_MEDIA_ID`]. The ids then collapse to their first positions, so the
/// order the client asked for survives to the write unchanged.
///
/// Whether each surviving id names a row its poster may attach is the
/// write's, made on the database as the status's own insert commits (see
/// [`crate::models::status::insert`]).
///
/// # Errors
///
/// [`AppError::Unprocessable`] with [`TOO_MANY_ATTACHMENTS`] or
/// [`BAD_MEDIA_ID`].
pub(super) fn checked_media(config: &Config, params: &CreateStatusParams) -> AppResult<Vec<Uuid>> {
    let Some(submitted) = params.media_ids.as_deref() else {
        return Ok(Vec::new());
    };
    if submitted.len() > config.max_media_attachments {
        return Err(AppError::Unprocessable(TOO_MANY_ATTACHMENTS.to_owned()));
    }
    let mut ids: Vec<Uuid> = Vec::with_capacity(submitted.len());
    for text in submitted {
        let id = parse_id(text).ok_or_else(|| AppError::Unprocessable(BAD_MEDIA_ID.to_owned()))?;
        if !ids.contains(&id) {
            ids.push(id);
        }
    }
    Ok(ids)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::services::create_status::{
        EMPTY_STATUS, check_body,
        test_support::{accepted, rejection, with_media},
    };
    #[test]
    fn checked_media_caps_the_submitted_ids_and_keeps_their_order() {
        let a = Uuid::now_v7().to_string();
        let b = Uuid::now_v7().to_string();

        // The cap counts the submission as sent: five asked-for attachments
        // are over the default limit of four, distinct ids or no.
        let many: Vec<String> = (0..5).map(|_| Uuid::now_v7().to_string()).collect();
        let strs: Vec<&str> = many.iter().map(String::as_str).collect();
        assert_eq!(rejection(&with_media("hi", &strs)), TOO_MANY_ATTACHMENTS);
        let exact = with_media("hi", &strs[..4]);
        assert_eq!(
            accepted(&exact).media_ids.len(),
            4,
            "the limit itself is exactly attachable"
        );

        // An id submitted twice claims its row once, at its first position.
        let twice = with_media("hi", &[a.as_str(), b.as_str(), a.as_str()]);
        assert_eq!(
            accepted(&twice).media_ids,
            vec![
                Uuid::parse_str(&a).expect("an id"),
                Uuid::parse_str(&b).expect("an id"),
            ],
            "the order the client asked for survives, duplicates folded out"
        );

        // The cap reads the submission before the dedup, and off the
        // instance's own number: five spellings of one id are still five
        // asked-for attachments against a limit of two.
        let mut tight = Config::for_tests();
        tight.max_media_attachments = 2;
        let five_same = with_media("hi", &[a.as_str(); 5]);
        let err = check_body(&tight, "public", false, &five_same)
            .expect_err("five entries under a limit of two is the refusal");
        assert_eq!(err.to_string(), TOO_MANY_ATTACHMENTS);
    }

    #[test]
    fn media_id_text_that_is_no_id_is_media_not_found() {
        for spelling in ["not-an-id", "", "  "] {
            let body = with_media("hi", &[spelling]);
            assert_eq!(
                rejection(&body),
                BAD_MEDIA_ID,
                "`{spelling}` names no row anybody could claim"
            );
        }
        let mixed = with_media("hi", &[&Uuid::now_v7().to_string(), "nope"]);
        assert_eq!(
            rejection(&mixed),
            BAD_MEDIA_ID,
            "and one bad entry refuses the whole body"
        );
    }

    #[test]
    fn attachments_stand_in_for_a_missing_text() {
        let one = Uuid::now_v7().to_string();

        let media_only = with_media("   ", &[one.as_str()]);
        let checked = accepted(&media_only);
        assert_eq!(checked.text, "", "the trimmed nothing stands as the text");
        assert_eq!(checked.media_ids.len(), 1, "beside its one attachment");

        let nothing = with_media("   ", &[]);
        assert_eq!(
            rejection(&nothing),
            EMPTY_STATUS,
            "an empty attachment list is no attachment: the text still owes the post"
        );

        let bad = with_media("", &["not-an-id"]);
        assert_eq!(
            rejection(&bad),
            BAD_MEDIA_ID,
            "the media gate is read before the text it relaxes, Pleroma's pipeline order"
        );
    }
}
