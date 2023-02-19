UPDATE
    api_key
SET
    deleted = true
WHERE
    id = $1
    AND user_id = (SELECT id FROM account WHERE uuid = $2)
RETURNING
    name;
