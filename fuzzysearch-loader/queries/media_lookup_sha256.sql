SELECT
    id
FROM
    media
WHERE
    file_sha256 IS NOT NULL
    AND file_sha256 = $1;
