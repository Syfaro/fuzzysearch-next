SELECT
    media.id,
    array_agg(media_frame.perceptual_gradient ORDER BY media_frame.frame_index) perceptual_gradients
FROM
    media
    JOIN media_frame ON media_frame.media_id = media.id
WHERE
    file_sha256 IS NOT NULL
    AND file_sha256 = $1
GROUP BY
    1;
