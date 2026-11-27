//! The satellite arrays of the note document: the `tag` list of hashtags,
//! mentions and used custom emojis, the `attachment` array of media objects,
//! and the poll fields that turn a `Note` into a `Question`.

use serde_json::{Map, Number, Value, json};

use crate::{
    config::Config,
    models::{custom_emoji::CustomEmoji, media_attachment::MediaAttachment, user::User},
    util::{entities::hashtag_spellings, time::mastodon_time},
};

use super::{
    NoteContext, PollData, PollOptionData, TYPE_COLLECTION, TYPE_DOCUMENT, TYPE_EMOJI,
    TYPE_HASHTAG, TYPE_IMAGE, TYPE_MENTION, TYPE_NOTE,
};

/// The `tag` array: hashtags first, then mentions, then the custom emojis
/// the content actually uses — Pleroma's `prepare_object` order
/// (`add_hashtags`, `add_mention_tags`, `add_emoji_tags`).
pub(super) fn tags_json(ctx: &NoteContext) -> Vec<Value> {
    let base = ctx.config.base_url.as_str();
    let spellings = hashtag_spellings(&ctx.status.raw_text);
    let mut tags: Vec<Value> = ctx
        .hashtags
        .iter()
        .map(|name| {
            let written = spellings
                .iter()
                .find_map(|(tag, written)| (tag == name).then_some(written.as_str()));
            hashtag_tag(base, name, written.unwrap_or(name))
        })
        .collect();
    tags.extend(
        ctx.mentions
            .iter()
            .map(|person| mention_tag(person, &ctx.config)),
    );
    tags.extend(used_emoji_tags(ctx));
    tags
}

/// One hashtag as its tag object, pointing at this instance's tag page.
///
/// The `href` takes the stored (normalized) `name`; the visible `name` field
/// takes `written`, the casing the author first typed.
fn hashtag_tag(base: &str, name: &str, written: &str) -> Value {
    json!({
        "type": TYPE_HASHTAG,
        "href": format!("{base}/tags/{name}"),
        "name": format!("#{written}"),
    })
}

/// One mentioned account as its tag object: `@nick` for a local account,
/// `@nick@domain` for a remote one, which is exactly how the nickname is
/// stored (decision 7).
///
/// When the instance splits its handle domain from its web host, a local
/// account is named by its handle, `@nick@LOCAL_DOMAIN`; the `href` stays
/// the actor id on the web host either way.
fn mention_tag(person: &User, config: &Config) -> Value {
    let split = config.local_domain() != config.web_host();
    let name = if split && !person.nickname.contains('@') {
        format!("@{}@{}", person.nickname, config.local_domain())
    } else {
        format!("@{}", person.nickname)
    };
    json!({
        "type": TYPE_MENTION,
        "href": &person.ap_id,
        "name": name,
    })
}

/// The emoji tags for the `ctx`'s known local emojis whose `:shortcode:`
/// appears in the post's text (source or rendered HTML), its content
/// warning or a poll option title.
fn used_emoji_tags(ctx: &NoteContext) -> Vec<Value> {
    ctx.emojis
        .iter()
        .filter(|emoji| content_uses(ctx, &emoji.shortcode))
        .map(emoji_tag)
        .collect()
}

/// Whether any text of the note carries `:shortcode:` verbatim: the source,
/// the rendered HTML, the content warning or a poll option title.
fn content_uses(ctx: &NoteContext, shortcode: &str) -> bool {
    let needle = format!(":{shortcode}:");
    let status = &ctx.status;
    let options = ctx.poll.iter().flat_map(|poll| &poll.options);
    [&status.raw_text, &status.content, &status.spoiler_text]
        .into_iter()
        .chain(options.map(|option| &option.title))
        .any(|text| text.contains(&needle))
}

/// One custom emoji as its tag object: the image URL as both `id` and
/// `icon.url`, the colon-wrapped shortcode as the `name`.
///
/// Shared by notes and actor documents.
pub(crate) fn emoji_tag(emoji: &CustomEmoji) -> Value {
    json!({
        "type": TYPE_EMOJI,
        "id": &emoji.image_url,
        "name": format!(":{}:", emoji.shortcode),
        "icon": { "type": TYPE_IMAGE, "url": &emoji.image_url },
    })
}

/// The `attachment` array: images federate as `Image` objects, everything
/// else as `Document` (the media class follows the stored `MIME` type, as
/// [`crate::entities::media_attachment`] classes it for the client API).
pub(super) fn attachments_json(attachments: &[MediaAttachment]) -> Vec<Value> {
    attachments.iter().map(attachment_json).collect()
}

/// One media row as its attachment object.
///
/// `focalPoint` (`[x, y]`, Mastodon's `-1..1` convention) and `width` /
/// `height` appear only when the row stores them; `blurhash` only when set.
/// A video or audio file also states its `duration` (an `ISO 8601`
/// duration, as `PeerTube` and Pleroma write it) and, when it has a stored
/// preview, an `icon` `Image` pointing at it — never the media file itself.
pub(crate) fn attachment_json(media: &MediaAttachment) -> Value {
    let class = if media.content_type.starts_with("image/") {
        TYPE_IMAGE
    } else {
        TYPE_DOCUMENT
    };
    let mut fields = Map::new();
    let mut put = |key: &str, value: Value| {
        let _previous = fields.insert(key.to_owned(), value);
    };
    put("type", json!(class));
    put("mediaType", json!(&media.content_type));
    put("url", json!(&media.url));
    put("name", json!(media.description.clone().unwrap_or_default()));
    if let Some(blurhash) = &media.blurhash {
        put("blurhash", json!(blurhash));
    }
    if let Some(point) = focal_point(media) {
        put("focalPoint", point);
    }
    if let (Some(width), Some(height)) = (media.width, media.height) {
        put("width", json!(width));
        put("height", json!(height));
    }
    if is_timed(&media.content_type) {
        if let Some(duration) = media.duration.and_then(iso_duration) {
            put("duration", json!(duration));
        }
        if let Some(preview) = &media.preview_url {
            put(
                "icon",
                json!({
                    "type": TYPE_IMAGE,
                    "mediaType": preview_media_type(preview),
                    "url": preview,
                }),
            );
        }
    }
    Value::Object(fields)
}

/// The `MIME` type of a stored preview image, read from its url's
/// extension: the encoder writes `JPEG` unless the picture has
/// transparency to keep (`PNG`), and a client-supplied preview is stored
/// re-encoded under the type the encoder wrote, so the extension is the
/// truth; anything unrecognised is `image/jpeg`, the common case.
fn preview_media_type(url: &str) -> &'static str {
    let path = url.split(['?', '#']).next().unwrap_or(url);
    match path.rsplit_once('.').map(|(_, extension)| extension) {
        Some(extension) if extension.eq_ignore_ascii_case("png") => "image/png",
        Some(extension) if extension.eq_ignore_ascii_case("gif") => "image/gif",
        Some(extension) if extension.eq_ignore_ascii_case("webp") => "image/webp",
        _ => "image/jpeg",
    }
}

/// Whether `content_type` is a video or audio type, the kinds that carry a
/// duration and a preview `icon`.
fn is_timed(content_type: &str) -> bool {
    content_type.starts_with("video/") || content_type.starts_with("audio/")
}

/// `seconds` as an `ISO 8601` duration (`PT3.75S`), to the millisecond with
/// trailing zeros dropped; `None` for a length that is not a positive,
/// finite number.
fn iso_duration(seconds: f64) -> Option<String> {
    if !seconds.is_finite() || seconds <= 0.0 {
        return None;
    }
    let text = format!("{seconds:.3}");
    let trimmed = text.trim_end_matches('0').trim_end_matches('.');
    (!trimmed.is_empty() && trimmed != "0").then(|| format!("PT{trimmed}S"))
}

/// The `[x, y]` focal point of `media`, when both axes are stored and finite.
fn focal_point(media: &MediaAttachment) -> Option<Value> {
    let x = axis_number(media.focus_x?)?;
    let y = axis_number(media.focus_y?)?;
    Some(Value::Array(vec![x, y]))
}

/// One focal axis as a JSON number at its shortest `f32` decimal form, so a
/// stored `0.3` federates as `0.3`, not `0.30000001192092896`.
fn axis_number(axis: f32) -> Option<Value> {
    let decimal = format!("{axis}").parse::<f64>().ok()?;
    Number::from_f64(decimal).map(Value::Number)
}

/// The poll fields of a `Question`: the choices under `oneOf` (single
/// choice) or `anyOf` (multiple), the `votersCount`, and the close pair —
/// `endTime` always names the deadline, `closed` repeats it only once
/// voting has run out (Pleroma's `QuestionValidator` drops a `closed` it
/// cannot ground in an end time).
pub(super) fn poll_fields(poll_data: &PollData) -> Vec<(&'static str, Value)> {
    let choices = if poll_data.multiple { "anyOf" } else { "oneOf" };
    let mut fields = vec![
        (
            choices,
            Value::Array(poll_data.options.iter().map(poll_option_json).collect()),
        ),
        ("votersCount", json!(poll_data.voters_count)),
    ];
    if let Some(expires_at) = poll_data.expires_at {
        let end_time = json!(mastodon_time(&expires_at));
        fields.push(("endTime", end_time.clone()));
        if poll_data.closed {
            fields.push(("closed", end_time));
        }
    }
    fields
}

/// One poll option as the `Note` answer-object Pleroma federates it as,
/// with its vote tally as a `Collection`'s `totalItems`.
fn poll_option_json(option: &PollOptionData) -> Value {
    json!({
        "type": TYPE_NOTE,
        "name": &option.title,
        "replies": { "type": TYPE_COLLECTION, "totalItems": option.votes_count },
    })
}

#[cfg(test)]
mod tests {
    use uuid::Uuid;

    use super::*;
    use crate::{
        federation::note::{note_json, test_support::*},
        models::status::Visibility,
        util::time::parse_time,
    };

    /// A poll deadline in the far future: the open-poll fixture.
    const FAR_END: &str = "2100-01-01T00:00:00.000Z";

    #[test]
    fn mentions_hashtags_and_used_emojis_become_tags() {
        let mut ctx = note_context(Visibility::Public);
        ctx.hashtags = vec!["fediverse".to_owned()];
        ctx.mentions = vec![mentioned_bob()];
        ctx.emojis = vec![
            fixture_emoji("railroad_track", "railroad_track"),
            fixture_emoji("gone", "gone"),
        ];
        ctx.status.raw_text = "hi @bob :railroad_track: #fediverse".to_owned();
        ctx.status.content = "<p>hi <span class=\"h-card\"><a href=\"https://remote.example/users/bob\" class=\"u-url mention\">@<span>bob</span></a></span> :railroad_track: <a href=\"https://test.example/tags/fediverse\" class=\"mention hashtag\" rel=\"tag\">#<span>fediverse</span></a></p>".to_owned();
        let document = note_json(&ctx).expect("a tagged note federates");
        assert_eq!(
            document["tag"],
            json!([
                {"type": "Hashtag", "href": format!("{BASE}/tags/fediverse"), "name": "#fediverse"},
                {"type": "Mention", "href": BOB_AP, "name": "@bob@remote.example"},
                {"type": "Emoji", "id": format!("{BASE}/emoji/railroad_track.png"), "name": ":railroad_track:",
                 "icon": {"type": "Image", "url": format!("{BASE}/emoji/railroad_track.png")}},
            ]),
            "hashtags, mentions, then the emojis the content actually uses"
        );
    }

    #[test]
    fn a_local_mention_is_named_by_its_handle_only_when_the_domains_split() {
        let mut ctx = note_context(Visibility::Public);
        ctx.mentions = vec![author()];
        let plain = note_json(&ctx).expect("a mentioning note federates");
        assert_eq!(
            plain["tag"],
            json!([{"type": "Mention", "href": AUTHOR_AP, "name": "@alice"}]),
            "unsplit, the name is unchanged"
        );
        ctx.config.local_domain = "whatever.com".to_owned();
        let split = note_json(&ctx).expect("a mentioning note federates");
        assert_eq!(
            split["tag"],
            json!([{"type": "Mention", "href": AUTHOR_AP, "name": "@alice@whatever.com"}]),
            "split, the handle domain names the account and the href stays on the web host"
        );
        ctx.mentions = vec![mentioned_bob()];
        let remote = note_json(&ctx).expect("a mentioning note federates");
        assert_eq!(
            remote["tag"][0]["name"], "@bob@remote.example",
            "a remote name is never rewritten"
        );
    }

    /// A context holding the `wiredcat` emoji, ready for one placement.
    fn wiredcat_context() -> NoteContext {
        let mut ctx = note_context(Visibility::Public);
        ctx.emojis = vec![fixture_emoji("wiredcat", "wiredcat")];
        ctx
    }

    /// A one-option open poll whose only option reads `title`.
    fn poll_titled(title: &str) -> PollData {
        PollData {
            multiple: false,
            expires_at: None,
            closed: false,
            voters_count: 0,
            options: vec![PollOptionData {
                title: title.to_owned(),
                votes_count: 0,
            }],
        }
    }

    #[test]
    fn an_emoji_used_only_in_the_content_warning_is_tagged() {
        let mut ctx = wiredcat_context();
        ctx.status.raw_text = "body without emoji".to_owned();
        ctx.status.content = "<p>body without emoji</p>".to_owned();
        ctx.status.spoiler_text = "CW with :wiredcat: emoji".to_owned();
        let document = note_json(&ctx).expect("a CW note federates");
        assert_eq!(
            document["tag"],
            json!([emoji_tag(&ctx.emojis[0])]),
            "the CW alone earns the Emoji tag"
        );
    }

    #[test]
    fn an_emoji_used_only_in_a_poll_option_is_tagged() {
        let mut ctx = wiredcat_context();
        ctx.status.raw_text = "poll emoji test".to_owned();
        ctx.status.content = "<p>poll emoji test</p>".to_owned();
        ctx.poll = Some(poll_titled(":wiredcat: yes"));
        let document = note_json(&ctx).expect("a poll note federates");
        assert_eq!(
            document["tag"],
            json!([emoji_tag(&ctx.emojis[0])]),
            "the option alone earns the Emoji tag"
        );
    }

    #[test]
    fn an_emoji_used_everywhere_is_tagged_once() {
        let mut ctx = wiredcat_context();
        ctx.status.raw_text = ":wiredcat: body".to_owned();
        ctx.status.content = "<p>:wiredcat: body</p>".to_owned();
        ctx.status.spoiler_text = ":wiredcat: cw".to_owned();
        ctx.poll = Some(poll_titled(":wiredcat: yes"));
        let document = note_json(&ctx).expect("a note federates");
        assert_eq!(
            document["tag"],
            json!([emoji_tag(&ctx.emojis[0])]),
            "body, CW and option still yield exactly one tag"
        );
    }

    #[test]
    fn an_emoji_used_nowhere_is_not_tagged() {
        let mut ctx = wiredcat_context();
        ctx.status.spoiler_text = "plain cw".to_owned();
        ctx.poll = Some(poll_titled("plain option"));
        let document = note_json(&ctx).expect("a note federates");
        assert_eq!(document["tag"], json!([]), "an unused emoji is no tag");
    }

    #[test]
    fn hashtag_names_keep_the_authors_casing_and_hrefs_stay_lowercase() {
        let mut ctx = note_context(Visibility::Public);
        ctx.hashtags = vec!["fedlab".to_owned(), "other".to_owned()];
        ctx.status.raw_text = "hello #FedLab and #fedlab".to_owned();
        let document = note_json(&ctx).expect("a tagged note federates");
        assert_eq!(
            document["tag"],
            json!([
                {"type": "Hashtag", "href": format!("{BASE}/tags/fedlab"), "name": "#FedLab"},
                {"type": "Hashtag", "href": format!("{BASE}/tags/other"), "name": "#other"},
            ]),
            "the first written casing names the tag; a tag not in the text keeps its stored name"
        );
    }

    /// A local custom-emoji row named `shortcode` with a URL under `BASE`.
    fn fixture_emoji(shortcode: &str, stem: &str) -> CustomEmoji {
        CustomEmoji {
            id: Uuid::parse_str(AUTHOR_ID).unwrap(),
            shortcode: shortcode.to_owned(),
            domain: None,
            image_url: format!("{BASE}/emoji/{stem}.png"),
            static_url: None,
            category: None,
            tags: Vec::new(),
            visible_in_picker: true,
            disabled: false,
            inserted_at: parse_time(PUBLISHED).unwrap(),
            updated_at: parse_time(PUBLISHED).unwrap(),
        }
    }

    #[test]
    fn attachments_federate_as_images_and_documents() {
        let mut ctx = note_context(Visibility::Public);
        let mut photo = fixture_media(&format!("{BASE}/media/cat.png"), "image/png");
        photo.description = Some("a cat".to_owned());
        photo.blurhash = Some("UBLHZ;9a".to_owned());
        let file = fixture_media(&format!("{BASE}/media/report.pdf"), "application/pdf");
        ctx.attachments = vec![photo, file];
        let document = note_json(&ctx).expect("attachments federate");
        assert_eq!(
            document["attachment"],
            json!([
                {"type": "Image", "mediaType": "image/png", "url": format!("{BASE}/media/cat.png"),
                 "name": "a cat", "blurhash": "UBLHZ;9a"},
                {"type": "Document", "mediaType": "application/pdf", "url": format!("{BASE}/media/report.pdf"),
                 "name": ""},
            ]),
            "image/* classifies as Image, the rest as Document; a null blurhash is omitted"
        );
    }

    #[test]
    fn an_image_with_focus_and_dimensions_renders_them() {
        let mut photo = fixture_media(&format!("{BASE}/media/photo.jpg"), "image/jpeg");
        photo.focus_x = Some(0.3);
        photo.focus_y = Some(0.6);
        photo.width = Some(400);
        photo.height = Some(300);
        let rendered = attachment_json(&photo);
        assert_eq!(rendered["focalPoint"], json!([0.3, 0.6]), "the focal point");
        assert_eq!(rendered["width"], 400, "the original width");
        assert_eq!(rendered["height"], 300, "the original height");
        assert_eq!(
            rendered["focalPoint"].to_string(),
            "[0.3,0.6]",
            "shortest decimal form, not the widened f32"
        );
    }

    #[test]
    fn missing_focus_or_dimensions_are_omitted() {
        let mut photo = fixture_media(&format!("{BASE}/media/photo.jpg"), "image/jpeg");
        photo.width = Some(400);
        photo.height = Some(300);
        let rendered = attachment_json(&photo);
        assert!(rendered.get("focalPoint").is_none(), "no focus, no key");
        let mut half = fixture_media(&format!("{BASE}/media/half.jpg"), "image/jpeg");
        half.focus_x = Some(0.5);
        half.width = Some(400);
        let partial = attachment_json(&half);
        assert!(partial.get("focalPoint").is_none(), "one axis is no point");
        assert!(partial.get("width").is_none(), "width alone is dropped");
        assert!(partial.get("height").is_none(), "no height stored");
    }

    #[test]
    fn an_audio_file_has_no_dimensions() {
        let audio = fixture_media(&format!("{BASE}/media/song.mp3"), "audio/mpeg");
        let rendered = attachment_json(&audio);
        assert_eq!(rendered["type"], "Document", "audio is a Document");
        for key in ["width", "height", "focalPoint", "blurhash"] {
            assert!(rendered.get(key).is_none(), "{key} is omitted");
        }
    }

    #[test]
    fn a_video_states_its_size_duration_and_preview_icon() {
        let mut clip = fixture_media(&format!("{BASE}/media/clip.mp4"), "video/mp4");
        clip.width = Some(640);
        clip.height = Some(360);
        clip.duration = Some(3.75);
        clip.preview_url = Some(format!("{BASE}/media/clip_small.jpg"));
        clip.blurhash = Some("UBLHZ;9a".to_owned());
        let rendered = attachment_json(&clip);
        assert_eq!(
            rendered,
            json!({
                "type": "Document",
                "mediaType": "video/mp4",
                "url": format!("{BASE}/media/clip.mp4"),
                "name": "",
                "blurhash": "UBLHZ;9a",
                "width": 640,
                "height": 360,
                "duration": "PT3.75S",
                "icon": {
                    "type": "Image",
                    "mediaType": "image/jpeg",
                    "url": format!("{BASE}/media/clip_small.jpg"),
                },
            }),
            "a video carries width, height, duration and a preview icon"
        );
    }

    #[test]
    fn the_icon_media_type_follows_the_stored_previews_extension() {
        for (url, expected) in [
            ("https://x.example/media/a_small.jpg", "image/jpeg"),
            ("https://x.example/media/a_small.PNG", "image/png"),
            ("https://x.example/media/a_small.webp?v=2", "image/webp"),
            ("https://x.example/media/a_small.gif#t", "image/gif"),
            ("https://x.example/media/a_small", "image/jpeg"),
        ] {
            assert_eq!(preview_media_type(url), expected, "{url}");
        }
        let mut song = fixture_media(&format!("{BASE}/media/song.mp3"), "audio/mpeg");
        song.preview_url = Some(format!("{BASE}/media/cover.png"));
        assert_eq!(
            attachment_json(&song)["icon"]["mediaType"],
            "image/png",
            "audio with a cover federates its icon too"
        );
    }

    #[test]
    fn audio_states_a_duration_and_no_size() {
        let mut song = fixture_media(&format!("{BASE}/media/song.mp3"), "audio/mpeg");
        song.duration = Some(61.0);
        let rendered = attachment_json(&song);
        assert_eq!(rendered["duration"], "PT61S", "whole seconds drop the dot");
        for key in ["width", "height", "icon"] {
            assert!(rendered.get(key).is_none(), "{key} is omitted");
        }
    }

    #[test]
    fn a_timed_file_without_the_facts_omits_the_keys() {
        let clip = fixture_media(&format!("{BASE}/media/clip.mp4"), "video/mp4");
        let rendered = attachment_json(&clip);
        for key in ["width", "height", "duration", "icon"] {
            assert!(rendered.get(key).is_none(), "{key} is omitted");
        }
        let mut bad = fixture_media(&format!("{BASE}/media/bad.mp4"), "video/mp4");
        for length in [0.0, -1.0, f64::NAN, f64::INFINITY, 0.0004] {
            bad.duration = Some(length);
            assert!(
                attachment_json(&bad).get("duration").is_none(),
                "{length} is no duration"
            );
        }
    }

    #[test]
    fn an_image_never_gets_a_duration_or_icon() {
        let mut photo = fixture_media(&format!("{BASE}/media/photo.png"), "image/png");
        photo.duration = Some(2.0);
        photo.preview_url = Some(format!("{BASE}/media/photo_small.jpg"));
        let rendered = attachment_json(&photo);
        for key in ["duration", "icon"] {
            assert!(rendered.get(key).is_none(), "{key} is for video and audio");
        }
    }

    #[test]
    fn durations_render_to_the_millisecond_without_trailing_zeros() {
        for (seconds, expected) in [
            (3.75, "PT3.75S"),
            (30.0, "PT30S"),
            (100.5, "PT100.5S"),
            (0.001, "PT0.001S"),
            (12.3456, "PT12.346S"),
        ] {
            assert_eq!(
                iso_duration(seconds).as_deref(),
                Some(expected),
                "{seconds}s"
            );
        }
    }

    #[test]
    fn negative_focus_keeps_the_mastodon_convention() {
        let mut photo = fixture_media(&format!("{BASE}/media/photo.jpg"), "image/jpeg");
        photo.focus_x = Some(0.5);
        photo.focus_y = Some(-0.25);
        assert_eq!(
            attachment_json(&photo)["focalPoint"],
            json!([0.5, -0.25]),
            "a negative y survives"
        );
    }

    /// The two-option `yes`/`no` choice pair, `yes` holding `first_votes`
    /// and `no` none.
    fn fixture_options(first_votes: i32) -> Vec<PollOptionData> {
        vec![
            PollOptionData {
                title: "yes".to_owned(),
                votes_count: first_votes,
            },
            PollOptionData {
                title: "no".to_owned(),
                votes_count: 0,
            },
        ]
    }

    #[test]
    fn a_single_choice_poll_renders_a_question_with_oneof() {
        let mut ctx = note_context(Visibility::Public);
        ctx.poll = Some(PollData {
            multiple: false,
            expires_at: Some(parse_time(FAR_END).unwrap()),
            closed: false,
            voters_count: 3,
            options: fixture_options(2),
        });
        let document = note_json(&ctx).expect("a question federates");
        assert_eq!(document["type"], "Question", "a poll makes it a Question");
        assert_eq!(
            document["oneOf"],
            json!([
                {"type": "Note", "name": "yes",
                 "replies": {"type": "Collection", "totalItems": 2}},
                {"type": "Note", "name": "no",
                 "replies": {"type": "Collection", "totalItems": 0}},
            ]),
            "the choices, with their tallies as collections"
        );
        assert!(document.get("anyOf").is_none(), "single choice uses oneOf");
        assert_eq!(document["votersCount"], 3, "the voter count");
        assert_eq!(document["endTime"], FAR_END, "the deadline");
        assert!(
            document.get("closed").is_none(),
            "an open poll reports no close"
        );
    }

    #[test]
    fn a_multiple_choice_expired_poll_uses_anyof_and_closes() {
        let mut ctx = note_context(Visibility::Public);
        ctx.poll = Some(PollData {
            multiple: true,
            expires_at: Some(parse_time(DELETED).unwrap()),
            closed: true,
            voters_count: 7,
            options: fixture_options(5),
        });
        let document = note_json(&ctx).expect("a closed question federates");
        assert!(
            document.get("oneOf").is_none() && document["anyOf"].is_array(),
            "multiple choice addresses its options as anyOf"
        );
        assert_eq!(document["endTime"], DELETED, "the deadline it ran out with");
        assert_eq!(document["closed"], DELETED, "closed repeats the end time");
        assert_eq!(document["votersCount"], 7, "the taller voter count");
    }
}
