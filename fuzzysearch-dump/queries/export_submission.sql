SELECT
    site "site!",
    site_submission_id "site_submission_id!",
    artists "artists: sqlx::types::Json<Vec<fuzzysearch_common::Artist>>",
    posted_at,
    retrieved_at,
    deleted "deleted!",
    media "media: sqlx::types::Json<Vec<fuzzysearch_common::Media>>"
FROM submission_latest;
