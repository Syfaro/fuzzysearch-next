SELECT
    data
FROM
    session
WHERE
    id = $1
    AND (
        expires_at IS NULL
        OR expires_at > current_timestamp
    );
