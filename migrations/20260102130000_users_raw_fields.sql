-- The editing half of the profile metadata (docs/ARCHITECTURE.md decision 1:
-- the HTTP shapes are what must match Pleroma). Reference: Pleroma's `User`
-- schema (`lib/pleroma/user.ex`), which keeps `raw_fields` beside `fields` —
-- the text the account typed next to the rendered markup `put_fields` derives
-- from it — because `AccountView` answers the owner `source.fields` with the
-- raw text and the public entity `fields` with the HTML.
ALTER TABLE users
    -- Profile metadata as its source text: a JSON array of {name, value}
    -- objects. The `fields` column holds the same entries with the value
    -- rendered to sanitized HTML by update_credentials.
    ADD COLUMN raw_fields jsonb NOT NULL DEFAULT '[]';
