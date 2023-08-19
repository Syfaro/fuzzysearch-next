create schema api;

create table api.account (
	id uuid primary key default gen_random_uuid(),
	username text not null,
	registered_at timestamp with time zone default current_timestamp
);

create unique index account_username_idx on api.account ((lower(username)));

create table api.key (
	id uuid primary key default gen_random_uuid(),
	account_id uuid not null references api.account (id),
	name text not null,
	token text not null,
	name_limit integer not null,
	image_limit integer not null,
	hash_limit integer not null,
	created_at timestamp with time zone default current_timestamp,
	deleted_at timestamp with time zone
);

create index api_key_user_lookup_idx on api.key (account_id) where (deleted_at is null);
create unique index api_key_token_idx on api.key (token) include (account_id, name, name_limit, image_limit, hash_limit) where (deleted_at is null);

create table api.session (
	id text primary key,
	expires_at timestamp with time zone,
	data jsonb
);

create index api_session_expires_idx on api.session (expires_at);

create table api.rate_limit (
	key_id uuid not null references api.key (id),
	time_window bigint not null,
	group_name text not null,
	count integer not null default 0,
	constraint unique_window primary key (key_id, time_window, group_name)
);

create table api.webauthn_credential (
	id bytea primary key,
	account_id uuid not null references api.account (id),
	credential jsonb not null
);

create table api.webhook (
	id uuid primary key default gen_random_uuid(),
	account_id uuid not null references api.account (id),
	endpoint text not null
);

create table api.dump (
	created_at timestamp with time zone primary key,
	url text not null
);
