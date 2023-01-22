INSERT INTO
    file_association
SELECT
    download_history.hash,
    download_history.site_id,
    download_history.submission_id,
    download_history.posted_at
FROM
    download_history
    JOIN file ON file.hash = download_history.hash
    LEFT JOIN file_association ON download_history.site_id = file_association.site_id
    AND download_history.submission_id::text = file_association.submission_id
WHERE
    file_association.submission_id IS NULL ON CONFLICT DO nothing;
