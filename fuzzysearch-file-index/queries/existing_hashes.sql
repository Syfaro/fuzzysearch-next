SELECT
    hash
FROM
    file
WHERE
    metadata_version >= $1
    AND hash = any($2);
