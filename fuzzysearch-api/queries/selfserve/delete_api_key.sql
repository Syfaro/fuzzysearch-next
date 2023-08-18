UPDATE
    api.key
SET
    deleted_at = current_timestamp
WHERE
    id = $1
    AND account_id = $2
RETURNING
    name;
