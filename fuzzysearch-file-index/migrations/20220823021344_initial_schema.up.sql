CREATE TABLE site (
    id SERIAL PRIMARY KEY,
    name TEXT NOT NULL UNIQUE
);

CREATE TABLE file (
    hash BYTEA PRIMARY KEY,
    size INTEGER NOT NULL,
    height INTEGER,
    width INTEGER,
    mime_type TEXT,
    created_at TIMESTAMP WITH TIME ZONE NOT NULL DEFAULT current_timestamp,
    updated_at TIMESTAMP WITH TIME ZONE NOT NULL DEFAULT current_timestamp
);

CREATE TABLE file_association (
	hash BYTEA NOT NULL REFERENCES file (hash),
	site_id INTEGER NOT NULL REFERENCES site (id),
	submission_id TEXT NOT NULL,
    PRIMARY KEY (hash, site_id, submission_id)
);

CREATE FUNCTION set_updated_at() RETURNS TRIGGER AS $$
BEGIN
	NEW.updated_at = now();
	RETURN new;
END;
$$
LANGUAGE plpgsql;

CREATE TRIGGER set_updated_at_file
	BEFORE UPDATE ON file
	FOR EACH ROW
	EXECUTE PROCEDURE set_updated_at();
