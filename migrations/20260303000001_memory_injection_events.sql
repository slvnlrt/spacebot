-- Memory injection events for channel timeline visualization.
--
-- SAFETY: This table is UI/history only and must never be fed back into LLM
-- context assembly, compaction input, or memory recall pipelines.

CREATE TABLE IF NOT EXISTS memory_injection_events (
    id TEXT PRIMARY KEY,
    channel_id TEXT NOT NULL,
    pinned_json TEXT NOT NULL DEFAULT '[]',
    contextual_json TEXT NOT NULL DEFAULT '[]',
    pinned_count INTEGER NOT NULL DEFAULT 0,
    contextual_count INTEGER NOT NULL DEFAULT 0,
    injected_at TIMESTAMP NOT NULL DEFAULT CURRENT_TIMESTAMP,
    FOREIGN KEY (channel_id) REFERENCES channels(id) ON DELETE CASCADE
);

CREATE INDEX IF NOT EXISTS idx_memory_injection_events_channel
    ON memory_injection_events(channel_id, injected_at);
