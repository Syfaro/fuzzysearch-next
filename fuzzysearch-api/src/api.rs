use std::borrow::Cow;

use axum::{
    extract::{ws, Multipart, Query},
    http::StatusCode,
    response::IntoResponse,
    Extension, Json,
};
use bkapi_client::BKApiClient;
use bytes::BufMut;
use eyre::Context;
use futures::{SinkExt, StreamExt};
use serde::{Deserialize, Serialize};
use sqlx::PgPool;
use tokio::task::JoinHandle;
use tokio_util::sync::CancellationToken;
use utoipa::{
    openapi::security::{HttpAuthScheme, HttpBuilder, SecurityScheme},
    IntoParams, Modify, OpenApi, ToSchema,
};

use fuzzysearch_common::*;

use crate::{db, hash_image, ReportError};

#[derive(OpenApi)]
#[openapi(
    paths(
        search_image_by_hashes,
        search_image_by_upload,
        search_image_by_url,
        lookup_submissions,
        dump_latest,
    ),
    components(
        schemas(
            Artist,
            FetchedSubmission,
            FetchedSubmissionData,
            FetchStatus,
            Image,
            ImageError,
            Media,
            MediaFrame,
            Rating,
            SearchResult,
            Site,
            SiteInfo,
            Submission,
            SubmissionFetchForm,
            SubmissionFetchItem,
            UrlError,
        )
    ),
    modifiers(&ApiTokenAddon),
)]
pub struct FuzzySearchApi;

struct ApiTokenAddon;

impl Modify for ApiTokenAddon {
    fn modify(&self, openapi: &mut utoipa::openapi::OpenApi) {
        if let Some(components) = openapi.components.as_mut() {
            components.add_security_scheme(
                "api_key",
                SecurityScheme::Http(HttpBuilder::new().scheme(HttpAuthScheme::Bearer).build()),
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
pub struct HashesQuery {
    /// Hashes to search for, comma separated.
    #[serde(alias = "hashes")]
    hash: String,
    /// Maximum distance for search results.
    distance: Option<u64>,
}

/// Search for images by hashes.
#[utoipa::path(
    get,
    path = "/v1/hashes",
    responses(
        (status = 200, description = "Image lookup completed successfully", body = [SearchResult]),
        (status = 401, description = "Invalid or missing API token"),
        (status = 429, description = "Rate limit exhausted"),
    ),
    security(
        ("api_key" = [])
    ),
    params(
        HashesQuery,
    )
)]
#[tracing::instrument(err, skip_all, fields(hash = query.hash, distance = query.distance))]
pub async fn search_image_by_hashes(
    Extension(pool): Extension<PgPool>,
    Extension(bkapi): Extension<BKApiClient>,
    Extension(nats): Extension<async_nats::Client>,
    Extension(api_key): Extension<db::UserApiKey>,
    Query(query): Query<HashesQuery>,
) -> Result<impl IntoResponse, ReportError> {
    let hashes: Vec<_> = query
        .hash
        .split(',')
        .filter_map(|hash| hash.parse::<i64>().ok())
        .collect();

    let headers = rate_limit!(&pool, api_key, &[("image", hashes.len() as i32)]);

    let found_images = db::lookup_hashes(
        &bkapi,
        &nats,
        &hashes,
        query.distance.unwrap_or(3),
        Some(30),
    )
    .await?;

    Ok((StatusCode::OK, headers, Json(found_images)).into_response())
}

#[derive(Debug, Serialize, Deserialize, ToSchema)]
#[serde(rename_all = "snake_case")]
enum ImageError {
    Missing,
    Unknown,
    TooLarge,
}

/// Search for images by image upload.
#[utoipa::path(
    post,
    path = "/v1/image",
    request_body(content = Image, content_type = "multipart/form-data"),
    responses(
        (status = 200, description = "Image lookup completed successfully", body = [SearchResult]),
        (status = 400, description = "Image was invalid", body = ImageError),
        (status = 401, description = "Invalid or missing API token"),
        (status = 429, description = "Rate limit exhausted"),
    ),
    security(
        ("api_key" = [])
    )
)]
#[tracing::instrument(err, skip_all)]
pub async fn search_image_by_upload(
    Extension(pool): Extension<PgPool>,
    Extension(bkapi): Extension<BKApiClient>,
    Extension(nats): Extension<async_nats::Client>,
    Extension(api_key): Extension<db::UserApiKey>,
    mut multipart: Multipart,
) -> Result<impl IntoResponse, ReportError> {
    const MAX_UPLOAD_SIZE: usize = 25_000_000;

    let mut hashes = Vec::new();
    let mut distance = 3;

    while let Some(mut field) = multipart.next_field().await? {
        match field.name() {
            Some("distance") => {
                let text = field.text().await.ok();
                if let Some(val) = text.as_deref().and_then(|val| val.parse().ok()) {
                    distance = val;
                } else {
                    tracing::warn!(value = text, "ignoring incorrect distance value");
                }
                continue;
            }
            Some("image") => tracing::debug!("found image field"),
            field => {
                tracing::warn!(field, "got unknown field");
                continue;
            }
        }

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
            ("hash", hashes.len() as i32),
            ("image", hashes.len() as i32)
        ]
    );

    tracing::debug!(count = hashes.len(), "hashed images in request");
    let found_images = db::lookup_hashes(&bkapi, &nats, &hashes, distance, Some(30)).await?;

    Ok((StatusCode::OK, headers, Json(found_images)).into_response())
}

#[derive(Debug, Serialize, Deserialize, IntoParams)]
pub struct SearchByUrlQuery {
    url: String,
}

#[derive(Debug, Serialize, Deserialize, ToSchema)]
#[serde(rename_all = "snake_case")]
enum UrlError {
    TooLarge,
    Unavailable,
}

/// Search for images by image available at URL.
#[utoipa::path(
    get,
    path = "/v1/url",
    responses(
        (status = 200, description = "Image lookup completed successfully", body = [SearchResult]),
        (status = 400, description = "URL invalid or too large", body = UrlError),
        (status = 401, description = "Invalid or missing API token"),
        (status = 429, description = "Rate limit exhausted"),
    ),
    params(
        SearchByUrlQuery,
    ),
    security(
        ("api_key" = [])
    )
)]
#[tracing::instrument(err, skip_all)]
pub async fn search_image_by_url(
    Extension(pool): Extension<PgPool>,
    Extension(bkapi): Extension<BKApiClient>,
    Extension(nats): Extension<async_nats::Client>,
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
    tracing::trace!(content_length, "got image content_length");
    if content_length > MAX_DOWNLOAD_SIZE {
        return Ok((StatusCode::BAD_REQUEST, headers, Json(UrlError::TooLarge)).into_response());
    }

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

    let found_images = db::lookup_hashes(&bkapi, &nats, &[hash], 3, Some(30)).await?;

    Ok((StatusCode::OK, headers, Json(found_images)).into_response())
}

#[derive(Debug, Serialize, Deserialize, IntoParams, ToSchema)]
pub struct SubmissionFetchItem {
    pub site: Site,
    pub id: String,
}

#[derive(Debug, Serialize, Deserialize, IntoParams, ToSchema)]
pub struct SubmissionFetchForm {
    pub submissions: Vec<SubmissionFetchItem>,
    pub refresh_after_days: Option<i64>,
}

/// Lookup submissions, re-fetching data if too old.
#[utoipa::path(
    post,
    path = "/v2/submission",
    request_body = SubmissionFetchForm,
    responses(
        (status = 200, description = "Image lookup completed successfully", body = [FetchedSubmission]),
        (status = 400, description = "Invalid request", body = String),
        (status = 401, description = "Invalid or missing API token"),
        (status = 429, description = "Rate limit exhausted"),
    ),
    security(
        ("api_key" = [])
    )
)]
#[tracing::instrument(err, skip_all)]
pub async fn lookup_submissions(
    Extension(pool): Extension<PgPool>,
    Extension(nats): Extension<async_nats::Client>,
    Extension(api_key): Extension<db::UserApiKey>,
    Json(form): Json<SubmissionFetchForm>,
) -> Result<impl IntoResponse, ReportError> {
    let submission_ids: Vec<_> = form
        .submissions
        .into_iter()
        .map(|item| (item.site, item.id))
        .collect();

    let headers = rate_limit!(&pool, api_key, &[("hash", submission_ids.len() as i32)]);

    let req = FetchRequest {
        query: SubmissionQuery::SubmissionId { submission_ids },
        policy: if let Some(days) = form.refresh_after_days {
            if days < 1 {
                return Ok((
                    StatusCode::BAD_REQUEST,
                    "Submissions may only be loaded once per day",
                )
                    .into_response());
            }

            FetchPolicy::Maybe {
                older_than: chrono::Utc::now() - chrono::Duration::days(days),
                return_stale: true,
            }
        } else {
            FetchPolicy::Never
        },
        timeout: None,
    };
    tracing::debug!("fetch policy: {:?}", req.policy);

    let resp = nats
        .request(
            "fuzzysearch.loader.fetch".to_string(),
            bytes::Bytes::from(serde_json::to_vec(&req)?),
        )
        .await
        .map_err(|err| eyre::eyre!("request error: {err}"))?
        .payload;
    let resp: FetchResponse = serde_json::from_slice(&resp)?;

    Ok((StatusCode::OK, headers, Json(resp.submissions)).into_response())
}

/// Get the URL of the latest FuzzySearch database dump.
#[utoipa::path(
    get,
    path = "/v1/dump/latest",
    responses(
        (status = 200, description = "File lookup completed successfully", body = String),
    ),
)]
#[tracing::instrument(err, skip_all)]
pub async fn dump_latest(
    Extension(pool): Extension<PgPool>,
) -> Result<impl IntoResponse, ReportError> {
    let url = sqlx::query_file_scalar!("queries/latest_dump.sql")
        .fetch_one(&pool)
        .await?;

    Ok(url)
}

#[derive(Deserialize)]
pub struct SocketQuery {
    pub seq: Option<u64>,
}

pub async fn ws_live(
    Extension(nats): Extension<async_nats::Client>,
    Extension(token): Extension<CancellationToken>,
    Extension(_api_key): Extension<db::UserApiKey>,
    Query(query): Query<SocketQuery>,
    ws: ws::WebSocketUpgrade,
) -> Result<impl IntoResponse, ReportError> {
    use async_nats::jetstream::consumer::{AckPolicy, DeliverPolicy};

    let jetstream = async_nats::jetstream::new(nats);

    let stream = jetstream.get_stream("fuzzysearch-submissions").await?;

    let deliver_policy = match query.seq {
        Some(start_sequence) if start_sequence > 0 => {
            tracing::info!("starting consumer at seq {start_sequence}");
            DeliverPolicy::ByStartSequence { start_sequence }
        }
        Some(start_sequence) if start_sequence == 0 => {
            tracing::info!("starting consumer at beginning");
            DeliverPolicy::All
        }
        _ => {
            tracing::info!("starting consumer for new submissions");
            DeliverPolicy::New
        }
    };

    let consumer = stream
        .create_consumer(async_nats::jetstream::consumer::pull::Config {
            ack_policy: AckPolicy::None,
            deliver_policy,
            ..Default::default()
        })
        .await?;

    let stream = consumer.messages().await?;

    Ok(ws.on_upgrade(move |socket| handle_live_socket(socket, stream, token)))
}

#[derive(Serialize)]
struct Payload {
    seq: u64,
    submission: serde_json::Value,
}

async fn handle_live_socket(
    socket: ws::WebSocket,
    mut stream: async_nats::jetstream::consumer::pull::Stream,
    token: CancellationToken,
) {
    let (mut tx, mut rx) = socket.split();

    let tx_token = token.clone();
    let tx_task: JoinHandle<Result<_, ReportError>> = tokio::spawn(async move {
        loop {
            tokio::select! {
                _ = tx_token.cancelled() => {
                    tracing::info!("got cancellation, ending tx task");
                    if let Err(err) = tx.send(ws::Message::Close(Some(ws::CloseFrame {
                        code: ws::close_code::NORMAL,
                        reason: Cow::from("stream was cancelled"),
                    }))).await {
                        tracing::error!("could not send close frame: {err}");
                    }
                    break;
                }
                Some(Ok(msg)) = stream.next() => {
                    let submission: serde_json::Value = serde_json::from_slice(&msg.payload)?;

                    let payload = Payload {
                        seq: msg.info().map_err(|err| eyre::eyre!(err))?.stream_sequence,
                        submission,
                    };

                    let data = serde_json::to_vec(&payload)?;

                    if let Err(err) = tx
                        .send(ws::Message::Binary(data))
                        .await
                    {
                        tracing::error!("could not send message: {err}");
                        tx_token.cancel();
                    } else {
                        tracing::trace!(seq = payload.seq, "sent payload to client");
                    }
                }
            }
        }

        Ok(())
    });

    tokio::select! {
        _ = token.cancelled() => {
            tracing::info!("got cancellation");
        }
        None | Some(Err(_)) = rx.next() => {
            tracing::info!("recv error");
            token.cancel();
        }
        res = tx_task => {
            match res {
                Ok(Ok(_)) => tracing::info!("tx task ended"),
                Ok(Err(err)) => tracing::error!("tx task had error: {err}"),
                Err(err) => tracing::warn!("could not join tx task: {err}"),
            }
            token.cancel();
        }
    }
}
