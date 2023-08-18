INSERT INTO submission_media (submission_id, media_id, site_media_id, url, extra)
    VALUES ($1, $2, $3, $4, $5)
    ON CONFLICT DO NOTHING
    RETURNING id;
