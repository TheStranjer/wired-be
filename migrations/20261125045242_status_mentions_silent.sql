-- A silent link grants a local account access to a status (a direct post's
-- thread, the private audience) because its `to`/`cc` named the account,
-- without the status mentioning it: no Mention tag, so no notification, no
-- entry in the `mentions` array and no standing with the unsolicited-post
-- gate. Existing rows keep the meaning they had: real mentions.
ALTER TABLE status_mentions ADD COLUMN silent boolean NOT NULL DEFAULT false;
