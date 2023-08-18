WITH requested_bucket AS (
	SELECT * FROM json_to_recordset($3::json)
		AS requested_bucket (bucket text, count integer)
)
INSERT INTO api.rate_limit (key_id, time_window, group_name, count)
	SELECT $1, $2, requested_bucket.bucket, requested_bucket.count FROM requested_bucket
	ON CONFLICT ON CONSTRAINT unique_window DO UPDATE SET count = rate_limit.count + EXCLUDED.count
	RETURNING rate_limit.group_name, rate_limit.count;
