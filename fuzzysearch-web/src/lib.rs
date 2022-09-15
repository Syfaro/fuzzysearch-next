use std::{collections::HashMap, rc::Rc};

use gloo::file::{callbacks::FileReader, File};
use serde::{Deserialize, Serialize};
use wasm_bindgen::JsCast;
use wasm_bindgen_futures::JsFuture;
use web_sys::{FileList, HtmlInputElement, Request, RequestInit, RequestMode, Response};
use yew::{platform::spawn_local, prelude::*};
use yew_agent::{use_bridge, Public, UseBridgeHandle, WorkerLink};

#[derive(Serialize, Deserialize)]
pub struct WorkerInput {
    pub contents: Vec<u8>,
}

#[derive(Serialize, Deserialize)]
pub enum WorkerOutput {
    Starting,
    Finished { hash: Option<i64> },
}

pub struct Worker {
    link: WorkerLink<Self>,
}

impl yew_agent::Worker for Worker {
    type Input = WorkerInput;
    type Message = ();
    type Output = WorkerOutput;
    type Reach = Public<Self>;

    fn create(link: WorkerLink<Self>) -> Self {
        Self { link }
    }

    fn update(&mut self, _msg: Self::Message) {}

    fn handle_input(&mut self, msg: Self::Input, id: yew_agent::HandlerId) {
        tracing::trace!("received input for hashing");

        self.link.respond(id, WorkerOutput::Starting);

        let hash = image::load_from_memory(&msg.contents)
            .ok()
            .map(|im| Self::get_hasher().hash_image(&im))
            .and_then(|hash| hash.as_bytes().try_into().ok())
            .map(|hash| i64::from_be_bytes(hash));

        tracing::debug!("finished hashing image");

        self.link.respond(id, WorkerOutput::Finished { hash });
    }

    fn name_of_resource() -> &'static str {
        "worker.js"
    }
}

impl Worker {
    fn get_hasher() -> img_hash::Hasher<[u8; 8]> {
        use img_hash::HashAlg;

        img_hash::HasherConfig::with_bytes_type::<[u8; 8]>()
            .hash_alg(HashAlg::Gradient)
            .hash_size(8, 8)
            .preproc_dct()
            .to_hasher()
    }
}

pub struct FileUploader {
    readers: HashMap<String, FileReader>,
}

pub enum FileUploaderMsg {
    Files(Vec<File>),
    Loaded(String, String, Option<Vec<u8>>),
}

#[derive(Properties, PartialEq)]
pub struct FileUploaderProps {
    pub on_file_upload: Callback<Vec<u8>>,
}

impl Component for FileUploader {
    type Message = FileUploaderMsg;
    type Properties = FileUploaderProps;

    fn create(_ctx: &Context<Self>) -> Self {
        Self {
            readers: Default::default(),
        }
    }

    fn update(&mut self, ctx: &Context<Self>, msg: Self::Message) -> bool {
        match msg {
            FileUploaderMsg::Files(files) => {
                tracing::debug!("got {} files", files.len());
                for file in files {
                    let file_name = file.name();
                    let mime_type = file.raw_mime_type();

                    let task = {
                        let link = ctx.link().clone();
                        let file_name = file_name.clone();

                        gloo::file::callbacks::read_as_bytes(&file, move |res| {
                            link.send_message(FileUploaderMsg::Loaded(
                                file_name,
                                mime_type,
                                res.ok(),
                            ))
                        })
                    };
                    self.readers.insert(file_name, task);
                }

                false
            }
            FileUploaderMsg::Loaded(name, mime, contents) => {
                tracing::info!(mime_type = mime, "loaded file {name}");
                if let Some(contents) = contents {
                    ctx.props().on_file_upload.emit(contents);
                }

                true
            }
        }
    }

    fn view(&self, ctx: &Context<Self>) -> Html {
        html! {
            <div
                ondrop={ctx.link().callback(|event: DragEvent| {
                    event.prevent_default();
                    let files = event.data_transfer().map(|dt| dt.files()).unwrap_or_default();
                    Self::upload_files(files)
                })}
                ondragover={Callback::from(|event: DragEvent| {
                    event.prevent_default();
                })}
                ondragenter={Callback::from(|event: DragEvent| {
                    event.prevent_default();
                })}
            >
                <input
                    id="file-upload"
                    type="file"
                    accept="image/jpeg,image/png,image/webp"
                    onchange={ctx.link().callback(move |e: Event| {
                        let input: HtmlInputElement = e.target_unchecked_into();
                        Self::upload_files(input.files())
                    })} />
            </div>
        }
    }
}

impl FileUploader {
    fn upload_files(files: Option<FileList>) -> FileUploaderMsg {
        let mut results = Vec::with_capacity(
            files
                .as_ref()
                .map(|files| files.length())
                .unwrap_or_default() as usize,
        );

        if let Some(files) = files {
            let files: Vec<File> = js_sys::try_iter(&files)
                .ok()
                .flatten()
                .map(|files| {
                    files
                        .filter_map(|file| file.ok())
                        .map(|file| web_sys::File::from(file))
                        .map(From::from)
                        .collect()
                })
                .unwrap_or_default();

            results.extend(files);
        }

        FileUploaderMsg::Files(results)
    }
}

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

#[derive(Properties, PartialEq)]
pub struct AppProps {
    pub fuzzysearch_api_token: String,
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
        message: String,
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
        message: String,
    },
}

impl Default for AppState {
    fn default() -> Self {
        Self::Waiting
    }
}

impl AppState {
    fn is_loading(&self) -> Option<&'static str> {
        if matches!(self, AppState::Hashing | AppState::Searching { .. }) {
            Some("true")
        } else {
            None
        }
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
            AppAction::EncounteredError { message } => AppState::Error { message },
        };

        next_state.into()
    }
}

#[function_component]
pub fn App(props: &AppProps) -> Html {
    let state = use_reducer(AppState::default);

    let bridge: UseBridgeHandle<Worker> = {
        let state = state.clone();
        let api_token = props.fuzzysearch_api_token.clone();

        use_bridge(move |response: WorkerOutput| match response {
            WorkerOutput::Starting => state.dispatch(AppAction::StartedHashing),
            WorkerOutput::Finished { hash: Some(hash) } => {
                state.dispatch(AppAction::FinishedHashing { hash });

                let state = state.clone();
                let api_token = api_token.clone();

                spawn_local(async move {
                    let fuzzysearch_api = FuzzySearchApi::new(api_token);

                    match fuzzysearch_api.search_hash(hash).await {
                        Some(results) => state.dispatch(AppAction::GotMatches { results }),
                        None => state.dispatch(AppAction::EncounteredError {
                            message: "could not get results".to_string(),
                        }),
                    }
                });
            }
            WorkerOutput::Finished { hash: None } => state.dispatch(AppAction::EncounteredError {
                message: "could not hash image".to_string(),
            }),
        })
    };

    let on_file_upload: Callback<Vec<u8>> = Callback::from(move |contents: Vec<u8>| {
        bridge.send(WorkerInput { contents });
    });

    html! {
        <main class="container">
            <h1>{ "FuzzySearch" }</h1>

            <form>
                <label for="file-upload">{ "Image upload" }</label>
                <FileUploader on_file_upload={on_file_upload} />
            </form>

            <div>
                {app_html(&state)}
            </div>
        </main>
    }
}

#[derive(Properties, PartialEq)]
struct SearchResultsProps {
    pub results: Rc<Vec<fuzzysearch_common::SearchResult>>,
}

#[function_component]
fn SearchResults(props: &SearchResultsProps) -> Html {
    if props.results.is_empty() {
        return html! {
            <div id="results">
                <h2>{"No matches found"}</h2>
            </div>
        };
    }

    html! {
        <div id="results">
            {props.results.iter().map(|result| {
                html! { <SearchResult key={result.site_id} result={result.to_owned()} /> }
            }).collect::<Html>()}
        </div>
    }
}

#[derive(Properties, PartialEq)]
struct SearchResultProps {
    pub result: fuzzysearch_common::SearchResult,
}

#[function_component]
fn SearchResult(props: &SearchResultProps) -> Html {
    let artist_name = props
        .result
        .artists
        .as_deref()
        .unwrap_or_default()
        .join(", ");

    let url_without_scheme = props
        .result
        .url()
        .replace("https://", "")
        .replace("http://", "");

    let display_url = url_without_scheme
        .strip_prefix("www.")
        .unwrap_or(&url_without_scheme);

    html! {
        <div class="search-result">
            <strong>{format!("Result on {} (distance {})", props.result.site_name(), props.result.distance.unwrap_or(10))}</strong>
            <br />
            {format!("Posted by {}", artist_name)}
            <br />
            <a href={props.result.url()} rel={"external nofollow"}>{display_url}</a>
        </div>
    }
}

fn app_html(state: &AppState) -> Html {
    match state {
        AppState::Waiting => html! { <p>{ "Upload an image" }</p> },
        AppState::Hashing => {
            html! { <p aria-busy={state.is_loading()}>{ "Processing image upload" }</p> }
        }
        AppState::Searching { hash } => {
            html! { <p aria-busy={state.is_loading()}>{ "Searching for hash " }<code>{ hash }</code></p> }
        }
        AppState::Results { results } => {
            html! { <SearchResults results={results} /> }
        }
        AppState::Error { message } => html! { <p>{ message }</p> },
    }
}
