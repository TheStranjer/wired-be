//! [`Criterion`]: the three ways one of the Mastodon listing's boolean
//! criteria can be stated, beside [`push_criterion`], the `WHERE`-arm
//! writer they all share.

use sqlx::{Postgres, QueryBuilder};

/// The three ways one of
/// [`mastodon_list`](super::mastodon::mastodon_list)'s boolean criteria can be stated.
///
/// The reference is `Pleroma.User.Query.build` weighing a filter parameter's
/// *value* rather than its mere presence (`compose_query({:active, false}, …)`
/// is a clause of its own, and `AccountController`'s `maybe_filter_local`
/// turns `local: false` into the `external` criterion): `true` keeps the rows
/// that satisfy the criterion, `false` the ones that do not, and a parameter
/// the request never sent — or whose text is no boolean — keeps both.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum Criterion {
    /// Nothing was asked: the criterion is not applied at all.
    #[default]
    Any,
    /// `true`: only the rows that satisfy it.
    Only,
    /// `false`: only the rows that do not.
    Except,
}

impl Criterion {
    /// The criterion one query parameter's text spells, in the three readings
    /// the ticket's filters have.
    ///
    /// `true` and `1` are [`Only`](Self::Only), `false` and `0` are
    /// [`Except`](Self::Except), both trimmed and case-insensitive — the same
    /// two spellings [`crate::api::helpers::is_true`] gives, widened to their
    /// negation — and everything else (absent, empty, `yes`, junk) is
    /// [`Any`](Self::Any). Pleroma's `CastAndValidate` refuses a value its
    /// boolean cast does not know; this listing ignores it instead, the way it
    /// ignores every criterion the ticket says it cannot weigh.
    #[must_use]
    pub fn parse(text: Option<&str>) -> Self {
        let Some(text) = text.map(str::trim) else {
            return Self::Any;
        };
        if text.eq_ignore_ascii_case("true") || text == "1" {
            Self::Only
        } else if text.eq_ignore_ascii_case("false") || text == "0" {
            Self::Except
        } else {
            Self::Any
        }
    }
}

/// Appends one boolean criterion's arm: `for_only` when the parameter asked
/// for the rows that satisfy it, `for_except` when it asked for the ones that
/// do not, and nothing at all when it was absent.
pub(super) fn push_criterion(
    builder: &mut QueryBuilder<Postgres>,
    criterion: Criterion,
    for_only: &str,
    for_except: &str,
) {
    let arm = match criterion {
        Criterion::Any => return,
        Criterion::Only => for_only,
        Criterion::Except => for_except,
    };
    let _pushed: &mut _ = builder.push(arm);
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The criterion parser: each boolean spelling decides its own way, and
    /// nothing else flips anything — absent, empty, unknown and wrongly-cased
    /// text all weigh nothing, which is how this listing treats a parameter it
    /// cannot read.
    #[test]
    fn the_criterion_parser_reads_three_states() {
        assert_eq!(Criterion::default(), Criterion::Any, "the default");
        assert_eq!(Criterion::parse(None), Criterion::Any, "an absent key");
        assert_eq!(Criterion::parse(Some("")), Criterion::Any, "an empty one");
        for text in ["maybe", "yes", "t", "  ", "2"] {
            assert_eq!(
                Criterion::parse(Some(text)),
                Criterion::Any,
                "`{text}` is no boolean this listing weighs"
            );
        }
        for text in ["true", "TRUE", " true ", "1"] {
            assert_eq!(Criterion::parse(Some(text)), Criterion::Only, "`{text}`");
        }
        for text in ["false", "False", " 0 "] {
            assert_eq!(
                Criterion::parse(Some(text)),
                Criterion::Except,
                "`{text}` is the negation"
            );
        }
    }
}
