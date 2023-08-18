INSERT INTO
    api.key (
        account_id,
        name,
        token,
        name_limit,
        image_limit,
        hash_limit
    )
VALUES
    ($1, $2, $3, 60, 60, 30);
