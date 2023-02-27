use std::{sync::Arc, time::Duration};

use async_nats::{service::ServiceExt, ServerAddr};
use clap::Parser;
use futures::StreamExt;
use sqlx::PgPool;

mod requests;
mod sites;

#[derive(Parser)]
pub struct Config {
    #[clap(long, env)]
    nats_host: ServerAddr,
    #[clap(long, env)]
    nats_nkey: String,

    #[clap(long, env)]
    database_url: String,

    #[clap(long, env)]
    user_agent: Option<String>,

    #[clap(long, env)]
    furaffinity_cookie_a: Option<String>,
    #[clap(long, env)]
    furaffinity_cookie_b: Option<String>,
    #[clap(long, env)]
    weasyl_api_token: Option<String>,
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

    let sites = Arc::new(sites::sites(&config, client));

    let service = nats
        .service_builder()
        .start("fuzzysearch-loader", env!("CARGO_PKG_VERSION"))
        .await
        .unwrap();

    let mut endpoint = service.endpoint("fuzzysearch.loader.fetch").await.unwrap();

    while let Some(request) = endpoint.next().await {
        let pool = pool.clone();
        let sites = sites.clone();

        tokio::spawn(async move {
            let resp = requests::handle_fetch(pool, sites, &request.message).await;

            if let Err(err) = request.respond(resp).await {
                tracing::error!("could not reply to service request: {err}");
            }
        });
    }

    Ok(())
}
