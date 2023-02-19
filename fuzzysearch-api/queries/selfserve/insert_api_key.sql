INSERT INTO
    api_key (
        user_id,
        name,
        key,
        name_limit,
        image_limit,
        hash_limit
    )
VALUES
    ((SELECT id FROM account WHERE uuid = $1), $2, $3, 120, 5, 15);
