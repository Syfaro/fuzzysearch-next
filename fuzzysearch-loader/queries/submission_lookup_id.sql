with search_query as (
	select
		query.site,
		query.site_submission_id
	from jsonb_to_recordset($1) AS query (site site, site_submission_id text)
)
select distinct on (submission_detail.site, submission_detail.site_submission_id)
	submission_detail.id "id!",
	submission_detail.site::text "site!",
	submission_detail.site_submission_id "site_submission_id!",
	submission_detail.link "link!",
	submission_detail.title,
	submission_detail.description,
	submission_detail.rating::text,
	submission_detail.posted_at,
	submission_detail.tags,
	submission_detail.deleted "deleted!",
	submission_detail.retrieved_at,
	submission_detail.extra,
	submission_detail.artists "artists: sqlx::types::Json<Vec<fuzzysearch_common::Artist>>",
	submission_detail.media "media: sqlx::types::Json<Vec<fuzzysearch_common::Media>>"
from search_query
	join submission_detail on submission_detail.site = search_query.site and submission_detail.site_submission_id = search_query.site_submission_id
order by
	submission_detail.site,
	submission_detail.site_submission_id,
	submission_detail.retrieved_at desc nulls last;
