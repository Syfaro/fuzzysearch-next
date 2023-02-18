SELECT count(*) FROM api_key WHERE user_id = $1 AND deleted = false;
