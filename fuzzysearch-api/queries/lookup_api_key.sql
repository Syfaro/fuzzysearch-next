SELECT id, user_id, key, name, name_limit, image_limit, hash_limit FROM api_key WHERE key = $1;
