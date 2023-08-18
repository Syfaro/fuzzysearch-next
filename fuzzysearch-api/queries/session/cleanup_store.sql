DELETE FROM api.session WHERE expires_at < current_timestamp;
