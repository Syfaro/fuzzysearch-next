use std::{collections::HashMap, time::Duration};

use axum::{
    extract::Request,
    http::{HeaderMap, HeaderName, HeaderValue, StatusCode},
    middleware::Next,
    response::Response,
    RequestExt,
};
use axum_extra::TypedHeader;
use bkapi_client::BKApiClient;
use chrono::TimeZone;
use eyre::Context;
use headers::{authorization::Bearer, Authorization};
use lazy_static::lazy_static;
use prometheus::{
    register_histogram, register_int_counter, register_int_counter_vec, Histogram, IntCounter,
    IntCounterVec,
};
use serde::Serialize;
use sqlx::PgPool;
use uuid::Uuid;

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
    pub id: Uuid,
    pub account_id: Uuid,
    pub token: String,
    pub name: String,
    pub name_limit: i32,
    pub image_limit: i32,
    pub hash_limit: i32,
}

pub async fn extract_api_key(mut req: Request, next: Next) -> Result<Response, StatusCode> {
    let api_key = if let Ok(header) = req
        .extract_parts::<TypedHeader<Authorization<Bearer>>>()
        .await
    {
        header.token().to_string()
    } else if let Some(value) = req.headers().get("x-api-key") {
        String::from_utf8_lossy(value.as_bytes()).to_string()
    } else {
        return Err(StatusCode::UNAUTHORIZED);
    };

    let pool = match req.extensions().get::<PgPool>() {
        Some(pool) => pool,
        None => return Err(StatusCode::INTERNAL_SERVER_ERROR),
    };

    let api_key = sqlx::query_file_as!(UserApiKey, "queries/lookup_api_key.sql", &api_key)
        .fetch_optional(pool)
        .await
        .map_err(|_err| StatusCode::INTERNAL_SERVER_ERROR)?
        .ok_or(StatusCode::UNAUTHORIZED)?;

    tracing::debug!(
        api_key_id = %api_key.id,
        account_id = %api_key.account_id,
        "found valid api key in request: {}",
        api_key.name
    );

    req.extensions_mut().insert(api_key);

    Ok(next.run(req).await)
}

#[derive(Debug, Serialize)]
struct BucketCount<'a> {
    bucket: &'a str,
    count: i32,
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
    pub allowed: i32,
    pub used: i32,
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
    #[tracing::instrument(err, skip_all, fields(api_key_id = %self.id))]
    pub async fn rate_limit(
        &self,
        pool: &PgPool,
        buckets: &[(&str, i32)],
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

    fn buckets(&self) -> HashMap<String, i32> {
        [
            ("name".to_string(), self.name_limit),
            ("image".to_string(), self.image_limit),
            ("hash".to_string(), self.hash_limit),
        ]
        .into_iter()
        .collect()
    }
}

#[tracing::instrument(err, skip(bkapi, nats))]
pub async fn lookup_hashes(
    bkapi: &BKApiClient,
    nats: &async_nats::Client,
    hashes: &[i64],
    distance: u64,
    refresh_days: Option<i64>,
) -> eyre::Result<Vec<SearchResult>> {
    tracing::info!(distance, "starting lookup for hashes: {:?}", hashes);

    let bkapi_timer = BKAPI_TIME.start_timer();
    let related_hashes = bkapi.search_many(hashes, distance).await?;
    tracing::debug!(
        duration = bkapi_timer.stop_and_record(),
        "got results from bkapi"
    );

    let all_hashes: Vec<_> = related_hashes
        .iter()
        .flat_map(|results| results.hashes.iter())
        .map(|search| search.hash.to_be_bytes())
        .collect();

    let mut hash_lookup = HashMap::with_capacity(related_hashes.len());
    for related_hash in related_hashes {
        for found_hash in related_hash.hashes {
            hash_lookup.insert(found_hash.hash, (related_hash.hash, found_hash.distance));
        }
    }

    let req = FetchRequest {
        query: SubmissionQuery::PerceptualHash { hashes: all_hashes },
        policy: match refresh_days {
            Some(days) if distance <= 3 => FetchPolicy::Maybe {
                older_than: chrono::Utc::now() - chrono::Duration::try_days(days).unwrap(),
                return_stale: true,
            },
            _ => FetchPolicy::Never,
        },
        timeout: Some(Duration::from_secs(10)),
    };
    tracing::debug!("fetch policy: {:?}", req.policy);

    let msg = nats
        .request(
            "fuzzysearch.loader.fetch".to_string(),
            bytes::Bytes::from(serde_json::to_vec(&req)?),
        )
        .await
        .map_err(|err| eyre::eyre!("request error: {err}"))?;
    let resp: FetchResponse = serde_json::from_slice(&msg.payload)?;

    let results: Vec<_> = resp
        .submissions
        .into_iter()
        .flat_map(|fetched_submission| match fetched_submission {
            FetchedSubmission {
                submission: FetchedSubmissionData::Success { submission, .. },
                ..
            } => Some(submission),
            _ => None,
        })
        .flat_map(|submission| {
            let media = submission.media.first()?;
            let media_frame = media.frames.as_deref().unwrap_or_default().first()?;
            let hash = media_frame.perceptual_gradient?;
            let hash_info = hash_lookup.get(&hash)?;

            let site_info = match submission.site {
                Site::FurAffinity => SiteInfo::FurAffinity {
                    file_id: media
                        .extra
                        .clone()
                        .unwrap_or_default()
                        .get("file_id")
                        .and_then(|file_id| file_id.as_str())
                        .and_then(|file_id| file_id.parse().ok())
                        .unwrap_or_default(),
                },
                Site::E621 => SiteInfo::E621 {
                    sources: submission
                        .extra
                        .clone()
                        .unwrap_or_default()
                        .get("sources")
                        .and_then(|sources| serde_json::from_value(sources.clone()).ok())
                        .unwrap_or_default(),
                },
                Site::Weasyl => SiteInfo::Weasyl,
                Site::Twitter => SiteInfo::Twitter,
            };

            Some(SearchResult {
                site_id: submission.submission_id.parse().unwrap_or_default(),
                site_id_str: submission.submission_id,
                url: media.url.clone().unwrap_or_default(),
                filename: media
                    .extra
                    .clone()
                    .unwrap_or_default()
                    .get("file_name")
                    .and_then(|name| name.as_str())
                    .map(|name| name.to_string())
                    .unwrap_or_default(),
                artists: Some(
                    submission
                        .artists
                        .into_iter()
                        .map(|artist| artist.name)
                        .collect(),
                ),
                rating: submission.rating,
                posted_at: submission.posted_at,
                tags: submission.tags,
                sha256: media.file_sha256.as_ref().map(hex::encode),
                hash: Some(hash),
                hash_str: Some(hash.to_string()),
                distance: Some(hash_info.1 as i64),
                searched_hash: Some(hash_info.0),
                searched_hash_str: Some(hash_info.0.to_string()),
                deleted: Some(submission.deleted),
                retrieved_at: submission.retrieved_at,
                site_info,
            })
        })
        .collect();

    tracing::info!(len = results.len(), "found results");

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
