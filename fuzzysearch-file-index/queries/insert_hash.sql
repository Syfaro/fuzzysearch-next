INSERT INTO file (hash, size, height, width, mime_type) VALUES($1, $2, $3, $4, $5)
    ON CONFLICT (hash) DO UPDATE SET
        size = EXCLUDED.size,
        height = EXCLUDED.height,
        width = EXCLUDED.width,
        mime_type = EXCLUDED.mime_type;
