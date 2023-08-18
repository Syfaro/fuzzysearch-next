SELECT id, registered_at FROM api.account WHERE lower(username) = lower($1);
