INSERT INTO file_association (hash, site_id, submission_id, posted_at)
    SELECT decode(item.hash, 'hex'), site_id, submission_id, posted_at
        FROM json_to_recordset($1::json) AS item(hash text, site_id integer, submission_id text, posted_at timestamp with time zone)
        WHERE exists(SELECT 1 FROM file WHERE file.hash = decode(item.hash, 'hex'))
    ON CONFLICT DO NOTHING;
