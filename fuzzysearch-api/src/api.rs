use axum::{
    extract::{Multipart, Path, Query},
    http::StatusCode,
    response::IntoResponse,
    Extension, Json,
};
use bkapi_client::BKApiClient;
use bytes::BufMut;
use eyre::Context;
use pomsky_macro::pomsky;
use regex::Regex;
use serde::{Deserialize, Serialize};
use sqlx::PgPool;
use utoipa::{
    openapi::security::{ApiKey, ApiKeyValue, SecurityScheme},
    IntoParams, Modify, OpenApi, ToSchema,
};

use fuzzysearch_common::*;

use crate::{
    db::{self, RateLimitBucketsHeaders},
    hash_image, ReportError,
};

#[derive(OpenApi)]
#[openapi(
    paths(
        search_image_by_hashes,
        search_image_by_upload,
        search_image_by_url,
        check_handle,
        lookup_furaffinity_file,
    ),
    components(
        schemas(Service, UrlError, SearchResult, Rating, SiteInfo, Image, ImageError, FurAffinityFile)
    ),
    modifiers(&ApiTokenAddon),
    tags(
        (name = "fuzzysearch-api", description = "FuzzySearch image search API")
    )
)]
pub struct FuzzySearchApi;

struct ApiTokenAddon;

impl Modify for ApiTokenAddon {
    fn modify(&self, openapi: &mut utoipa::openapi::OpenApi) {
        if let Some(components) = openapi.components.as_mut() {
            components.add_security_scheme(
                "api_key",
                SecurityScheme::ApiKey(ApiKey::Header(ApiKeyValue::new("x-api-key"))),
            )
        }
    }
}

macro_rules! rate_limit {
    ($pool:expr, $api_key:expr, $buckets:expr) => {{
        let (exceeded, buckets) = $api_key.rate_limit($pool, $buckets).await?;
        let headers = buckets.headers()?;

        if exceeded {
            return Ok((StatusCode::TOO_MANY_REQUESTS, headers).into_response());
        }

        headers
    }};
}

#[derive(Debug, Serialize, Deserialize, IntoParams)]
pub struct HandleQuery {
    /// The handle to search for, case-insensitive.
    handle: String,
}

/// Check if a handle is being indexed for a given service.
#[utoipa::path(
    get,
    path = "/handle/{service}",
    responses(
        (status = 200, description = "Handle looked up successfully", body = bool),
    ),
    params(
        ("service" = Service, Path, description = "Service to check handle"),
        HandleQuery,
    )
)]
pub async fn check_handle(
    Extension(pool): Extension<PgPool>,
    Path(service): Path<Service>,
    Query(query): Query<HandleQuery>,
) -> Result<impl IntoResponse, ReportError> {
    let exists = match service {
        Service::Twitter => {
            sqlx::query_file_scalar!("queries/check_handle_twitter.sql", query.handle)
                .fetch_one(&pool)
                .await?
        }
    };

    Ok((StatusCode::OK, Json(exists.unwrap_or(false))))
}

#[derive(Debug, Serialize, Deserialize, IntoParams)]
pub struct HashesQuery {
    /// Hashes to search for, comma separated.
    hash: String,
}

/// Search for images by hashes.
#[utoipa::path(
    get,
    path = "/hashes",
    responses(
        (status = 200, description = "Image lookup completed successfully", body = [SearchResult]),
        (status = 429, description = "Rate limit exhausted"),
        (status = 401, description = "Invalid or missing API token"),
    ),
    security(
        ("api_key" = [])
    ),
    params(
        HashesQuery,
    )
)]
#[axum::debug_handler]
pub async fn search_image_by_hashes(
    Extension(pool): Extension<PgPool>,
    Extension(bkapi): Extension<BKApiClient>,
    Extension(api_key): Extension<db::UserApiKey>,
    Query(query): Query<HashesQuery>,
) -> Result<impl IntoResponse, ReportError> {
    let hashes: Vec<_> = query
        .hash
        .split(',')
        .filter_map(|hash| hash.parse::<i64>().ok())
        .collect();

    let headers = rate_limit!(&pool, api_key, &[("image", hashes.len() as i16)]);

    let found_images = db::lookup_hashes(&bkapi, &pool, &hashes, 3).await?;

    Ok((StatusCode::OK, headers, Json(found_images)).into_response())
}

#[derive(Debug, Serialize, Deserialize, ToSchema)]
#[serde(rename_all = "snake_case")]
enum ImageError {
    #[schema(example = "Image was missing")]
    Missing,
    #[schema(example = "Image was unknown type")]
    Unknown,
    #[schema(example = "Image at URL was too large")]
    TooLarge,
}

/// Search for images by image upload.
#[utoipa::path(
    post,
    path = "/image",
    request_body(content = Image, content_type = "multipart/form-data"),
    responses(
        (status = 200, description = "Image lookup completed successfully", body = [SearchResult]),
        (status = 429, description = "Rate limit exhausted"),
        (status = 400, description = "Image was invalid", body = ImageError),
        (status = 401, description = "Invalid or missing API token"),
    ),
    security(
        ("api_key" = [])
    )
)]
pub async fn search_image_by_upload(
    Extension(pool): Extension<PgPool>,
    Extension(bkapi): Extension<BKApiClient>,
    Extension(api_key): Extension<db::UserApiKey>,
    mut multipart: Multipart,
) -> Result<impl IntoResponse, ReportError> {
    const MAX_UPLOAD_SIZE: usize = 25_000_000;

    let mut hashes = Vec::new();

    while let Some(mut field) = multipart.next_field().await? {
        if field.name() != Some("image") {
            continue;
        }
        tracing::debug!("found image field");

        let mut buf = bytes::BytesMut::new();
        while let Some(chunk) = field.chunk().await? {
            if buf.len() + chunk.len() > MAX_UPLOAD_SIZE {
                return Ok((StatusCode::OK, Json(ImageError::TooLarge)).into_response());
            }

            buf.put(chunk);
        }
        tracing::trace!(bytes = buf.len(), "finished reading image");

        let hash = match hash_image(buf.freeze()).await {
            Ok(hash) => hash,
            Err(_err) => {
                return Ok((StatusCode::BAD_REQUEST, Json(ImageError::Unknown)).into_response())
            }
        };
        tracing::trace!(hash, "hashed uploaded image");

        hashes.push(hash);
    }

    let headers = rate_limit!(
        &pool,
        api_key,
        &[
            ("hash", hashes.len() as i16),
            ("image", hashes.len() as i16)
        ]
    );

    tracing::debug!(count = hashes.len(), "hashed images in request");
    let found_images = db::lookup_hashes(&bkapi, &pool, &hashes, 3).await?;

    Ok((StatusCode::OK, headers, Json(found_images)).into_response())
}

#[derive(Debug, Serialize, Deserialize, IntoParams)]
pub struct SearchByUrlQuery {
    url: String,
}

#[derive(Debug, Serialize, Deserialize, ToSchema)]
#[serde(rename_all = "snake_case")]
enum UrlError {
    #[schema(example = "Image at URL was too large")]
    TooLarge,
    #[schema(example = "Image at URL was unavailable")]
    Unavailable,
}

/// Search for images by image available at URL.
#[utoipa::path(
    get,
    path = "/url",
    responses(
        (status = 200, description = "Image lookup completed successfully", body = [SearchResult]),
        (status = 429, description = "Rate limit exhausted"),
        (status = 400, description = "URL invalid or too large", body = UrlError),
        (status = 401, description = "Invalid or missing API token"),
    ),
    params(
        SearchByUrlQuery,
    ),
    security(
        ("api_key" = [])
    )
)]
pub async fn search_image_by_url(
    Extension(pool): Extension<PgPool>,
    Extension(bkapi): Extension<BKApiClient>,
    Extension(client): Extension<reqwest::Client>,
    Extension(api_key): Extension<db::UserApiKey>,
    Query(query): Query<SearchByUrlQuery>,
) -> Result<impl IntoResponse, ReportError> {
    const MAX_DOWNLOAD_SIZE: usize = 10_000_000;

    tracing::debug!("starting to download image");

    let headers = rate_limit!(&pool, api_key, &[("hash", 1), ("image", 1)]);

    let mut resp = match client
        .get(query.url)
        .send()
        .await
        .map(|resp| resp.error_for_status())
    {
        Ok(Ok(resp)) => resp,
        _ => {
            return Ok((
                StatusCode::BAD_REQUEST,
                headers,
                Json(UrlError::Unavailable),
            )
                .into_response())
        }
    };

    let content_length = resp
        .headers()
        .get("content-length")
        .and_then(|content_length| {
            String::from_utf8_lossy(content_length.as_bytes())
                .parse::<usize>()
                .ok()
        })
        .unwrap_or(0);
    if content_length > MAX_DOWNLOAD_SIZE {
        return Ok((StatusCode::BAD_REQUEST, headers, Json(UrlError::TooLarge)).into_response());
    }
    tracing::trace!(content_length, "got image content_length");

    let mut buf = bytes::BytesMut::with_capacity(content_length);
    while let Some(chunk) = resp.chunk().await.wrap_err("Could not fetch image chunk")? {
        if buf.len() + chunk.len() > MAX_DOWNLOAD_SIZE {
            return Ok((StatusCode::BAD_REQUEST, headers, Json(UrlError::TooLarge)).into_response());
        }

        buf.put(chunk);
    }
    tracing::trace!(bytes = buf.len(), "finished downloading image");

    let hash = match hash_image(buf.freeze()).await {
        Ok(hash) => hash,
        Err(_err) => {
            return Ok((
                StatusCode::BAD_REQUEST,
                headers,
                Json(UrlError::Unavailable),
            )
                .into_response())
        }
    };
    tracing::trace!(hash, "hashed image at url");

    let found_images = db::lookup_hashes(&bkapi, &pool, &[hash], 3).await?;

    Ok((StatusCode::OK, headers, Json(found_images)).into_response())
}

#[derive(Debug, Serialize, Deserialize, IntoParams)]
pub struct FurAffinityFileQuery {
    /// The search query. IDs, URLs, and filenames are accepted.
    pub search: String,
}

/// Get information about a file on FurAffinity.
#[utoipa::path(
    get,
    path = "/file/furaffinity",
    responses(
        (status = 200, description = "File lookup completed successfully", body = [FurAffinityFile]),
        (status = 400, description = "Unknown search input"),
        (status = 429, description = "Rate limit exhausted"),
    ),
    params(
        FurAffinityFileQuery,
    ),
    security(
        ("api_key" = [])
    )
)]
pub async fn lookup_furaffinity_file(
    Extension(pool): Extension<PgPool>,
    Extension(api_key): Extension<db::UserApiKey>,
    Query(query): Query<FurAffinityFileQuery>,
) -> Result<impl IntoResponse, ReportError> {
    let headers = rate_limit!(&pool, api_key, &[("name", 1)]);

    tracing::debug!("attempting to look up file: {}", query.search);

    let file_name = Regex::new(
        pomsky!(Start :id([digit]+ lazy) "." :name([!space]+) "." :extension([word]{3,4}) End),
    )
    .unwrap();

    let results = if file_name.is_match(&query.search) {
        tracing::trace!("searched appeared to be file name");

        db::lookup_furaffinity_file(&pool, &query.search).await?
    } else if let Ok(url) = url::Url::parse(&query.search) {
        tracing::trace!("search appeared to be url");
        static FURAFFINITY_HOSTS: &[&str] = &["facdn.net", "furaffinity.net"];
        if !matches!(url.host_str(), Some(host) if FURAFFINITY_HOSTS.iter().any(|fa_host| host.contains(fa_host)))
        {
            tracing::trace!("search url had unknown host");
            return Ok((StatusCode::BAD_REQUEST, headers).into_response());
        }

        let last_segment = url.path().split('/').last().unwrap_or_default();
        tracing::trace!("identified last segment: {}", last_segment);
        if !file_name.is_match(last_segment) {
            tracing::trace!("search url did not end in furaffinity file name");
            return Ok((StatusCode::BAD_REQUEST, headers).into_response());
        }

        db::lookup_furaffinity_file(&pool, last_segment).await?
    } else if let Ok(id) = query.search.parse::<i32>() {
        db::lookup_furaffinity_id(&pool, id).await?
    } else {
        tracing::trace!("search data was unknown");
        return Ok((StatusCode::BAD_REQUEST, headers).into_response());
    };

    let results: Vec<FurAffinityFile> = results.into_iter().map(From::from).collect();

    Ok((StatusCode::OK, headers, Json(results)).into_response())
}
