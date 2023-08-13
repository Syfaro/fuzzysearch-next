use std::{collections::HashMap, io::Seek, path::PathBuf, sync::Arc};

use async_trait::async_trait;
use eyre::eyre;
use foxlib::hash::image::AnimationDecoder;
use futures::{stream::FuturesUnordered, TryStreamExt};
use fuzzysearch_common::{Media, MediaFrame, Site, Submission};
use lazy_static::lazy_static;
use prometheus::{
    register_histogram_vec, register_int_counter_vec, HistogramOpts, HistogramVec, IntCounterVec,
    Opts,
};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use sqlx::{PgPool, Postgres, Transaction};
use tap::{TapFallible, TapOptional};
use tokio::io::{AsyncSeekExt, AsyncWriteExt};
use tokio_stream::StreamExt;
use uuid::Uuid;

use crate::{
    requests::FetchReason,
    sites::{e621::E621, furaffinity::FurAffinity, weasyl::Weasyl},
    Config,
};

mod e621;
mod furaffinity;
mod weasyl;

lazy_static! {
    static ref FETCH_ERRORS: IntCounterVec = register_int_counter_vec!(
        Opts::new(
            "fuzzysearch_watcher_fetch_errors",
            "Number of errors fetching submissions."
        ),
        &["site"]
    )
    .unwrap();
    static ref FETCHED_SUBMISSIONS: HistogramVec = register_histogram_vec!(
        HistogramOpts::new(
            "fuzzysearch_watcher_fetched_submissions",
            "Number of submissions fetched on each site."
        ),
        &["site"]
    )
    .unwrap();
}

#[derive(Clone, Debug, Deserialize, Serialize)]
pub enum SubmissionResult {
    Fetched(Submission),
    Error {
        site: Site,
        submission_id: String,
        message: Option<String>,
    },
}

pub type BoxSite = Box<Arc<dyn LoadableSite + Send + Sync>>;

/// All of the methods required to load a submission from a site.
#[async_trait]
pub trait LoadableSite {
    /// The site it's loading from.
    fn site(&self) -> Site;

    /// Load submissions with the given IDs.
    ///
    /// Only return errors for system failures, use an errored submission result
    /// if a submission can't be loaded.
    async fn load_multiple(&self, ids: Vec<&str>) -> eyre::Result<Vec<SubmissionResult>> {
        futures::stream::iter(ids)
            .then(|id| self.load(id))
            .try_collect()
            .await
    }

    /// Load a single submission.
    async fn load(&self, id: &str) -> eyre::Result<SubmissionResult>;
}

/// Get all of the sites.
pub async fn sites(
    config: &Config,
    client: reqwest::Client,
    pool: PgPool,
    nats: async_nats::Client,
) -> Vec<BoxSite> {
    let mut sites: Vec<BoxSite> = Vec::with_capacity(3);

    if let (Some(cookie_a), Some(cookie_b)) = (
        config.furaffinity_cookie_a.as_ref(),
        config.furaffinity_cookie_b.as_ref(),
    ) {
        tracing::info!("adding furaffinity");
        let fa = FurAffinity::new(
            config.site(),
            cookie_a.clone(),
            cookie_b.clone(),
            config.furaffinity_bot_threshold,
            client.clone(),
            pool.clone(),
            nats,
        )
        .await;
        sites.push(Box::new(fa))
    }

    if let Some(weasyl_api_token) = config.weasyl_api_token.as_ref() {
        tracing::info!("adding weasyl");
        let weasyl = Weasyl::new(
            config.site(),
            weasyl_api_token.clone(),
            client.clone(),
            pool.clone(),
        );
        sites.push(Box::new(weasyl));
    }

    if let (Some(e621_login), Some(e621_api_token)) =
        (config.e621_login.as_ref(), config.e621_api_token.as_ref())
    {
        tracing::info!("adding e621");
        let e621 = E621::new(
            config.site(),
            e621_login.clone(),
            e621_api_token.clone(),
            client.clone(),
            pool.clone(),
        );
        sites.push(Box::new(e621));
    }

    sites
}

#[async_trait]
pub trait LoadSubmissions {
    async fn load_submissions(
        &self,
        submission_ids: &[(Site, String)],
    ) -> eyre::Result<Vec<SubmissionResult>>;
}

#[async_trait]
impl LoadSubmissions for &[BoxSite] {
    async fn load_submissions(
        &self,
        submission_ids: &[(Site, String)],
    ) -> eyre::Result<Vec<SubmissionResult>> {
        tracing::info!(len = submission_ids.len(), "wanting to load submissions");

        let mut per_site_batches: HashMap<Site, Vec<&str>> =
            HashMap::with_capacity(submission_ids.len());

        for (site, submission_id) in submission_ids {
            per_site_batches
                .entry(*site)
                .or_default()
                .push(submission_id);
        }

        tracing::info!(
            len = per_site_batches.len(),
            "found sites needed to load submissions"
        );

        let site_loads: FuturesUnordered<_> = per_site_batches
            .into_iter()
            .flat_map(|(site, ids)| {
                self.iter()
                    .find(|loadable_site| loadable_site.site() == site)
                    .tap_some(|site| tracing::trace!(site = %site.site(), "matched site"))
                    .map(|loadable_site| (loadable_site, ids))
            })
            .map(|(loadable_site, ids)| loadable_site.load_multiple(ids))
            .collect();

        let submissions: Vec<Vec<SubmissionResult>> = site_loads.try_collect().await?;
        let submissions: Vec<SubmissionResult> = submissions.into_iter().flatten().collect();

        tracing::info!(len = submissions.len(), "loaded submissions");

        Ok(submissions)
    }
}

#[tracing::instrument(skip(pool, download_path, client, site_id))]
pub async fn process_file(
    pool: &PgPool,
    download_path: &Option<PathBuf>,
    client: &reqwest::Client,
    site_id: Option<String>,
    url: &str,
) -> eyre::Result<Media> {
    tracing::info!("attempting to download image");

    const MAX_DOWNLOAD_SIZE: usize = 2_000_000_000;

    let mut req = client.get(url).send().await?;

    let named_file = tempfile::NamedTempFile::new()?;
    let (file, path) = named_file.into_parts();
    tracing::trace!("got temp path {}", path.to_string_lossy());

    let mut file = tokio::fs::File::from_std(file);
    let mut file_size = 0;

    let mut sha256 = Sha256::new();
    let mut head = bytes::BytesMut::with_capacity(8192);

    while let Ok(Some(chunk)) = req.chunk().await {
        let new_len = chunk.len() + file_size;
        if new_len > MAX_DOWNLOAD_SIZE {
            tracing::warn!("file was at least {new_len} bytes");
            eyre::bail!("file was greater than max download size");
        }

        let needed_head_bytes = 8192 - head.len();
        if !chunk.is_empty() && needed_head_bytes > 0 {
            head.extend(&chunk[..needed_head_bytes.clamp(1, chunk.len())]);
        }

        file_size += chunk.len();
        sha256.update(&chunk);
        file.write_all(&chunk).await?;
    }

    tracing::debug!("file was {file_size} bytes");

    let sha256 = sha256.finalize();
    tracing::debug!("sha256 hash: {}", hex::encode(sha256));

    let mime_type = infer::get(&head)
        .map(|typ| typ.mime_type().to_string())
        .tap_none(|| tracing::warn!("could not guess mime type"))
        .tap_some(|mime_type| tracing::debug!(mime_type, "got mime type"));

    if let Ok(Some(media)) =
        sqlx::query_file!("queries/media_lookup_sha256_full.sql", sha256.to_vec())
            .fetch_optional(pool)
            .await
    {
        tracing::info!("already had media for this hash, skipping processing");

        return Ok(Media {
            site_id,
            deleted: false,
            url: Some(url.to_string()),
            file_sha256: Some(sha256.to_vec()),
            file_size: Some(file_size as i64),
            mime_type,
            frames: media
                .perceptual_gradient
                .map(|hash| MediaFrame {
                    frame_index: 0,
                    perceptual_gradient: Some(hash.to_be_bytes()),
                })
                .into_iter()
                .collect(),
            extra: None,
        });
    }

    if let Some(download_path) = download_path {
        let sha256_hex = hex::encode(sha256);
        let dir = download_path
            .join(&sha256_hex[0..2])
            .join(&sha256_hex[2..4]);
        if !dir.exists() {
            let _ = tokio::fs::create_dir_all(&dir).await;
        }
        let path = dir.join(sha256_hex);
        tracing::debug!("writing image to {}", path.to_string_lossy());

        match tokio::fs::File::create(path).await {
            Ok(mut download) => {
                file.rewind().await?;
                if let Err(err) = tokio::io::copy(&mut file, &mut download).await {
                    tracing::error!("could not write file: {err}");
                }
            }
            Err(err) => tracing::error!("could not create file: {err}"),
        }
    }

    file.rewind().await?;
    let mut file = file.into_std().await;

    let hashing_span = tracing::info_span!("load_and_hash_image");
    let frames = tokio::task::spawn_blocking(move || {
        let _entered = hashing_span.enter();
        let hasher = foxlib::hash::ImageHasher::default();

        if infer::is_video(&head) {
            tracing::debug!("mime type suggests video");
            match decode_video(&path) {
                Ok(frames) => return Some(frames),
                Err(err) => tracing::warn!("could not be decoded as video: {err}"),
            }
        }

        if infer::is(&head, "gif") {
            tracing::debug!("mime type suggests gif");
            if let Ok(gif) = foxlib::hash::image::codecs::gif::GifDecoder::new(&file) {
                let hashes: Vec<_> = gif
                    .into_frames()
                    .filter_map(Result::ok)
                    .map(|frame| hasher.hash_image(frame.buffer()))
                    .collect();
                tracing::info!(len = hashes.len(), "extracted frames from gif");

                return Some(hashes);
            }

            tracing::warn!("could not be decoded as gif");
            file.rewind()
                .tap_err(|err| tracing::error!("could not rewind image: {err}"))
                .ok()?;
        }

        let file = std::io::BufReader::new(file);
        let im = foxlib::hash::image::io::Reader::new(file)
            .with_guessed_format()
            .tap_err(|err| tracing::error!("could not guess format: {err}"))
            .ok()?
            .decode()
            .tap_err(|err| tracing::error!("could not decode image: {err}"))
            .ok()?;
        tracing::info!("loaded static image");

        if let Err(err) = path.close() {
            tracing::error!("could not close path: {err}");
        }

        Some(vec![hasher.hash_image(&im)])
    })
    .await?;

    let frames: Vec<_> = frames
        .unwrap_or_default()
        .into_iter()
        .enumerate()
        .map(|(index, frame)| MediaFrame {
            frame_index: index as i64,
            perceptual_gradient: Some(frame.0),
        })
        .collect();

    Ok(Media {
        site_id,
        deleted: false,
        url: Some(url.to_string()),
        file_sha256: Some(sha256.to_vec()),
        file_size: Some(file_size as i64),
        mime_type,
        frames,
        extra: None,
    })
}

fn decode_video(path: &tempfile::TempPath) -> eyre::Result<Vec<foxlib::hash::ImageHash>> {
    use ffmpeg_next::{
        codec, decoder,
        format::{self, input},
        media,
        software::scaling,
        util::frame,
    };
    use foxlib::hash::{self, image};

    let mut ictx = input(&path)?;
    let input = ictx
        .streams()
        .best(media::Type::Video)
        .ok_or(ffmpeg_next::Error::StreamNotFound)?;
    let video_stream_index = input.index();

    let context_decoder = codec::Context::from_parameters(input.parameters())?;
    let mut decoder = context_decoder.decoder().video()?;

    let mut scaler = scaling::Context::get(
        decoder.format(),
        decoder.width(),
        decoder.height(),
        format::Pixel::RGB24,
        decoder.width(),
        decoder.height(),
        scaling::Flags::BILINEAR,
    )?;

    let hasher = hash::ImageHasher::default();
    let mut hashes = Vec::new();
    let mut frame_index = 0;

    let mut receive_and_process_decoded_frames =
        |decoder: &mut decoder::Video| -> Result<(), ffmpeg_next::Error> {
            let mut decoded = frame::Video::empty();

            while decoder.receive_frame(&mut decoded).is_ok() {
                let mut rgb_frame = frame::Video::empty();
                scaler.run(&decoded, &mut rgb_frame)?;
                if frame_index % 100 == 0 {
                    tracing::trace!(frame_index, "decoded and scaled frame");
                }

                let data = rgb_frame.data(0).to_vec();
                let im: image::RgbImage =
                    image::ImageBuffer::from_raw(decoder.width(), decoder.height(), data)
                        .ok_or(ffmpeg_next::Error::InvalidData)?;

                hashes.push(hasher.hash_image(&im));
                frame_index += 1;
            }

            tracing::debug!(frame_count = frame_index, "finished video decode");
            Ok(())
        };

    for (stream, packet) in ictx.packets() {
        if stream.index() == video_stream_index {
            decoder.send_packet(&packet)?;
            receive_and_process_decoded_frames(&mut decoder)?;
        }
    }
    decoder.send_eof()?;
    receive_and_process_decoded_frames(&mut decoder)?;

    Ok(hashes)
}

#[tracing::instrument(skip_all)]
async fn insert_media(tx: &mut Transaction<'_, Postgres>, media: &Media) -> eyre::Result<Uuid> {
    tracing::debug!("attempting to insert media");

    if let Some(sha256) = &media.file_sha256 {
        tracing::debug!("media had sha256: {}", hex::encode(sha256));
        if let Ok(Some(id)) = sqlx::query_file_scalar!("queries/media_lookup_sha256.sql", sha256)
            .fetch_optional(&mut *tx)
            .await
        {
            tracing::info!("media was already known as {id}");
            return Ok(id);
        }
    } else {
        tracing::warn!("media was missing sha256");
    }

    let media_id = sqlx::query_file_scalar!(
        "queries/media_insert.sql",
        media.file_sha256,
        media.file_size,
        media.mime_type,
        media.frames.len() == 1
    )
    .fetch_optional(&mut *tx)
    .await?;

    let media_id = match (media_id, &media.file_sha256) {
        (Some(id), _) => id,
        (None, Some(sha256)) => {
            tracing::warn!("media must have been inserted while inserting");
            sqlx::query_file_scalar!("queries/media_lookup_sha256.sql", sha256)
                .fetch_one(&mut *tx)
                .await?
        }
        (None, None) => {
            tracing::error!("media was not inserted and did not have sha256");
            return Err(eyre!("invalid media insert state"));
        }
    };

    let frames: Vec<_> = media
        .frames
        .iter()
        .enumerate()
        .map(|(index, frame)| {
            serde_json::json!({
                "media_id": media_id,
                "frame_index": index,
                "perceptual_gradient": frame.perceptual_gradient.map(i64::from_be_bytes),
            })
        })
        .collect();

    sqlx::query_file!(
        "queries/media_frame_insert.sql",
        serde_json::Value::Array(frames)
    )
    .execute(tx)
    .await?;

    tracing::info!("inserted new media with id {media_id}");
    Ok(media_id)
}

#[tracing::instrument(skip(tx, media))]
async fn link_submission_media(
    tx: &mut Transaction<'_, Postgres>,
    submission_id: Uuid,
    media_id: Uuid,
    media: &Media,
) -> eyre::Result<()> {
    tracing::debug!("attempting to link site media: {:?}", media.site_id);

    let submission_media_id = sqlx::query_file_scalar!(
        "queries/submission_media_insert.sql",
        submission_id,
        media_id,
        media.site_id,
        media.url,
        media.deleted,
        media.extra,
    )
    .fetch_optional(&mut *tx)
    .await?;

    if let Some(submission_media_id) = submission_media_id {
        tracing::info!("inserted new media link {submission_media_id}");
    } else {
        tracing::info!("media link already existed");
    }

    Ok(())
}

pub async fn insert_submission(
    conn: &PgPool,
    nats: &async_nats::Client,
    reason: FetchReason,
    submission: &mut Submission,
) -> eyre::Result<Uuid> {
    let mut tx = conn.begin().await?;

    let futs = submission.artists.iter().map(|artist| {
        sqlx::query_file_scalar!(
            "queries/artist_insert.sql",
            submission.site.to_string(),
            artist.site_artist_id,
            artist.name,
            artist.link
        )
        .fetch_one(conn)
    });

    let artist_ids = futures::future::try_join_all(futs).await?;

    let submission_id = sqlx::query_file_scalar!(
        "queries/submission_insert.sql",
        submission.site.to_string(),
        submission.submission_id,
        submission.deleted,
        submission.posted_at,
        submission.link,
        submission.title,
        &submission.tags,
        submission.description,
        submission
            .rating
            .as_ref()
            .map(|rating| serde_plain::to_string(rating).unwrap()),
        submission.retrieved_at,
        submission.extra,
    )
    .fetch_one(&mut tx)
    .await?;

    let submission_artist_ids: Vec<_> = artist_ids
        .into_iter()
        .map(|artist_id| {
            serde_json::json!({
                "submission_id": submission_id,
                "artist_id": artist_id,
            })
        })
        .collect();

    sqlx::query_file!(
        "queries/submission_associate_artists.sql",
        serde_json::Value::Array(submission_artist_ids)
    )
    .execute(&mut tx)
    .await?;

    for media in &submission.media {
        let media_id = insert_media(&mut tx, media).await?;
        link_submission_media(&mut tx, submission_id, media_id, media).await?;
    }

    tx.commit().await?;

    submission.id = Some(submission_id);

    if let Err(err) = notify_submission(nats, reason, submission).await {
        tracing::error!("could not notify for submission: {err}");
    }

    Ok(submission_id)
}

#[derive(Debug)]
pub struct DbSubmission {
    pub id: Uuid,
    pub site_name: String,
    pub site_submission_id: String,
    pub site_media_id: Option<String>,
    pub link: String,
    pub title: Option<String>,
    pub description: Option<String>,
    pub media_url: Option<String>,
    pub file_sha256: Option<Vec<u8>>,
    pub artists: Option<serde_json::Value>,
    pub rating: Option<String>,
    pub posted_at: Option<chrono::DateTime<chrono::Utc>>,
    pub tags: Option<Vec<String>>,
    pub deleted: Option<bool>,
    pub retrieved_at: Option<chrono::DateTime<chrono::Utc>>,
    pub extra: Option<serde_json::Value>,
    pub perceptual_gradient: Option<i64>,
    pub media_id: Option<Uuid>,
    pub file_size: Option<i64>,
    pub mime_type: Option<String>,
    pub submission_media_extra: Option<serde_json::Value>,
    pub frame_index: Option<i64>,
}

pub fn collapse_db_submissions(
    db_submissions: Vec<DbSubmission>,
) -> eyre::Result<HashMap<(Site, String), Submission>> {
    tracing::info!(len = db_submissions.len(), "collapsing submissions");

    let mut media_frames: HashMap<Uuid, Vec<MediaFrame>> =
        HashMap::with_capacity(db_submissions.len());
    for sub in &db_submissions {
        if let Some(media_id) = sub.media_id {
            media_frames.entry(media_id).or_default().push(MediaFrame {
                frame_index: sub
                    .frame_index
                    .tap_none(|| tracing::warn!("media frame was missing frame index"))
                    .unwrap_or_default(),
                perceptual_gradient: sub.perceptual_gradient.map(i64::to_be_bytes),
            });
        }
    }
    tracing::debug!(len = media_frames.len(), "found frames");

    let mut media: HashMap<Uuid, Vec<Media>> = HashMap::with_capacity(media_frames.len());
    for sub in &db_submissions {
        media.entry(sub.id).or_default().push(Media {
            site_id: sub.site_media_id.clone(),
            deleted: sub.deleted.unwrap_or_default(),
            url: sub.media_url.clone(),
            file_sha256: sub.file_sha256.clone(),
            file_size: sub.file_size,
            mime_type: sub.mime_type.clone(),
            frames: sub
                .media_id
                .and_then(|media_id| media_frames.get(&media_id).cloned())
                .unwrap_or_default(),
            extra: sub.submission_media_extra.clone(),
        });
    }
    tracing::debug!(len = media.len(), "found media");

    let mut submissions = HashMap::with_capacity(media.len());
    for db_submission in db_submissions {
        let site = serde_plain::from_str(&db_submission.site_name)?;
        let rating = db_submission
            .rating
            .map(|rating| serde_plain::from_str(&rating))
            .transpose()?;

        submissions
            .entry((site, db_submission.site_submission_id.clone()))
            .or_insert_with(|| Submission {
                id: Some(db_submission.id),
                site,
                submission_id: db_submission.site_submission_id,
                deleted: db_submission.deleted.unwrap_or_default(),
                posted_at: db_submission.posted_at,
                link: db_submission.link,
                title: db_submission.title,
                artists: db_submission
                    .artists
                    .map(serde_json::from_value)
                    .transpose()
                    .tap_err(|err| tracing::error!("could not deserialize artists: {err}"))
                    .ok()
                    .flatten()
                    .unwrap_or_default(),
                tags: db_submission.tags.unwrap_or_default(),
                description: db_submission.description,
                rating,
                media: media.get(&db_submission.id).cloned().unwrap_or_default(),
                retrieved_at: db_submission.retrieved_at,
                extra: db_submission.extra,
            });
    }
    tracing::info!(len = submissions.len(), "found unique submissions");

    Ok(submissions)
}

trait Slug<'a> {
    fn slug(&self) -> std::borrow::Cow<'a, str>;
}

impl Slug<'_> for fuzzysearch_common::Site {
    fn slug(&self) -> std::borrow::Cow<'static, str> {
        match self {
            Self::E621 => "e621",
            Self::FurAffinity => "furaffinity",
            Self::Twitter => "twitter",
            Self::Weasyl => "weasyl",
        }
        .into()
    }
}

async fn notify_submission(
    nats: &async_nats::Client,
    reason: FetchReason,
    submission: &Submission,
) -> eyre::Result<()> {
    let subject = format!(
        "fuzzysearch.loader.submission.{}.{}",
        serde_plain::to_string(&reason)?,
        submission.site.slug(),
    );

    let data = serde_json::to_vec(submission)?;

    nats.publish(subject, data.into()).await?;

    Ok(())
}
