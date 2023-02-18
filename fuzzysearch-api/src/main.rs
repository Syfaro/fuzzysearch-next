use std::{fmt::Display, net::SocketAddr, time::Duration};

use axum::{
    http::{header::HeaderName, Method, StatusCode},
    middleware,
    response::{IntoResponse, Redirect},
    routing, Extension, Router, Server,
};
use axum_tracing_opentelemetry::opentelemetry_tracing_layer;
use bkapi_client::BKApiClient;
use clap::Parser;
use eyre::Report;
use lazy_static::lazy_static;
use prometheus::{register_histogram, Histogram};
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
    auth_secret: String,
    #[clap(long, env)]
    passwordless_secret: String,
    #[clap(long, env)]
    passwordless_public: String,

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

    let auth_secret = hex::decode(&config.auth_secret).expect("auth secret was not hex");
    if auth_secret.len() != 32 {
        panic!("wrong auth secret length");
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
        .layer(Extension(pool))
        .layer(Extension(client))
        .layer(TraceLayer::new_for_http());

    let selfserve_config = selfserve::Config {
        secret: auth_secret,

        passwordless_public: config.passwordless_public,
        passwordless_secret: config.passwordless_secret,
    };

    let app = Router::new()
        .route("/", routing::get(|| async { Redirect::to("/swagger-ui") }))
        .merge(
            SwaggerUi::new("/swagger-ui")
                .url("/api-doc/openapi.json", api::FuzzySearchApi::openapi()),
        )
        .merge(api)
        .nest(
            "/selfserve",
            selfserve::router().layer(Extension(selfserve_config)),
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
