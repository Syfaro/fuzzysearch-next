SELECT
    api_key.id,
    user_id,
    key,
    name,
    name_limit,
    image_limit,
    hash_limit
FROM
    api_key
    JOIN account ON account.id = api_key.user_id
WHERE
    account.uuid = $1
    AND deleted = false
ORDER BY
    api_key.id ASC;
