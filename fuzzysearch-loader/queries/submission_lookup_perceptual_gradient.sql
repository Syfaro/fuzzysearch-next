SELECT
	DISTINCT ON (submission.site_id, submission.site_submission_id)
	site.name,
	submission.site_submission_id
FROM
	media_frame
	JOIN submission_media ON submission_media.media_id = media_frame.media_id
    JOIN media ON media.id = submission_media.media_id
	JOIN submission ON submission.id = submission_media.submission_id
	JOIN site ON site.id = submission.site_id
WHERE
	media_frame.perceptual_gradient = ANY($1)
	AND media_frame.perceptual_gradient IS NOT NULL
    AND media.single_frame = true
ORDER BY
	submission.site_id,
	submission.site_submission_id,
	submission.retrieved_at DESC NULLS LAST;
