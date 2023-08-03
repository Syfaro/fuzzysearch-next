INSERT INTO
    artist (site_id, site_artist_id, display_name, link)
VALUES
    ((SELECT id FROM site WHERE name = $1), $2, $3, $4) ON CONFLICT (site_id, site_artist_id)
        DO UPDATE SET display_name = EXCLUDED.display_name, link = EXCLUDED.link
    RETURNING id;
