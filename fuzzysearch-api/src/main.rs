use std::{fmt::Display, net::SocketAddr, sync::Arc, time::Duration};

use axum::{
    async_trait,
    http::{header::HeaderName, Method, StatusCode},
    middleware,
    response::{IntoResponse, Redirect},
    routing, Extension, Router, Server,
};
use axum_sessions::{
    async_session::{Session, SessionStore},
    SessionLayer,
};
use axum_tracing_opentelemetry::opentelemetry_tracing_layer;
use bkapi_client::BKApiClient;
use clap::Parser;
use eyre::Report;
use lazy_static::lazy_static;
use prometheus::{register_histogram, Histogram};
use reqwest::Url;
use sqlx::PgPool;
use tower::ServiceBuilder;
use tower_http::{
    cors::{Any, CorsLayer},
    services::ServeDir,
    trace::TraceLayer,
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
}

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

    let session_secret = hex::decode(&config.session_secret).expect("auth secret was not hex");
    if session_secret.len() != 64 {
        panic!("wrong session secret length");
    }

    foxlib::MetricsServer::serve(config.metrics_host, true).await;

    let bkapi = BKApiClient::new(config.bkapi_endpoint);
    let pool = PgPool::connect(&config.database_url).await?;

    let client = reqwest::ClientBuilder::default()
        .timeout(Duration::from_secs(10))
        .build()?;

    let cors = CorsLayer::new()
        .allow_methods([Method::GET, Method::POST])
        .allow_origin(Any)
        .allow_headers([HeaderName::from_static("x-api-key")]);

    let api_layer = ServiceBuilder::new().layer(cors).layer(Extension(bkapi));

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
        .layer(Extension(client))
        .layer(TraceLayer::new_for_http());

    let url = Url::parse(&format!("https://{}", config.rp_id)).unwrap();

    let webauthn = webauthn_rs::WebauthnBuilder::new(&config.rp_id, &url)
        .unwrap()
        .rp_name(config.rp_name.as_ref().unwrap_or(&config.rp_id))
        .build()
        .unwrap();
    let webauthn = Arc::new(webauthn);

    let store = DbStore { pool };
    let selfserve_session =
        SessionLayer::new(store, &session_secret).with_cookie_name("fuzzysearch.selfserve");

    let app = Router::new()
        .route("/", routing::get(|| async { Redirect::to("/swagger-ui") }))
        .merge(
            SwaggerUi::new("/swagger-ui")
                .url("/api-doc/openapi.json", api::FuzzySearchApi::openapi()),
        )
        .merge(api)
        .nest(
            "/selfserve",
            selfserve::router()
                .layer(selfserve_session)
                .layer(Extension(webauthn)),
        )
        .nest_service(
            "/assets",
            routing::get_service(ServeDir::new("./dist")).handle_error(
                |err: std::io::Error| async move { (StatusCode::NOT_FOUND, err.to_string()) },
            ),
        )
        .layer(app_layer)
        .layer(opentelemetry_tracing_layer());

    let addr = SocketAddr::from(([0, 0, 0, 0], 8080));
    tracing::info!("starting on {}", addr);

    Server::bind(&addr).serve(app.into_make_service()).await?;

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

#[async_trait]
impl SessionStore for DbStore {
    async fn load_session(
        &self,
        cookie_value: String,
    ) -> axum_sessions::async_session::Result<Option<axum_sessions::async_session::Session>> {
        let id = Session::id_from_cookie_value(&cookie_value)?;

        let data = sqlx::query_file_scalar!("queries/session/load_session.sql", id)
            .fetch_optional(&self.pool)
            .await?
            .flatten()
            .and_then(|data| serde_json::from_value(data).ok());

        Ok(data)
    }

    async fn store_session(
        &self,
        session: axum_sessions::async_session::Session,
    ) -> axum_sessions::async_session::Result<Option<String>> {
        let id = session.id();
        let expires_at = session.expiry();
        let data = serde_json::to_value(&session)?;

        sqlx::query_file!("queries/session/store_session.sql", id, expires_at, data)
            .execute(&self.pool)
            .await?;

        Ok(session.into_cookie_value())
    }

    async fn destroy_session(
        &self,
        session: axum_sessions::async_session::Session,
    ) -> axum_sessions::async_session::Result {
        let id = session.id();

        sqlx::query_file!("queries/session/destroy_session.sql", id)
            .execute(&self.pool)
            .await?;

        Ok(())
    }

    async fn clear_store(&self) -> axum_sessions::async_session::Result {
        sqlx::query_file!("queries/session/clear_store.sql")
            .execute(&self.pool)
            .await?;

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
