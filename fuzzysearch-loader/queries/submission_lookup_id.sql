WITH search_query AS (
    SELECT
        site.id site_id,
        query.submission_id
    FROM jsonb_to_recordset($1)
        AS query (site_name TEXT, submission_id TEXT)
    JOIN submissions.site ON site.name = query.site_name
)
SELECT
	DISTINCT ON (submission.site_id, submission.site_submission_id, submission_media.site_media_id)
	submission.id,
	site.name site_name,
	submission.site_submission_id,
	submission_media.site_media_id,
	submission.link,
	submission.title,
	submission.description,
	submission_media.url media_url,
	submission_media.extra submission_media_extra,
	media.file_sha256 file_sha256,
	submission.artists,
	submission.rating,
	submission.posted_at,
	submission.tags,
	(submission.deleted OR submission_media.deleted) deleted,
	submission.retrieved_at,
	submission.extra,
	media_frame.perceptual_gradient,
	media.id "media_id?",
	media.file_size,
	media.mime_type,
	media_frame.frame_index "frame_index?"
FROM search_query
    JOIN submissions.submission ON submission.site_id = search_query.site_id AND submission.site_submission_id = search_query.submission_id
	JOIN submissions.site ON site.id = submission.site_id
	LEFT JOIN submissions.submission_media ON submission_media.submission_id = submission.id
	LEFT JOIN submissions.media ON media.id = submission_media.media_id
	LEFT JOIN submissions.media_frame ON media_frame.media_id = media.id
ORDER BY
	submission.site_id,
	submission.site_submission_id,
	submission_media.site_media_id,
	submission.retrieved_at DESC NULLS LAST
