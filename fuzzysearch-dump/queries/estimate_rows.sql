SELECT sum(reltuples)::bigint AS estimate FROM pg_class WHERE relname IN ('submission', 'weasyl', 'e621');
