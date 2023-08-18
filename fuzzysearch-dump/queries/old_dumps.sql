SELECT
    url
FROM
    api.dump
WHERE
    created_at < current_timestamp - interval '14 days'
    AND created_at <> (
        SELECT
            max(created_at)
        FROM
            api.dump
    );
