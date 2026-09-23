CREATE TABLE IF NOT EXISTS background_stream_refresh_jobs (
    user_id BLOB NOT NULL,
    media_id BLOB NOT NULL,
    series_id BLOB,
    priority INTEGER NOT NULL DEFAULT 0,
    run_after TEXT NOT NULL,
    lease_until TEXT,
    attempts INTEGER NOT NULL DEFAULT 0,
    last_error TEXT,
    created_at TEXT NOT NULL DEFAULT CURRENT_TIMESTAMP,
    updated_at TEXT NOT NULL DEFAULT CURRENT_TIMESTAMP,
    PRIMARY KEY (user_id, media_id)
);

CREATE INDEX IF NOT EXISTS idx_background_stream_refresh_due
    ON background_stream_refresh_jobs (run_after, priority DESC);
