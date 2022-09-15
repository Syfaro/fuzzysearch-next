use dotenvy_macro::dotenv;

use fuzzysearch_web::{App, AppProps};

fn main() {
    console_error_panic_hook::set_once();
    tracing_wasm::set_as_global_default();

    let fuzzysearch_token = dotenv!("FUZZYSEARCH_API_TOKEN");

    yew::Renderer::<App>::with_props(AppProps {
        fuzzysearch_api_token: fuzzysearch_token.to_string(),
    })
    .render();
}
