-- User domain blocks: the domains an account refuses to interact with,
-- the data behind `GET/POST/DELETE /api/v1/domain_blocks`, the
-- `domain_blocking` flag of the Relationship entity, and (through later
-- tickets) timeline and notification filtering. Reference: Pleroma keeps
-- these as the `domain_blocks` text array on the users row
-- (`lib/pleroma/user.ex`, `block_domain`/`unblock_domain`); here each
-- block is its own row, which is what lets the endpoint page by
-- `UUIDv7` cursors (decision 4).
CREATE TABLE user_domain_blocks (
    -- UUIDv7 made in Rust (decision 4); no database-side default.
    id uuid PRIMARY KEY,
    -- The blocking account. Deleting the account deletes its blocks.
    user_id uuid NOT NULL REFERENCES users(id) ON DELETE CASCADE,
    -- Normalised domain: lowercase, without a leading `@` or scheme or
    -- a trailing slash, and containing a dot (the endpoint validates;
    -- the array Pleroma stores carries the same spelling).
    domain text NOT NULL,
    inserted_at timestamptz NOT NULL DEFAULT now(),
    -- One block per (account, domain): the key block/unblock are
    -- idempotent on.
    CONSTRAINT user_domain_blocks_user_id_domain_key UNIQUE (user_id, domain)
);

-- Every read here starts from the owning user (the listing, the batch
-- flag load, the existence check), which the unique constraint's index
-- already serves; the domain side is only ever reached from a deleted
-- user row's cascade.
