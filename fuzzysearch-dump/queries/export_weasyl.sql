SELECT
    id,
    data->>'owner' "owner",
    hash,
    data->>'posted_at' "posted_at",
    sha256,
    deleted,
    data->'media'->'submission'->0->>'url' "submission"
FROM
    weasyl;
