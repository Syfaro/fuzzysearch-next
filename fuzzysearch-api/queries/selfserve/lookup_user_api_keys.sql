SELECT
    key.id,
    account_id,
    token,
    name,
    name_limit,
    image_limit,
    hash_limit
FROM
    api.key
    JOIN api.account ON account.id = key.account_id
WHERE
    account.id = $1
    AND deleted_at IS NULL
ORDER BY
    key.id;
