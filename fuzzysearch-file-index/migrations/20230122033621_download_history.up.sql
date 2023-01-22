CREATE TABLE download_history (
    site_id INTEGER NOT NULL REFERENCES site (id),
    submission_id INTEGER NOT NULL,
    posted_at TIMESTAMP WITH TIME ZONE,
    last_attempt TIMESTAMP WITH TIME ZONE NOT NULL DEFAULT current_timestamp,
    url TEXT NOT NULL,
    successful BOOLEAN NOT NULL,
    hash BYTEA,
    PRIMARY KEY (site_id, submission_id),
    CHECK (successful = false OR hash IS NOT NULL)
);
