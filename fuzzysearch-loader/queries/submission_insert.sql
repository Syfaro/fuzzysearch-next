INSERT INTO submission (site_id, site_submission_id, deleted, posted_at, link, title, tags, description, rating, retrieved_at, extra)
    VALUES ((SELECT id FROM site WHERE name = $1), $2, $3, $4, $5, $6, $7, $8, $9, $10, $11)
    ON CONFLICT DO NOTHING
    RETURNING id;
