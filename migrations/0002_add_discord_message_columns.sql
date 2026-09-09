-- Record where each URL was posted so we can, e.g., link back to the
-- original message. Nullable since existing rows have no message to point to.
ALTER TABLE reposts ADD COLUMN message_id TEXT;
ALTER TABLE reposts ADD COLUMN channel_id TEXT;
ALTER TABLE reposts ADD COLUMN guild_id TEXT;
