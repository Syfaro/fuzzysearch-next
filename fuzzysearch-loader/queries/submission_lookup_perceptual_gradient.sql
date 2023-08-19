select
	distinct on (submission.site, submission.site_submission_id)
	submission.site::text "site!",
	submission.site_submission_id "site_submission_id!"
from
	media_frame
	join submission_media on submission_media.media_id = media_frame.media_id
    join media on media.id = submission_media.media_id
	join submission on submission.id = submission_media.submission_id
where
	media_frame.perceptual_gradient = any($1)
	and media_frame.perceptual_gradient is not null
    and media.single_frame = true
order by
	submission.site,
	submission.site_submission_id,
	submission.retrieved_at desc nulls last;
