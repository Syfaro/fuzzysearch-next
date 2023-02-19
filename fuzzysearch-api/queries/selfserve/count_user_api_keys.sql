SELECT
    count(*)
FROM
    api_key
    JOIN account ON account.id = api_key.user_id
WHERE
    account.uuid = $1
    AND deleted = false;
