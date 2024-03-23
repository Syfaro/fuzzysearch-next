use std::time::Duration;

use async_compression::tokio::write::GzipEncoder;
use clap::Parser;
use object_store::{aws::AmazonS3Builder, ObjectStore};
use serde::Serialize;
use serde_with::{base64::Base64, serde_as};
use tokio::io::AsyncWriteExt;
use tokio_stream::StreamExt;

#[serde_as]
#[derive(Clone, Debug, Serialize)]
struct Item<'a> {
    site: &'a str,
    id: &'a str,
    #[serde(with = "artist")]
    artists: &'a [String],
    hash: Option<i64>,
    posted_at: Option<chrono::DateTime<chrono::Utc>>,
    updated_at: Option<chrono::DateTime<chrono::Utc>>,
    #[serde_as(as = "Option<Base64>")]
    sha256: Option<[u8; 32]>,
    deleted: bool,
}

mod artist {
    pub fn serialize<S>(artists: &[String], serializer: S) -> Result<S::Ok, S::Error>
    where
        S: serde::Serializer,
    {
        serializer.serialize_str(&artists.join(","))
    }
}

#[derive(Parser)]
struct Config {
    /// Database URL to pull entries from and store dump information.
    #[clap(long, env)]
    database_url: String,
    /// Prefix to use for URLs of files uploaded to the store.
    #[clap(long, env)]
    file_prefix: String,
    /// Bucket for storing files.
    #[clap(long, env)]
    store_bucket: String,
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

    let object_store = AmazonS3Builder::from_env()
        .with_bucket_name(&config.store_bucket)
        .build()
        .expect("could not build object store");

    let object_path = format!(
        "fuzzysearch-dump-{}.csv.gz",
        chrono::Utc::now().format("%Y%m%d")
    )
    .try_into()
    .unwrap();

    let (_id, wtr) = object_store
        .put_multipart(&object_path)
        .await
        .expect("put multipart failed");
    let compressor = GzipEncoder::new(wtr);
    let mut dump = csv_async::AsyncSerializer::from_writer(compressor);

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
    pb.enable_steady_tick(Duration::from_millis(100));
    pb.set_message("Storing Submissions");

    let rows = sqlx::query_file!("queries/export_submission.sql").fetch(&pool);
    let mut rows = pb.wrap_stream(rows);

    while let Some(row) = rows.try_next().await.expect("could not get row") {
        let artists: Vec<_> = row
            .artists
            .map(|artists| artists.0)
            .unwrap_or_default()
            .into_iter()
            .map(|artist| artist.site_artist_id)
            .collect();
        let media = row.media.map(|media| media.0).unwrap_or_default();

        if media.is_empty() {
            dump.serialize(&Item {
                site: &row.site,
                id: &row.site_submission_id,
                artists: &artists,
                hash: None,
                posted_at: row.posted_at,
                updated_at: row.retrieved_at,
                sha256: None,
                deleted: row.deleted,
            })
            .await
            .expect("could not write empty row");
        } else {
            for entry in media {
                dump.serialize(&Item {
                    site: &row.site,
                    id: &row.site_submission_id,
                    artists: &artists,
                    hash: entry
                        .frames
                        .unwrap_or_default()
                        .first()
                        .and_then(|frame| frame.perceptual_gradient),
                    posted_at: row.posted_at,
                    updated_at: row.retrieved_at,
                    sha256: entry.file_sha256,
                    deleted: row.deleted,
                })
                .await
                .expect("could not write media row");
            }
        }
    }

    let mut compressor = dump.into_inner().await.unwrap();
    compressor.shutdown().await.unwrap();

    let mut wtr = compressor.into_inner();
    wtr.flush().await.unwrap();

    pb.abandon_with_message("Completed Dump");

    tracing::info!("inserting row");

    sqlx::query_file!(
        "queries/insert_dump.sql",
        format!("{}/{}", config.file_prefix, object_path)
    )
    .execute(&pool)
    .await
    .unwrap();

    let needing_delete = sqlx::query_file_scalar!("queries/old_dumps.sql")
        .fetch_all(&pool)
        .await
        .unwrap();

    for url in needing_delete {
        let path = url
            .split('/')
            .last()
            .expect("could not get path from url")
            .into();
        tracing::info!(%path, "deleting old dump");

        if let Err(err) = object_store.delete(&path).await {
            tracing::error!("could not delete object: {err}");
            continue;
        }

        sqlx::query_file!("queries/delete_dump.sql", url)
            .execute(&pool)
            .await
            .expect("could not delete dump from db");
    }

    tracing::info!("completed");
}
