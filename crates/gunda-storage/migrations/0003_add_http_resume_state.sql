CREATE TABLE http_resume_state (
    download_id INTEGER PRIMARY KEY NOT NULL
        CHECK (download_id > 0),

    strong_etag BLOB NOT NULL
        CHECK (
            typeof(strong_etag) = 'blob'
            AND length(strong_etag) >= 2
        ),

    total_bytes INTEGER NOT NULL
        CHECK (
            typeof(total_bytes) = 'integer'
            AND total_bytes >= 0
        ),

    durable_bytes INTEGER NOT NULL DEFAULT 0
        CHECK (
            typeof(durable_bytes) = 'integer'
            AND durable_bytes >= 0
            AND durable_bytes <= total_bytes
        ),

    FOREIGN KEY (download_id)
        REFERENCES downloads (id)
        ON DELETE CASCADE
);