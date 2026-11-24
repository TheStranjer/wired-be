//! The parameters of `DELETE /api/v1/notifications/destroy_multiple`: the
//! `ids` array under either spelling clients send, parsed to distinct ids
//! and capped at [`MAX_BATCH_IDS`](crate::api::helpers::MAX_BATCH_IDS).

use serde::Deserialize;
use uuid::Uuid;

use crate::{
    api::helpers::{StringList, capped_distinct_ids, names_of},
    error::AppResult,
};

/// The bulk dismissal's parameters: `ids` under either spelling clients
/// send — a list (`ids[]=a&ids[]=b`, or a JSON array) or a lone value —
/// from the query string, the body, or both merged.
#[derive(Debug, Deserialize)]
pub(super) struct DestroyParams {
    /// The notifications to dismiss; absent asks for none.
    #[serde(default)]
    ids: Option<StringList>,
}

impl DestroyParams {
    /// The parsed ids, in the order sent, each named once; values that
    /// are not id text are skipped — they can name no row anyway.
    ///
    /// # Errors
    ///
    /// [`Unprocessable`](crate::error::AppError::Unprocessable) with
    /// [`TOO_MANY_IDS`](crate::api::helpers::TOO_MANY_IDS) for more than
    /// [`MAX_BATCH_IDS`](crate::api::helpers::MAX_BATCH_IDS) distinct ids.
    pub(super) fn parsed_ids(&self) -> AppResult<Vec<Uuid>> {
        Ok(capped_distinct_ids(names_of(self.ids.as_ref()))?.0)
    }
}

#[cfg(test)]
mod tests {
    use serde_json::{Value, json};

    use super::*;
    use crate::{
        api::helpers::{MAX_BATCH_IDS, TOO_MANY_IDS},
        error::AppError,
    };

    /// Deserializes `value` as the endpoint's parameters, as the extractor
    /// would.
    fn params(value: Value) -> DestroyParams {
        crate::extract::into_params(value).expect("the fixture deserializes")
    }

    #[test]
    fn the_bulk_ids_parse_from_lists_and_lone_values() {
        let id = "01934567-89ab-7cde-f012-3456789abcde";
        let other = "01934567-89ab-7cde-f012-3456789abcdf";
        let list = params(json!({"ids": [id, other, id]}));
        assert_eq!(
            list.parsed_ids().expect("two ids are under the cap"),
            vec![
                id.parse::<Uuid>().expect("fixture id"),
                other.parse().expect("fixture id")
            ],
            "the json array, each id once"
        );
        let lone = params(json!({"ids": id}));
        assert_eq!(
            lone.parsed_ids().expect("one id").len(),
            1,
            "and the lone `ids=` value"
        );
        let mixed = params(json!({"ids": [id, "not-an-id", "7"]}));
        assert_eq!(
            mixed.parsed_ids().expect("one id").len(),
            1,
            "values that name no row are skipped, not an error"
        );
        let empty = params(json!({}));
        assert!(
            empty.parsed_ids().expect("none").is_empty(),
            "absent asks for none"
        );
    }

    #[test]
    fn the_bulk_ids_are_capped_by_distinct_count() {
        let many: Vec<String> = (1..=MAX_BATCH_IDS + 1)
            .map(|n| format!("{n:032x}"))
            .collect();
        let over = params(json!({"ids": &many}));
        let err = over.parsed_ids().expect_err("one past the cap");
        assert!(
            matches!(&err, AppError::Unprocessable(message) if message.as_str() == TOO_MANY_IDS),
            "is the 422, not {err}"
        );
        let mut repeated: Vec<String> = many.iter().take(MAX_BATCH_IDS).cloned().collect();
        repeated.extend(repeated.clone());
        let at_cap = params(json!({"ids": repeated}));
        assert_eq!(
            at_cap
                .parsed_ids()
                .expect("repeats fold into the cap")
                .len(),
            MAX_BATCH_IDS
        );
    }
}
