use std::{
    collections::HashSet,
    path::PathBuf,
    sync::{
        atomic::{AtomicU64, Ordering},
        Arc,
    },
};

use async_trait::async_trait;
use chrono::TimeZone;
use eyre::ContextCompat;
use futures::{StreamExt, TryStreamExt};
use fuzzysearch_common::{Artist, Rating, Site};
use lazy_static::lazy_static;
use prometheus::{register_int_gauge, register_int_gauge_vec, IntGauge, IntGaugeVec, Opts};
use regex::Regex;
use scraper::{Html, Selector};
use serde::Deserialize;
use sqlx::PgPool;
use tap::TapOptional;
use tokio::sync::Mutex;
use tracing::Instrument;

use crate::{
    sites::{process_file, LoadableSite, Submission, SubmissionResult},
    SiteConfig,
};

lazy_static! {
    static ref PAGE_TITLE: Selector = Selector::parse("title").unwrap();
    static ref ERROR_MESSAGE: Selector = Selector::parse(
        ".error-message-box, div#standardpage section.notice-message p.link-override"
    )
    .unwrap();
    static ref ARTIST: Selector =
        Selector::parse(".submission-id-sub-container .submission-title + a").unwrap();
    static ref TITLE: Selector = Selector::parse(".submission-title h2 p").unwrap();
    static ref IMAGE_URL: Selector = Selector::parse("#submissionImg").unwrap();
    static ref FLASH_OBJECT: Selector = Selector::parse("#flash_embed").unwrap();
    static ref POSTED_AT: Selector =
        Selector::parse(".submission-id-sub-container strong span.popup_date").unwrap();
    static ref TAGS: Selector = Selector::parse("section.tags-row a").unwrap();
    static ref DESCRIPTION: Selector = Selector::parse(".submission-content section").unwrap();
    static ref RATING: Selector =
        Selector::parse(".stats-container .rating span.rating-box").unwrap();
    static ref LATEST_SUBMISSION: Selector =
        Selector::parse("#gallery-frontpage-submissions figure:first-child b u a").unwrap();
    static ref ONLINE_NUMBER: Regex = Regex::new(r"(\d+)").unwrap();
    static ref ONLINE_STATS_ELEMENT: Selector = Selector::parse(".online-stats").unwrap();
    static ref DATE_CLEANER: Regex = Regex::new(r"(\d{1,2})(st|nd|rd|th)").unwrap();
}

lazy_static! {
    static ref USERS_ONLINE: IntGaugeVec = register_int_gauge_vec!(
        Opts::new(
            "fuzzysearch_watcher_users_online",
            "Number of users online for a site."
        )
        .const_label("site", "FurAffinity"),
        &["group"]
    )
    .unwrap();
    static ref MISSING_SUBMISSIONS: IntGauge = register_int_gauge!(Opts::new(
        "fuzzysearch_site_missing_submissions",
        "Number of submissions known to be missing for a site."
    )
    .const_label("site", "FurAffinity"))
    .unwrap();
    static ref LATEST_ID: IntGauge = register_int_gauge!(Opts::new(
        "fuzzysearch_site_latest_id",
        "Latest known ID for sites with sequential IDs."
    )
    .const_label("site", "FurAffinity"))
    .unwrap();
}

#[derive(Debug, thiserror::Error)]
#[error("missing field: {field_name}")]
struct MissingFieldError {
    field_name: &'static str,
}

impl MissingFieldError {
    pub fn new(field_name: &'static str) -> Self {
        Self { field_name }
    }
}

#[derive(Debug, Deserialize)]
enum FurAffinityRating {
    General,
    Mature,
    Adult,
}

impl FurAffinityRating {
    pub fn normalized(&self) -> Rating {
        match self {
            Self::General => Rating::General,
            Self::Mature => Rating::Mature,
            Self::Adult => Rating::Adult,
        }
    }
}

pub struct FurAffinity {
    pool: PgPool,
    download_path: Option<PathBuf>,

    client: reqwest::Client,
    cookies: String,

    latest_id: AtomicU64,

    bot_threshold: u64,
    below_bot_threshold: tokio::sync::watch::Receiver<bool>,
    auto_fetch_submissions: bool,

    missing_ids: Mutex<HashSet<i32>>,
}

struct OnlineCounts {
    #[allow(dead_code)]
    total: u64,
    guests: u64,
    registered: u64,
    other: u64,
}

impl FurAffinity {
    pub async fn new(
        site_config: SiteConfig,
        cookie_a: String,
        cookie_b: String,
        bot_threshold: u64,
        client: reqwest::Client,
        pool: PgPool,
    ) -> Arc<Self> {
        let cookies = format!("a={cookie_a}; b={cookie_b}");

        let known_ids: HashSet<i32> = sqlx::query_scalar!(
            "SELECT site_submission_id FROM submissions.submission WHERE site_id = 1"
        )
        .fetch_all(&pool)
        .await
        .expect("could not get existing submissions")
        .into_iter()
        .flat_map(|id| id.parse().ok())
        .collect();
        tracing::info!(len = known_ids.len(), "discovered known ids");

        let max_id = known_ids.iter().max().copied().unwrap_or(1);
        let all_ids: HashSet<i32> = (1..=max_id).collect();
        let missing_ids: HashSet<i32> = all_ids.difference(&known_ids).copied().collect();
        MISSING_SUBMISSIONS.set(missing_ids.len() as i64);
        tracing::info!(len = missing_ids.len(), "calculated missing ids");

        let latest_id = AtomicU64::new(0);
        let (tx, below_bot_threshold) = tokio::sync::watch::channel(false);

        let fa = Arc::new(Self {
            pool,
            download_path: site_config.download_path,
            cookies,
            client,
            latest_id,
            bot_threshold,
            below_bot_threshold,
            auto_fetch_submissions: site_config.auto_fetch_submissions,
            missing_ids: Mutex::new(missing_ids),
        });

        fa.start_background_tasks(tx).await;

        fa
    }

    async fn start_background_tasks(self: &Arc<Self>, tx: tokio::sync::watch::Sender<bool>) {
        use std::time::Duration;
        use tokio::time::{self, MissedTickBehavior};

        self.refresh_metadata(&tx)
            .await
            .expect("could not collect initial metadata");

        {
            let fa = self.clone();
            tokio::spawn(
                async move {
                    let mut interval = time::interval(Duration::from_secs(60));
                    interval.set_missed_tick_behavior(MissedTickBehavior::Delay);

                    loop {
                        interval.tick().await;
                        tracing::debug!("refreshing metadata");
                        if let Err(err) = fa.refresh_metadata(&tx).await {
                            tracing::error!("could not refresh metadata: {err}");
                        }
                    }
                }
                .instrument(tracing::info_span!("metadata_fetch")),
            );
        }

        {
            let fa = self.clone();
            tokio::spawn(
                async move {
                    if !fa.auto_fetch_submissions {
                        tracing::warn!("auto fetch disabled, not loading new submissions");
                        return;
                    }

                    let mut interval = time::interval(Duration::from_secs(60));
                    interval.set_missed_tick_behavior(MissedTickBehavior::Delay);

                    loop {
                        interval.tick().await;
                        tracing::info!("loading new submissions");
                        if let Err(err) = fa.load_new_submissions().await {
                            tracing::error!("could not load new submissions: {err}");
                        }
                    }
                }
                .instrument(tracing::info_span!("load_submissions")),
            );
        }

        tokio::spawn(
            self.clone()
                .backfill_submissions()
                .instrument(tracing::info_span!("backfill_submissions")),
        );
    }

    async fn refresh_metadata(&self, tx: &tokio::sync::watch::Sender<bool>) -> eyre::Result<()> {
        let page = self
            .client
            .get("https://www.furaffinity.net/")
            .header("cookie", &self.cookies)
            .send()
            .await?
            .text()
            .await?;

        let doc = Html::parse_document(&page);

        let latest_id = doc
            .select(&LATEST_SUBMISSION)
            .next()
            .context("missing latest submission")?
            .value()
            .attr("href")
            .and_then(|href| href.split('/').filter(|part| !part.is_empty()).last())
            .and_then(|part| part.parse().ok())
            .context("no valid latest id")?;
        self.latest_id.store(latest_id, Ordering::Relaxed);
        LATEST_ID.set(latest_id as i64);
        tracing::info!(latest_id, "got latest id");

        let online = doc
            .select(&ONLINE_STATS_ELEMENT)
            .next()
            .map(join_text_nodes)
            .context("no online stats element")?;
        let mut numbers = ONLINE_NUMBER
            .find_iter(&online)
            .filter_map(|num| num.as_str().parse::<u64>().ok());

        let online = OnlineCounts {
            total: numbers.next().unwrap_or_default(),
            guests: numbers.next().unwrap_or_default(),
            registered: numbers.next().unwrap_or_default(),
            other: numbers.next().unwrap_or_default(),
        };

        USERS_ONLINE
            .with_label_values(&["guest"])
            .set(online.guests as i64);
        USERS_ONLINE
            .with_label_values(&["registered"])
            .set(online.registered as i64);
        USERS_ONLINE
            .with_label_values(&["other"])
            .set(online.other as i64);

        let below_threshold = online.registered < self.bot_threshold;
        tx.send_if_modified(|state: &mut bool| {
            if below_threshold != *state {
                *state = below_threshold;
                true
            } else {
                false
            }
        });
        tracing::info!(
            below_threshold,
            online_registered = online.registered,
            "updated users online"
        );

        Ok(())
    }

    #[tracing::instrument(skip(self))]
    pub async fn load_submission(&self, id: &str) -> eyre::Result<SubmissionResult> {
        tracing::info!("loading submission");

        let hist = super::FETCHED_SUBMISSIONS
            .with_label_values(&["FurAffinity"])
            .start_timer();

        let url = format!("https://www.furaffinity.net/view/{id}/");

        let page = match self
            .client
            .get(&url)
            .header("cookie", &self.cookies)
            .send()
            .await
            .and_then(|page| page.error_for_status())
        {
            Ok(page) => page,
            Err(err) => {
                hist.stop_and_discard();
                super::FETCH_ERRORS
                    .with_label_values(&["FurAffinity"])
                    .inc();

                return Ok(SubmissionResult::Error {
                    site: Site::FurAffinity,
                    submission_id: id.to_string(),
                    message: Some(err.to_string()),
                });
            }
        };

        let content = match page.text().await {
            Ok(content) => content,
            Err(err) => {
                hist.stop_and_discard();
                super::FETCH_ERRORS
                    .with_label_values(&["FurAffinity"])
                    .inc();

                return Ok(SubmissionResult::Error {
                    site: Site::FurAffinity,
                    submission_id: id.to_string(),
                    message: Some(err.to_string()),
                });
            }
        };

        let (mut submission, content_url_parts) = parse_submission(id.to_string(), url, &content)?;

        if let Some(content_url_parts) = content_url_parts {
            let mut media = match process_file(
                &self.pool,
                &self.download_path,
                &self.client,
                None,
                &content_url_parts.url,
            )
            .await
            {
                Ok(media) => media,
                Err(err) => {
                    return Ok(SubmissionResult::Error {
                        site: Site::FurAffinity,
                        submission_id: id.to_string(),
                        message: Some(err.to_string()),
                    })
                }
            };
            let file_id = content_url_parts
                .filename
                .split_once('.')
                .map(|(first, _second)| first);
            media.extra = Some(serde_json::json!({
                "file_name": content_url_parts.filename,
                "file_id": file_id,
            }));

            match submission {
                SubmissionResult::Fetched(ref mut submission) => submission.media.push(media),
                _ => tracing::warn!("had content url parts but no fetched submission"),
            }
        }

        hist.stop_and_record();

        if let Ok(id) = id.parse() {
            self.loaded_submission(id).await;
        }

        Ok(submission)
    }

    async fn loaded_submission(&self, id: i32) {
        let count = {
            let mut missing_ids = self.missing_ids.lock().await;
            missing_ids.remove(&id);
            missing_ids.len()
        };

        MISSING_SUBMISSIONS.set(count as i64)
    }

    async fn load_and_save_submission(&self, id: String) -> eyre::Result<String> {
        match self.load_submission(&id).await? {
            SubmissionResult::Fetched(sub) => {
                crate::sites::insert_submission(&self.pool, &sub).await?;
            }
            SubmissionResult::Error { message, .. } => {
                tracing::error!("could not load submission: {message:?}");
            }
        }

        Ok(id)
    }

    async fn load_new_submissions(&self) -> eyre::Result<()> {
        let latest_known_id = self.latest_id.load(Ordering::Relaxed) as i64;

        let latest_loaded_id = sqlx::query_scalar!(
            "SELECT max(site_submission_id::bigint) FROM submissions.submission WHERE site_id = 1"
        )
        .fetch_one(&self.pool)
        .await?
        .unwrap_or(0);

        tracing::info!(latest_known_id, latest_loaded_id, "found latest ids");

        let missing_ids = (latest_loaded_id + 1)..=latest_known_id;
        for missing_id in missing_ids {
            self.load_and_save_submission(missing_id.to_string())
                .await?;
        }

        Ok(())
    }

    async fn backfill_submissions(self: Arc<Self>) {
        if !self.auto_fetch_submissions {
            tracing::info!("auto fetch disabled, not backfilling submissions");
            return;
        }

        const CONCURRENT_BACKFILLS: usize = 4;

        if self.missing_ids.lock().await.is_empty() {
            tracing::info!("has no submissions to backfill");
            return;
        }

        let mut rx = self.below_bot_threshold.clone();
        while rx.changed().await.is_ok() {
            if !*self.below_bot_threshold.borrow() {
                tracing::debug!("not below bot threshold");
                continue;
            }

            if self.missing_ids.lock().await.is_empty() {
                tracing::info!("backfill was completed");
                break;
            }

            tracing::info!("above bot threshold, resuming backfill");

            // This is kind of a gross hack to ensure we can run concurrent
            // operations. We need to track which items are currently being
            // processed so they can be skipped when finding the next element.
            let in_flight_ids: Arc<Mutex<HashSet<i32>>> =
                Arc::new(Mutex::new(HashSet::with_capacity(CONCURRENT_BACKFILLS)));

            futures::stream::unfold((), |_| async {
                let missing_ids = self.missing_ids.lock().await;
                let mut in_flight_ids = in_flight_ids.lock().await;

                let missing_id = missing_ids
                    .iter()
                    .copied()
                    .find(|missing_id| !in_flight_ids.contains(missing_id));

                if let Some(missing_id) = missing_id {
                    tracing::trace!(id = missing_id, "got new backfill id");
                    in_flight_ids.insert(missing_id);
                }

                missing_id.map(|missing_id| (missing_id, ()))
            })
            .take_until(rx.changed())
            .for_each_concurrent(CONCURRENT_BACKFILLS, |missing_id| {
                let fa = self.clone();
                let in_flight_ids = in_flight_ids.clone();

                async move {
                    match fa.load_and_save_submission(missing_id.to_string()).await {
                        Ok(_id) => {
                            tracing::info!(id = missing_id, "backfilled submission")
                        }
                        Err(err) => {
                            tracing::error!(id = missing_id, "could not backfill submission: {err}")
                        }
                    }

                    in_flight_ids.lock().await.remove(&missing_id);
                }
            })
            .await;
        }
    }
}

#[async_trait]
impl LoadableSite for FurAffinity {
    fn site(&self) -> Site {
        Site::FurAffinity
    }

    #[tracing::instrument(skip(self))]
    async fn load(&self, ids: Vec<&str>) -> eyre::Result<Vec<SubmissionResult>> {
        tracing::info!("starting to load submissions");

        futures::stream::iter(ids)
            .then(|id| self.load_submission(id))
            .try_collect()
            .await
    }
}

fn join_text_nodes(elem: scraper::ElementRef) -> String {
    elem.text().collect::<String>().trim().to_string()
}

fn parse_date(date: &str) -> eyre::Result<chrono::DateTime<chrono::Utc>> {
    let date_str = DATE_CLEANER.replace(date, "$1");

    let date = chrono::Utc.datetime_from_str(&date_str, "%b %e, %Y %l:%M %p")?;

    Ok(date)
}

struct ContentUrlParts {
    url: String,
    filename: String,
}

fn extract_url(elem: scraper::ElementRef, attr: &'static str) -> Option<ContentUrlParts> {
    let url = "https:".to_owned() + elem.value().attr(attr)?;
    let filename = url.split('/').last()?.to_string();

    Some(ContentUrlParts { url, filename })
}

fn parse_submission(
    id: String,
    url: String,
    content: &str,
) -> eyre::Result<(SubmissionResult, Option<ContentUrlParts>)> {
    let doc = Html::parse_document(content);

    if doc
        .select(&PAGE_TITLE)
        .next()
        .map(|elem| join_text_nodes(elem) == "System Error")
        .unwrap_or(false)
    {
        tracing::trace!("page had system error: {content}");

        return Ok((
            SubmissionResult::Fetched(Submission {
                site: Site::FurAffinity,
                submission_id: id,
                deleted: true,
                posted_at: None,
                link: url,
                title: None,
                artists: Vec::new(),
                tags: Vec::new(),
                description: None,
                rating: None,
                media: Vec::new(),
                retrieved_at: Some(chrono::Utc::now()),
                extra: None,
            }),
            None,
        ));
    }

    if doc.select(&ERROR_MESSAGE).next().is_some() {
        tracing::trace!("page had error message: {content}");

        return Ok((
            SubmissionResult::Fetched(Submission {
                site: Site::FurAffinity,
                submission_id: id,
                deleted: true,
                posted_at: None,
                link: url,
                title: None,
                artists: Vec::new(),
                tags: Vec::new(),
                description: None,
                rating: None,
                media: Vec::new(),
                retrieved_at: Some(chrono::Utc::now()),
                extra: None,
            }),
            None,
        ));
    }

    let title = doc
        .select(&TITLE)
        .next()
        .map(join_text_nodes)
        .ok_or(MissingFieldError::new("title"))?;

    let artist = doc
        .select(&ARTIST)
        .next()
        .map(join_text_nodes)
        .ok_or(MissingFieldError::new("artist"))?;

    let content_url_parts = if let Some(elem) = doc.select(&IMAGE_URL).next() {
        extract_url(elem, "src").ok_or(MissingFieldError::new("image url"))?
    } else if let Some(elem) = doc.select(&FLASH_OBJECT).next() {
        extract_url(elem, "data").ok_or(MissingFieldError::new("flash url"))?
    } else {
        return Err(MissingFieldError::new("content").into());
    };

    let rating: Option<FurAffinityRating> = doc
        .select(&RATING)
        .next()
        .map(join_text_nodes)
        .tap_some(|rating| tracing::trace!("extracted rating: {rating}"))
        .and_then(|rating| serde_plain::from_str(&rating).ok())
        .ok_or(MissingFieldError::new("rating"))?;

    let posted_at = doc
        .select(&POSTED_AT)
        .next()
        .and_then(|elem| elem.value().attr("title"))
        .tap_some(|posted_at| tracing::trace!("extracted posted_at: {posted_at}"))
        .and_then(|posted_at| parse_date(posted_at).ok())
        .ok_or(MissingFieldError::new("posted at"))?;

    let tags: Vec<_> = doc
        .select(&TAGS)
        .map(join_text_nodes)
        .map(|tag| {
            tag.strip_suffix(',')
                .map(|tag| tag.to_string())
                .unwrap_or(tag)
        })
        .collect();

    let description = doc
        .select(&DESCRIPTION)
        .next()
        .ok_or(MissingFieldError::new("description"))?
        .inner_html();

    Ok((
        SubmissionResult::Fetched(Submission {
            site: Site::FurAffinity,
            submission_id: id,
            deleted: false,
            posted_at: Some(posted_at),
            link: url,
            title: Some(title),
            artists: vec![Artist {
                link: Some(format!("https://www.furaffinity.net/user/{artist}/")),
                site_artist_id: artist.clone(),
                name: artist,
            }],
            tags,
            description: Some(description),
            rating: rating.map(|rating| rating.normalized()),
            media: Vec::with_capacity(1),
            retrieved_at: Some(chrono::Utc::now()),
            extra: None,
        }),
        Some(content_url_parts),
    ))
}

#[cfg(test)]
mod tests {
    use chrono::TimeZone;

    use super::parse_date;

    #[test]
    fn test_parse_date() {
        let cases = [(
            "Feb 26, 2023 03:47 PM",
            chrono::Utc
                .with_ymd_and_hms(2023, 2, 26, 15, 47, 0)
                .unwrap(),
        )];

        for (input, output) in cases {
            assert_eq!(parse_date(input).unwrap(), output);
        }
    }
}
