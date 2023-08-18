INSERT INTO
    api.account (username)
VALUES
    ($1) RETURNING id;
