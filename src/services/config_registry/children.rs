//! The per-key child descriptions: the documented subkeys of every
//! registered entry.

use super::{
    INSTANCE_LIMIT,
    types::{ChildDescription, Suggestion, ValueKind, hinted, leaf},
};

/// The `:instance` children: the nine whitelisted subkeys ([`RegistryEntry::subkeys`]),
/// documented the way Pleroma's `config/description.exs` does.
pub(super) const INSTANCE_CHILDREN: [ChildDescription; 9] = [
    hinted(
        "name",
        ValueKind::String,
        "Name of the instance",
        &[Suggestion::Text("Wired")],
    ),
    hinted(
        "description",
        ValueKind::String,
        "The instance's description. It can be seen in nodeinfo and `/api/v1/instance`",
        &[Suggestion::Text("Very cool instance")],
    ),
    hinted(
        "email",
        ValueKind::String,
        "Email used to reach an Administrator/Moderator of the instance",
        &[Suggestion::Text("email@example.com")],
    ),
    leaf(
        "registrations_open",
        ValueKind::Boolean,
        "Enable registrations for anyone. Invitations require this setting to be disabled.",
    ),
    leaf(
        "account_approval_required",
        ValueKind::Boolean,
        "Require users to be manually approved by an admin before signing in",
    ),
    leaf(
        "account_activation_required",
        ValueKind::Boolean,
        "Require users to confirm their emails before signing in",
    ),
    leaf(
        "invites_enabled",
        ValueKind::Boolean,
        "Enable user invitations for admins (depends on `registrations_open` being disabled)",
    ),
    hinted(
        INSTANCE_LIMIT,
        ValueKind::Integer,
        "Posts character limit (CW/Subject included in the counter)",
        &[Suggestion::Count(5_000)],
    ),
    hinted(
        "max_media_attachments",
        ValueKind::Integer,
        "Maximum number of post media attachments",
        &[Suggestion::Count(4)],
    ),
];

/// The `:mrf` children: the one whitelisted subkey, the policy chain.
pub(super) const MRF_CHILDREN: [ChildDescription; 1] = [hinted(
    "policies",
    ValueKind::AtomList,
    "The MRF policies applied to incoming activities, by short name.",
    &[
        Suggestion::Text("anti_link_spam"),
        Suggestion::Text("hellthread"),
        Suggestion::Text("keyword"),
        Suggestion::Text("normalize_markup"),
        Suggestion::Text("object_age"),
        Suggestion::Text("simple"),
        Suggestion::Text("tag"),
    ],
)];

/// The `:mrf_simple` children: the eleven per-instance lists the policy
/// reads, described in the words of Pleroma's `MRFSimple` knowledge doc.
pub(super) const MRF_SIMPLE_CHILDREN: [ChildDescription; 11] = [
    leaf(
        "accept",
        ValueKind::AtomList,
        "List of instances to accept, even if they violate the reject policies",
    ),
    leaf(
        "reject",
        ValueKind::AtomList,
        "List of instances to reject all activities from",
    ),
    leaf(
        "reject_deletes",
        ValueKind::AtomList,
        "List of instances whose Delete activities will be rejected",
    ),
    leaf(
        "media_removal",
        ValueKind::AtomList,
        "List of instances to remove media from posts of",
    ),
    leaf(
        "media_nsfw",
        ValueKind::AtomList,
        "List of instances to mark media in posts from as sensitive",
    ),
    leaf(
        "avatar_removal",
        ValueKind::AtomList,
        "List of instances to remove the avatars of",
    ),
    leaf(
        "banner_removal",
        ValueKind::AtomList,
        "List of instances to remove the banners of",
    ),
    leaf(
        "background_removal",
        ValueKind::AtomList,
        "List of instances to remove the profiles' backgrounds of",
    ),
    leaf(
        "report_removal",
        ValueKind::AtomList,
        "List of instances to reject reports from",
    ),
    leaf(
        "federated_timeline_removal",
        ValueKind::AtomList,
        "List of instances to remove public posts from, so that they only appear in the local timeline",
    ),
    leaf(
        "followers_only",
        ValueKind::AtomList,
        "List of instances whose followers-only posts will be rejected",
    ),
];

/// The `:mrf_keyword` children: the three phrase lists the policy reads.
pub(super) const MRF_KEYWORD_CHILDREN: [ChildDescription; 3] = [
    hinted(
        "reject",
        ValueKind::AtomList,
        "List of phrases and regular expressions which will be rejected if they are found in a post's content.",
        &[Suggestion::Text("/^buy\\s+now\\b/i")],
    ),
    leaf(
        "federated_timeline_removal",
        ValueKind::AtomList,
        "List of phrases and regular expressions which will cause a post to be removed from the federated timeline.",
    ),
    hinted(
        "replace",
        ValueKind::AtomList,
        "List of `[pattern, replacement]` pairs rewritten in a post's content.",
        &[Suggestion::Text("bad word")],
    ),
];

/// The `:mrf_hellthread` children: the two mention gates, with this
/// repo's defaults as the example values.
pub(super) const MRF_HELLTHREAD_CHILDREN: [ChildDescription; 2] = [
    hinted(
        "delist_threshold",
        ValueKind::Integer,
        "The number of local users mentioned which will cause a post to be removed from public timelines. Set to `0` to disable delisting.",
        &[Suggestion::Count(10)],
    ),
    hinted(
        "reject_threshold",
        ValueKind::Integer,
        "The number of local users mentioned which will cause a post to be rejected entirely. Set to `0` to disable rejection.",
        &[Suggestion::Count(20)],
    ),
];

/// The `:mrf_object_age` children: the age gate and the actions it runs.
pub(super) const MRF_OBJECT_AGE_CHILDREN: [ChildDescription; 2] = [
    hinted(
        "threshold",
        ValueKind::Integer,
        "Age in seconds past which the configured actions run.",
        &[Suggestion::Count(604_800)],
    ),
    hinted(
        "actions",
        ValueKind::AtomList,
        "The actions to run on objects older than `threshold`.",
        &[
            Suggestion::Text("delist"),
            Suggestion::Text("strip_followers"),
            Suggestion::Text("reject"),
        ],
    ),
];

/// The `:mrf_hashtag` children: the three tag lists, in the words of
/// Pleroma's `MRFHashtag` knowledge doc.
pub(super) const MRF_HASHTAG_CHILDREN: [ChildDescription; 3] = [
    hinted(
        "sensitive",
        ValueKind::AtomList,
        "A list of hashtags which result in the post being marked as sensitive (default: `nsfw`).",
        &[Suggestion::Text("nsfw")],
    ),
    leaf(
        "reject",
        ValueKind::AtomList,
        "A list of hashtags which result in the message being rejected.",
    ),
    leaf(
        "federated_timeline_removal",
        ValueKind::AtomList,
        "A list of hashtags which result in the message being removed from the federated timeline (a.k.a. the whole known network).",
    ),
];
