//! `GET /api/v1/timelines/public`: the instance's public timeline, its
//! request parameters and the `RESTRICT_UNAUTHENTICATED` gate in front
//! of it.

use axum::{
    Json,
    extract::{OriginalUri, State},
    http::HeaderMap,
};

use super::{
    AUTHENTICATION_REQUIRED, common_filters, parse_instance, parse_public_reply_visibility,
    timeline_area,
};
use crate::{
    api::helpers::{QueryPairs, timeline_response},
    auth::MaybeAuthUser,
    config::Config,
    entities::status::Status as StatusEntity,
    error::{AppError, AppResult},
    models::user::User,
    oauth::scopes::READ_STATUSES,
    pagination::PageParams,
    services::timeline::{TimelineFilters, TimelineQuery, TimelineSource},
    state::AppState,
};

/// `GET /api/v1/timelines/public` — the instance's public page.
///
/// The [`TimelineSource::Public`] rows (Pleroma's `fetch_public_activities`),
/// newest first, one [`PageParams`] keyset page at a time. Auth is optional
/// (Pleroma's `read:statuses` scopes plug with
/// `fallback: :proceed_unauthenticated`): a presented user token must grant
/// the scope, an app-only token and no token at all both read anonymously,
/// and an invalid token is the extractor's 401 — all before the
/// [`PublicParams::ensure_open`] restriction gate runs.
///
/// The `Link` header's path is the request's own [`OriginalUri`], as in
/// [`tag_timeline`].
///
/// # Errors
///
/// [`AppError::Forbidden`] from [`MaybeAuthUser::require_user_scope`] when
/// the presented user token lacks `read:statuses`, and from
/// [`PublicParams::ensure_open`] when the instance restricts this
/// unauthenticated read; [`AppError::BadRequest`] from [`PageParams`] for a
/// malformed cursor. Any database failure arrives through `?` as
/// [`AppError::Database`], and [`set_link_header`](crate::pagination::set_link_header) reports a `Link` value
/// that is not header-safe as [`AppError::Internal`].
pub(super) async fn public_timeline(
    State(state): State<AppState>,
    auth: MaybeAuthUser,
    page: PageParams,
    OriginalUri(uri): OriginalUri,
) -> AppResult<(HeaderMap, Json<Vec<StatusEntity>>)> {
    auth.require_user_scope(READ_STATUSES)?;
    let params = PublicParams::parse(uri.query());
    params.ensure_open(&state.config, auth.0.is_some())?;
    let query = params.into_timeline(auth.0, page);
    timeline_response(&state, &query, &uri).await
}

/// The public timeline's request parameters.
///
/// The origin/media/mute narrowings and visibility exclusions the tag route
/// shares, plus Pleroma's two public-page extensions, `reply_visibility`
/// and `instance` — every one of them a no-op in its [`Default`] spelling.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
struct PublicParams {
    /// The narrowings [`common_filters`] reads (`local`, `remote`,
    /// `only_media`, `with_muted`, `exclude_visibilities[]`) plus the two
    /// public-page extras: `reply_visibility` (keep only replies addressed
    /// to the viewer (`self`) or to the viewer and their follows
    /// (`following`); any other spelling — or no value — keeps every
    /// reply) and `instance` (keep only statuses whose author is a remote
    /// account of this domain, Pleroma's author-domain extension; `None`
    /// filters nothing).
    ///
    /// `local` is also the parameter that picks which [`Config::restricts`]
    /// area gates an anonymous read (see [`timeline_area`]).
    filters: TimelineFilters,
}

impl PublicParams {
    /// Reads the parameters out of a raw query string the way
    /// [`TagParams::parse`] reads its own.
    ///
    /// Percent-decoded; `key` and `key[]` fill the exclusion list in
    /// occurrence order; a repeated single-value key takes its last value;
    /// an unrelated key is never looked at.
    fn parse(query: Option<&str>) -> Self {
        let pairs = QueryPairs::parse(query);
        Self {
            filters: TimelineFilters {
                reply_visibility: pairs
                    .value("reply_visibility")
                    .and_then(|text| parse_public_reply_visibility(&text)),
                instance: pairs
                    .value("instance")
                    .and_then(|text| parse_instance(&text)),
                ..common_filters(&pairs)
            },
        }
    }

    /// Pleroma's `restrict_unauthenticated?` gate, in
    /// `timeline_controller.ex`.
    ///
    /// With no user behind the request, the `local=true` page belongs to
    /// the `timelines_local` area and every other read to
    /// `timelines_federated`, and a listed area refuses the request
    /// [`AUTHENTICATION_REQUIRED`]. An authenticated request (`authenticated`
    /// — a live *user* token was presented; an app-only one is
    /// unauthenticated here, as in Pleroma's `conn.assigns[:user]`) and
    /// every unlisted area pass.
    ///
    /// # Errors
    ///
    /// [`AppError::Forbidden`] with [`AUTHENTICATION_REQUIRED`] when the
    /// anonymous read of this page's area is restricted.
    fn ensure_open(&self, config: &Config, authenticated: bool) -> AppResult<()> {
        if authenticated {
            return Ok(());
        }
        if config.restricts(timeline_area(self.filters.local_only.get())) {
            return Err(AppError::Forbidden(AUTHENTICATION_REQUIRED.to_owned()));
        }
        Ok(())
    }

    /// Assembles the [`TimelineQuery`] the request asks for.
    ///
    /// The [`TimelineSource::Public`] page, narrowed by exactly these
    /// [`TimelineFilters`], as `viewer` (or anonymously, `None`).
    fn into_timeline(self, viewer: Option<User>, page: PageParams) -> TimelineQuery {
        TimelineQuery {
            viewer,
            source: TimelineSource::Public,
            filters: self.filters,
            page,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::super::{TIMELINES_FEDERATED, TIMELINES_LOCAL};
    use super::*;
    use crate::{models::status::Visibility, services::timeline::ReplyVisibility};

    /// The parsed public-page query as [`PublicParams`].
    fn public_parsed(query: &str) -> PublicParams {
        PublicParams::parse(Some(query))
    }

    /// A [`Config`] whose `RESTRICT_UNAUTHENTICATED` lists `areas`.
    fn restricted(areas: &[&str]) -> Config {
        let mut config = Config::for_tests();
        config.restrict_unauthenticated = areas.iter().map(|area| (*area).to_owned()).collect();
        config
    }

    #[test]
    fn an_empty_public_query_defaults_to_everything_off() {
        for query in ["", "foo=bar&limit=5"] {
            assert_eq!(
                public_parsed(query),
                PublicParams::default(),
                "`{query}` narrows nothing"
            );
        }
    }

    #[test]
    fn the_public_flags_and_exclusions_read_like_the_tags_ones() {
        let params = public_parsed(
            "local=true&remote=1&only_media=TRUE&with_muted=true&exclude_visibilities[]=private",
        );
        let filters = &params.filters;
        assert!(filters.local_only.get(), "`local=true`");
        assert!(filters.remote_only.get(), "`remote=1`");
        assert!(filters.only_media.get(), "`only_media` is case-insensitive");
        assert!(filters.with_muted.get(), "`with_muted=true`");
        assert_eq!(
            filters.exclude_visibilities,
            vec![Visibility::Private],
            "the verbatim visibility name is kept"
        );
        let off = public_parsed("local=false&remote=&exclude_visibilities[]=Public").filters;
        assert!(
            !off.local_only.get() && !off.remote_only.get() && off.exclude_visibilities.is_empty(),
            "`false`, empty and un-spelled values narrow nothing"
        );
    }

    #[test]
    fn reply_visibility_names_the_two_filters_and_shrugs_at_the_rest() {
        assert_eq!(
            public_parsed("reply_visibility=following")
                .filters
                .reply_visibility,
            Some(ReplyVisibility::Following),
            "`following` is Pleroma's friends-and-self filter"
        );
        assert_eq!(
            public_parsed("reply_visibility=%20self%20")
                .filters
                .reply_visibility,
            Some(ReplyVisibility::SelfOnly),
            "`self` is trimmed like every other value"
        );
        for value in ["all", "ignored", "Self", ""] {
            assert_eq!(
                public_parsed(&format!("reply_visibility={value}"))
                    .filters
                    .reply_visibility,
                None,
                "`{value}` is no filter and keeps every reply"
            );
        }
    }

    #[test]
    fn instance_is_trimmed_and_an_empty_one_filters_nothing() {
        assert_eq!(
            public_parsed("instance=remote.example")
                .filters
                .instance
                .as_deref(),
            Some("remote.example"),
            "the domain is kept as spelled"
        );
        assert_eq!(
            public_parsed("instance=%20cdn.example%20")
                .filters
                .instance
                .as_deref(),
            Some("cdn.example"),
            "whitespace trims off"
        );
        assert_eq!(
            public_parsed("instance=").filters.instance,
            None,
            "an empty domain is no filter at all"
        );
        assert_eq!(
            public_parsed("instance=%20%20%20").filters.instance,
            None,
            "and a blank one reads the same"
        );
    }

    #[test]
    fn the_public_page_maps_to_the_public_source_with_its_filters() {
        let query =
            public_parsed("local=true&only_media=1&reply_visibility=self&instance=remote.example")
                .into_timeline(
                    None,
                    PageParams {
                        max_id: None,
                        min_id: None,
                        since_id: None,
                        limit: 20,
                        offset: None,
                    },
                );
        assert_eq!(query.source, TimelineSource::Public, "the public page");
        assert!(query.filters.local_only.get(), "`local` rides along");
        assert!(query.filters.only_media.get(), "`only_media` rides along");
        assert_eq!(
            query.filters.reply_visibility,
            Some(ReplyVisibility::SelfOnly),
            "`reply_visibility` rides along"
        );
        assert_eq!(
            query.filters.instance.as_deref(),
            Some("remote.example"),
            "`instance` rides along"
        );
        assert!(query.viewer.is_none(), "an anonymous query has no viewer");
    }

    #[test]
    fn the_restriction_gate_leaves_every_open_read_through() {
        let open = Config::for_tests();
        let local = public_parsed("local=true");
        let federated = public_parsed("");
        local
            .ensure_open(&open, false)
            .expect("an open instance answers an anonymous `local=true`");
        federated
            .ensure_open(&open, false)
            .expect("an open instance answers an anonymous federated read");
        federated
            .ensure_open(&open, true)
            .expect("an authenticated read was never the gate's business");
    }

    #[test]
    fn timelines_local_restricted_refuses_only_the_anonymous_local_read() {
        use axum::{http::StatusCode, response::IntoResponse as _};

        let config = restricted(&[TIMELINES_LOCAL]);
        let refusal = public_parsed("local=true")
            .ensure_open(&config, false)
            .expect_err("the anonymous `local=true` read is refused");
        assert_eq!(
            refusal.to_string(),
            AUTHENTICATION_REQUIRED,
            "the refusal states the ticket's exact wording"
        );
        assert_eq!(
            refusal.into_response().status(),
            StatusCode::FORBIDDEN,
            "and answers it as a 403"
        );
        public_parsed("local=false")
            .ensure_open(&config, false)
            .expect("a `local=false` read is the federated page, and that one is open");
        public_parsed("")
            .ensure_open(&config, false)
            .expect("the plain federated read stays open");
        public_parsed("local=true")
            .ensure_open(&config, true)
            .expect("an authenticated `local=true` read passes");
    }

    #[test]
    fn timelines_federated_restricted_refuses_every_other_anonymous_read() {
        let config = restricted(&[TIMELINES_FEDERATED]);
        let refusal = public_parsed("")
            .ensure_open(&config, false)
            .expect_err("the anonymous federated read is refused");
        assert_eq!(
            refusal.to_string(),
            AUTHENTICATION_REQUIRED,
            "the refusal is the same wording"
        );
        let remote_refusal = public_parsed("remote=true")
            .ensure_open(&config, false)
            .expect_err("`remote=true` is still the federated page");
        assert_eq!(
            remote_refusal.to_string(),
            AUTHENTICATION_REQUIRED,
            "`remote=true` is the federated page and shares its refusal"
        );
        public_parsed("local=true")
            .ensure_open(&config, false)
            .expect("the `local=true` page belongs to the other area and stays open");
        public_parsed("")
            .ensure_open(&config, true)
            .expect("an authenticated federated read passes");
    }

    #[test]
    fn both_areas_restricted_refuse_the_whole_anonymous_timeline() {
        let config = restricted(&[TIMELINES_LOCAL, TIMELINES_FEDERATED]);
        for query in ["", "local=true", "local=false&remote=true"] {
            let refusal = public_parsed(query)
                .ensure_open(&config, false)
                .expect_err("a listed area refuses every anonymous read");
            assert_eq!(
                refusal.to_string(),
                AUTHENTICATION_REQUIRED,
                "both areas listed, every anonymous read is the same refusal"
            );
        }
        public_parsed("local=true")
            .ensure_open(&config, true)
            .expect("and an authenticated read passes whatever is listed");
    }
}
