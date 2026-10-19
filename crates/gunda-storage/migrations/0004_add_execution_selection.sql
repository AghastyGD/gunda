ALTER TABLE downloads
    ADD COLUMN execution_selection TEXT
        CHECK (
            execution_selection IS NULL
            OR (
                typeof(execution_selection) = 'text'
                AND length(execution_selection) BETWEEN 1 AND 128
            )
        );
