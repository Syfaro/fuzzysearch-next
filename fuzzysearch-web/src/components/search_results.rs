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

fn strip_parts<'a>(s: &'a str, prefixes: &[&'static str], suffixes: &[&'static str]) -> &'a str {
    let s = prefixes
        .iter()
        .fold(s, |s, prefix| s.strip_prefix(*prefix).unwrap_or(s));

    let s = suffixes
        .iter()
        .fold(s, |s, suffix| s.strip_suffix(*suffix).unwrap_or(s));

    s
}

#[function_component]
fn SearchResult(props: &SearchResultProps) -> Html {
    let artist_name = props
        .result
        .artists
        .as_deref()
        .unwrap_or_default()
        .join(", ");

    let url = props.result.url();
    let display_url = strip_parts(&url, &["https://", "http://", "www."], &["/"]);

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

#[cfg(test)]
mod tests {
    use super::strip_parts;

    #[test]
    fn test_strip_parts() {
        let test_cases: [(&'static str, &[&'static str], &[&'static str], &'static str); 2] = [
            ("google.com", &["https://"], &["/"], "google.com"),
            (
                "https://www.google.com/",
                &["https://", "www."],
                &["/"],
                "google.com",
            ),
        ];

        for (input, prefixes, suffixes, output) in test_cases {
            assert_eq!(output, strip_parts(input, prefixes, suffixes));
        }
    }
}
