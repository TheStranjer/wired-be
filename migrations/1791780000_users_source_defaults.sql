-- The two posting defaults the `source` block of the `Account` entity
-- renders (docs/ARCHITECTURE.md decision 1: the HTTP shapes are what must
-- match Pleroma). Reference: Pleroma's `User` schema (`lib/pleroma/user.ex`)
-- and `AccountView.do_render("show.json", …)`
-- (`lib/pleroma/web/mastodon_api/views/account_view.ex`), which put
-- `source.sensitive` and `source.language` in for the account's own user.
-- Nothing writes either column yet — the profile-update ticket that lets an
-- account choose them is a later one — so every stored row answers the
-- block's documented defaults.
ALTER TABLE users
    -- Whether new statuses are flagged sensitive by default
    -- (`source.sensitive`).
    ADD COLUMN sensitive_default boolean NOT NULL DEFAULT false,
    -- BCP 47 language tag new statuses default to (`source.language`);
    -- NULL means the account stated no preference.
    ADD COLUMN language text;
