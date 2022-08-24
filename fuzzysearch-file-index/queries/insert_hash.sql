INSERT INTO file (hash, size, height, width, mime_type)
    SELECT decode(item.hash, 'hex'), size, height, width, mime_type
        FROM json_to_recordset($1::json) AS item(hash text, size integer, height integer, width integer, mime_type text)
    ON CONFLICT (hash) DO UPDATE SET
        size = EXCLUDED.size,
        height = EXCLUDED.height,
        width = EXCLUDED.width,
        mime_type = EXCLUDED.mime_type;
