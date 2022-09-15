SELECT
    id,
    array(
        select
            jsonb_array_elements_text(data->'tags'->'artist')
    ) "artists",
    hash,
    data->>'created_at' "created_at",
    sha256,
    deleted,
    data->'file'->>'url' "content_url"
FROM
    e621;
