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
    tags.tags "tags"
FROM submission
LEFT JOIN artist
    ON artist.id = submission.artist_id
LEFT JOIN (
	SELECT tag_to_post.post_id, array_agg(tag.name) tags
	FROM tag_to_post
	JOIN tag ON tag.id = tag_to_post.tag_id
	GROUP BY (tag_to_post.post_id)
) tags
    ON tags.post_id = submission.id
WHERE
    submission.id = $1
    OR submission.file_id = $1;
