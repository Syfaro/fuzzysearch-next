use std::{collections::HashMap, sync::Arc};

use async_trait::async_trait;
use fuzzysearch_common::{Artist, Rating, Site, Submission};
use serde::Deserialize;

use crate::{
    sites::{process_file, LoadableSite, SubmissionResult},
    SiteContext,
};

const INVALID_ARTISTS: &[&str] = &[
    "unknown_artist",
    "conditional_dnp",
    "anonymous_artist",
    "sound_warning",
];

pub struct E621 {
    ctx: Arc<SiteContext>,
    authorization: (String, String),
}

impl E621 {
    pub fn new(ctx: Arc<SiteContext>, login: String, api_token: String) -> Arc<Self> {
        Arc::new(Self {
            ctx,
            authorization: (login, api_token),
        })
    }

    async fn load_submission(&self, id: &str) -> eyre::Result<SubmissionResult> {
        let resp = match self
            .ctx
            .client
            .get(format!("https://e621.net/posts/{id}.json"))
            .basic_auth(&self.authorization.0, Some(&self.authorization.1))
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

        let link = format!("https://e621.net/posts/{id}");

        let post = match serde_json::from_value(json.clone()) {
            Ok(E621Resp { post: Some(post) }) => post,
            Ok(_resp) => {
                return Ok(SubmissionResult::Fetched(Submission {
                    id: None,
                    site: self.site(),
                    submission_id: id.to_string(),
                    deleted: true,
                    posted_at: None,
                    link,
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
            Err(err) => {
                return Ok(SubmissionResult::Error {
                    site: self.site(),
                    submission_id: id.to_string(),
                    message: Some(err.to_string()),
                })
            }
        };

        let mut submission = Submission {
            id: None,
            site: self.site(),
            submission_id: id.to_string(),
            deleted: false,
            posted_at: Some(post.created_at),
            link,
            title: None,
            artists: post
                .tags
                .clone()
                .into_iter()
                .filter(|(category, _tags)| category == "artist")
                .flat_map(|(_category, tags)| tags.into_iter())
                .filter(|tag| !INVALID_ARTISTS.contains(&tag.as_str()))
                .map(|tag| Artist {
                    site_artist_id: tag.clone(),
                    name: tag,
                    link: None,
                })
                .collect(),
            tags: post.tags.into_values().flatten().collect(),
            description: post.description,
            rating: Some(post.rating.normalized()),
            media: Vec::with_capacity(1),
            retrieved_at: Some(chrono::Utc::now()),
            extra: Some(json),
        };

        if let Some(file_url) = post.file.url {
            let media = match process_file(&self.ctx, None, &file_url).await {
                Ok(media) => media,
                Err(err) => {
                    return Ok(SubmissionResult::Error {
                        site: self.site(),
                        submission_id: id.to_string(),
                        message: Some(err.to_string()),
                    })
                }
            };

            submission.media.push(media);
        }

        Ok(SubmissionResult::Fetched(submission))
    }
}

#[async_trait]
impl LoadableSite for E621 {
    fn site(&self) -> Site {
        Site::E621
    }

    #[tracing::instrument(skip(self))]
    async fn load(&self, id: &str) -> eyre::Result<SubmissionResult> {
        self.load_submission(id).await
    }
}

#[derive(Debug, Deserialize)]
struct E621Resp {
    post: Option<E621Post>,
}

#[derive(Debug, Deserialize)]
struct E621Post {
    created_at: chrono::DateTime<chrono::Utc>,
    rating: E621Rating,
    file: E621PostFile,
    tags: HashMap<String, Vec<String>>,
    description: Option<String>,
}

#[derive(Debug, Deserialize)]
enum E621Rating {
    #[serde(rename = "s")]
    Safe,
    #[serde(rename = "q")]
    Questionable,
    #[serde(rename = "e")]
    Explicit,
}

impl E621Rating {
    pub fn normalized(&self) -> Rating {
        match self {
            Self::Safe => Rating::General,
            Self::Questionable => Rating::Mature,
            Self::Explicit => Rating::Adult,
        }
    }
}

#[derive(Debug, Deserialize)]
struct E621PostFile {
    url: Option<String>,
}
