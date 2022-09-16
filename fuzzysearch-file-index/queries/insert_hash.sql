INSERT INTO file (hash, size, height, width, mime_type, metadata_version, exif_entries)
    SELECT decode(item.hash, 'hex'), size, height, width, mime_type, metadata_version, exif_entries
        FROM json_to_recordset($1::json) AS item(hash text, size integer, height integer, width integer, mime_type text, metadata_version integer, exif_entries jsonb)
    ON CONFLICT (hash) DO UPDATE SET
        size = EXCLUDED.size,
        height = EXCLUDED.height,
        width = EXCLUDED.width,
        mime_type = EXCLUDED.mime_type,
        metadata_version = EXCLUDED.metadata_version,
        exif_entries = EXCLUDED.exif_entries;
