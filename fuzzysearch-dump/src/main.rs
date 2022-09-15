use clap::Parser;
use serde::Serialize;
use tokio_stream::StreamExt;

#[derive(Clone, Debug, Serialize)]
#[serde(rename_all = "lowercase")]
enum Site {
    FurAffinity,
    Weasyl,
    E621,
}

#[derive(Clone, Debug, Serialize)]
struct Item {
    site: Site,
    id: i64,
    #[serde(with = "artist")]
    artists: Vec<String>,
    hash: Option<i64>,
    posted_at: Option<chrono::DateTime<chrono::Utc>>,
    updated_at: Option<chrono::DateTime<chrono::Utc>>,
    #[serde(with = "b64_vec")]
    sha256: Option<Vec<u8>>,
    deleted: bool,
    content_url: Option<String>,
}

mod artist {
    pub fn serialize<S>(artists: &Vec<String>, serializer: S) -> Result<S::Ok, S::Error>
    where
        S: serde::Serializer,
    {
        serializer.serialize_str(&artists.join(","))
    }
}

mod b64_vec {
    pub fn serialize<S>(bytes: &Option<Vec<u8>>, serializer: S) -> Result<S::Ok, S::Error>
    where
        S: serde::Serializer,
    {
        match bytes {
            Some(bytes) => serializer.serialize_str(&base64::encode(bytes)),
            None => serializer.serialize_none(),
        }
    }
}

#[derive(Parser)]
struct Config {
    #[clap(long, env)]
    database_url: String,
}

#[tokio::main]
async fn main() {
    let _ = dotenvy::dotenv();
    tracing_subscriber::fmt::init();

    let config = Config::parse();

    let pool = sqlx::postgres::PgPoolOptions::new()
        .max_connections(1)
        .connect(&config.database_url)
        .await
        .expect("must be able to connect to database");

    tracing::info!("creating export file");

    let file = tokio::fs::File::create("fuzzysearch-dump.csv")
        .await
        .expect("could not create output file");
    let mut dump = csv_async::AsyncSerializer::from_writer(file);

    let estimated_count = sqlx::query_file_scalar!("queries/estimate_rows.sql")
        .fetch_one(&pool)
        .await
        .expect("could not get estimated row count")
        .unwrap_or_default() as u64;

    let pb = indicatif::ProgressBar::new(estimated_count);
    pb.set_style(
        indicatif::ProgressStyle::default_bar()
            .template(
                "[{elapsed_precise}] {eta} remaining {wide_bar} {pos}/{len} ({per_sec}) {msg}",
            )
            .unwrap(),
    );

    pb.set_message("Exporting FurAffinity");

    let mut rows = sqlx::query_file!("queries/export_furaffinity.sql").fetch(&pool);

    while let Some(row) = rows.try_next().await.expect("could not get row") {
        if pb.position() + 1 > estimated_count {
            pb.set_length(pb.position() + 1);
        }

        dump.serialize(&Item {
            site: Site::FurAffinity,
            id: row.id as i64,
            artists: row.artist_name.map(|name| vec![name]).unwrap_or_default(),
            hash: row.hash_int,
            posted_at: row.posted_at,
            updated_at: row.updated_at,
            sha256: row.file_sha256,
            deleted: row.deleted,
            content_url: row.url,
        })
        .await
        .expect("could not write row");

        pb.inc(1);
    }

    pb.set_message("Exporting Weasyl");

    let mut rows = sqlx::query_file!("queries/export_weasyl.sql").fetch(&pool);

    while let Some(row) = rows.try_next().await.expect("could not get row") {
        if pb.position() + 1 > estimated_count {
            pb.set_length(pb.position() + 1);
        }

        let posted_at = row
            .posted_at
            .and_then(|posted_at| chrono::DateTime::parse_from_rfc3339(&posted_at).ok())
            .map(|posted_at| chrono::DateTime::<chrono::Utc>::from(posted_at));

        dump.serialize(&Item {
            site: Site::Weasyl,
            id: row.id as i64,
            artists: row.owner.map(|owner| vec![owner]).unwrap_or_default(),
            hash: row.hash,
            posted_at,
            updated_at: None,
            sha256: row.sha256,
            deleted: row.deleted,
            content_url: row.submission,
        })
        .await
        .expect("could not write row");

        pb.inc(1);
    }

    pb.set_message("Exporting e621");

    let mut rows = sqlx::query_file!("queries/export_e621.sql").fetch(&pool);

    while let Some(row) = rows.try_next().await.expect("could not get row") {
        if pb.position() + 1 > estimated_count {
            pb.set_length(pb.position() + 1);
        }

        let posted_at = row
            .created_at
            .and_then(|created_at| chrono::DateTime::parse_from_rfc3339(&created_at).ok())
            .map(|created_at| chrono::DateTime::<chrono::Utc>::from(created_at));

        dump.serialize(&Item {
            site: Site::E621,
            id: row.id as i64,
            artists: row.artists.unwrap_or_default(),
            hash: row.hash,
            posted_at,
            updated_at: None,
            sha256: row.sha256,
            deleted: row.deleted,
            content_url: row.content_url,
        })
        .await
        .expect("could not write row");

        pb.inc(1);
    }

    pb.abandon_with_message("Completed Export");

    tracing::info!("completed");
}
