INSERT INTO
    submissions.media (file_sha256, file_size, mime_type, single_frame)
VALUES
    ($1, $2, $3, $4) ON CONFLICT DO NOTHING RETURNING id;
