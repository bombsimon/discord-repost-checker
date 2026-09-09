-- Baseline schema, matching what `new_with_url` used to create inline. Kept as
-- `IF NOT EXISTS` so it's a no-op on databases created before migrations
-- existed, and still creates everything on a fresh database.
CREATE TABLE IF NOT EXISTS reposts (
    url TEXT NOT NULL,
    user_id TEXT NOT NULL,
    posted_at TEXT NOT NULL
);

CREATE TABLE IF NOT EXISTS ignore_hosts (
    host TEXT PRIMARY KEY
);

CREATE TABLE IF NOT EXISTS always_enabled_hosts (
    host TEXT PRIMARY KEY
);

CREATE TABLE IF NOT EXISTS preserve_full_url_hosts (
    host TEXT PRIMARY KEY
);

CREATE INDEX IF NOT EXISTS idx_reposts_url ON reposts(url);
