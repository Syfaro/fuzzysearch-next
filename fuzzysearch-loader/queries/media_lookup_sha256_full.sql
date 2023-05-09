SELECT
    media.id,
    media_frame.perceptual_gradient
FROM
    submissions.media
    LEFT JOIN submissions.media_frame ON media_frame.media_id = media.id AND media_frame.frame_index = 0
WHERE
    file_sha256 IS NOT NULL
    AND file_sha256 = $1;
