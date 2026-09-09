ALTER TABLE downloads
    ADD COLUMN resolved_destination_path BLOB;

ALTER TABLE downloads
    ADD COLUMN resource_kind TEXT
        CHECK (
            resource_kind IN (
                'unknown',
                'file',
                'hls',
                'dash'
            )
        );

ALTER TABLE downloads
    ADD COLUMN resource_display_name TEXT;

ALTER TABLE downloads
    ADD COLUMN resource_content_type TEXT
        CHECK (
            resource_kind IS NOT NULL
            OR (
                resource_display_name IS NULL
                AND resource_content_type IS NULL
            )
        );

ALTER TABLE downloads
    ADD COLUMN last_failure_kind TEXT
        CHECK (
            last_failure_kind IN (
                'network',
                'authentication',
                'remote_rejected',
                'invalid_response',
                'unsupported_resource',
                'permission_denied',
                'disk_full',
                'integrity',
                'storage',
                'internal'
            )
        );

ALTER TABLE downloads
    ADD COLUMN last_failure_message TEXT;

ALTER TABLE downloads
    ADD COLUMN last_failure_retryable INTEGER
        CHECK (
            (
                last_failure_kind IS NULL
                AND last_failure_message IS NULL
                AND last_failure_retryable IS NULL
            )
            OR
            (
                last_failure_kind IS NOT NULL
                AND last_failure_message IS NOT NULL
                AND last_failure_retryable IS NOT NULL
                AND last_failure_retryable IN (0, 1)
            )
        );