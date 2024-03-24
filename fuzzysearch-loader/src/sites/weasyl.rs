use std::{collections::HashMap, sync::Arc};

use async_trait::async_trait;
use fuzzysearch_common::{Artist, Rating, Site, Submission};
use serde::Deserialize;

use crate::{
    sites::{process_file, LoadableSite, SubmissionResult},
    SiteContext,
};

pub struct Weasyl {
    pub api_key: String,
    ctx: Arc<SiteContext>,
}

impl Weasyl {
    pub fn new(ctx: Arc<SiteContext>, api_key: String) -> Arc<Self> {
        Arc::new(Self { ctx, api_key })
    }

    #[tracing::instrument(skip(self))]
    pub async fn load_submission(&self, id: &str) -> eyre::Result<SubmissionResult> {
        tracing::info!("loading submission");

        let url = format!("https://www.weasyl.com/api/submissions/{}/view", id);

        let resp = match self
            .ctx
            .client
            .get(url)
            .header("x-weasyl-api-key", &self.api_key)
            .send()
            .await
        {
            Ok(resp) => resp,
            Err(err) => {
                return Ok(SubmissionResult::Error {
                    site: self.site(),
                    submission_id: id.to_string(),
                    message: Some(err.to_string()),
                })
            }
        };

        let json: serde_json::Value = match resp.json().await {
            Ok(resp) => resp,
            Err(err) => {
                return Ok(SubmissionResult::Error {
                    site: self.site(),
                    submission_id: id.to_string(),
                    message: Some(err.to_string()),
                })
            }
        };

        let resp: WeasylResponse<WeasylSubmission> = match serde_json::from_value(json.clone()) {
            Ok(resp) => resp,
            Err(err) => {
                return Ok(SubmissionResult::Error {
                    site: self.site(),
                    submission_id: id.to_string(),
                    message: Some(err.to_string()),
                })
            }
        };

        let mut sub = match resp {
            WeasylResponse::Success(sub) => sub,
            WeasylResponse::Error { error } if error.name == "submissionRecordMissing" => {
                return Ok(SubmissionResult::Fetched(Submission {
                    id: None,
                    site: self.site(),
                    submission_id: id.to_string(),
                    deleted: true,
                    posted_at: None,
                    link: format!("https://www.weasyl.com/view/{id}"),
                    title: None,
                    artists: Vec::new(),
                    tags: Vec::new(),
                    description: None,
                    rating: None,
                    media: Vec::new(),
                    retrieved_at: Some(chrono::Utc::now()),
                    extra: Some(json),
                }))
            }
            WeasylResponse::Error { error } => {
                return Ok(SubmissionResult::Error {
                    site: self.site(),
                    submission_id: id.to_string(),
                    message: Some(error.name),
                })
            }
        };

        if sub.submitid.to_string() != id {
            return Ok(SubmissionResult::Error {
                site: self.site(),
                submission_id: id.to_string(),
                message: Some(format!("site returned id {}, expected {id}", sub.submitid)),
            });
        }

        let mut submission_media = Vec::with_capacity(1);
        for media in sub.media.remove("submission").unwrap_or_default() {
            tracing::debug!(id = media.mediaid, "processing media");
            let image =
                match process_file(&self.ctx, Some(media.mediaid.to_string()), &media.url).await {
                    Ok(image) => image,
                    Err(err) => {
                        return Ok(SubmissionResult::Error {
                            site: self.site(),
                            submission_id: id.to_string(),
                            message: Some(err.to_string()),
                        })
                    }
                };
            submission_media.push(image);
        }

        let submission = Submission {
            id: None,
            site: self.site(),
            submission_id: id.to_string(),
            deleted: false,
            posted_at: Some(sub.posted_at),
            link: sub.link,
            title: Some(sub.title),
            artists: vec![Artist {
                link: Some(format!("https://www.weasyl.com/~{}", sub.owner_login)),
                site_artist_id: sub.owner_login.clone(),
                name: sub.owner_login,
            }],
            tags: sub.tags,
            description: Some(sub.description),
            rating: Some(sub.rating.normalized()),
            media: submission_media,
            retrieved_at: Some(chrono::Utc::now()),
            extra: Some(json),
        };

        Ok(SubmissionResult::Fetched(submission))
    }
}

#[async_trait]
impl LoadableSite for Weasyl {
    fn site(&self) -> Site {
        Site::Weasyl
    }

    #[tracing::instrument(skip(self))]
    async fn load(&self, id: &str) -> eyre::Result<SubmissionResult> {
        self.load_submission(id).await
    }
}

#[derive(Debug, Deserialize)]
#[serde(untagged)]
enum WeasylResponse<T> {
    Error { error: WeasylError },
    Success(T),
}

#[derive(Debug, Deserialize)]
struct WeasylError {
    name: String,
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "snake_case")]
enum WeasylRating {
    General,
    Mature,
    Explicit,
}

impl WeasylRating {
    fn normalized(&self) -> Rating {
        match self {
            WeasylRating::General => Rating::General,
            WeasylRating::Mature => Rating::Mature,
            WeasylRating::Explicit => Rating::Adult,
        }
    }
}

#[derive(Debug, Deserialize)]
struct WeasylSubmission {
    pub submitid: i32,
    pub title: String,
    pub owner_login: String,
    pub media: HashMap<String, Vec<WeasylMedia>>,
    pub description: String,
    pub posted_at: chrono::DateTime<chrono::Utc>,
    pub tags: Vec<String>,
    pub link: String,
    pub rating: WeasylRating,
}

#[derive(Debug, Deserialize)]
struct WeasylMedia {
    pub url: String,
    pub mediaid: i32,
}