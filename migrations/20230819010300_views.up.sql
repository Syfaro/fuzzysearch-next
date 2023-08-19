create view submission_detail (
	id,
	site,
	site_submission_id,
	link,
	title,
	description,
	rating,
	posted_at,
	tags,
	deleted,
	retrieved_at,
	extra,
	artists,
	media
) as
select
	submission.id,
    submission.site,
	submission.site_submission_id,
	submission.link,
	submission.title,
	submission.description,
	submission.rating,
	submission.posted_at,
	submission.tags,
	submission.deleted,
	submission.retrieved_at,
	submission.extra,
	(
		select
			jsonb_agg(jsonb_build_object(
				'site_artist_id', artist.site_artist_id,
				'name', artist.display_name,
				'link', artist.link
			))
		from submission_artist
			join artist on artist.id = submission_artist.artist_id
		where submission_artist.submission_id = submission.id
	) artists,
	(
		select
			jsonb_agg(jsonb_build_object(
				'site_id', submission_media.site_media_id,
				'url', submission_media.url,
				'file_sha256', encode(media.file_sha256, 'hex'),
				'file_size', media.file_size,
				'mime_type', media.mime_type,
				'extra', submission_media.extra,
				'frames', (
					with related_frame as (
						select
							media_frame.*
						from media_frame
						where media_frame.media_id = media.id
						order by media_frame.frame_index
						limit 50
					)
					select
						jsonb_agg(jsonb_build_object(
							'frame_index', related_frame.frame_index,
							'perceptual_gradient', related_frame.perceptual_gradient
						))
					from related_frame
				)
			))
		from submission_media
			join media on media.id = submission_media.media_id
		where submission_media.submission_id = submission.id
	) media
from submission;

create view submission_latest (
	id,
	site,
	site_submission_id,
	link,
	title,
	description,
	rating,
	posted_at,
	tags,
	deleted,
	retrieved_at,
	extra,
	artists,
	media
) as
select distinct on (submission_detail.site, submission_detail.site_submission_id)
	submission_detail.*
from submission_detail
order by
	submission_detail.site,
	submission_detail.site_submission_id,
	submission_detail.retrieved_at desc nulls last;
