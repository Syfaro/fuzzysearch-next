use std::{
    collections::{HashMap, HashSet},
    path::PathBuf,
    time::Duration,
};

use clap::{Parser, Subcommand};
use eyre::Result;
use futures::StreamExt;
use futures_batch::ChunksTimeoutStreamExt;
use image::GenericImageView;
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use sqlx::PgPool;
use tokio::io::{AsyncBufReadExt, AsyncReadExt};

#[derive(Debug, Parser)]
struct Config {
    /// PostgreSQL database URL.
    #[clap(short = 'd', long, env)]
    database_url: String,
    /// Which function to perform.
    #[clap(subcommand)]
    command: Command,
}

#[derive(Debug, Subcommand)]
enum Command {
    /// Index a directory by scanning for all unknown files.
    IndexDirectory {
        /// Directory to watch for file changes.
        #[clap(env, index = 1)]
        directory: String,
    },

    /// Import changed files, as determined by rsync log.
    ImportChangedFiles {
        /// FuzzySearch dump file path.
        #[clap(env, index = 1)]
        changes: String,
        /// Directory to where files were downloaded.
        #[clap(env, index = 2)]
        directory: String,
    },

    /// Import known file associations from a FuzzySearch dump.
    ImportAssociations {
        /// FuzzySearch dump file path.
        #[clap(env, index = 1)]
        dump: String,
    },

    /// Explore data.
    Explore {
        #[clap(subcommand)]
        exploration_command: ExplorationCommand,
    },
}

#[derive(Debug, Subcommand)]
enum ExplorationCommand {
    /// Generate an image demonstrating submission dimensions.
    DimensionsImage {
        /// Height of generated image.
        #[clap(long, env, default_value = "2000")]
        height: u32,
        /// Width of generated image.
        #[clap(long, env, default_value = "2000")]
        width: u32,
        /// If image should use colors instead of being black and white.
        #[clap(short = 'c', long, env)]
        use_color: bool,
        /// Amount to scale colors. Each pixel's color will be exactly
        /// `count * color_scale`.
        #[clap(long, env, default_value = "10")]
        color_scale: usize,
        /// Name of file where image is saved.
        #[clap(index = 1, env, default_value = "submission-dimensions.png")]
        output: String,
    },
}

#[tokio::main]
async fn main() -> Result<()> {
    tracing_subscriber::fmt::init();

    tracing::info!("starting fuzzysearch-file-index");
    let config = Config::parse();
    tracing::trace!("{:#?}", config);

    tracing::debug!("connecting to database");
    let pool = PgPool::connect(&config.database_url).await?;

    tracing::trace!("running database migrations");
    sqlx::migrate!().run(&pool).await?;

    let available_parallelism: usize = std::thread::available_parallelism()?.into();

    match config.command {
        Command::IndexDirectory { directory } => {
            tracing::info!("indexing directory: {}", directory);

            let (tx, rx) = tokio::sync::mpsc::channel(available_parallelism);

            tokio::task::spawn_blocking(move || discover_files(&directory, tx));

            tokio_stream::wrappers::ReceiverStream::new(rx)
                .chunks_timeout(100, Duration::from_secs(10))
                .map(|chunk| tokio::task::spawn(process_chunk(pool.clone(), chunk)))
                .buffer_unordered(available_parallelism)
                .for_each(|_| async {})
                .await;
        }

        Command::ImportChangedFiles { changes, directory } => {
            tracing::info!("indexing changed files from {} in {}", changes, directory);

            let file = tokio::fs::File::open(changes).await?;

            let (tx, rx) = tokio::sync::mpsc::channel(available_parallelism);

            tokio::task::spawn(async move {
                let mut lines = tokio::io::BufReader::new(file).lines();

                while let Ok(Some(line)) = lines.next_line().await {
                    if !line.starts_with('>') {
                        continue;
                    }

                    let (_prefix, file) = match line.split_once(' ') {
                        Some(parts) => parts,
                        None => continue,
                    };

                    tracing::trace!("found modified file: {}", file);

                    let mut path = PathBuf::from(&directory);
                    path.push(file);

                    tx.send(path).await.unwrap();
                }
            });

            tokio_stream::wrappers::ReceiverStream::new(rx)
                .chunks_timeout(100, Duration::from_secs(10))
                .map(|chunk| tokio::task::spawn(process_chunk(pool.clone(), chunk)))
                .buffer_unordered(available_parallelism)
                .for_each(|_| async {})
                .await;
        }

        Command::ImportAssociations { dump } => {
            tracing::info!("importing associations: {}", dump);

            let (tx, rx) = tokio::sync::mpsc::channel(available_parallelism);

            tokio::task::spawn_blocking(move || read_dump(PathBuf::from(dump), tx));

            tokio_stream::wrappers::ReceiverStream::new(rx)
                .chunks_timeout(1_000, Duration::from_secs(10))
                .for_each(move |items| {
                    let pool = pool.clone();

                    let items: Vec<DbItem> = items
                        .into_iter()
                        .filter_map(|item| item.try_into().ok())
                        .collect();

                    let count = items.len();

                    async move {
                        let rows_affected = sqlx::query_file!(
                            "queries/insert_dump.sql",
                            serde_json::to_value(items).unwrap()
                        )
                        .execute(&pool)
                        .await
                        .unwrap()
                        .rows_affected();

                        tracing::debug!(count, rows_affected, "added associations");
                    }
                })
                .await;
        }

        Command::Explore {
            exploration_command:
                ExplorationCommand::DimensionsImage {
                    height,
                    width,
                    use_color,
                    color_scale,
                    output,
                },
        } => {
            type ImageDepth = u16;

            let mut im = image::ImageBuffer::from_pixel(
                width,
                height,
                image::Rgb([ImageDepth::MAX, ImageDepth::MAX, ImageDepth::MAX]),
            );

            let mut pixel_counts: HashMap<Dimension, usize> = HashMap::new();

            let mut rows = sqlx::query_file!("queries/file_dimensions.sql").fetch(&pool);
            while let Some(Ok(row)) = rows.next().await {
                let dimension = match (row.height, row.width) {
                    (Some(height), Some(width)) => Dimension {
                        height: height as u32,
                        width: width as u32,
                    },
                    _ => continue,
                };

                *pixel_counts.entry(dimension).or_default() += 1;
            }

            pixel_counts.into_iter().for_each(|(dimension, count)| {
                if dimension.height > height || dimension.width > width {
                    return;
                }

                let color = if use_color {
                    (count * color_scale).clamp(0, ImageDepth::MAX.try_into().unwrap())
                        as ImageDepth
                } else {
                    0
                };

                let (x, y) = (dimension.width - 1, dimension.height - 1);
                im.put_pixel(x, y, image::Rgb([0, color, 0]));
            });

            im.save(output).unwrap();
        }
    }

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

            // all valid files are sha256 hashes
            if decoded.len() != 32 {
                return None;
            }

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
        match evaluate_file(path).await {
            Ok(file) => files.push(file),
            Err(err) => tracing::warn!("could not evaluate file {}: {}", hex::encode(hash), err),
        }
    }

    if files.is_empty() {
        tracing::debug!("no new files were found");
        return Ok(());
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

async fn evaluate_file(path: PathBuf) -> Result<File> {
    tracing::trace!("evaluating {}", path.to_string_lossy());

    let mut file = tokio::fs::File::open(&path).await?;

    let mut contents = Vec::with_capacity(2_000_000);
    let mut _read = file.read_to_end(&mut contents).await?;

    let digest = Sha256::digest(&contents);
    let mime_type = infer::get(&contents).map(|inf| inf.mime_type().to_string());

    let (width, height) = image::load_from_memory(&contents)
        .ok()
        .map(|im| im.dimensions())
        .map(|dim| (Some(dim.0 as i32), Some(dim.1 as i32)))
        .unwrap_or_default();

    Ok(File {
        hash: digest.to_vec(),
        size: contents.len() as i32,
        mime_type,
        height,
        width,
    })
}

fn read_dump(path: PathBuf, tx: tokio::sync::mpsc::Sender<Item>) {
    let f = std::fs::File::open(path).unwrap();

    let mut rdr = csv::Reader::from_reader(f);
    let files = rdr.deserialize::<Item>();

    for file in files.filter_map(|f| f.ok()) {
        tx.blocking_send(file).expect("could not send item");
    }
}

#[derive(Clone, Debug, Deserialize)]
#[serde(rename_all = "lowercase")]
enum Site {
    FurAffinity,
    Weasyl,
    E621,
}

impl Site {
    fn id(&self) -> i32 {
        match self {
            Self::FurAffinity => 1,
            Self::E621 => 2,
            Self::Weasyl => 4,
        }
    }
}

#[derive(Clone, Debug, Deserialize)]
struct Item {
    site: Site,
    id: i64,
    posted_at: Option<chrono::DateTime<chrono::Utc>>,
    #[serde(with = "b64_vec")]
    sha256: Option<Vec<u8>>,
}

#[derive(Debug, Serialize)]
struct DbItem {
    hash: String,
    site_id: i32,
    submission_id: String,
    posted_at: Option<chrono::DateTime<chrono::Utc>>,
}

impl TryFrom<Item> for DbItem {
    type Error = eyre::Report;

    fn try_from(item: Item) -> Result<Self, eyre::Report> {
        Ok(Self {
            hash: hex::encode(item.sha256.ok_or_else(|| eyre::eyre!("missing hash"))?),
            site_id: item.site.id(),
            submission_id: item.id.to_string(),
            posted_at: item.posted_at,
        })
    }
}

mod b64_vec {
    use serde::{Deserialize, Deserializer};

    pub fn deserialize<'de, D>(deserializer: D) -> Result<Option<Vec<u8>>, D::Error>
    where
        D: Deserializer<'de>,
    {
        let b64: Option<String> = Deserialize::deserialize(deserializer)?;

        b64.map(|b64| base64::decode(b64).map_err(serde::de::Error::custom))
            .transpose()
    }
}

#[derive(Debug, Serialize, Deserialize, PartialEq, Eq, Hash)]
struct Dimension {
    height: u32,
    width: u32,
}
