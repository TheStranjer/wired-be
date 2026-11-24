-- Drop the preferred-frontend picker (WBE-691): this backend serves no
-- frontend, so the stored choice selected nothing.
ALTER TABLE users DROP COLUMN preferred_frontend;
