INSERT INTO submissions.media_frame (media_id, frame_index, perceptual_gradient)
    SELECT * FROM jsonb_to_recordset($1)
        AS frame (media_id uuid, frame_index bigint, perceptual_gradient bigint)
    ON CONFLICT DO NOTHING;
