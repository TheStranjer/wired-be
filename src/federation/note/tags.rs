//! The satellite arrays of the note document: the `tag` list of hashtags,
//! mentions and used custom emojis, the `attachment` array of media objects,
//! and the poll fields that turn a `Note` into a `Question`.

use serde_json::{Map, Number, Value, json};

use crate::{
    models::{
        custom_emoji::CustomEmoji, media_attachment::MediaAttachment, status::Status, user::User,
    },
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
    tags.extend(ctx.mentions.iter().map(mention_tag));
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
fn mention_tag(person: &User) -> Value {
    json!({
        "type": TYPE_MENTION,
        "href": &person.ap_id,
        "name": format!("@{}", person.nickname),
    })
}

/// The emoji tags for the `ctx`'s known local emojis whose `:shortcode:`
/// appears in the post's text (source or rendered HTML).
fn used_emoji_tags(ctx: &NoteContext) -> Vec<Value> {
    ctx.emojis
        .iter()
        .filter(|emoji| content_uses(&ctx.status, &emoji.shortcode))
        .map(emoji_tag)
        .collect()
}

/// Whether either text of `status` carries `:shortcode:` verbatim.
fn content_uses(status: &Status, shortcode: &str) -> bool {
    let needle = format!(":{shortcode}:");
    status.raw_text.contains(&needle) || status.content.contains(&needle)
}

/// One custom emoji as its tag object: the image URL as both `id` and
/// `icon.url`, the colon-wrapped shortcode as the `name`.
fn emoji_tag(emoji: &CustomEmoji) -> Value {
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
    Value::Object(fields)
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
