CREATE TABLE downloads (
    id INTEGER PRIMARY KEY AUTOINCREMENT
        CHECK (id > 0),

    source_url TEXT NOT NULL,

    origin TEXT NOT NULL
        CHECK (origin IN ('desktop', 'cli', 'browser')),

    source_page_url TEXT,
    source_page_title TEXT,

    destination_directory BLOB NOT NULL,
    preferred_filename TEXT,

    conflict_policy TEXT NOT NULL
        CHECK (conflict_policy IN ('rename', 'overwrite', 'fail')),

    state TEXT NOT NULL
        CHECK (
            state IN (
                'queued',
                'inspecting',
                'downloading',
                'paused',
                'finalizing',
                'completed',
                'failed',
                'cancelled',
                'interrupted'
            )
        ),

    downloaded_bytes INTEGER NOT NULL DEFAULT 0
        CHECK (downloaded_bytes >= 0),

    total_bytes INTEGER
        CHECK (total_bytes IS NULL OR total_bytes >= 0),

    created_at_unix_ms INTEGER NOT NULL,
    updated_at_unix_ms INTEGER NOT NULL,

    CHECK (
        total_bytes IS NULL
        OR downloaded_bytes <= total_bytes
    ),

    CHECK (
        (
            origin = 'browser'
            AND source_page_url IS NOT NULL
        )
        OR
        (
            origin IN ('desktop', 'cli')
            AND source_page_url IS NULL
            AND source_page_title IS NULL
        )
    )
);

-- This table stores only request headers classified as safe for
-- durable plaintext persistence. Sensitive request context requires
-- a separate secure storage design.
CREATE TABLE download_headers (
    download_id INTEGER NOT NULL,
    position INTEGER NOT NULL
        CHECK (position >= 0),

    name TEXT NOT NULL,
    value TEXT NOT NULL,

    sensitivity TEXT NOT NULL
        CHECK (sensitivity = 'public'),

    PRIMARY KEY (download_id, position),

    FOREIGN KEY (download_id)
        REFERENCES downloads (id)
        ON DELETE CASCADE
);

CREATE INDEX downloads_state_idx
    ON downloads (state);