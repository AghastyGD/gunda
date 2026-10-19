CREATE TABLE stream_resume_state (
    download_id INTEGER PRIMARY KEY NOT NULL
        CHECK (download_id > 0),

    plan_fingerprint BLOB NOT NULL
        CHECK (
            typeof(plan_fingerprint) = 'blob'
            AND length(plan_fingerprint) = 16
        ),

    track_count INTEGER NOT NULL
        CHECK (track_count BETWEEN 1 AND 2),

    FOREIGN KEY (download_id)
        REFERENCES downloads (id)
        ON DELETE CASCADE
);

CREATE TABLE stream_resume_tracks (
    download_id INTEGER NOT NULL,

    track INTEGER NOT NULL
        CHECK (track BETWEEN 0 AND 1),

    next_resource INTEGER NOT NULL DEFAULT 0
        CHECK (next_resource >= 0),

    durable_bytes INTEGER NOT NULL DEFAULT 0
        CHECK (durable_bytes >= 0),

    PRIMARY KEY (download_id, track),

    FOREIGN KEY (download_id)
        REFERENCES stream_resume_state (download_id)
        ON DELETE CASCADE
);
