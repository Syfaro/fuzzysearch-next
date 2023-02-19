SELECT id FROM account WHERE lower(email) = lower($1);
