SELECT
    1 one
FROM
    download_history
WHERE
    site_id = $1
    and submission_id = $2;
