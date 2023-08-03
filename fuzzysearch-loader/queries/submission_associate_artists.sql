INSERT INTO submission_artist (submission_id, artist_id)
    SELECT * FROM jsonb_to_recordset($1)
        AS submission_artist (submission_id uuid, artist_id uuid)
    ON CONFLICT DO NOTHING;
