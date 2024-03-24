use std::{collections::HashMap, net::SocketAddr, sync::Arc, time::Duration};

use async_nats::service::ServiceExt;
use clap::Parser;
use futures::StreamExt;
use object_store::aws::AmazonS3Builder;
use sqlx::PgPool;
use tokio::sync::Semaphore;
use tokio_util::sync::CancellationToken;
use tracing::Instrument;

mod requests;
mod sites;

#[derive(Clone, Parser)]
pub struct Config {
    #[clap(long, env)]
    metrics_host: SocketAddr,
    #[clap(long, env)]
    json_logs: bool,

    #[clap(long, env)]
    nats_url: String,
    #[clap(long, env)]
    nats_nkey: String,

    #[clap(long, env)]
    database_url: String,
    #[clap(long, env)]
    run_migrations: bool,

    #[clap(long, env)]
    user_agent: Option<String>,

    #[clap(long, env)]
    store_bucket: String,
    #[clap(long, env, default_value = "4")]
    concurrent_fetches: u32,
    #[clap(long, env)]
    auto_fetch_submissions: bool,

    #[clap(long, env)]
    furaffinity_cookie_a: Option<String>,
    #[clap(long, env)]
    furaffinity_cookie_b: Option<String>,
    #[clap(long, env, default_value = "10000")]
    furaffinity_bot_threshold: u64,

    #[clap(long, env)]
    e621_login: Option<String>,
    #[clap(long, env)]
    e621_api_token: Option<String>,

    #[clap(long, env)]
    weasyl_api_token: Option<String>,
}

pub struct SiteContext {
    config: Config,
    client: reqwest::Client,
    pool: PgPool,
    nats: async_nats::Client,
    token: CancellationToken,
    object_store: object_store::aws::AmazonS3,
}

#[tokio::main]
async fn main() -> eyre::Result<()> {
    let _ = dotenvy::dotenv();

    let config = Config::parse();

    foxlib::trace_init(foxlib::TracingConfig {
        namespace: "fuzzysearch",
        name: env!("CARGO_PKG_NAME"),
        version: env!("CARGO_PKG_VERSION"),
        otlp: config.json_logs,
    });

    ffmpeg_next::init()?;

    let token = CancellationToken::new();

    let metrics = foxlib::MetricsServer::serve(config.metrics_host, false).await;

    let pool = PgPool::connect(&config.database_url).await?;
    if config.run_migrations {
        tracing::info!("running database migrations");
        sqlx::migrate!("../migrations").run(&pool).await?;
    }

    let nats = async_nats::ConnectOptions::with_nkey(config.nats_nkey.clone())
        .connect(config.nats_url.clone())
        .await?;

    let mut client_builder = reqwest::ClientBuilder::default().timeout(Duration::from_secs(10));
    if let Some(user_agent) = config.user_agent.as_ref() {
        tracing::debug!("adding user agent to http client");
        client_builder = client_builder.user_agent(user_agent);
    }
    let client = client_builder.build()?;

    let object_store = AmazonS3Builder::from_env()
        .with_bucket_name(&config.store_bucket)
        .build()?;

    let ctx = Arc::new(SiteContext {
        config: config.clone(),
        client,
        pool: pool.clone(),
        nats: nats.clone(),
        token: token.clone(),
        object_store,
    });

    let sites = Arc::new(sites::sites(ctx).await);

    let service = nats
        .service_builder()
        .start("fuzzysearch-loader", env!("CARGO_PKG_VERSION"))
        .await
        .unwrap();

    let fetch = service.endpoint("fuzzysearch.loader.fetch").await.unwrap();
    let watch = service.endpoint("fuzzysearch.loader.watch").await.unwrap();

    let requests = tokio_stream::StreamExt::merge(fetch, watch).take_until(token.cancelled());
    tokio::pin!(requests);

    metrics.set_ready(true);

    let token = token.clone();
    tokio::spawn(async move {
        tokio::signal::ctrl_c()
            .await
            .expect("could not install ctrl+c handler");
        tracing::info!("got ctrl+c, cancelling tasks");
        metrics.set_ready(false);
        token.cancel();
    });

    let active_requests = Arc::new(Semaphore::new(config.concurrent_fetches as usize));

    while let Some(request) = requests.next().await {
        let permit = active_requests.clone().acquire_owned().await?;

        let pool = pool.clone();
        let nats = nats.clone();
        let sites = sites.clone();

        let span = tracing::info_span!("service_request", subject = %request.message.subject);

        tracing::debug!(message = ?request.message);

        if let Some(headers) = request.message.headers.as_ref() {
            let headers: HashMap<_, _> = headers
                .iter()
                .flat_map(|(name, value)| {
                    value
                        .first()
                        .map(|value| (name.to_string(), value.to_string()))
                })
                .collect();

            tracing::trace!(?headers, "got message headers");

            let cx = opentelemetry::global::get_text_map_propagator(|propagator| {
                propagator.extract(&headers)
            });

            tracing_opentelemetry::OpenTelemetrySpanExt::set_parent(&span, cx);
        }

        tokio::spawn(
            async move {
                match request.message.subject.as_ref() {
                    "fuzzysearch.loader.fetch" => {
                        let resp =
                            requests::handle_fetch(pool, nats, sites, &request.message).await;

                        if let Err(err) = request.respond(resp).await {
                            tracing::error!("could not reply to service request: {err}");
                        }
                    }
                    "fuzzysearch.loader.watch" => {
                        todo!("watching")
                    }
                    _ => unreachable!("got unknown subject: {}", request.message.subject),
                }

                drop(permit);
            }
            .instrument(span),
        );
    }

    tracing::info!("shutting down");

    if let Err(err) = service.stop().await {
        tracing::error!("could not stop service: {err}");
    }

    match tokio::time::timeout(
        std::time::Duration::from_secs(60),
        active_requests.acquire_many(config.concurrent_fetches),
    )
    .await
    {
        Ok(Ok(_permit)) => tracing::info!("all tasks completed"),
        Ok(Err(_err)) => tracing::error!("active requests was closed"),
        Err(_err) => tracing::warn!("active requests after timeout"),
    }

    Ok(())
}
