SELECT height "height!", width "width!"
    FROM file
    JOIN file_association ON file_association.hash = file.hash
    WHERE height IS NOT NULL AND width IS NOT NULL AND file_association.site_id = $1;
