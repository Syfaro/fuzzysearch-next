use std::rc::Rc;

use wasm_bindgen::JsCast;
use wasm_bindgen_futures::JsFuture;
use web_sys::{Request, RequestInit, RequestMode, Response};
use yew::{platform::spawn_local, prelude::*};
use yew_agent::{use_bridge, UseBridgeHandle};

use crate::components::{file_uploader::FileUploader, search_results::SearchResults};
use crate::workers::{ImageHasherWorker, ImageHasherWorkerInput, ImageHasherWorkerOutput};

pub mod components;
pub mod workers;

pub struct FuzzySearchApi {
    api_token: String,
}

impl FuzzySearchApi {
    const ENDPOINT: &'static str = "https://api.fuzzysearch.net";

    pub fn new<S>(api_token: S) -> Self
    where
        S: ToString,
    {
        Self {
            api_token: api_token.to_string(),
        }
    }

    pub async fn search_hash(&self, hash: i64) -> Option<Vec<fuzzysearch_common::SearchResult>> {
        let mut opts = RequestInit::new();
        opts.method("GET");
        opts.mode(RequestMode::Cors);

        let url = format!("{}/hashes?distance=3&hashes={}", Self::ENDPOINT, hash);

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

enum AppError {
    InvalidImage,
    Api,
}

impl AppError {
    fn message(&self) -> &'static str {
        match self {
            Self::InvalidImage => "Image was not understood",
            Self::Api => "Image lookup error",
        }
    }
}

enum AppAction {
    StartedHashing,
    FinishedHashing {
        hash: i64,
    },
    GotMatches {
        results: Vec<fuzzysearch_common::SearchResult>,
    },
    EncounteredError {
        error: AppError,
    },
}

enum AppState {
    Waiting,
    Hashing,
    Searching {
        hash: i64,
    },
    Results {
        results: Rc<Vec<fuzzysearch_common::SearchResult>>,
    },
    Error {
        error: AppError,
    },
}

impl Default for AppState {
    fn default() -> Self {
        Self::Waiting
    }
}

impl Reducible for AppState {
    type Action = AppAction;

    fn reduce(self: Rc<Self>, action: Self::Action) -> Rc<Self> {
        let next_state = match action {
            AppAction::StartedHashing => AppState::Hashing,
            AppAction::FinishedHashing { hash } => AppState::Searching { hash },
            AppAction::GotMatches { results } => AppState::Results {
                results: Rc::new(results),
            },
            AppAction::EncounteredError { error } => AppState::Error { error },
        };

        next_state.into()
    }
}

#[function_component]
pub fn App(props: &AppProps) -> Html {
    let state = use_reducer(AppState::default);

    let bridge: UseBridgeHandle<ImageHasherWorker> = {
        let state = state.clone();
        let api_token = props.fuzzysearch_api_token.clone();

        use_bridge(move |response: ImageHasherWorkerOutput| match response {
            ImageHasherWorkerOutput::Starting => state.dispatch(AppAction::StartedHashing),
            ImageHasherWorkerOutput::Finished { hash: Some(hash) } => {
                state.dispatch(AppAction::FinishedHashing { hash });

                let state = state.clone();
                let api_token = api_token.clone();

                spawn_local(async move {
                    let fuzzysearch_api = FuzzySearchApi::new(api_token);

                    match fuzzysearch_api.search_hash(hash).await {
                        Some(results) => state.dispatch(AppAction::GotMatches { results }),
                        None => state.dispatch(AppAction::EncounteredError {
                            error: AppError::Api,
                        }),
                    }
                });
            }
            ImageHasherWorkerOutput::Finished { hash: None } => {
                state.dispatch(AppAction::EncounteredError {
                    error: AppError::InvalidImage,
                })
            }
        })
    };

    let on_file_upload: Callback<Vec<u8>> = Callback::from(move |contents: Vec<u8>| {
        bridge.send(ImageHasherWorkerInput { contents });
    });

    html! {
        <main class="container">
            <div class="sidebar">
                <h1 class="title">{ "FuzzySearch" }</h1>
                <h2 class="tagline">{ "Search images on FurAffinity, Weasyl, e621, and Twitter" }</h2>

                <form>
                    <FileUploader on_file_upload={on_file_upload} />
                </form>

                <div class="about">
                    <p class="bot-links">
                        { "Want to integrate this service in your chats? Check out FoxBot for " }
                        <a href="https://t.me">{ "Telegram" }</a>
                        { " and " }
                        <a href="https://discord.com/oauth2/authorize?client_id=824071620783243336&scope=applications.commands">{ "Discord" }</a>
                        { "!" }
                    </p>

                    <p class="credit">{ "FuzzySearch is a project developed by " }<a href="https://syfaro.net">{ "Syfaro" }</a></p>
                </div>
            </div>

            <div class="content">
                {app_html(&state)}
            </div>
        </main>
    }
}

fn app_html(state: &AppState) -> Html {
    match state {
        AppState::Waiting => html! { <h2>{ "Welcome! Upload an image to get started." }</h2> },
        AppState::Hashing => {
            html! {
                <div>
                    <h2>{ "Analyzing" }</h2>
                    <p class="help-text">{ "This image is being processed entirely on your device." }</p>
                </div>
            }
        }
        AppState::Searching { hash } => {
            html! {
                <div>
                    <h2>{ "Searching" }</h2>
                    <p class="help-text">{ "Your image's magic number is "}<code>{hash}</code></p>
                </div>
            }
        }
        AppState::Results { results } => {
            html! { <SearchResults results={results} /> }
        }
        AppState::Error { error } => html! {
            <div class="error">
                <h2>{ "Error" }</h2>
                <p class="help-text">{ error.message() }</p>
            </div>
        },
    }
}
