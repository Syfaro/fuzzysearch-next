create collation numeric (provider = icu, locale = 'en@colNumeric=yes');

create type site as enum ('FurAffinity', 'e621', 'Weasyl', 'Twitter');
create type rating as enum ('general', 'mature', 'adult');

create table artist (
	id uuid primary key default gen_random_uuid(),
	site site not null,
	site_artist_id text not null,
	display_name text not null,
	link text,
	unique (site, site_artist_id)
);

create table submission (
	id uuid primary key default gen_random_uuid(),
	site site not null,
	site_submission_id text not null collate numeric,
	retrieved_at timestamp with time zone,
	fetch_reason text not null,
	link text not null,
	deleted boolean not null,
	posted_at timestamp with time zone,
	title text,
	tags text[],
	description text,
	rating rating,
	extra jsonb
);

create unique index submission_site_lookup_idx on submission (site, site_submission_id, retrieved_at desc) nulls not distinct;
create index submission_retrieved_at_idx on submission (retrieved_at desc nulls last);
create index submission_site_id_num_idx on submission (site, (site_submission_id::bigint));

create table submission_artist (
	submission_id uuid not null references submission on delete cascade,
	artist_id uuid not null references artist,
	primary key (submission_id, artist_id)
);

create table media (
	id uuid primary key default gen_random_uuid(),
	file_sha256 bytea unique,
	file_size bigint,
	single_frame boolean,
	mime_type text,
	created_at timestamp without time zone not null default current_timestamp
);

create table media_frame (
	id uuid primary key default gen_random_uuid(),
	media_id uuid not null references media on delete cascade,
	frame_index bigint not null,
	perceptual_gradient bigint
);

create unique index media_frame_idx on media_frame (media_id, frame_index) include (perceptual_gradient);
create index media_frame_perceptual_gradient_idx on media_frame using hash (perceptual_gradient) where (perceptual_gradient is not null);

create table submission_media (
	id uuid primary key default gen_random_uuid(),
	submission_id uuid not null references submission on delete cascade,
	media_id uuid not null references media on delete cascade,
	site_media_id text collate numeric,
	url text,
	extra jsonb
);

create index submission_media_idx on submission_media using hash (media_id);
create unique index submission_media_site_idx on submission_media (submission_id, site_media_id) nulls not distinct;
