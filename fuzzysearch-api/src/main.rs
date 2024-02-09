use std::{fmt::Display, net::SocketAddr, sync::Arc};

use axum::{
    async_trait,
    http::{header::HeaderName, Method, StatusCode},
    middleware,
    response::{IntoResponse, Redirect},
    routing, Extension, Router,
};
use axum_tracing_opentelemetry::middleware::OtelAxumLayer;
use bkapi_client::BKApiClient;
use clap::Parser;
use enum_map::Enum;
use eyre::Report;
use lazy_static::lazy_static;
use prometheus::{register_histogram, Histogram};
use reqwest::Url;
use serde::{Deserialize, Serialize};
use sqlx::PgPool;
use tower::ServiceBuilder;
use tower_http::{
    cors::{Any, CorsLayer},
    services::ServeDir,
    trace::TraceLayer,
};
use tower_sessions::{
    cookie::time::Duration, session::Record, session_store, SessionManagerLayer, SessionStore,
};
use utoipa::OpenApi;
use utoipa_swagger_ui::SwaggerUi;

mod api;
mod db;
mod selfserve;

lazy_static! {
    static ref IMAGE_LOAD: Histogram = register_histogram!(
        "fuzzysearch_api_image_load_seconds",
        "Amount of time to load an image."
    )
    .unwrap();
    static ref IMAGE_HASH: Histogram = register_histogram!(
        "fuzzysearch_api_image_hash_seconds",
        "Amount of time to hash an image."
    )
    .unwrap();
}

#[derive(clap::Parser)]
struct Config {
    #[clap(long, env)]
    bkapi_endpoint: String,
    #[clap(long, env)]
    database_url: String,

    #[clap(long, env)]
    session_secret: String,
    #[clap(long, env)]
    rp_id: String,
    #[clap(long, env)]
    rp_name: Option<String>,

    #[clap(long, env)]
    metrics_host: SocketAddr,
    #[clap(long, env)]
    json_logs: bool,

    #[clap(long, env)]
    unleash_api_url: String,
    #[clap(long, env)]
    unleash_secret: String,
}

#[derive(Clone, Debug, Serialize, Deserialize, Enum)]
enum Features {
    #[serde(rename = "fuzzysearch.api.self-serve")]
    SelfServe,
    #[serde(rename = "fuzzysearch.api.self-serve.discoverable-auth")]
    DiscoverableAuth,
}

type Unleash = foxlib::flags::Unleash<Features>;

#[tokio::main]
async fn main() -> eyre::Result<()> {
    let _ = dotenvy::dotenv();

    let config = Config::parse();

    foxlib::trace::init(foxlib::trace::TracingConfig {
        namespace: "fuzzysearch-next",
        name: "fuzzysearch-api",
        version: env!("CARGO_PKG_VERSION"),
        otlp: config.json_logs,
    });

    foxlib::MetricsServer::serve(config.metrics_host, true).await;

    let bkapi = BKApiClient::new(config.bkapi_endpoint);
    let pool = PgPool::connect(&config.database_url).await?;

    let client = reqwest::ClientBuilder::default()
        .timeout(std::time::Duration::from_secs(10))
        .build()?;

    let unleash = foxlib::flags::client::<Features>(
        "fuzzysearch-api",
        &config.unleash_api_url,
        config.unleash_secret,
    )
    .await
    .expect("could not create unleash client");

    let cors = CorsLayer::new()
        .allow_methods([Method::GET, Method::POST])
        .allow_origin(Any)
        .allow_headers([HeaderName::from_static("x-api-key")]);

    let api_layer = ServiceBuilder::new()
        .layer(cors)
        .layer(Extension(bkapi))
        .layer(Extension(client));

    let authenticated_api = Router::new()
        .route("/hashes", routing::get(api::search_image_by_hashes))
        .route("/image", routing::post(api::search_image_by_upload))
        .route("/url", routing::get(api::search_image_by_url))
        .route(
            "/file/furaffinity",
            routing::get(api::lookup_furaffinity_file),
        )
        .route_layer(middleware::from_fn(db::extract_api_key));

    let api = Router::new()
        .merge(authenticated_api)
        .route("/handle/:service", routing::get(api::check_handle))
        .route("/dump/latest", routing::get(api::dump_latest))
        .layer(api_layer);

    let app_layer = ServiceBuilder::new()
        .layer(Extension(pool.clone()))
        .layer(Extension(unleash))
        .layer(TraceLayer::new_for_http());

    let url = Url::parse(&format!("https://{}", config.rp_id)).unwrap();

    let webauthn = webauthn_rs::WebauthnBuilder::new(&config.rp_id, &url)
        .unwrap()
        .rp_name(config.rp_name.as_ref().unwrap_or(&config.rp_id))
        .allow_subdomains(true)
        .build()
        .unwrap();
    let webauthn = Arc::new(webauthn);

    let store = DbStore { pool };
    store.cleanup_task().await;

    let selfserve_session = SessionManagerLayer::new(store)
        .with_name("fuzzysearch.selfserve")
        .with_same_site(tower_sessions::cookie::SameSite::Lax)
        .with_expiry(tower_sessions::Expiry::OnInactivity(Duration::days(7)));

    let app = Router::new()
        .route("/", routing::get(|| async { Redirect::to("/swagger-ui") }))
        .route("/healthz", routing::get(|| async { "OK" }))
        .merge(
            SwaggerUi::new("/swagger-ui")
                .url("/api-doc/openapi.json", api::FuzzySearchApi::openapi()),
        )
        .nest("/v1", api.clone())
        .merge(api)
        .nest(
            "/selfserve",
            selfserve::router()
                .layer(selfserve_session)
                .layer(Extension(webauthn)),
        )
        .nest_service(
            "/assets",
            routing::get_service(ServeDir::new("./dist"))
                .handle_error(|_| async { StatusCode::NOT_FOUND }),
        )
        .layer(app_layer)
        .layer(OtelAxumLayer::default());

    let addr = SocketAddr::from(([0, 0, 0, 0], 8080));
    let listener = tokio::net::TcpListener::bind(addr).await?;
    tracing::info!("starting on {}", addr);
    axum::serve(listener, app).await?;

    Ok(())
}

pub struct ReportError(Report);

impl Display for ReportError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        self.0.fmt(f)
    }
}

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

#[derive(Clone, Debug)]
struct DbStore {
    pool: PgPool,
}

impl DbStore {
    async fn cleanup_task(&self) {
        let store = self.clone();
        let period = std::time::Duration::from_secs(60 * 5);

        tokio::task::spawn(async move {
            let mut interval = tokio::time::interval(period);

            loop {
                interval.tick().await;
                tracing::debug!("running store cleanup");

                match sqlx::query_file!("queries/session/cleanup_store.sql")
                    .execute(&store.pool)
                    .await
                {
                    Ok(result) => {
                        tracing::debug!(rows_affected = result.rows_affected(), "cleaned up store")
                    }
                    Err(err) => tracing::error!("could not clean up session store: {err}"),
                }
            }
        });
    }
}

#[async_trait]
impl SessionStore for DbStore {
    async fn save(&self, record: &Record) -> session_store::Result<()> {
        let id = record.id.0;
        let expires_at =
            chrono::DateTime::<chrono::Utc>::from_timestamp(record.expiry_date.unix_timestamp(), 0)
                .unwrap();
        let data = serde_json::to_value(record)
            .map_err(|err| session_store::Error::Encode(err.to_string()))?;

        sqlx::query_file!(
            "queries/session/store_session.sql",
            id.to_string(),
            expires_at,
            data
        )
        .execute(&self.pool)
        .await
        .map_err(|err| session_store::Error::Backend(err.to_string()))?;

        Ok(())
    }

    async fn load(
        &self,
        session_id: &tower_sessions::session::Id,
    ) -> session_store::Result<Option<Record>> {
        let data =
            sqlx::query_file_scalar!("queries/session/load_session.sql", session_id.0.to_string())
                .fetch_optional(&self.pool)
                .await
                .map_err(|err| session_store::Error::Backend(err.to_string()))?
                .flatten()
                .and_then(|data| serde_json::from_value(data).ok());

        Ok(data)
    }

    async fn delete(&self, session_id: &tower_sessions::session::Id) -> session_store::Result<()> {
        sqlx::query_file!(
            "queries/session/destroy_session.sql",
            session_id.0.to_string()
        )
        .execute(&self.pool)
        .await
        .map_err(|err| session_store::Error::Backend(err.to_string()))?;

        Ok(())
    }
}

async fn hash_image(buf: bytes::Bytes) -> eyre::Result<i64> {
    let span = tracing::info_span!("hash_image");

    let hash = tokio::task::spawn_blocking(move || -> eyre::Result<i64> {
        let _enter = span.entered();

        let load_timer = IMAGE_LOAD.start_timer();
        let im = foxlib::hash::image::load_from_memory(&buf)?;
        tracing::debug!(duration = load_timer.stop_and_record(), "loaded image");

        let hash_timer = IMAGE_HASH.start_timer();
        let hash = foxlib::hash::ImageHasher::default().hash_image(&im);
        tracing::debug!(duration = hash_timer.stop_and_record(), "hashed image");

        Ok(hash.into())
    })
    .await??;

    Ok(hash)
}
