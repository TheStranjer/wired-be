-- Base migration: database extensions the schema relies on.
-- citext: case-insensitive text (nicknames, emails)
-- pg_trgm: trigram indexes for search
-- pgcrypto: gen_random_uuid() and hashing helpers
CREATE EXTENSION IF NOT EXISTS citext;
CREATE EXTENSION IF NOT EXISTS pg_trgm;
CREATE EXTENSION IF NOT EXISTS pgcrypto;
