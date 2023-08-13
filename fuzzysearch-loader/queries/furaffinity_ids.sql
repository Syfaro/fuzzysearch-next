SELECT DISTINCT ON (site_id, site_submission_id::int)
    site_submission_id::int,
    deleted
FROM
    submission
WHERE
    site_id = 1
ORDER BY
    site_id,
    site_submission_id::int,
    retrieved_at DESC NULLS LAST;
