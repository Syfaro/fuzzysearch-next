use async_trait::async_trait;
use chrono::TimeZone;
use fuzzysearch_common::{Rating, Site};
use lazy_static::lazy_static;
use regex::Regex;
use scraper::{Html, Selector};
use serde::Deserialize;
use tap::TapOptional;

use crate::sites::{process_image, LoadableSite, Submission, SubmissionResult};

lazy_static! {
    static ref PAGE_TITLE: Selector = Selector::parse("title").unwrap();
    static ref ERROR_MESSAGE: Selector = Selector::parse(
        ".error-message-box, div#standardpage section.notice-message p.link-override"
    )
    .unwrap();
    static ref ARTIST: Selector =
        Selector::parse(".submission-id-sub-container .submission-title + a").unwrap();
    static ref TITLE: Selector = Selector::parse(".submission-title h2 p").unwrap();
    static ref IMAGE_URL: Selector = Selector::parse("#submissionImg").unwrap();
    static ref FLASH_OBJECT: Selector = Selector::parse("#flash_embed").unwrap();
    static ref POSTED_AT: Selector =
        Selector::parse(".submission-id-sub-container strong span.popup_date").unwrap();
    static ref TAGS: Selector = Selector::parse("section.tags-row a").unwrap();
    static ref DESCRIPTION: Selector = Selector::parse(".submission-content section").unwrap();
    static ref RATING: Selector =
        Selector::parse(".stats-container .rating span.rating-box").unwrap();
    static ref DATE_CLEANER: Regex = Regex::new(r"(\d{1,2})(st|nd|rd|th)").unwrap();
}

#[derive(Debug, thiserror::Error)]
#[error("missing field: {field_name}")]
struct MissingFieldError {
    field_name: &'static str,
}

impl MissingFieldError {
    pub fn new(field_name: &'static str) -> Self {
        Self { field_name }
    }
}

#[derive(Debug, Deserialize)]
enum FurAffinityRating {
    General,
    Mature,
    Adult,
}

impl FurAffinityRating {
    pub fn normalized(&self) -> Rating {
        match self {
            Self::General => Rating::General,
            Self::Mature => Rating::Mature,
            Self::Adult => Rating::Adult,
        }
    }
}

pub struct FurAffinity {
    client: reqwest::Client,
    cookies: String,
}

impl FurAffinity {
    pub fn new(client: reqwest::Client, cookie_a: String, cookie_b: String) -> Self {
        let cookies = format!("a={cookie_a}; b={cookie_b}");
        Self { client, cookies }
    }

    #[tracing::instrument(skip(self))]
    pub async fn load_submission(&self, id: &str) -> eyre::Result<SubmissionResult> {
        tracing::info!("loading submission");

        let url = format!("https://www.furaffinity.net/view/{id}/");

        let page = match self
            .client
            .get(&url)
            .header("cookie", &self.cookies)
            .send()
            .await
            .and_then(|page| page.error_for_status())
        {
            Ok(page) => page,
            Err(err) => {
                return Ok(SubmissionResult::Error {
                    site: self.site(),
                    submission_id: id.to_string(),
                    message: Some(err.to_string()),
                })
            }
        };

        let content = match page.text().await {
            Ok(content) => content,
            Err(err) => {
                return Ok(SubmissionResult::Error {
                    site: self.site(),
                    submission_id: id.to_string(),
                    message: Some(err.to_string()),
                })
            }
        };

        let (mut submission, content_url_parts) = parse_submission(id.to_string(), url, &content)?;

        if let Some(content_url_parts) = content_url_parts {
            let mut media = match process_image(&self.client, None, &content_url_parts.url).await {
                Ok(media) => media,
                Err(err) => {
                    return Ok(SubmissionResult::Error {
                        site: self.site(),
                        submission_id: id.to_string(),
                        message: Some(err.to_string()),
                    })
                }
            };
            let file_id = content_url_parts
                .filename
                .split_once('.')
                .map(|(first, _second)| first);
            media.extra = Some(serde_json::json!({
                "file_name": content_url_parts.filename,
                "file_id": file_id,
            }));

            match submission {
                SubmissionResult::Fetched(ref mut submission) => submission.media.push(media),
                _ => tracing::warn!("had content url parts but no fetched submission"),
            }
        }

        Ok(submission)
    }
}

#[async_trait]
impl LoadableSite for FurAffinity {
    fn site(&self) -> Site {
        Site::FurAffinity
    }

    #[tracing::instrument(skip(self))]
    async fn load(&self, ids: Vec<&str>) -> eyre::Result<Vec<SubmissionResult>> {
        tracing::info!("starting to load submissions");

        let mut submissions = Vec::with_capacity(ids.len());

        for id in ids {
            let submission = self.load_submission(id).await?;
            submissions.push(submission);
        }

        Ok(submissions)
    }
}

fn join_text_nodes(elem: scraper::ElementRef) -> String {
    elem.text().collect::<String>().trim().to_string()
}

fn parse_date(date: &str) -> eyre::Result<chrono::DateTime<chrono::Utc>> {
    let date_str = DATE_CLEANER.replace(date, "$1");

    let date = chrono::Utc.datetime_from_str(&date_str, "%b %e, %Y %l:%M %p")?;

    Ok(date)
}

struct ContentUrlParts {
    url: String,
    filename: String,
}

fn extract_url(elem: scraper::ElementRef, attr: &'static str) -> Option<ContentUrlParts> {
    let url = "https:".to_owned() + elem.value().attr(attr)?;
    let filename = url.split('/').last()?.to_string();

    Some(ContentUrlParts { url, filename })
}

fn parse_submission(
    id: String,
    url: String,
    content: &str,
) -> eyre::Result<(SubmissionResult, Option<ContentUrlParts>)> {
    let doc = Html::parse_document(content);

    if doc
        .select(&PAGE_TITLE)
        .next()
        .map(|elem| join_text_nodes(elem) == "System Error")
        .unwrap_or(false)
    {
        tracing::trace!("page had system error: {content}");

        return Ok((
            SubmissionResult::Fetched(Submission {
                site: Site::FurAffinity,
                submission_id: id,
                deleted: true,
                posted_at: None,
                link: url,
                title: None,
                artists: Vec::new(),
                tags: Vec::new(),
                description: None,
                rating: None,
                media: Vec::new(),
                retrieved_at: Some(chrono::Utc::now()),
                extra: None,
            }),
            None,
        ));
    }

    if doc.select(&ERROR_MESSAGE).next().is_some() {
        tracing::trace!("page had error message: {content}");

        return Ok((
            SubmissionResult::Fetched(Submission {
                site: Site::FurAffinity,
                submission_id: id,
                deleted: true,
                posted_at: None,
                link: url,
                title: None,
                artists: Vec::new(),
                tags: Vec::new(),
                description: None,
                rating: None,
                media: Vec::new(),
                retrieved_at: Some(chrono::Utc::now()),
                extra: None,
            }),
            None,
        ));
    }

    let title = doc
        .select(&TITLE)
        .next()
        .map(join_text_nodes)
        .ok_or(MissingFieldError::new("title"))?;

    let artist = doc
        .select(&ARTIST)
        .next()
        .map(join_text_nodes)
        .ok_or(MissingFieldError::new("artist"))?;

    let content_url_parts = if let Some(elem) = doc.select(&IMAGE_URL).next() {
        extract_url(elem, "src").ok_or(MissingFieldError::new("image url"))?
    } else if let Some(elem) = doc.select(&FLASH_OBJECT).next() {
        extract_url(elem, "data").ok_or(MissingFieldError::new("flash url"))?
    } else {
        return Err(MissingFieldError::new("content").into());
    };

    let rating: Option<FurAffinityRating> = doc
        .select(&RATING)
        .next()
        .map(join_text_nodes)
        .tap_some(|rating| tracing::trace!("extracted rating: {rating}"))
        .and_then(|rating| serde_plain::from_str(&rating).ok())
        .ok_or(MissingFieldError::new("rating"))?;

    let posted_at = doc
        .select(&POSTED_AT)
        .next()
        .and_then(|elem| elem.value().attr("title"))
        .tap_some(|posted_at| tracing::trace!("extracted posted_at: {posted_at}"))
        .and_then(|posted_at| parse_date(posted_at).ok())
        .ok_or(MissingFieldError::new("posted at"))?;

    let tags: Vec<_> = doc
        .select(&TAGS)
        .into_iter()
        .map(join_text_nodes)
        .map(|tag| {
            tag.strip_suffix(',')
                .map(|tag| tag.to_string())
                .unwrap_or(tag)
        })
        .collect();

    let description = doc
        .select(&DESCRIPTION)
        .next()
        .ok_or(MissingFieldError::new("description"))?
        .inner_html();

    Ok((
        SubmissionResult::Fetched(Submission {
            site: Site::FurAffinity,
            submission_id: id,
            deleted: false,
            posted_at: Some(posted_at),
            link: url,
            title: Some(title),
            artists: vec![artist],
            tags,
            description: Some(description),
            rating: rating.map(|rating| rating.normalized()),
            media: Vec::with_capacity(1),
            retrieved_at: Some(chrono::Utc::now()),
            extra: None,
        }),
        Some(content_url_parts),
    ))
}

#[cfg(test)]
mod tests {
    use chrono::TimeZone;

    use super::parse_date;

    #[test]
    fn test_parse_date() {
        let cases = [(
            "Feb 26, 2023 03:47 PM",
            chrono::Utc
                .with_ymd_and_hms(2023, 2, 26, 15, 47, 0)
                .unwrap(),
        )];

        for (input, output) in cases {
            assert_eq!(parse_date(input).unwrap(), output);
        }
    }
}
