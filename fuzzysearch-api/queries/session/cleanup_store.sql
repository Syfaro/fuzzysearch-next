DELETE FROM session WHERE expires_at < current_timestamp;
