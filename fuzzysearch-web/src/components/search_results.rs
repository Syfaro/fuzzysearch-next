use std::rc::Rc;

use yew::prelude::*;

#[derive(Properties, PartialEq, Eq)]
pub struct SearchResultsProps {
    pub results: Rc<Vec<fuzzysearch_common::SearchResult>>,
}

#[function_component]
pub fn SearchResults(props: &SearchResultsProps) -> Html {
    if props.results.is_empty() {
        return html! {
            <div id="results">
                <h2>{ "Results" }</h2>
                <p>{ "Sorry, no matches were found." }</p>
            </div>
        };
    }

    html! {
        <div id="results">
            <h2>{ "Results" }</h2>

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

fn name_for_distance(distance: i64) -> &'static str {
    match distance {
        0 => "Exact match",
        1..=2 => "Very close match",
        3 => "Close match",
        4..=7 => "Possible match",
        _ => "Unlikely match",
    }
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

    let distance = props.result.distance.unwrap_or(10);

    html! {
        <div class="search-result">
            <h3>
                {props.result.site_name()}
            </h3>
            <p class="posted-by">
                <span title={format!("Perceptual distance of {distance}")}>{ name_for_distance(distance) }</span>
                { " posted by " }
                <strong>{ artist_name }</strong>
            </p>
            <p class="link">
                <a href={props.result.url()} rel={"external nofollow"}>{display_url}</a>
            </p>
        </div>
    }
}
