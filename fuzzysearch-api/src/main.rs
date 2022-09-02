use std::{collections::HashMap, net::SocketAddr, time::Duration};

use axum::{
    extract::{Multipart, Path, Query},
    http::{header::HeaderName, HeaderMap, HeaderValue, Method, Request, StatusCode},
    middleware::{self, Next},
    response::{IntoResponse, Response},
    routing, Extension, Json, Router, Server,
};
use bkapi_client::BKApiClient;
use bytes::BufMut;
use clap::Parser;
use eyre::{Context, Report};
use pomsky_macro::pomsky;
use regex::Regex;
use serde::{Deserialize, Serialize};
use sqlx::PgPool;
use tower::ServiceBuilder;
use tower_http::{
    cors::{Any, CorsLayer},
    trace::TraceLayer,
};
use utoipa::{
    openapi::security::{ApiKey, ApiKeyValue, SecurityScheme},
    IntoParams, Modify, OpenApi, ToSchema,
};
use utoipa_swagger_ui::SwaggerUi;

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
struct FuzzySearchApi;

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

#[derive(Clone)]
struct UserApiKey {
    id: i32,
    user_id: i32,
    name: Option<String>,
    name_limit: i16,
    image_limit: i16,
    hash_limit: i16,
}

#[derive(Debug, Serialize)]
struct BucketCount<'a> {
    bucket: &'a str,
    count: i16,
}

struct RateLimitBucket {
    bucket: String,
    allowed: i16,
    used: i16,
}

trait RateLimitBucketsHeaders {
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
            let remaining = u16::try_from(bucket.allowed)
                .wrap_err("Allowed should always fit in u16")?
                .saturating_sub(
                    u16::try_from(bucket.used).wrap_err("Used should always fit in u16")?,
                );
            let value = HeaderValue::from_str(&remaining.to_string())?;
            headers.insert(name, value);
        }

        Ok(headers)
    }
}

impl UserApiKey {
    async fn rate_limit(
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

async fn extract_api_key<B>(mut req: Request<B>, next: Next<B>) -> Result<Response, StatusCode> {
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

#[derive(clap::Parser)]
struct Config {
    #[clap(long, env)]
    bkapi_endpoint: String,
    #[clap(long, env)]
    database_url: String,
}

#[tokio::main]
async fn main() -> eyre::Result<()> {
    let _ = dotenv::dotenv();
    tracing_subscriber::fmt::init();
    let config = Config::parse();

    let bkapi = BKApiClient::new(config.bkapi_endpoint);

    let client = reqwest::ClientBuilder::default()
        .timeout(Duration::from_secs(10))
        .build()?;

    let pool = PgPool::connect(&config.database_url).await?;

    let cors = CorsLayer::new()
        .allow_methods([Method::GET, Method::POST])
        .allow_origin(Any)
        .allow_headers([HeaderName::from_static("x-api-key")]);

    let api_layer = ServiceBuilder::new()
        .layer(cors)
        .layer(Extension(bkapi))
        .layer(Extension(client));

    let authenticated_api = Router::new()
        .route("/hashes", routing::get(search_image_by_hashes))
        .route("/image", routing::post(search_image_by_upload))
        .route("/url", routing::get(search_image_by_url))
        .route("/file/furaffinity", routing::get(lookup_furaffinity_file))
        .route_layer(middleware::from_fn(extract_api_key));

    let api = Router::new()
        .merge(authenticated_api)
        .route("/handle/:service", routing::get(check_handle))
        .layer(api_layer);

    let app_layer = ServiceBuilder::new()
        .layer(Extension(pool))
        .layer(TraceLayer::new_for_http());

    let app = Router::new()
        .merge(
            SwaggerUi::new("/swagger-ui/*tail")
                .url("/api-doc/openapi.json", FuzzySearchApi::openapi()),
        )
        .merge(api)
        .layer(app_layer);

    let addr = SocketAddr::from(([127, 0, 0, 1], 8080));
    tracing::info!("starting on {}", addr);

    Server::bind(&addr).serve(app.into_make_service()).await?;

    Ok(())
}

struct ReportError(Report);

impl<R> From<R> for ReportError
where
    R: Into<Report>,
{
    fn from(err: R) -> Self {
        ReportError(err.into())
    }
}

impl IntoResponse for ReportError {
    fn into_response(self) -> axum::response::Response {
        (
            StatusCode::INTERNAL_SERVER_ERROR,
            format!("Internal server error: {}", self.0),
        )
            .into_response()
    }
}

#[derive(Debug, Serialize, Deserialize, ToSchema)]
#[serde(rename_all = "snake_case")]
enum Service {
    Twitter,
}

impl std::fmt::Display for Service {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Twitter => write!(f, "Twitter"),
        }
    }
}

#[derive(Debug, Serialize, Deserialize, IntoParams)]
struct HandleQuery {
    handle: String,
}

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
async fn check_handle(
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
struct SearchByUrlQuery {
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

#[derive(Debug, Serialize, Deserialize, IntoParams)]
struct HashesQuery {
    hash: String,
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
async fn search_image_by_hashes(
    Extension(pool): Extension<PgPool>,
    Extension(bkapi): Extension<BKApiClient>,
    Extension(api_key): Extension<UserApiKey>,
    Query(query): Query<HashesQuery>,
) -> Result<impl IntoResponse, ReportError> {
    let hashes: Vec<_> = query
        .hash
        .split(',')
        .filter_map(|hash| hash.parse::<i64>().ok())
        .collect();

    let headers = rate_limit!(&pool, api_key, &[("image", hashes.len() as i16)]);

    let found_images = lookup_hashes(&bkapi, &pool, &hashes, 3).await?;

    Ok((StatusCode::OK, headers, Json(found_images)).into_response())
}

#[allow(dead_code)]
#[derive(ToSchema)]
struct Image {
    #[schema(value_type = String, format = Binary)]
    image: Vec<u8>,
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
async fn search_image_by_upload(
    Extension(pool): Extension<PgPool>,
    Extension(bkapi): Extension<BKApiClient>,
    Extension(api_key): Extension<UserApiKey>,
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
    let found_images = lookup_hashes(&bkapi, &pool, &hashes, 3).await?;

    Ok((StatusCode::OK, headers, Json(found_images)).into_response())
}

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
async fn search_image_by_url(
    Extension(pool): Extension<PgPool>,
    Extension(bkapi): Extension<BKApiClient>,
    Extension(client): Extension<reqwest::Client>,
    Extension(api_key): Extension<UserApiKey>,
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

    let found_images = lookup_hashes(&bkapi, &pool, &[hash], 3).await?;

    Ok((StatusCode::OK, headers, Json(found_images)).into_response())
}

fn get_hasher() -> img_hash::Hasher<[u8; 8]> {
    img_hash::HasherConfig::with_bytes_type::<[u8; 8]>()
        .hash_alg(img_hash::HashAlg::Gradient)
        .hash_size(8, 8)
        .preproc_dct()
        .to_hasher()
}

async fn hash_image(buf: bytes::Bytes) -> eyre::Result<i64> {
    let hash = tokio::task::spawn_blocking(move || -> eyre::Result<i64> {
        let im = image::load_from_memory(&buf)?;
        let hash = get_hasher().hash_image(&im);

        let bytes: [u8; 8] = hash.as_bytes().try_into()?;

        Ok(i64::from_be_bytes(bytes))
    })
    .await??;

    Ok(hash)
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

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq, Hash, ToSchema)]
#[serde(rename_all = "lowercase")]
pub enum Rating {
    General,
    Mature,
    Adult,
}

impl std::str::FromStr for Rating {
    type Err = &'static str;

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        let rating = match s {
            "g" | "s" | "general" => Rating::General,
            "m" | "q" | "mature" => Rating::Mature,
            "a" | "e" | "adult" | "explicit" => Rating::Adult,
            _ => return Err("unknown rating"),
        };

        Ok(rating)
    }
}

#[derive(Clone, Debug, Serialize, Deserialize, ToSchema)]
#[serde(tag = "site", content = "site_info")]
pub enum SiteInfo {
    FurAffinity {
        file_id: i32,
    },
    #[serde(rename = "e621")]
    E621 {
        sources: Vec<String>,
    },
    Twitter,
    Weasyl,
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

#[derive(Clone, Debug, Default, Serialize, Deserialize, ToSchema)]
struct SearchResult {
    pub site_id: i64,
    pub site_id_str: String,
    pub url: String,
    pub filename: String,
    pub artists: Option<Vec<String>>,
    pub rating: Option<Rating>,
    pub posted_at: Option<chrono::DateTime<chrono::Utc>>,
    pub sha256: Option<String>,

    #[serde(skip_serializing_if = "Option::is_none")]
    pub hash: Option<i64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub hash_str: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub distance: Option<i64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub searched_hash: Option<i64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub searched_hash_str: Option<String>,
}

async fn lookup_hashes(
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

#[derive(Debug, Serialize, Deserialize, ToSchema)]
struct FurAffinityFile {
    pub id: i32,
    pub file_id: Option<i32>,
    pub artist: Option<String>,
    pub hash: Option<i64>,
    pub hash_str: Option<String>,
    pub url: Option<String>,
    pub filename: Option<String>,
    pub rating: Option<Rating>,
    pub posted_at: Option<chrono::DateTime<chrono::Utc>>,
    pub file_size: Option<i32>,
    pub sha256: Option<String>,
    pub updated_at: Option<chrono::DateTime<chrono::Utc>>,
    pub deleted: bool,
    pub tags: Vec<String>,
}

impl FurAffinityFile {
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
            url: Self::replace_cdn_host(file.url),
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

#[derive(Debug, Serialize, Deserialize, IntoParams)]
struct FurAffinityFileQuery {
    pub search: String,
}

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
async fn lookup_furaffinity_file(
    Extension(pool): Extension<PgPool>,
    Extension(api_key): Extension<UserApiKey>,
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

        sqlx::query_file_as!(
            DbFurAffinityFile,
            "queries/lookup_furaffinity_filename.sql",
            query.search
        )
        .fetch_all(&pool)
        .await?
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

        sqlx::query_file_as!(
            DbFurAffinityFile,
            "queries/lookup_furaffinity_filename.sql",
            last_segment
        )
        .fetch_all(&pool)
        .await?
    } else if let Ok(id) = query.search.parse::<i32>() {
        sqlx::query_file_as!(DbFurAffinityFile, "queries/lookup_furaffinity_id.sql", id)
            .fetch_all(&pool)
            .await?
    } else {
        tracing::trace!("search data was unknown");
        return Ok((StatusCode::BAD_REQUEST, headers).into_response());
    };

    let results: Vec<FurAffinityFile> = results.into_iter().map(From::from).collect();

    Ok((StatusCode::OK, headers, Json(results)).into_response())
}
