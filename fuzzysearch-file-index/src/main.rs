use std::{collections::HashSet, path::PathBuf, time::Duration};

use clap::Parser;
use eyre::Result;
use futures::StreamExt;
use futures_batch::ChunksTimeoutStreamExt;
use image::GenericImageView;
use sha2::Digest;
use sha2::Sha256;
use sqlx::PgPool;
use tokio::io::AsyncReadExt;

#[derive(Parser)]
struct Config {
    /// PostgreSQL database URL.
    #[clap(short = 'd', long, env)]
    database_url: String,

    /// Directory to watch for file changes.
    #[clap(env, index = 1)]
    directory: String,
}

#[tokio::main]
async fn main() -> Result<()> {
    tracing_subscriber::fmt::init();

    tracing::info!("starting fuzzysearch-file-index");
    let config = Config::parse();

    tracing::debug!("connecting to database");
    let pool = PgPool::connect(&config.database_url).await?;

    tracing::trace!("running database migrations");
    sqlx::migrate!().run(&pool).await?;

    let (tx, rx) = tokio::sync::mpsc::channel(12);

    tokio::task::spawn_blocking(move || discover_files(&config.directory, tx));

    tokio_stream::wrappers::ReceiverStream::new(rx)
        .chunks_timeout(100, Duration::from_secs(10))
        .map(|chunk| tokio::task::spawn(process_chunk(pool.clone(), chunk)))
        .buffer_unordered(std::thread::available_parallelism()?.into())
        .for_each(|_| async { () })
        .await;

    Ok(())
}

/// Discover all files in directory and send entries for processing.
fn discover_files(dir: &str, tx: tokio::sync::mpsc::Sender<PathBuf>) {
    tracing::info!("starting to discover files");

    for entry in walkdir::WalkDir::new(dir)
        .sort_by_file_name()
        .into_iter()
        .filter_map(|e| e.ok())
    {
        tx.blocking_send(entry.into_path())
            .expect("could not send entry");
    }
}

#[derive(Debug)]
struct File {
    hash: Vec<u8>,
    size: i32,
    height: Option<i32>,
    width: Option<i32>,
    mime_type: Option<String>,
}

async fn process_chunk(pool: PgPool, paths: Vec<PathBuf>) -> eyre::Result<()> {
    let decoded_paths: Vec<_> = paths
        .into_iter()
        .filter_map(|path| {
            let last_segment = path.to_string_lossy().split('/').last()?.to_owned();
            let decoded = hex::decode(last_segment).ok()?;

            Some((path, decoded))
        })
        .collect();

    let mut files = Vec::with_capacity(decoded_paths.len());

    let discovered_hashes: Vec<_> = decoded_paths
        .iter()
        .map(|(_path, hash)| hash.clone())
        .collect();

    let existing_hashes: HashSet<Vec<u8>> =
        sqlx::query_file_scalar!("queries/existing_hashes.sql", &discovered_hashes)
            .fetch_all(&pool)
            .await?
            .into_iter()
            .collect();

    for (path, hash) in decoded_paths
        .into_iter()
        .filter(|(_path, hash)| !existing_hashes.contains(hash))
    {
        if !path.is_file() {
            continue;
        }

        if let Some(file) = evaluate_file(path).await {
            files.push(file);
        } else {
            tracing::warn!("could not evaluate file: {}", hex::encode(hash));
        }
    }

    let count = files.len();

    let mut tx = pool.begin().await?;

    for file in files {
        sqlx::query_file!(
            "queries/insert_hash.sql",
            file.hash,
            file.size,
            file.height,
            file.width,
            file.mime_type
        )
        .execute(&mut tx)
        .await?;
    }

    tx.commit().await?;

    tracing::info!(count, "added new files");

    Ok(())
}

async fn evaluate_file(path: PathBuf) -> Option<File> {
    tracing::trace!("evaluating {}", path.to_string_lossy());

    let mut file = tokio::fs::File::open(&path).await.ok()?;

    let mut contents = Vec::with_capacity(2_000_000);
    let mut _read = file.read_to_end(&mut contents).await.ok()?;

    let digest = Sha256::digest(&contents);
    let mime_type = infer::get(&contents).map(|inf| inf.mime_type().to_string());

    let dimensions = image::load_from_memory(&contents)
        .ok()
        .map(|im| im.dimensions());

    Some(File {
        hash: digest.to_vec(),
        size: contents.len() as i32,
        mime_type,
        height: dimensions.map(|dim| dim.0 as i32),
        width: dimensions.map(|dim| dim.1 as i32),
    })
}
