create table media_ignore (
	id uuid primary key default gen_random_uuid(),
	file_sha256 bytea not null unique,
	perceptual_gradient bigint,
	created_at timestamp without time zone not null default current_timestamp,
	reason text not null
);

create index media_ignore_pagination_idx on media_ignore (created_at, id);
