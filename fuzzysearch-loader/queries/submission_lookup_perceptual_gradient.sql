SELECT
	DISTINCT ON (submission.site_id, submission.site_submission_id)
	site.name,
	submission.site_submission_id
FROM
	submissions.media_frame
	JOIN submissions.submission_media ON submission_media.media_id = media_frame.media_id
    JOIN submissions.media ON media.id = submission_media.media_id
	JOIN submissions.submission ON submission.id = submission_media.submission_id
	JOIN submissions.site ON site.id = submission.site_id
WHERE
	media_frame.perceptual_gradient = ANY($1)
	AND media_frame.perceptual_gradient IS NOT NULL
    AND media.single_frame = true
ORDER BY
	submission.site_id,
	submission.site_submission_id,
	submission.retrieved_at DESC NULLS LAST;
