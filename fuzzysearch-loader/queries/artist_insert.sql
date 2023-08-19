insert into
    artist (site, site_artist_id, display_name, link)
values
    ($1::text::site, $2, $3, $4) ON CONFLICT (site, site_artist_id)
        do update set display_name = excluded.display_name, link = excluded.link
    returning id;
