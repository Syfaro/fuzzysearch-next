UPDATE
    api_key
SET
    deleted = true
WHERE
    id = $1 RETURNING name;
