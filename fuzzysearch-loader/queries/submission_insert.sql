INSERT INTO submissions.submission (site_id, site_submission_id, deleted, posted_at, link, title, artists, tags, description, rating, retrieved_at, extra)
    VALUES ((SELECT id FROM submissions.site WHERE name = $1), $2, $3, $4, $5, $6, $7, $8, $9, $10, $11, $12)
    ON CONFLICT (site_id, site_submission_id, content_hash)
        DO UPDATE SET retrieved_at = EXCLUDED.retrieved_at
    RETURNING id;
