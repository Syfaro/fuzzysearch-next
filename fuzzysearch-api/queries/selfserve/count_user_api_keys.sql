SELECT
    count(*)
FROM
    api.key
WHERE
    account_id = $1
    AND deleted_at IS NULL;
