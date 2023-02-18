INSERT INTO
    account (email, password)
VALUES
    ($1, 0) RETURNING id;
