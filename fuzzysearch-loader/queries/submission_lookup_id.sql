WITH search_query AS (
	SELECT
		site.id site_id,
		query.site_submission_id
	FROM jsonb_to_recordset($1) AS query (site TEXT, site_submission_id TEXT)
	JOIN site ON site.name = query.site
)
SELECT DISTINCT ON (submission_detail.site_id, submission_detail.site_submission_id)
	submission_detail.id "id!",
	submission_detail.site "site!",
	submission_detail.site_submission_id "site_submission_id!",
	submission_detail.link "link!",
	submission_detail.title,
	submission_detail.description,
	submission_detail.rating,
	submission_detail.posted_at,
	submission_detail.tags,
	submission_detail.deleted "deleted!",
	submission_detail.retrieved_at,
	submission_detail.extra,
	submission_detail.artists "artists: sqlx::types::Json<Vec<fuzzysearch_common::Artist>>",
	submission_detail.media "media: sqlx::types::Json<Vec<fuzzysearch_common::Media>>"
FROM search_query
	JOIN submission_detail ON submission_detail.site_id = search_query.site_id AND submission_detail.site_submission_id = search_query.site_submission_id
ORDER BY
	submission_detail.site_id,
	submission_detail.site_submission_id,
	submission_detail.retrieved_at DESC NULLS LAST;
