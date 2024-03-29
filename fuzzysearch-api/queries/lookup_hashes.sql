WITH hashes AS (
    SELECT * FROM json_to_recordset($1::json)
        AS hashes (searched_hash bigint, found_hash bigint, distance bigint)
)
SELECT
    'FurAffinity' "site!",
    submission.id "id!",
    submission.hash_int "hash!",
    submission.url,
    submission.filename,
    ARRAY(SELECT artist.name) artists,
    submission.file_id,
    null sources,
    submission.rating,
    submission.posted_at,
    ARRAY(SELECT tag.name FROM tag_to_post JOIN tag ON tag.id = tag_to_post.tag_id WHERE tag_to_post.post_id = submission.id) tags,
    hashes.searched_hash "searched_hash!",
    hashes.distance "distance!",
    submission.file_sha256 sha256
FROM hashes
JOIN submission ON hashes.found_hash = submission.hash_int
JOIN artist ON submission.artist_id = artist.id
WHERE hash_int IN (SELECT hashes.found_hash)
UNION ALL
SELECT
    'e621' site,
    e621.id,
    e621.hash,
    e621.data->'file'->>'url' url,
    (e621.data->'file'->>'md5') || '.' || (e621.data->'file'->>'ext') filename,
    ARRAY(SELECT jsonb_array_elements_text(e621.data->'tags'->'artist')) artists,
    null file_id,
    ARRAY(SELECT jsonb_array_elements_text(e621.data->'sources')) sources,
    e621.data->>'rating' rating,
    to_timestamp(data->>'created_at', 'YYYY-MM-DD"T"HH24:MI:SS"Z"') posted_at,
    e621.tags,
    hashes.searched_hash,
    hashes.distance,
    e621.sha256
FROM hashes
JOIN e621 ON hashes.found_hash = e621.hash
WHERE e621.hash IN (SELECT hashes.found_hash)
UNION ALL
SELECT
    'Weasyl' site,
    weasyl.id,
    weasyl.hash,
    weasyl.data->>'link' url,
    null filename,
    ARRAY(SELECT weasyl.data->>'owner_login') artists,
    null file_id,
    null sources,
    weasyl.data->>'rating' rating,
    to_timestamp(data->>'posted_at', 'YYYY-MM-DD"T"HH24:MI:SS"Z"') posted_at,
    ARRAY(select jsonb_array_elements_text(weasyl.data->'tags')) tags,
    hashes.searched_hash,
    hashes.distance,
    weasyl.sha256
FROM hashes
JOIN weasyl ON hashes.found_hash = weasyl.hash
WHERE weasyl.hash IN (SELECT hashes.found_hash)
UNION ALL
SELECT
    'Twitter' site,
    tweet.id,
    tweet_media.hash,
    tweet_media.url,
    null filename,
    ARRAY(SELECT tweet.data->'user'->>'screen_name') artists,
    null file_id,
    null sources,
    CASE
        WHEN (tweet.data->'possibly_sensitive')::boolean IS true THEN 'adult'
        WHEN (tweet.data->'possibly_sensitive')::boolean IS false THEN 'general'
    END rating,
    to_timestamp(tweet.data->>'created_at', 'DY Mon DD HH24:MI:SS +0000 YYYY') posted_at,
    array[]::text[] tags,
    hashes.searched_hash,
    hashes.distance,
    null sha256
FROM hashes
JOIN tweet_media ON hashes.found_hash = tweet_media.hash
JOIN tweet ON tweet_media.tweet_id = tweet.id
WHERE tweet_media.hash IN (SELECT hashes.found_hash);
