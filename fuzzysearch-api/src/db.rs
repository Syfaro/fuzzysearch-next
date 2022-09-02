use std::collections::HashMap;

use axum::{
    http::{HeaderMap, HeaderValue, Request, StatusCode},
    middleware::Next,
    response::Response,
};
use bkapi_client::BKApiClient;
use eyre::Context;
use reqwest::header::HeaderName;
use serde::{Deserialize, Serialize};
use sqlx::PgPool;

use crate::api::{FurAffinityFile, SearchResult};

#[derive(Clone)]
pub struct UserApiKey {
    id: i32,
    user_id: i32,
    name: Option<String>,
    name_limit: i16,
    image_limit: i16,
    hash_limit: i16,
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

pub trait RateLimitBucketsHeaders {
    fn headers(self) -> eyre::Result<HeaderMap>;
}

impl RateLimitBucketsHeaders for Vec<RateLimitBucket> {
    fn headers(self) -> eyre::Result<HeaderMap> {
        let mut headers = HeaderMap::with_capacity(self.len() * 2);

        for bucket in self {
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

impl UserApiKey {
    pub async fn rate_limit(
        &self,
        pool: &PgPool,
        buckets: &[(&str, i16)],
    ) -> eyre::Result<(bool, Vec<RateLimitBucket>)> {
        let now = chrono::Utc::now();
        let timestamp = now.timestamp();
        let time_window = timestamp - (timestamp % 60);

        let bucket_counts: Vec<_> = buckets
            .iter()
            .map(|bucket| BucketCount {
                bucket: bucket.0,
                count: bucket.1,
            })
            .collect();

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

        Ok((over_limit, applied_limits))
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
        }
    }
}

pub async fn lookup_hashes(
    bkapi: &BKApiClient,
    pool: &PgPool,
    hashes: &[i64],
    distance: u64,
) -> eyre::Result<Vec<SearchResult>> {
    tracing::debug!(distance, "starting lookup for hashes: {:?}", hashes);

    let related_hashes = bkapi.search_many(hashes, distance).await?;
    tracing::trace!("got results from bkapi");

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

    let results = sqlx::query_file_as!(
        DbResult,
        "queries/lookup_hashes.sql",
        serde_json::to_value(search)?
    )
    .map(SearchResult::from)
    .fetch_all(pool)
    .await?;
    tracing::trace!(count = results.len(), "found hashes");

    Ok(results)
}

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

pub async fn lookup_furaffinity_id(pool: &PgPool, id: i32) -> eyre::Result<Vec<FurAffinityFile>> {
    let results = sqlx::query_file_as!(DbFurAffinityFile, "queries/lookup_furaffinity_id.sql", id)
        .map(FurAffinityFile::from)
        .fetch_all(pool)
        .await?;

    Ok(results)
}
