use std::rc::Rc;

use wasm_bindgen::JsCast;
use wasm_bindgen_futures::JsFuture;
use web_sys::{Request, RequestInit, RequestMode, Response};
use yew::prelude::*;
use yew_agent::{Bridge, Bridged};

use crate::components::{file_uploader::FileUploader, search_results::SearchResults};
use crate::workers::{ImageHasherWorker, ImageHasherWorkerInput, ImageHasherWorkerOutput};

pub mod components;
pub mod umami;
pub mod workers;

pub struct FuzzySearchApi {
    api_token: String,
}

impl FuzzySearchApi {
    const ENDPOINT: &'static str = "https://api-next.fuzzysearch.net";

    pub fn new<S>(api_token: S) -> Self
    where
        S: ToString,
    {
        Self {
            api_token: api_token.to_string(),
        }
    }

    pub async fn search_hash(&self, hash: i64) -> Option<Vec<fuzzysearch_common::SearchResult>> {
        self.search(format!(
            "{}/hashes?distance=3&hash={}",
            Self::ENDPOINT,
            hash
        ))
        .await
    }

    pub async fn search_url(&self, url: &str) -> Option<Vec<fuzzysearch_common::SearchResult>> {
        let url = js_sys::encode_uri_component(url);

        self.search(format!("{}/url?url={}", Self::ENDPOINT, url))
            .await
    }

    async fn search(&self, url: String) -> Option<Vec<fuzzysearch_common::SearchResult>> {
        tracing::debug!("performing search at {url}");

        let mut opts = RequestInit::new();
        opts.method("GET");
        opts.mode(RequestMode::Cors);

        let request = Request::new_with_str_and_init(&url, &opts).ok()?;
        request.headers().set("x-api-key", &self.api_token).ok()?;

        let window = web_sys::window()?;
        let resp_value = JsFuture::from(window.fetch_with_request(&request))
            .await
            .ok()?;
        let resp: Response = resp_value.dyn_into().ok()?;

        // Ideally resp.json() would be used, but that causes JavaScript to
        // parse i64 values as floats and throws away data.
        let data = JsFuture::from(resp.text().ok()?).await.ok()?.as_string()?;
        let mut results: Vec<fuzzysearch_common::SearchResult> =
            serde_json::from_str(&data).ok()?;
        results.sort_by(|a, b| a.distance.unwrap_or(10).cmp(&b.distance.unwrap_or(10)));

        Some(results)
    }
}

#[derive(Properties, PartialEq, Eq)]
pub struct AppProps {
    pub fuzzysearch_api_token: String,
}

#[derive(Debug)]
enum AppError {
    InvalidImage,
    Api,
}

impl AppError {
    fn message(&self) -> &'static str {
        match self {
            Self::InvalidImage => "Unknown format, only JPEG, PNG, and WebP files are supported",
            Self::Api => "Image lookup failed",
        }
    }

    fn event(&self) -> &'static str {
        match self {
            Self::InvalidImage => "invalid-image",
            Self::Api => "api",
        }
    }
}

pub enum AppMsg {
    SearchingUrl,
    GotFileUpload {
        contents: Vec<u8>,
    },
    GotHasherWorkerOutput {
        output: ImageHasherWorkerOutput,
    },
    GotSearchResult {
        result: Option<Vec<fuzzysearch_common::SearchResult>>,
    },
}

pub struct App {
    state: AppState,
    fuzzysearch_api: Rc<FuzzySearchApi>,
    hasher: Box<dyn Bridge<ImageHasherWorker>>,
}

#[derive(Debug, Default)]
enum AppState {
    #[default]
    Waiting,
    Hashing,
    SearchingHash {
        hash: i64,
    },
    SearchingUrl,
    DisplayingResults {
        results: Rc<Vec<fuzzysearch_common::SearchResult>>,
    },
    DisplayingError {
        error: AppError,
    },
}

impl Component for App {
    type Message = AppMsg;
    type Properties = AppProps;

    fn create(ctx: &Context<Self>) -> Self {
        let fuzzysearch_api = Rc::new(FuzzySearchApi::new(
            ctx.props().fuzzysearch_api_token.clone(),
        ));

        let cb = {
            let link = ctx.link().clone();
            move |output| link.send_message(AppMsg::GotHasherWorkerOutput { output })
        };

        Self {
            state: Default::default(),
            fuzzysearch_api,
            hasher: ImageHasherWorker::bridge(Rc::new(cb)),
        }
    }

    fn update(&mut self, ctx: &Context<Self>, msg: Self::Message) -> bool {
        match msg {
            AppMsg::SearchingUrl => {
                self.state = AppState::SearchingUrl;
                true
            }
            AppMsg::GotFileUpload { contents } => {
                self.hasher.send(ImageHasherWorkerInput { contents });
                false
            }
            AppMsg::GotHasherWorkerOutput {
                output: ImageHasherWorkerOutput::Starting,
            } => {
                self.state = AppState::Hashing;
                true
            }
            AppMsg::GotHasherWorkerOutput {
                output: ImageHasherWorkerOutput::Finished { hash: Some(hash) },
            } => {
                let api = self.fuzzysearch_api.clone();
                ctx.link().send_future(async move {
                    AppMsg::GotSearchResult {
                        result: api.search_hash(hash).await,
                    }
                });
                self.state = AppState::SearchingHash { hash };
                true
            }
            AppMsg::GotHasherWorkerOutput {
                output: ImageHasherWorkerOutput::Finished { hash: None },
            } => {
                let error = AppError::InvalidImage;

                umami::track_event(error.event(), "error");
                self.state = AppState::DisplayingError { error };
                true
            }
            AppMsg::GotSearchResult {
                result: Some(results),
            } => {
                self.state = AppState::DisplayingResults {
                    results: Rc::new(results),
                };
                true
            }
            AppMsg::GotSearchResult { result: None } => {
                let error = AppError::Api;

                umami::track_event(error.event(), "error");
                self.state = AppState::DisplayingError { error };
                true
            }
        }
    }

    fn rendered(&mut self, ctx: &Context<Self>, first_render: bool) {
        if !first_render {
            return;
        }

        let hash = match web_sys::window().and_then(|window| window.location().hash().ok()) {
            Some(hash) => hash,
            None => return,
        };

        if hash.len() < 2 {
            return;
        }

        let url = match web_sys::UrlSearchParams::new_with_str(&hash[1..])
            .ok()
            .and_then(|params| params.get("url"))
        {
            Some(url) => url,
            None => return,
        };

        tracing::info!(url, "found url in hash, searching");

        ctx.link().send_message(AppMsg::SearchingUrl);

        let api = self.fuzzysearch_api.clone();
        ctx.link().send_future(async move {
            AppMsg::GotSearchResult {
                result: api.search_url(&url).await,
            }
        });
    }

    fn view(&self, ctx: &Context<Self>) -> Html {
        let on_file_upload = ctx
            .link()
            .callback(|contents| AppMsg::GotFileUpload { contents });

        html! {
            <main class="container">
                <div class="sidebar">
                    <h1 class="title">{ "FuzzySearch" }</h1>
                    <h2 class="tagline">{ "Reverse image search for FurAffinity, Weasyl, e621, and Twitter" }</h2>

                    <FileUploader on_file_upload={on_file_upload} />

                    {self.about()}
                </div>

                <div class="content">
                    {self.content()}
                </div>
            </main>
        }
    }
}

impl App {
    fn content(&self) -> Html {
        match &self.state {
            AppState::Waiting => html! {
                <div>
                    <h2>{ "Welcome! Upload an image to get started." }</h2>
                </div>
            },
            AppState::Hashing => html! {
                <div>
                    <h2>{ "Analyzing" }</h2>
                    <p class="help-text">{ "This image is being processed entirely on your device." }</p>
                </div>
            },
            AppState::SearchingHash { hash } => html! {
                <div>
                    <h2>{ "Searching" }</h2>
                    <p class="help-text">{ "Your image's magic number is "}<code>{hash}</code></p>
                </div>
            },
            AppState::SearchingUrl { .. } => html! {
                <div>
                    <h2>{ "Loading URL" }</h2>
                    <p class="help-text">{ "Downloading and analyzing URL." }</p>
                </div>
            },
            AppState::DisplayingResults { results } => html! {
                <SearchResults results={results} />
            },
            AppState::DisplayingError { error } => html! {
                <div class="error">
                    <h2>{ "Error" }</h2>
                    <p class="help-text">{ error.message() }</p>
                </div>
            },
        }
    }

    fn about(&self) -> Html {
        html! {
            <div class="about">
                <p class="bot-links">
                    { "Want to integrate this service in your chats? Check out FoxBot for " }
                    <a href="https://t.me">{ "Telegram" }</a>
                    { " and " }
                    <a href="https://discord.com/oauth2/authorize?client_id=824071620783243336&scope=applications.commands">{ "Discord" }</a>
                    { "!" }
                </p>

                <p>
                    { "Are you interested in notifications when your artwork is reposted? Try " }
                    <a href="https://owo.fuzzysearch.net">{ "FuzzySearch OwO" }</a>
                    { "." }
                </p>

                <p class="credit">
                    { "FuzzySearch is a project developed by " }
                    <a href="https://syfaro.net">{ "Syfaro" }</a>
                    { "." }
                </p>

                <p>
                    <a href="https://api-next.fuzzysearch.net/swagger-ui/#/">{ "API documentation" }</a>
                </p>
            </div>
        }
    }
}
