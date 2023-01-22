INSERT INTO
    download_history (
        site_id,
        submission_id,
        posted_at,
        url,
        successful,
        hash
    )
VALUES
    ($1, $2, $3, $4, true, decode($5, 'hex'));
