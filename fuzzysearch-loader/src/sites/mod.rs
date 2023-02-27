use std::collections::HashMap;

use async_trait::async_trait;
use enum_dispatch::enum_dispatch;
use eyre::eyre;
use foxlib::hash::image::AnimationDecoder;
use futures::{stream::FuturesUnordered, TryStreamExt};
use fuzzysearch_common::{Media, MediaFrame, Site, Submission};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use sqlx::{PgPool, Postgres, Transaction};
use tap::TapOptional;
use uuid::Uuid;

use crate::{
    sites::{furaffinity::FurAffinity, weasyl::Weasyl},
    Config,
};

mod furaffinity;
mod weasyl;

#[derive(Clone, Debug, Deserialize, Serialize)]
pub enum SubmissionResult {
    Fetched(Submission),
    Error {
        site: Site,
        submission_id: String,
        message: Option<String>,
    },
}

/// An enum representing each loadable site.
#[enum_dispatch]
pub enum LoadableSites {
    FurAffinity,
    Weasyl,
}

/// All of the methods required to load a submission from a site.
#[async_trait]
#[enum_dispatch(LoadableSites)]
pub trait LoadableSite {
    /// The site it's loading from.
    fn site(&self) -> Site;

    /// Load submissions with the given IDs.
    ///
    /// Only return errors for system failures, use an errored submission result
    /// if a submission can't be loaded.
    async fn load(&self, ids: Vec<&str>) -> eyre::Result<Vec<SubmissionResult>>;
}

/// Get all of the sites.
pub fn sites(config: &Config, client: reqwest::Client) -> Vec<LoadableSites> {
    let mut sites: Vec<LoadableSites> = Vec::with_capacity(4);

    if let (Some(cookie_a), Some(cookie_b)) = (
        config.furaffinity_cookie_a.as_ref(),
        config.furaffinity_cookie_b.as_ref(),
    ) {
        tracing::info!("adding furaffinity");
        sites.push(FurAffinity::new(client.clone(), cookie_a.clone(), cookie_b.clone()).into())
    }

    if let Some(weasyl_api_token) = config.weasyl_api_token.as_ref() {
        tracing::info!("adding weasyl");
        sites.push(Weasyl::new(weasyl_api_token.clone(), client).into())
    }

    if sites.is_empty() {
        tracing::error!("no sites are registered");
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
impl LoadSubmissions for &[LoadableSites] {
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
            .map(|(loadable_site, ids)| loadable_site.load(ids))
            .collect();

        let submissions: Vec<Vec<SubmissionResult>> = site_loads.try_collect().await?;
        let submissions: Vec<SubmissionResult> = submissions.into_iter().flatten().collect();

        tracing::info!(len = submissions.len(), "loaded submissions");

        Ok(submissions)
    }
}

#[tracing::instrument(skip(client, site_id))]
pub async fn process_image(
    client: &reqwest::Client,
    site_id: Option<String>,
    url: &str,
) -> eyre::Result<Media> {
    tracing::info!("attempting to download image");

    let buf = client.get(url).send().await?.bytes().await?;
    let file_size = buf.len() as i64;
    tracing::debug!("file size: {file_size}");

    let sha256 = Sha256::digest(&buf);
    tracing::debug!("sha256 hash: {}", hex::encode(sha256));

    let mime_type = infer::get(&buf).map(|typ| typ.mime_type().to_string());
    tracing::debug!("mime type: {mime_type:?}");

    let hashing_span = tracing::info_span!("load_and_hash_image", len = buf.len());
    let frames = tokio::task::spawn_blocking(move || {
        let _entered = hashing_span.enter();
        let hasher = foxlib::hash::ImageHasher::default();

        if infer::is(&buf, "gif") {
            tracing::debug!("mime type suggests gif");
            let cursor = std::io::Cursor::new(&buf);

            if let Ok(gif) = foxlib::hash::image::codecs::gif::GifDecoder::new(cursor) {
                let hashes: Vec<_> = gif
                    .into_frames()
                    .filter_map(Result::ok)
                    .map(|frame| hasher.hash_image(frame.buffer()))
                    .collect();
                tracing::info!(len = hashes.len(), "extracted frames from gif");

                return Some(hashes);
            }
        }

        let im = foxlib::hash::image::load_from_memory(&buf).ok()?;
        tracing::info!("loaded static image");

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
        file_size: Some(file_size),
        mime_type,
        frames,
        extra: None,
    })
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

pub async fn insert_submission(conn: &PgPool, submission: &Submission) -> eyre::Result<Uuid> {
    let mut tx = conn.begin().await?;

    let submission_id = sqlx::query_file_scalar!(
        "queries/submission_insert.sql",
        submission.site.to_string(),
        submission.submission_id,
        submission.deleted,
        submission.posted_at,
        submission.link,
        submission.title,
        &submission.artists,
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

    for media in &submission.media {
        let media_id = insert_media(&mut tx, media).await?;
        link_submission_media(&mut tx, submission_id, media_id, media).await?;
    }

    tx.commit().await?;

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
    pub artists: Option<Vec<String>>,
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
                site,
                submission_id: db_submission.site_submission_id,
                deleted: db_submission.deleted.unwrap_or_default(),
                posted_at: db_submission.posted_at,
                link: db_submission.link,
                title: db_submission.title,
                artists: db_submission.artists.unwrap_or_default(),
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
