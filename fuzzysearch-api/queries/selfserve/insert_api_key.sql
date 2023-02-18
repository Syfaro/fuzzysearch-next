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
    ($1, $2, $3, 120, 5, 15);
