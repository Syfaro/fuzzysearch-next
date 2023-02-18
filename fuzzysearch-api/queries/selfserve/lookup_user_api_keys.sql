SELECT
    id, user_id, key, name, name_limit, image_limit, hash_limit
FROM
    api_key
WHERE
    user_id = $1
    AND deleted = false
ORDER BY
    id ASC;
