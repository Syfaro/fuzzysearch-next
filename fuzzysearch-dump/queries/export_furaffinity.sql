SELECT
    submission.id "id!",
    artist.name "artist_name",
    submission.hash_int,
    submission.posted_at,
    submission.updated_at,
    submission.file_sha256,
    submission.deleted "deleted!",
    submission.url
FROM
    submission
    LEFT JOIN artist ON artist.id = submission.artist_id;
