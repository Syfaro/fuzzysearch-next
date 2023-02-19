INSERT INTO
    session (id, expires_at, data)
VALUES
    ($1, $2, $3) ON CONFLICT (id) DO
UPDATE
SET
    expires_at = EXCLUDED.expires_at,
    data = EXCLUDED.data;
