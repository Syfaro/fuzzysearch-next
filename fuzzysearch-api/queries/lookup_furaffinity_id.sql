SELECT
    submission.id,
    file_id,
    artist.name "artist?",
    submission.hash_int hash,
    submission.url,
    submission.filename,
    submission.rating,
    submission.posted_at,
    submission.file_size,
    submission.file_sha256 "sha256",
    submission.updated_at,
    submission.deleted,
    ARRAY(
        SELECT
            tag.name
        FROM
            tag_to_post
            JOIN tag ON tag.id = tag_to_post.tag_id
        WHERE
            tag_to_post.post_id = submission.id
    ) tags
FROM submission
LEFT JOIN artist
    ON artist.id = submission.artist_id
WHERE
    submission.id = $1
    OR submission.file_id = $1;
