SELECT id, account_id, token, name, name_limit, image_limit, hash_limit FROM api.key WHERE token = $1;
