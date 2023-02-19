SELECT uuid, registered_at FROM account WHERE lower(email) = lower($1);
