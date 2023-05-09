use std::{net::SocketAddr, path::PathBuf, sync::Arc, time::Duration};

use async_nats::{service::ServiceExt, ServerAddr};
use clap::Parser;
use futures::StreamExt;
use sqlx::PgPool;

mod requests;
mod sites;

#[derive(Parser)]
pub struct Config {
    #[clap(long, env)]
    metrics_host: SocketAddr,

    #[clap(long, env)]
    nats_host: ServerAddr,
    #[clap(long, env)]
    nats_nkey: String,

    #[clap(long, env)]
    database_url: String,

    #[clap(long, env)]
    user_agent: Option<String>,

    #[clap(long, env)]
    download_path: Option<PathBuf>,
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

pub struct SiteConfig {
    download_path: Option<PathBuf>,
    auto_fetch_submissions: bool,
}

impl Config {
    fn site(&self) -> SiteConfig {
        SiteConfig {
            download_path: self.download_path.clone(),
            auto_fetch_submissions: self.auto_fetch_submissions,
        }
    }
}

#[tokio::main]
async fn main() -> eyre::Result<()> {
    let _ = dotenvy::dotenv();

    let config = Config::parse();

    foxlib::trace_init(foxlib::TracingConfig {
        namespace: "fuzzysearch",
        name: env!("CARGO_PKG_NAME"),
        version: env!("CARGO_PKG_VERSION"),
        otlp: false,
    });

    ffmpeg_next::init()?;

    let metrics = foxlib::MetricsServer::serve(config.metrics_host, false).await;

    let pool = PgPool::connect(&config.database_url).await?;

    let nats = async_nats::ConnectOptions::with_nkey(config.nats_nkey.clone())
        .connect(config.nats_host.clone())
        .await?;

    let mut client_builder = reqwest::ClientBuilder::default().timeout(Duration::from_secs(10));
    if let Some(user_agent) = config.user_agent.as_ref() {
        tracing::info!("adding user agent to http client");
        client_builder = client_builder.user_agent(user_agent);
    }
    let client = client_builder.build()?;

    let sites = Arc::new(sites::sites(&config, client, pool.clone()).await);

    let service = nats
        .service_builder()
        .start("fuzzysearch-loader", env!("CARGO_PKG_VERSION"))
        .await
        .unwrap();

    let fetch = service.endpoint("fuzzysearch.loader.fetch").await.unwrap();
    let watch = service.endpoint("fuzzysearch.loader.watch").await.unwrap();

    let mut requests = tokio_stream::StreamExt::merge(fetch, watch);

    metrics.set_ready(true);

    while let Some(request) = requests.next().await {
        let pool = pool.clone();
        let sites = sites.clone();

        tokio::spawn(async move {
            match request.message.subject.as_ref() {
                "fuzzysearch.loader.fetch" => {
                    let resp = requests::handle_fetch(pool, sites, &request.message).await;

                    if let Err(err) = request.respond(resp).await {
                        tracing::error!("could not reply to service request: {err}");
                    }
                }
                "fuzzysearch.loader.watch" => {
                    todo!()
                }
                _ => unreachable!("got unknown subject: {}", request.message.subject),
            }
        });
    }

    Ok(())
}
