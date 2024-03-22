insert into submission (site, site_submission_id, deleted, posted_at, link, title, tags, description, rating, retrieved_at, extra, fetch_reason)
    values ($1::text::site, $2, $3, $4, $5, $6, $7, $8, $9::text::rating, $10, $11, $12)
    on conflict do nothing
    returning id;
