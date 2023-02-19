use std::collections::HashMap;

use axum::{
    http::{HeaderMap, HeaderValue, Request, StatusCode},
    middleware::Next,
    response::Response,
};
use bkapi_client::BKApiClient;
use chrono::TimeZone;
use eyre::Context;
use lazy_static::lazy_static;
use prometheus::{
    register_histogram, register_int_counter, register_int_counter_vec, Histogram, IntCounter,
    IntCounterVec,
};
use reqwest::header::HeaderName;
use serde::{Deserialize, Serialize};
use sqlx::PgPool;

use fuzzysearch_common::*;

const RATE_LIMIT_WINDOW: i64 = 60;

lazy_static! {
    static ref BKAPI_TIME: Histogram = register_histogram!(
        "fuzzysearch_api_bkapi_request_seconds",
        "Amount of time to complete a BKApi request."
    )
    .unwrap();
    static ref DATABASE_TIME: Histogram = register_histogram!(
        "fuzzysearch_api_database_seconds",
        "Amount of time to lookup hashes."
    )
    .unwrap();
    static ref RATE_LIMIT_COUNT: IntCounterVec = register_int_counter_vec!(
        "fuzzysearch_api_rate_limit_count",
        "Number of requests for each bucket type.",
        &["bucket"]
    )
    .unwrap();
    static ref RATE_LIMITED_REQUEST_COUNT: IntCounter = register_int_counter!(
        "fuzzysearch_api_rate_limited_request_count",
        "Number of requests that exceeded a rate limit."
    )
    .unwrap();
    static ref HASH_SEARCH_RESULTS: IntCounter = register_int_counter!(
        "fuzzysearch_api_hash_search_count",
        "Total number of resulting images."
    )
    .unwrap();
    static ref GOOD_HASH_SEARCH_RESULTS: IntCounter = register_int_counter!(
        "fuzzysearch_api_hash_search_good_count",
        "Total number of good resulting images."
    )
    .unwrap();
    static ref NO_HASH_SEARCH_RESULTS: IntCounter = register_int_counter!(
        "fuzzysearch_api_hash_search_none_count",
        "Total number of no resulting images."
    )
    .unwrap();
}

#[derive(Clone)]
pub struct UserApiKey {
    pub id: i32,
    pub user_id: i32,
    pub key: String,
    pub name: Option<String>,
    pub name_limit: i16,
    pub image_limit: i16,
    pub hash_limit: i16,
}

pub async fn extract_api_key<B>(
    mut req: Request<B>,
    next: Next<B>,
) -> Result<Response, StatusCode> {
    let api_key = match req
        .headers()
        .get("x-api-key")
        .map(|api_key| String::from_utf8_lossy(api_key.as_bytes()))
    {
        Some(api_key) => api_key,
        None => return Err(StatusCode::UNAUTHORIZED),
    };

    let pool = match req.extensions().get::<PgPool>() {
        Some(pool) => pool,
        None => return Err(StatusCode::INTERNAL_SERVER_ERROR),
    };

    let api_key = sqlx::query_file_as!(UserApiKey, "queries/lookup_api_key.sql", api_key.as_ref())
        .fetch_optional(pool)
        .await
        .map_err(|_err| StatusCode::INTERNAL_SERVER_ERROR)?
        .ok_or(StatusCode::UNAUTHORIZED)?;

    tracing::debug!(
        api_key_id = api_key.id,
        user_id = api_key.user_id,
        "found valid api key in request: {}",
        api_key.name.as_deref().unwrap_or("unknown")
    );

    req.extensions_mut().insert(api_key);

    Ok(next.run(req).await)
}

#[derive(Debug, Serialize)]
struct BucketCount<'a> {
    bucket: &'a str,
    count: i16,
}

pub struct RateLimitData {
    pub buckets: Vec<RateLimitBucket>,
    pub next_time_window: chrono::DateTime<chrono::Utc>,
}

impl RateLimitData {
    pub fn headers(self) -> eyre::Result<HeaderMap> {
        let mut headers = HeaderMap::with_capacity(1 + self.buckets.len() * 2);

        let seconds_until_next = (self.next_time_window - chrono::Utc::now())
            .num_seconds()
            .clamp(1, RATE_LIMIT_WINDOW);
        let seconds_header_value = HeaderValue::from_str(&seconds_until_next.to_string())?;
        headers.insert("x-rate-limit-reset", seconds_header_value);

        for bucket in self.buckets {
            let name =
                HeaderName::from_bytes(format!("x-rate-limit-total-{}", bucket.bucket).as_bytes())?;
            let value = HeaderValue::from_str(&bucket.allowed.to_string())?;
            headers.insert(name, value);

            let name = HeaderName::from_bytes(
                format!("x-rate-limit-remaining-{}", bucket.bucket).as_bytes(),
            )?;
            let value = HeaderValue::from_str(&bucket.remaining()?.to_string())?;
            headers.insert(name, value);
        }

        Ok(headers)
    }
}

pub struct RateLimitBucket {
    pub bucket: String,
    pub allowed: i16,
    pub used: i16,
}

impl RateLimitBucket {
    fn remaining(&self) -> eyre::Result<u16> {
        let remaining = u16::try_from(self.allowed)
            .wrap_err("Allowed should always fit in u16")?
            .saturating_sub(u16::try_from(self.used).wrap_err("Used should always fit in u16")?);

        Ok(remaining)
    }
}

impl UserApiKey {
    #[tracing::instrument(err, skip_all, fields(api_key_id = self.id))]
    pub async fn rate_limit(
        &self,
        pool: &PgPool,
        buckets: &[(&str, i16)],
    ) -> eyre::Result<(bool, RateLimitData)> {
        let now = chrono::Utc::now();
        let timestamp = now.timestamp();
        let time_window = timestamp - (timestamp % RATE_LIMIT_WINDOW);

        let bucket_counts: Vec<_> = buckets
            .iter()
            .map(|bucket| BucketCount {
                bucket: bucket.0,
                count: bucket.1,
            })
            .collect();

        bucket_counts.iter().for_each(|bucket| {
            RATE_LIMIT_COUNT
                .with_label_values(&[bucket.bucket])
                .inc_by(bucket.count as u64);
        });

        let allowed_buckets = self.buckets();

        let applied_limits = sqlx::query_file!(
            "queries/apply_rate_limit.sql",
            self.id,
            time_window,
            serde_json::to_value(bucket_counts)?
        )
        .map(|row| {
            let allowed = allowed_buckets
                .get(&row.group_name)
                .copied()
                .unwrap_or_default();

            RateLimitBucket {
                bucket: row.group_name,
                allowed,
                used: row.count,
            }
        })
        .fetch_all(pool)
        .await?;

        for bucket in applied_limits.iter() {
            tracing::trace!(
                bucket = bucket.bucket,
                allowed = bucket.allowed,
                used = bucket.used,
                "evaluated bucket"
            );
        }

        let over_limit = applied_limits
            .iter()
            .any(|bucket| bucket.used > bucket.allowed);

        tracing::debug!(over_limit, "applied rate limit");
        if over_limit {
            RATE_LIMITED_REQUEST_COUNT.inc();
        }

        let rate_limit_data = RateLimitData {
            buckets: applied_limits,
            next_time_window: chrono::Utc
                .timestamp_opt(time_window + RATE_LIMIT_WINDOW, 0)
                .unwrap(),
        };

        Ok((over_limit, rate_limit_data))
    }

    fn buckets(&self) -> HashMap<String, i16> {
        [
            ("name".to_string(), self.name_limit),
            ("image".to_string(), self.image_limit),
            ("hash".to_string(), self.hash_limit),
        ]
        .into_iter()
        .collect()
    }
}

#[derive(Debug)]
struct DbFurAffinityFile {
    pub id: i32,
    pub file_id: Option<i32>,
    pub artist: Option<String>,
    pub hash: Option<i64>,
    pub url: Option<String>,
    pub filename: Option<String>,
    pub rating: Option<String>,
    pub posted_at: Option<chrono::DateTime<chrono::Utc>>,
    pub file_size: Option<i32>,
    pub sha256: Option<Vec<u8>>,
    pub updated_at: Option<chrono::DateTime<chrono::Utc>>,
    pub deleted: bool,
    pub tags: Option<Vec<String>>,
}

impl DbFurAffinityFile {
    fn replace_cdn_host(url: Option<String>) -> Option<String> {
        url.and_then(|url| url::Url::parse(&url).ok())
            .and_then(|mut url| {
                url.set_host(Some("d.furaffinity.net")).ok()?;
                Some(url.to_string())
            })
    }
}

impl From<DbFurAffinityFile> for FurAffinityFile {
    fn from(file: DbFurAffinityFile) -> Self {
        FurAffinityFile {
            id: file.id,
            file_id: file.file_id,
            artist: file.artist,
            hash: file.hash,
            hash_str: file.hash.map(|hash| hash.to_string()),
            url: DbFurAffinityFile::replace_cdn_host(file.url),
            filename: file.filename,
            rating: file.rating.and_then(|rating| rating.parse().ok()),
            posted_at: file.posted_at,
            file_size: file.file_size,
            sha256: file.sha256.map(hex::encode),
            updated_at: file.updated_at,
            deleted: file.deleted,
            tags: file.tags.unwrap_or_default(),
        }
    }
}

#[derive(Debug, Serialize)]
struct HashSearch {
    searched_hash: i64,
    found_hash: i64,
    distance: u64,
}

#[derive(Debug, Serialize, Deserialize)]
struct DbResult {
    site: String,
    id: i64,
    hash: i64,
    url: Option<String>,
    filename: Option<String>,
    artists: Option<Vec<String>>,
    file_id: Option<i32>,
    sources: Option<Vec<String>>,
    rating: Option<String>,
    posted_at: Option<chrono::DateTime<chrono::Utc>>,
    searched_hash: i64,
    distance: i64,
    sha256: Option<Vec<u8>>,
}

impl From<DbResult> for SearchResult {
    fn from(result: DbResult) -> Self {
        Self {
            site_id: result.id,
            site_id_str: result.id.to_string(),
            url: result.url.unwrap_or_default(),
            filename: result.filename.unwrap_or_default(),
            artists: result.artists,
            rating: result.rating.and_then(|rating| rating.parse().ok()),
            posted_at: result.posted_at,
            sha256: result.sha256.map(hex::encode),
            hash: Some(result.hash),
            hash_str: Some(result.hash.to_string()),
            distance: Some(result.distance),
            searched_hash: Some(result.searched_hash),
            searched_hash_str: Some(result.searched_hash.to_string()),
            site_info: match result.site.as_ref() {
                "FurAffinity" => SiteInfo::FurAffinity {
                    file_id: result.file_id.unwrap_or_default(),
                },
                "e621" => SiteInfo::E621 {
                    sources: result.sources.unwrap_or_default(),
                },
                "Weasyl" => SiteInfo::Weasyl,
                "Twitter" => SiteInfo::Twitter,
                _ => SiteInfo::Unknown,
            },
        }
    }
}

#[tracing::instrument(err, skip(bkapi, pool))]
pub async fn lookup_hashes(
    bkapi: &BKApiClient,
    pool: &PgPool,
    hashes: &[i64],
    distance: u64,
) -> eyre::Result<Vec<SearchResult>> {
    tracing::debug!(distance, "starting lookup for hashes: {:?}", hashes);

    let bkapi_timer = BKAPI_TIME.start_timer();
    let related_hashes = bkapi.search_many(hashes, distance).await?;
    tracing::trace!(
        duration = bkapi_timer.stop_and_record(),
        "got results from bkapi"
    );

    let search: Vec<_> = related_hashes
        .into_iter()
        .flat_map(|results| {
            let searched_hash = results.hash;

            results.hashes.into_iter().map(move |result| HashSearch {
                searched_hash,
                found_hash: result.hash,
                distance: result.distance,
            })
        })
        .collect();
    tracing::trace!(count = search.len(), "looking up discovered hashes");

    let database_timer = DATABASE_TIME.start_timer();
    let results = sqlx::query_file_as!(
        DbResult,
        "queries/lookup_hashes.sql",
        serde_json::to_value(search)?
    )
    .map(SearchResult::from)
    .fetch_all(pool)
    .await?;
    tracing::trace!(
        duration = database_timer.stop_and_record(),
        count = results.len(),
        "found hashes"
    );

    HASH_SEARCH_RESULTS.inc_by(results.len() as u64);
    GOOD_HASH_SEARCH_RESULTS.inc_by(
        results
            .iter()
            .filter_map(|result| Some(result.distance? <= 3))
            .count() as u64,
    );

    if results.is_empty() {
        NO_HASH_SEARCH_RESULTS.inc();
    }

    Ok(results)
}

#[tracing::instrument(err, skip(pool))]
pub async fn lookup_furaffinity_file(
    pool: &PgPool,
    file: &str,
) -> eyre::Result<Vec<FurAffinityFile>> {
    let results = sqlx::query_file_as!(
        DbFurAffinityFile,
        "queries/lookup_furaffinity_filename.sql",
        file
    )
    .map(FurAffinityFile::from)
    .fetch_all(pool)
    .await?;

    Ok(results)
}

#[tracing::instrument(err, skip(pool))]
pub async fn lookup_furaffinity_id(pool: &PgPool, id: i32) -> eyre::Result<Vec<FurAffinityFile>> {
    let results = sqlx::query_file_as!(DbFurAffinityFile, "queries/lookup_furaffinity_id.sql", id)
        .map(FurAffinityFile::from)
        .fetch_all(pool)
        .await?;

    Ok(results)
}
