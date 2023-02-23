SELECT
    url
FROM
    dump
WHERE
    created_at < current_timestamp - interval '14 days'
    AND created_at <> (
        SELECT
            max(created_at)
        FROM
            dump
    );
