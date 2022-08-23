CREATE VIEW file_detail AS
SELECT
	hash,
	encode(hash, 'hex') hash_hex,
	size,
	height,
	width,
	mime_type,
	created_at,
	(
        SELECT
            json_agg(json_build_object('site_name', site.name, 'submission_id', file_association.submission_id, 'posted_at', file_association.posted_at))
        FROM
            file_association
            JOIN site ON site.id = file_association.site_id
        WHERE
            file_association.hash = file.hash) associations
	FROM
		file;
