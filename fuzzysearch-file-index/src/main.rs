use std::{
    collections::{HashMap, HashSet},
    path::PathBuf,
    pin::Pin,
    time::Duration,
};

use clap::{Parser, Subcommand};
use eyre::Result;
use futures::StreamExt;
use futures_batch::ChunksTimeoutStreamExt;
use image::GenericImageView;
use lazy_static::lazy_static;
use prometheus::{register_counter, register_histogram, Counter, Histogram};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use sqlx::PgPool;
use tokio::io::{AsyncBufReadExt, AsyncRead, AsyncReadExt, AsyncWriteExt};

lazy_static! {
    static ref FILE_PROCESSING_TIME: Histogram = register_histogram!(
        "fuzzysearch_file_index_processing_seconds",
        "Time to process a file."
    )
    .unwrap();
    static ref FILE_DOWNLOAD_TIME: Histogram = register_histogram!(
        "fuzzysearch_file_index_download_seconds",
        "Time to download a file."
    )
    .unwrap();
    static ref FILE_DOWNLOAD_ERRORS: Counter = register_counter!(
        "fuzzysearch_file_index_download_error_count",
        "Number of errors when downloading a file."
    )
    .unwrap();
}

const METADATA_VERSION: i32 = 2;

#[derive(Debug, Parser)]
struct Config {
    /// PostgreSQL database URL.
    #[clap(short = 'd', long, env)]
    database_url: String,

    #[clap(flatten)]
    prometheus: PrometheusConfig,

    /// Which function to perform.
    #[clap(subcommand)]
    command: Command,
}

#[derive(Clone, Debug, Parser)]
struct PrometheusConfig {
    /// Prometheus Pushgateway host to use for pushing metrics.
    #[clap(long, env)]
    prometheus_host: Option<String>,

    /// Prometheus Pushgateway basic auth username, if needed.
    #[clap(long, env)]
    prometheus_username: Option<String>,
    /// Prometheus Pushgateway basic auth password, if needed.
    #[clap(long, env)]
    prometheus_password: Option<String>,
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

        /// Path to file containing updated hash for a submission.
        #[clap(short, long)]
        updates_file: Option<String>,
    },

    /// Download previously indexed submissions missing a sha256 hash.
    DownloadUnknown {
        /// Path to file containing IDs and URLs.
        #[clap(env, index = 1)]
        submissions: String,
        /// Directory to download files.
        #[clap(env, index = 2)]
        directory: PathBuf,
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
        /// Filter to only images on a given site.
        #[clap(long, value_enum)]
        site: Option<Site>,
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

    if config.prometheus.prometheus_host.is_some() {
        let config = config.prometheus.clone();

        std::thread::spawn(move || loop {
            push_metrics(&config);
            std::thread::sleep(std::time::Duration::from_secs(10));
        });
    }

    match config.command {
        Command::IndexDirectory { directory } => {
            tracing::info!("indexing directory: {}", directory);

            let (tx, rx) = tokio::sync::mpsc::channel(available_parallelism);
            tokio::task::spawn_blocking(move || discover_files(&directory, tx));
            process_files(pool, rx, available_parallelism).await;
        }

        Command::ImportChangedFiles { changes, directory } => {
            tracing::info!("indexing changed files from {} in {}", changes, directory);

            type ChangeSource = Pin<Box<dyn AsyncRead + Send>>;
            let change_source = if changes == "-" {
                Box::pin(tokio::io::stdin()) as ChangeSource
            } else {
                Box::pin(tokio::fs::File::open(changes).await?) as ChangeSource
            };

            let (tx, rx) = tokio::sync::mpsc::channel(available_parallelism);

            tokio::task::spawn(async move {
                let mut lines = tokio::io::BufReader::new(change_source).lines();

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

            process_files(pool, rx, available_parallelism).await;
        }

        Command::ImportAssociations { dump, updates_file } => {
            tracing::info!("importing associations: {}", dump);

            let (tx, rx) = tokio::sync::mpsc::channel(available_parallelism);

            tokio::task::spawn_blocking(move || {
                read_dump(PathBuf::from(dump), updates_file.map(PathBuf::from), tx)
            });

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
        Command::DownloadUnknown {
            submissions,
            directory,
        } => {
            tracing::info!("downloading unknown submissions");

            let (tx, rx) = tokio::sync::mpsc::channel(available_parallelism);

            tokio::task::spawn_blocking(move || {
                let file = std::fs::File::open(submissions).unwrap();
                let mut rdr = csv::Reader::from_reader(file);
                rdr.deserialize::<FileToDownload>()
                    .for_each(|file| tx.blocking_send(file).unwrap());
            });

            let (tx, file_paths_rx) = tokio::sync::mpsc::channel(available_parallelism);

            let pool_clone = pool.clone();
            tokio::task::spawn(async move {
                let client = reqwest::ClientBuilder::default().build().unwrap();
                tokio_stream::wrappers::ReceiverStream::new(rx)
                    .for_each_concurrent(available_parallelism, |file| async {
                        let file = match file {
                            Ok(file) => file,
                            Err(err) => {
                                tracing::error!("could not parse file: {err}");
                                return;
                            }
                        };

                        let file_id = file.id;
                        if let Err(err) = download_file(
                            pool_clone.clone(),
                            client.clone(),
                            tx.clone(),
                            directory.clone(),
                            file,
                        )
                        .await
                        {
                            tracing::error!(file_id, "could not download file: {err}");
                        }
                    })
                    .await;
            });

            process_files(pool.clone(), file_paths_rx, available_parallelism).await;

            sqlx::query_file!("queries/create_assocations_from_downloads.sql")
                .execute(&pool)
                .await
                .unwrap();
        }
        Command::Explore {
            exploration_command:
                ExplorationCommand::DimensionsImage {
                    height,
                    width,
                    site,
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

            if let Some(site) = site {
                let mut rows = sqlx::query_file_as!(
                    RowDimension,
                    "queries/file_dimensions_site.sql",
                    site.id()
                )
                .fetch(&pool);
                while let Some(Ok(row)) = rows.next().await {
                    *pixel_counts.entry(row.into()).or_default() += 1;
                }
            } else {
                let mut rows =
                    sqlx::query_file_as!(RowDimension, "queries/file_dimensions.sql").fetch(&pool);
                while let Some(Ok(row)) = rows.next().await {
                    *pixel_counts.entry(row.into()).or_default() += 1;
                }
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

    push_metrics(&config.prometheus);

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
    exif_entries: Option<Vec<ExifEntry>>,
}

#[derive(Debug, Serialize)]
struct DbFile {
    hash: String,
    size: i32,
    height: Option<i32>,
    width: Option<i32>,
    mime_type: Option<String>,
    metadata_version: i32,
    exif_entries: Option<serde_json::Value>,
}

async fn process_chunk(pool: PgPool, paths: Vec<PathBuf>) -> eyre::Result<Vec<File>> {
    let decoded_paths: Vec<_> = paths
        .into_iter()
        .filter_map(|path| {
            let path_str = path.to_string_lossy();
            let last = path_str.split('/').last()?;

            let decoded = hex::decode(last).ok()?;

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

    let existing_hashes: HashSet<Vec<u8>> = sqlx::query_file_scalar!(
        "queries/existing_hashes.sql",
        METADATA_VERSION,
        &discovered_hashes
    )
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

    let count = files.len();
    if count == 0 {
        tracing::trace!("no new files discovered");
    } else {
        tracing::debug!(count, "discovered new files");
    }

    Ok(files)
}

async fn process_files(
    pool: PgPool,
    rx: tokio::sync::mpsc::Receiver<PathBuf>,
    available_parallelism: usize,
) {
    tokio_stream::wrappers::ReceiverStream::new(rx)
        .chunks_timeout(100, Duration::from_secs(10))
        .map(|chunk| tokio::task::spawn(process_chunk(pool.clone(), chunk)))
        .buffer_unordered(available_parallelism)
        .filter_map(|chunk| async {
            match chunk {
                Err(err) => {
                    tracing::error!("spawn error: {}", err);
                    None
                }
                Ok(Err(err)) => {
                    tracing::error!("chunk error: {}", err);
                    None
                }
                Ok(Ok(files)) => Some(futures::stream::iter(files)),
            }
        })
        .flatten()
        .chunks_timeout(100, Duration::from_secs(10))
        .for_each(|files| async {
            let count = files.len();

            tracing::trace!(
                "inserting new files: {:?}",
                files
                    .iter()
                    .map(|file| hex::encode(&file.hash))
                    .collect::<Vec<_>>()
            );

            // Somehow, duplicate values can make it through. Collect to a
            // HashMap and then back to a Vec to ensure uniqueness.
            #[allow(clippy::iter_kv_map)]
            let values = files
                .into_iter()
                .map(|file| {
                    let hex_str = hex::encode(&file.hash);

                    (
                        file.hash,
                        DbFile {
                            hash: hex_str,
                            size: file.size,
                            height: file.height,
                            width: file.width,
                            mime_type: file.mime_type,
                            metadata_version: METADATA_VERSION,
                            exif_entries: file
                                .exif_entries
                                .and_then(|entries| serde_json::to_value(entries).ok()),
                        },
                    )
                })
                .collect::<HashMap<Vec<u8>, DbFile>>()
                .into_iter()
                .map(|(_hash, file)| file)
                .collect::<Vec<_>>();

            let value = serde_json::to_value(values).unwrap();

            let rows_affected = sqlx::query_file!("queries/insert_hash.sql", value)
                .execute(&pool)
                .await
                .unwrap()
                .rows_affected();

            tracing::info!(count, rows_affected, "inserted new files");
        })
        .await;
}

#[derive(Debug, Serialize)]
struct ExifEntry {
    tag_number: u16,
    tag_description: Option<String>,
    display_value: String,
}

async fn evaluate_file(path: PathBuf) -> Result<File> {
    tracing::trace!("evaluating {}", path.to_string_lossy());

    let _timer = FILE_PROCESSING_TIME.start_timer();

    let mut file = tokio::fs::File::open(&path).await?;

    let mut contents = Vec::with_capacity(2_000_000);
    let read = file.read_to_end(&mut contents).await?;

    let digest = Sha256::digest(&contents);
    let mime_type = infer::get(&contents).map(|inf| inf.mime_type().to_string());

    let (width, height) = std::panic::catch_unwind(|| {
        image::load_from_memory(&contents)
            .ok()
            .map(|im| im.dimensions())
            .map(|dim| (Some(dim.0 as i32), Some(dim.1 as i32)))
            .unwrap_or_default()
    })
    .map_err(|_err| eyre::eyre!("decoding image panicked"))?;

    let exif_entries: Option<Vec<_>> = tokio::task::spawn_blocking(move || {
        let mut file = std::io::BufReader::new(std::fs::File::open(path).unwrap());

        exif::Reader::new()
            .read_from_container(&mut file)
            .map(|exif| {
                exif.fields()
                    .map(|field| ExifEntry {
                        tag_number: field.tag.number(),
                        tag_description: field.tag.description().map(ToString::to_string),
                        display_value: field.display_value().to_string(),
                    })
                    .collect()
            })
            .ok()
    })
    .await?;

    Ok(File {
        hash: digest.to_vec(),
        size: read as i32,
        mime_type,
        height,
        width,
        exif_entries,
    })
}

#[derive(Deserialize)]
struct UpdateFileRow {
    site: Site,
    id: i64,
    #[serde(with = "b64_vec")]
    sha256: Option<Vec<u8>>,
}

fn read_dump(path: PathBuf, updates: Option<PathBuf>, tx: tokio::sync::mpsc::Sender<Item>) {
    let update_lookup = if let Some(updates) = updates {
        let updates = std::fs::File::open(updates).unwrap();
        let mut rdr = csv::Reader::from_reader(updates);

        rdr.deserialize::<UpdateFileRow>()
            .filter_map(|file| file.ok())
            .filter_map(|row| row.sha256.map(|hash| ((row.site, row.id), hash)))
            .collect()
    } else {
        HashMap::new()
    };

    let f = std::fs::File::open(path).unwrap();

    let mut rdr = csv::Reader::from_reader(f);
    let files = rdr.deserialize::<Item>();

    for file in files.filter_map(|f| f.ok()) {
        if let Some(hash) = update_lookup.get(&(file.site, file.id)) {
            tracing::trace!(site = %file.site, id = file.id, "file had updated hash");
            tx.blocking_send(Item {
                sha256: Some(hash.to_owned()),
                ..file
            })
            .expect("could not send updated item");
        }

        tx.blocking_send(file).expect("could not send item");
    }
}

#[derive(Clone, Copy, Debug, Deserialize, Hash, PartialEq, Eq, clap::ValueEnum)]
#[serde(rename_all = "lowercase")]
#[clap(rename_all = "lowercase")]
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

impl std::fmt::Display for Site {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::FurAffinity => write!(f, "FurAffinity"),
            Self::Weasyl => write!(f, "Weasyl"),
            Self::E621 => write!(f, "e621"),
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
    use base64::Engine;
    use serde::{Deserialize, Deserializer};

    pub fn deserialize<'de, D>(deserializer: D) -> Result<Option<Vec<u8>>, D::Error>
    where
        D: Deserializer<'de>,
    {
        let b64: Option<String> = Deserialize::deserialize(deserializer)?;

        b64.map(|b64| {
            base64::engine::general_purpose::STANDARD
                .decode(b64)
                .map_err(serde::de::Error::custom)
        })
        .transpose()
    }
}

struct RowDimension {
    height: i32,
    width: i32,
}

#[derive(Debug, Serialize, Deserialize, PartialEq, Eq, Hash)]
struct Dimension {
    height: u32,
    width: u32,
}

impl From<RowDimension> for Dimension {
    fn from(row: RowDimension) -> Self {
        Dimension {
            height: row.height as u32,
            width: row.width as u32,
        }
    }
}

#[derive(Debug, Deserialize)]
struct FileToDownload {
    id: i32,
    posted_at: Option<chrono::DateTime<chrono::Utc>>,
    url: String,
}

#[tracing::instrument(skip(pool, client, tx, directory, file), fields(submission_id = file.id))]
async fn download_file(
    pool: PgPool,
    client: reqwest::Client,
    tx: tokio::sync::mpsc::Sender<PathBuf>,
    directory: PathBuf,
    file: FileToDownload,
) -> eyre::Result<()> {
    let timer = FILE_DOWNLOAD_TIME.start_timer();

    if sqlx::query_file_scalar!("queries/has_attempted_download.sql", 1, file.id)
        .fetch_optional(&pool)
        .await?
        .is_some()
    {
        tracing::debug!("file already in history");
        timer.stop_and_discard();
        return Ok(());
    }

    let url = file
        .url
        .replace("d.facdn.net", "d.furaffinity.net")
        .replace("d2.facdn.net", "d.furaffinity.net");

    let data = match client.get(&url).send().await {
        Ok(data) if data.status().as_u16() == 200 => data,
        Ok(data) => {
            tracing::warn!("got bad status code: {}", data.status());
            FILE_DOWNLOAD_ERRORS.inc();
            sqlx::query_file!("queries/mark_download_unsuccessful.sql", 1, file.id, url)
                .execute(&pool)
                .await?;
            return Ok(());
        }
        Err(err) => {
            tracing::error!("got network error: {err}");
            FILE_DOWNLOAD_ERRORS.inc();
            sqlx::query_file!("queries/mark_download_unsuccessful.sql", 1, file.id, url)
                .execute(&pool)
                .await?;
            return Ok(());
        }
    };

    let body = match data.bytes().await {
        Ok(body) => body,
        Err(err) => {
            tracing::error!("could not ready body: {err}");
            FILE_DOWNLOAD_ERRORS.inc();
            sqlx::query_file!("queries/mark_download_unsuccessful.sql", 1, file.id, url)
                .execute(&pool)
                .await?;
            return Ok(());
        }
    };

    let hash = Sha256::digest(&body);
    let hash = hex::encode(hash);

    let prefix = directory.join(&hash[0..2]).join(&hash[2..4]);
    tokio::fs::create_dir_all(&prefix).await?;
    let path = prefix.join(&hash);
    if path.exists() {
        tracing::debug!("file already existed");
    } else {
        tracing::debug!("was new file");
        let mut f = tokio::fs::File::create(&path).await?;
        f.write_all(&body).await?;
    }

    sqlx::query_file!(
        "queries/insert_download_history.sql",
        1,
        file.id,
        file.posted_at,
        url,
        hash
    )
    .execute(&pool)
    .await?;
    tx.send(path).await?;

    Ok(())
}

#[cfg(feature = "push-metrics")]
fn push_metrics(config: &PrometheusConfig) {
    let host = match &config.prometheus_host {
        Some(host) => host,
        _ => return,
    };

    tracing::debug!("pushing metrics");

    let auth = match (
        config.prometheus_username.clone(),
        config.prometheus_password.clone(),
    ) {
        (Some(username), Some(password)) => {
            Some(prometheus::BasicAuthentication { username, password })
        }
        _ => None,
    };

    let metrics = prometheus::gather();

    prometheus::push_metrics("fuzzysearch_file_index", labels! {}, host, metrics, auth).unwrap();
}

#[cfg(not(feature = "push-metrics"))]
fn push_metrics(_config: &PrometheusConfig) {}
